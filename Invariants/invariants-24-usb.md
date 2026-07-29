# BedrockOS Invariants — USB/xHCI Host Controller Driver

**Version:** 0.1.0
**Date:** 2026-07-29
**Source paths:**
- `kernel/src/usb/mod.rs` — crate root
- `kernel/src/usb/dma.rs` — DMA allocator
- `kernel/src/usb/usb/mod.rs` — USB protocol constants and `SetupPacket`
- `kernel/src/usb/usb/descriptors.rs` — descriptor structs and parsers
- `kernel/src/usb/xhci/mod.rs` — controller init, MSI-X/INTx fallback, device enumeration
- `kernel/src/usb/xhci/registers.rs` — MMIO register map, `PortRegisterSet`, ERST/EventRing
- `kernel/src/usb/xhci/memory.rs` — TRB ring, `Trb`, `InputControlContext`, TRB constructors
- `kernel/src/usb/xhci/event.rs` — event ring consumption, IRQ handler, command completion atomics
- `kernel/src/usb/xhci/command.rs` — doorbell ring, command submission, timeout wait
- `kernel/src/usb/xhci/device.rs` — `UsbDevice`, `UsbDeviceManager`, enumeration
- `kernel/src/usb/xhci/ports.rs` — port state machine, reset, status change handling

---

## DMA Allocator (`kernel/src/usb/dma.rs`)

**USB-001** The USB DMA VMM region is at `USB_VMM_VADDR = 0xFFFFFF7F70000000`, 512 MiB below the AHCI VMM region (`0xFFFFFF7FB0000000`) and 1280 MiB below `KERNEL_VMA_BASE`. The floor is `USB_VMM_VADDR - 0x2000_0000` (512 MiB of virtual address space).

**USB-002** Allocations grow downward from `USB_VMM_VADDR`: each allocation subtracts its page-aligned size from `next_vaddr` and fails if the result would fall below `vaddr_floor`.

**USB-003** `map_mmio()` maps physical MMIO regions with `PageFlags::READ | PageFlags::WRITE | PageFlags::NO_CACHE` and returns the virtual address. Failure modes: address space exhausted (overflow below floor).

**USB-004** `alloc_page()` allocates one physical frame via `BitmapAllocator::alloc()`, maps it with NO_CACHE, zeroes it, and returns a `DmaBuffer { phys, virt, size: 4096 }`. Returns `None` on OOM.

**USB-005** `alloc_contiguous(count)` allocates `count` contiguous physical frames, maps them, zeroes them, and returns a `DmaBuffer { phys, virt, size: count * 4096 }`. Returns `None` on OOM.

**USB-006** `UsbDmaAllocator` is `unsafe impl Send` because it wraps raw pointers (`root`, `alloc`). The caller must ensure that only one thread uses the allocator at a time (or provides external synchronisation).

**USB-007** The allocator's `VMM::map()` uses the caller-supplied `root` page-table root and `BitmapAllocator` for page-table frame allocation, ensuring all mappings are visible in the address space used by the xHCI DMA.

---

## USB Protocol (`kernel/src/usb/usb/mod.rs`)

**USB-008** Standard USB request types, recipient fields, and direction bits follow the USB 2.0/3.0 specification. `SetupPacket` encodes a USB control transfer setup stage.

**USB-009** `SetupPacket::get_descriptor()` sets direction to device-to-host, type to standard, recipient to device. Valid for any descriptor type.

**USB-010** `SetupPacket::set_address()` sets direction to host-to-device. The address is passed via `w_value`; `w_length` is 0 (no data stage).

**USB-011** `SetupPacket::clear_feature()` and `set_feature()` accept a `recip` parameter to target device, interface, or endpoint as appropriate; the caller selects the correct recipient.

**USB-012** Speed constants (`SPEED_LS=1`, `SPEED_FS=2`, `SPEED_HS=3`, `SPEED_SS=4`) map to xHCI PORTSC speed field values.

---

## USB Descriptors (`kernel/src/usb/usb/descriptors.rs`)

**USB-013** All descriptor parse functions (`DeviceDescriptor::parse`, `ConfigDescriptor::parse`, `InterfaceDescriptor::parse`, `EndpointDescriptor::parse`, `SsEndpointCompanionDescriptor::parse`) verify:
- Buffer length >= `size_of::<T>()`
- `b_length` field >= `size_of::<T>()` (first byte length check)
- `b_descriptor_type` matches the expected type

If any check fails, `None` is returned.

**USB-014** Parsed descriptors are obtained via `unsafe { &*(data.as_ptr() as *const T) }`. This is safe because the length checks above guarantee the buffer is large enough and the type byte is correct.

**USB-015** Field accessors for packed struct members use `core::ptr::addr_of!(self.field).read_unaligned()` to avoid undefined behaviour from misaligned loads on packed structs.

**USB-016** `ConfigDescriptor::total_length()` returns `u16::from_le(...)` to handle little-endian wire format correctly on big-endian hosts (presently no BE targets, but correct by spec).

**USB-017** `parse_config_descriptors()` walks a configuration descriptor blob by iterating `(length, type)` pairs, parsing each recognised descriptor. Malformed data is silently skipped (length < 2, offset + length > limit, unknown type).

---

## xHCI Registers (`kernel/src/usb/xhci/registers.rs`)

**XHCI-001** `XhciRegisters` maps the first 64 KiB (`MMIO_SIZE = 0x10000`) of the xHCI BAR0 via `UsbDmaAllocator::map_mmio()` with NO_CACHE. All register accesses use `read_volatile`/`write_volatile`.

**XHCI-002** The `caplength` register (at MMIO base + 0) determines the offset to the Operational registers. The `op_base = mmio_va + caplength` and all OP register offsets are relative to `op_base`.

**XHCI-003** `rts_off` is read from MMIO + 0x18 (RTSOFF), masked to align to 32 bytes. If non-zero it indicates the Runtime register offset; a zero value defaults to offset `0x8000`.

**XHCI-004** `dboff` is read from MMIO + 0x14 (DBOFF), masked to align to 4 bytes. The Doorbell array is at `mmio_va + dboff`.

**XHCI-005** OP register offsets (`OP_USBCMD=0x00`, `OP_USBSTS=0x04`, `OP_CRCR=0x18`, `OP_DCBAAP=0x30`, `OP_CONFIG=0x38`) are defined as constants relative to the Operational register base.

**XHCI-006** `HcsParams1::max_slots()` returns bits [0:8], `max_intrs()` returns bits [8:19], `max_ports()` returns bits [24:32]. These are read from capability register HCSPARAMS1 at MMIO + 0x04.

**XHCI-007** `HcsParams2::scratchpad_bufs()` extracts a 10-bit field from bits [21:31] of HCSPARAMS2 (MMIO + 0x08).

**XHCI-008** `HccParams1::ac64()` checks bit 0 (64-bit addressing capability), `csz()` checks bit 2 (context size = 64 bytes vs 32), `xecp()` returns the extended capability pointer (bits [16:31] << 2) from HCCPARAMS1 (MMIO + 0x10).

**XHCI-009** `PortRegisterSet` computes PORTSC offsets as `caplength + 0x400 + (port_num - 1) * 16`. PORTSC is 4 bytes wide per port, with 12 bytes reserved between each.

**XHCI-010** `Erst::new()` allocates one DMA page and zeroes only the first 16 bytes (one ERST segment entry: 8 bytes segment address low/high, 4 bytes size, 4 bytes reserved).

**XHCI-011** `EventRing::new()` allocates 256 TRBs (4096 bytes, 1 page) via `alloc_contiguous`, zeroes it, writes the segment pointer into the ERST segment at `erst_seg_va`. The `trb_count` field is set to 256, `dequeue_index` to 0.

**XHCI-012** PORTSC bit masks are validated:
- `PORTSC_CCS` (bit 0): current connect status
- `PORTSC_PED` (bit 1): port enabled/disabled
- `PORTSC_PR` (bit 4): port reset
- `PORTSC_PP` (bit 9): port power
- `PORTSC_SPEED_MASK` (bits 10-13): port speed
- `PORTSC_STATUS_BITS` (bits 17-23): all change bits (CSC, PEC, WRC, OCC, PRC, PLC, CEC) — these are RW1C

---

## xHCI Memory / TRB Ring (`kernel/src/usb/xhci/memory.rs`)

**XHCI-013** A `TrbRing` is a contiguous DMA buffer of 16-byte TRB entries. When created with `TrbRing::new(dma, size)`, the buffer is zeroed and the initial cycle bit is 1.

**XHCI-014** The ring has `trb_count = (buffer_size / 16) - 1` entries. The last slot in physical memory is reserved for the Link TRB and is never used for data. The `trb_count` field is the count of usable entries.

**XHCI-015** `enqueue()` writes the TRB fields with the current cycle bit OR'd into `control`, advances `enqueue_index`, and wraps via a Link TRB when `enqueue_index >= trb_count`. The Link TRB is written *before* the wrap so the controller sees a valid TRB chain.

**XHCI-016** The Link TRB is written with:
- `parameter = self.phys` (ring base physical address)
- `status = 0`
- `control = (TRB_TYPE_LINK << 10) | LINK_TOGGLE_CYCLE | TRB_TC | self.cycle`

`LINK_TOGGLE_CYCLE` toggles the cycle bit on the link target; `TRB_TC` (Toggle Cycle) instructs the controller to toggle the cycle bit when following the link.

**XHCI-017** After writing a Link TRB and wrapping, `enqueue_index` resets to 0 and `cycle ^= 1`. This implements the producer/consumer cycle bit protocol where the producer writes with cycle=1 on the first pass and cycle=0 on the second.

**XHCI-018** `enqueue_raw()` has the same behaviour as `enqueue()` but takes raw field values instead of a `Trb` struct. The caller must provide the cycle bit in `control`; the function OR's `self.cycle` into the written control word.

**XHCI-019** `flush()` is a no-op (the TRB ring is kept in WC/NO_CACHE memory and writes are ordered by the xHC doorbell).

**XHCI-020** TRB type constants (1-23) follow the xHCI specification. Event TRB types (32-47) are identified by `Trb::is_event()` which checks `trb_type() >= 32 && trb_type() <= 47`.

**XHCI-021** `InputControlContext` has 8 `drop_flags` + `add_flags` dwords, 6 reserved dwords, an 8-dword slot context, and 31 × 8-dword endpoint contexts. Total size is `(2 + 6 + 8 + 31*8) * 4 = 1056` bytes (not page-aligned; callers must place it in DMA memory).

**XHCI-022** TRB constructor functions:
- `make_setup_stage_trb(setup, trt)`: IDT=1 (immediate data), IOC=1
- `make_data_stage_trb(phys, len, dir_in)`: CHAIN=1, DIR_IN if dir_in
- `make_status_stage_trb(dir_in)`: DIR_IN if dir_in, IOC=1
- `make_enable_slot_trb()`: no special flags
- `make_address_device_trb(input_ctx_phys, slot_id, bsr)`: BSR if bsr, IOC=1, slot_id in bits 24-31
- `make_configure_endpoint_trb(input_ctx_phys, slot_id, deconfigure)`: DC if deconfigure, IOC=1, slot_id in bits 24-31
- `make_evaluate_context_trb(ctx_phys, slot_id)`: IOC=1, slot_id in bits 24-31
- `make_normal_trb(data_phys, len, slot_id, endpoint_id)`: IOC=1, slot_id in bits 24-31, endpoint_id in bits 16-23
- `make_no_op_command_trb()`: IOC=1

---

## xHCI Event Ring (`kernel/src/usb/xhci/event.rs`)

**XHCI-023** The event ring state is stored in `AtomicU64`/`AtomicU32`/`AtomicU16` statics, not in a struct, because the IRQ handler (`xhci_irq_handler`) runs in interrupt context with no `&mut self` access.

**XHCI-024** `XHCI_IRQ_COUNT` is monotonically increasing. `irq_count()` reads it with `Ordering::Relaxed`. It is incremented once per `xhci_irq_handler()` invocation.

**XHCI-025** `set_event_ring_info()` stores the virtual address, physical address, TRB count, and initial dequeue index. These are set once during controller init and never modified thereafter (except `XHCI_ER_DEQUEUE` and `XHCI_ER_CYCLE` which track consumption state).

**XHCI-026** `consume_pending_events()` reads events from the ring until the cycle bit does not match the expected value, then updates the dequeue index and ERDP register. The ERDP register is updated every time, even when no events are consumed, to clear spurious interrupt conditions.

**XHCI-027** Command completion events (TRB type 33) atomically publish `(slot_id, cc, param)` to `LAST_CMD_STATE` with a seen flag (bit 63) using `Ordering::Release`. Port status change events (type 34) are printed via serial. Host controller events (type 37) and transfer events (type 32) are recognised but not processed.

**XHCI-028** After consuming events, ERDP is updated to the current dequeue position with `ERDP = paddr + dequeue_index * 16 | (1 << 3)`. The `1 << 3` (ERDP_BUSY) bit tells the xHC that the driver has consumed events and the controller may write new ones.

**XHCI-029** IMAN IP (Interrupt Pending) is cleared by writing `(iman & IMAN_IE) | 1` (preserving the IE bit, setting the IP=1 clear bit). This is done both in `drain_pending_and_clear_intr()` and in `consume_pending_events()` to prevent interrupt storms from unacknowledged IP.

**XHCI-030** `xhci_irq_handler()` increments `XHCI_IRQ_COUNT`, calls `idt::verify_integrity()`, clears USBSTS EINT (bit 3) by writing `1 << 3` to OP_USBSTS, then calls `consume_pending_events()`. It does NOT explicitly EOI (the LAPIC auto-EOI or the OS's EOI at the end of the IDT handler handles that).

**XHCI-031** `drain_pending_and_clear_intr()` is a synchronous version of the IRQ handler: it calls `consume_pending_events()`, clears USBSTS EINT, and clears IMAN IP. This is used during init before interrupts are enabled.

**XHCI-032** `last_command_completion()` atomically reads and clears `LAST_CMD_STATE` with `Ordering::AcqRel`. If bit 63 is set, it returns `Some((slot_id, cc, param))`; otherwise `None`. This is a single-consumer API — calling twice on the same completion loses the second read.

**XHCI-033** `read_event_completion_at(trb_va)` reads a raw event TRB from a known virtual address and returns `(param, completion_code, slot_id, trb_type)`. This is used for diagnostic polling in the IRQ verification path.

---

## xHCI Command Doorbell (`kernel/src/usb/xhci/command.rs`)

**XHCI-034** `ring_doorbell(doorbell_va, slot_id, target)` writes the doorbell register at `doorbell_va + slot_id * 4`. For the command doorbell, slot_id=0, target=0.

**XHCI-035** `wait_for_completion()` polls with a 5-second `ApicTimeout`. It alternates between checking `last_command_completion()` and calling `consume_pending_events()`. On timeout, returns `Err("command completion timeout")`. On a completion code != 1 (success), returns `Err("command failed")`.

**XHCI-036** All command submission functions follow the same pattern:
1. Build the TRB via a constructor from `memory.rs`
2. `cmd_ring.enqueue(&trb)`
3. `cmd_ring.flush()` (no-op)
4. `ring_command_doorbell(doorbell_va)`
5. `wait_for_completion()`

Functions: `submit_enable_slot`, `submit_address_device`, `submit_configure_endpoint`, `submit_evaluate_context`, `submit_no_op`.

**XHCI-037** The doorbell write ordering ensures the TRB write is visible to the xHC before the doorbell is rung because `map_mmio` uses NO_CACHE (uncached, strongly-ordered) and the doorbell write is also uncached. No memory barrier is needed between `enqueue` and the doorbell write.

---

## xHCI Port Management (`kernel/src/usb/xhci/ports.rs`)

**XHCI-038** `UsbPort` tracks `port_num`, `connected`, `enabled`, `speed`, `resetting`. Initial state: all false/0.

**XHCI-039** `UsbPorts::init_ports()` iterates all ports. For each port:
- If PORTSC_PP (port power) is 0, writes PP=1 and waits 10 ms
- If PORTSC_CCS (connect status) is set, marks connected=true, extracts speed from bits 10-13
- For USB 2.0 and below (speed != 4/SS), calls `reset_port_by_idx()`
- Writes back PORTSC with PED masked out to clear RW1C change bits (17-23) without accidentally clearing PED

**XHCI-040** `reset_port_by_idx()`:
1. Sets `resetting = true`
2. Writes PORTSC with PR=1, masking out PED and all status bits to avoid RW1C side effects
3. Polls for PR=0 with 500 ms timeout
4. After reset completes, checks PED: if enabled, sets `enabled=true` and re-reads speed
5. Sets `resetting = false`

**XHCI-041** PORTSC_PED is an RW1C (Read-Write-1-to-Clear) bit. Writing 1 to it disables the port. All PORTSC writes that intend to modify other bits MUST mask PED out: `portsc & !PORTSC_PED`. The same caution applies to the status change bits (PORTSC_STATUS_BITS).

**XHCI-042** `handle_port_status_change()` processes connect/disconnect events:
- Connect: records speed, calls `reset_port_by_idx()`
- Disconnect: clears `connected`, `enabled`, `speed`
- If disconnected but enabled, clears `enabled`
- After processing, re-reads PORTSC to capture any new change bits from the reset, then clears status bits (with PED masked)

**XHCI-043** `reset_port()` and `port_speed()` are public convenience wrappers. `port_count()` returns the number of ports in the vector.

---

## xHCI Device Management (`kernel/src/usb/xhci/device.rs`)

**XHCI-044** `UsbDevice` stores slot_id, port_num, speed, address, vendor/product IDs, USB/bcd versions, max_packet_size0, and num_configs. `UsbDevice::new()` derives `max_packet_size0` from the speed: LS=8, FS=64, HS=64, SS=512.

**XHCI-045** `UsbDeviceManager` maintains a `Vec<UsbDevice>` and a `next_address` counter starting at 1.

**XHCI-046** `enumerate_port()` assigns `slot_id = devices.len() + 1` (capped at 31), allocates the next sequential address, logs the device info, and pushes a new `UsbDevice`. The slot_id allocation is 1-based; slot 0 is reserved by the xHCI spec.

**XHCI-047** `allocate_dcbaa()` is a convenience duplicate of the same function in `xhci/mod.rs`. It allocates contiguous DMA pages for the Device Context Base Address Array, sized for `(max_slots + 1) * 8` bytes.

---

## Controller Init (`kernel/src/usb/xhci/mod.rs`)

**XHCI-048** `init_all()` iterates all PCI devices, filtering for class=0x0C, subclass=0x03, prog_if=0x30 (xHCI USB controller). For each match, it creates a `UsbDmaAllocator` and calls `init_controller()`.

**XHCI-049** `init_controller()` init sequence:
1. Read BAR0, validate it is memory-mapped
2. `XhciRegisters::new()` maps MMIO
3. Read capability registers (HCSPARAMS1/2, HCCPARAMS1)
4. Parse extended capabilities (USB Legacy Support, Supported Protocol)
5. `controller_reset()` — assert HCRST, poll for self-clearing with 500 ms timeout
6. Allocate DCBAA and write DCBAAP
7. Write CONFIG (max slots)
8. Allocate command TRB ring and write CRCR
9. Allocate ERST and event ring; write ERSTSZ, ERSTBA, ERDP to runtime registers
10. Set event ring globals (`set_event_ring_info`, `set_erdp_register_va`, `set_op_base_va`)
11. `setup_interrupts()` — enable MSI-X (or MSI, or INTx fallback)
12. Arm interrupter (IMAN.IE=1), set IMOD=0
13. Write USBCMD = RUN | INTE | HSEE
14. Poll USBSTS HCH=0 with 500 ms timeout
15. `drain_pending_and_clear_intr()`
16. `UsbPorts::init_ports()` — port power, reset, clear change bits
17. `drain_pending_and_clear_intr()` again
18. Diagnostic dump of USBSTS, IMAN, PORTSC1, ERDP
19. `enumerate_initial_ports()` — log connected devices
20. `verify_message_interrupt_delivery()` — send No-Op command, verify IRQ

**XHCI-050** `controller_reset()` sets USBCMD_HCRST and polls until it self-clears (500 ms timeout). During reset the xHCI does not respond to operational register writes.

**XHCI-051** `alloc_dcbaa()` allocates `(max_slots + 1) * 8` bytes rounded up to pages, zeroes them, and returns a `DmaBuffer`.

**XHCI-052** `alloc_scratchpad_array()` allocates `spbuf_cnt` entries (one per scratchpad buffer). Each entry is either 8 bytes (ac64) or 4 bytes (not ac64). Each scratchpad buffer is a separate `alloc_page()`. The array is zeroed; then each entry is filled with the physical address of its respective buffer.

**XHCI-053** The DCBAAP register is written as two 32-bit writes when `ac64=true` (low then high 32 bits of the physical address). The same dual-write pattern is used for CRCR, ERSTBA, and ERDP. This is necessary on 32-bit registers that compose a 64-bit value.

**XHCI-054** `setup_interrupts()` follows this priority:
1. **MSI-X** (preferred): register IDT handler, get MSI-X table info, map non-BAR0 MSI-X BAR via `dma.map_mmio()`, disable MSI if present, call `msix::enable()`. Returns `MsixFallback` with the MSI capability info for fallback.
2. **MSI** (fallback if no MSI-X): register IDT handler, call `msi::enable()`. Returns `None`.
3. **INTx** (last resort): if `interrupt_line != 0`, register IDT handler, call `ioapic::enable_irq()` with ActiveLow/Level trigger. Returns `None`.

**XHCI-055** IMAN.IP clear ordering: MSI-X is enabled *before* the interrupter is armed (IMAN.IE=1) and before the controller is started (USBCMD.RUN=1). This ensures the xHC has a valid interrupt target before interrupts can be generated.

**XHCI-056** `verify_message_interrupt_delivery()`:
1. Record pre-command `irq_count()`
2. Enqueue No-Op command TRB and ring doorbell
3. Wait for IRQ with `halt()` (to avoid starving QEMU TCG) with 100 ms timeout
4. If IRQ fired — success
5. If no IRQ but event completed (polling recovered it):
   - Dump LAPIC SVR and TPR for diagnostics
   - If `MsixFallback` available, disable MSI-X and enable MSI, retry No-Op with MSI, log result
6. If no completion at all — log "stuck"
7. Always drain events after the test

**XHCI-057** The `halt()` in the verification loop is critical for QEMU TCG correctness: busy-spinning starves the device emulation scheduler, preventing the MSI-X write from completing. `halt()` yields to the interrupt/device scheduler.

**XHCI-058** `enumerate_initial_ports()` is a stub that iterates `usb_ports.ports`, printing device info for enabled+connected ports. Full enumeration (slot assignment, address device, descriptor fetch) is planned.

---

## Interrupt State Atomics

**XHCI-059** The following global atomics coordinate between the init path (BSP, no interrupts) and the IRQ handler (interrupt context):
- `XHCI_IRQ_COUNT` (AtomicU64) — incremented by IRQ handler, read by init for delivery verification
- `XHCI_ER_VADDR`, `XHCI_ER_PADDR`, `XHCI_ER_SIZE` (AtomicU64/U32) — set once during init, read by IRQ handler
- `XHCI_ER_DEQUEUE` (AtomicU16) — read/written by both init and IRQ handler; the event ring is single-consumer (only one thread calls `consume_pending_events` at a time)
- `XHCI_ER_CYCLE` (AtomicU32) — same access pattern as `XHCI_ER_DEQUEUE`
- `XHCI_RT_VA` (AtomicU64) — set once during init, read by IRQ handler for IMAN/ERDP writes
- `XHCI_OP_VA` (AtomicU64) — set once during init, read by IRQ handler for USBSTS EINT clear
- `LAST_CMD_STATE` (AtomicU64) — written by `consume_pending_events` (IRQ or poll), read-and-clear by `last_command_completion` (init or command submit path). The seen flag (bit 63) prevents stale reads

**XHCI-060** All atomic accesses use `Ordering::Relaxed` except `LAST_CMD_STATE` which uses `Release` (store in consumer) and `AcqRel` (swap in `last_command_completion`).

---

## API Contracts

### `UsbDmaAllocator` (caller must ensure)
- The `alloc: *mut BitmapAllocator` pointer must remain valid for the allocator's lifetime
- `UsbDmaAllocator` is `Send` but not `Sync` — concurrent use from multiple threads requires external synchronisation
- `map_mmio()` consumes virtual address space; overflow is checked and returns `Err`

### `TrbRing` (caller must ensure)
- The ring is writable with NO_CACHE semantics (no caching, writes reach the controller)
- `enqueue()` is not re-entrant; the caller must serialise access
- The ring's DMA memory is not touched by the CPU while the xHC may read it (between enqueue and completion)

### Event handling (caller must ensure)
- `consume_pending_events()` is not re-entrant; the IRQ handler and poll path must not run concurrently
- `last_command_completion()` consumes the last completion state; polling without consuming will see `None`
- The init sequence must not enable interrupts before `set_event_ring_info()`, `set_erdp_register_va()`, and `set_op_base_va()` are called

### Port operations (caller must ensure)
- Port numbers are 1-indexed per xHCI spec; valid range is `1..=max_ports`
- `handle_port_status_change()` should only be called when a port status change event (type 34) is received, not on every poll

---

## Design Notes

- **Why atomic statics instead of a struct**: The IRQ handler cannot access `&self` of the controller struct; it only has a function call. Global atomics provide lock-free access to the event ring state from both the init path and the interrupt handler without requiring a static `Mutex<Controller>`.
- **Why MSI-X before MSI**: MSI-X offers per-vector masking, more entries (typically 2048 vs 32 for MSI), and no PCI config space access to change masks. The fallback to MSI handles QEMU configurations where MSI-X routing is broken but MSI works.
- **Why `halt()` in the IRQ verification loop**: Under QEMU TCG (emulation without KVM), busy-spinning is non-preemptive — the device emulation thread never gets CPU time to complete the MSI write. `halt()` gives the scheduler a chance to run the device model.
- **Why PORTSC_PED must be masked on every write**: PORTSC is a RW1C register. Writing 1 to bit 1 (PED) disables the port. Any write that touches PORTSC must mask PED out unless the intent is to disable the port.
