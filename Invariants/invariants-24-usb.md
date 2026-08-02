# BedrockOS Invariants — USB/xHCI Host Controller Driver

**Version:** 0.4.0
**Date:** 2026-08-02
**Source paths:**
- `kernel/src/usb/mod.rs` — crate root
- `kernel/src/usb/usb/mod.rs` — USB protocol constants and `SetupPacket`
- `kernel/src/usb/usb/descriptors.rs` — descriptor structs and parsers
- `kernel/src/usb/xhci/mod.rs` — controller init, MSI-X/INTx fallback, device enumeration
- `kernel/src/usb/xhci/registers.rs` — MMIO register map, `PortRegisterSet`, ERST/EventRing
- `kernel/src/usb/xhci/memory.rs` — TRB ring, `Trb`, `InputControlContext`, TRB constructors
- `kernel/src/usb/xhci/context.rs` — `InputControlContext` / endpoint context builders
- `kernel/src/usb/xhci/event.rs` — event ring consumption, IRQ handler, command/transfer completion atomics, SPSC port-change ring
- `kernel/src/usb/xhci/command.rs` — doorbell ring, command submission, timeout wait
- `kernel/src/usb/xhci/device.rs` — `DeviceSlot`, `DeviceSlotManager`, control/bulk/interrupt transfers, enumeration
- `kernel/src/usb/xhci/ports.rs` — port state machine, reset, status change handling
- `kernel/src/usb/class/driver.rs` — USB class-driver registry (`UsbClassDriver`, `InterfaceResources`, `EndpointResource`, `find_driver`)
- `kernel/src/usb/class/mass_storage.rs` — USB Bulk-Only Transport mass-storage driver + `MassStorageDriver`
- `kernel/src/usb/class/hid.rs` — USB HID boot-keyboard driver (`HidDriver`)
- `make_demo_drive.py` — script to build a QEMU demo USB drive image

---

## DMA Allocator (`kernel/src/services/dma.rs`)

xHCI uses the kernel-wide DMA allocator, `services::dma::KernelDma`, obtained
via `kernel_services().dma`. There is no USB-specific allocator.

**USB-001** All DMA (MMIO, rings, contexts, data buffers) is allocated from the single `KernelDma` VMM window: `KERNEL_VMA_BASE - 0x5000_0000` down to `- 0x2000_0000` (512 MiB), directly below the PCI ECAM window. The pool is shared with AHCI; allocations grow downward.
- Location: `kernel/src/services/dma.rs:23-30`

**USB-002** Allocations grow downward from the window base: each allocation subtracts its page-aligned size from `next_vaddr` and fails if the result would fall below `vaddr_floor`. The allocator is `&self` with an internal `Mutex`, so drivers share it safely.
- Location: `kernel/src/services/dma.rs:63-87`

**USB-003** `map_mmio()` maps physical MMIO regions with `PageFlags::READ | PageFlags::WRITE | PageFlags::NO_CACHE` and returns the page-aligned virtual address. Failure modes: address space exhausted (overflow below floor).
- Location: `kernel/src/services/dma.rs:97-114`

**USB-004** `alloc_page()` allocates one physical frame via `BitmapAllocator::alloc()`, maps it with NO_CACHE, zeroes it, and returns a `DmaBuffer { phys, virt, size: 4096 }`. Returns `None` on OOM.

**USB-005** `alloc_contiguous(count)` allocates `count` contiguous physical frames, maps them, zeroes them, and returns a `DmaBuffer { phys, virt, size: count * 4096 }`. Returns `None` on OOM.

**USB-006** `KernelDma` is `Send + Sync` (methods take `&self` and lock internally), so concurrent drivers share it safely — replacing the old USB-specific `UsbDmaAllocator` (Send-only, `&mut self`).

**USB-007** `virt_to_phys()` translates arbitrary kernel virtual addresses via the shared 64-entry translation cache; xHCI carries `phys` in each `DmaBuffer`, so it only needs this for indirect translations.

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

**XHCI-001** `XhciRegisters` maps the first 64 KiB (`MMIO_SIZE = 0x10000`) of the xHCI BAR0 via `kernel_services().dma.map_mmio()` with NO_CACHE. All register accesses use `read_volatile`/`write_volatile`.

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

**XHCI-019** **A Link TRB is written at allocation time**: `TrbRing::new()` writes a valid Link TRB at the segment end (`trb_count - 1`) with cycle=1 before returning. `write_link_trb()` is the single helper for both allocation-time and wrap-time Link writes.

**XHCI-020** `flush()` issues a `fence(Ordering::SeqCst)` — it is no longer a no-op. The ordering barrier is what makes TRB writes visible before the doorbell write on all paths (the NCU/UC doorbell access alone is insufficient on some implementations).

**XHCI-021** TRB type constants (1-23) follow the xHCI specification. Event TRB types (32-47) are identified by `Trb::is_event()` which checks `trb_type() >= 32 && trb_type() <= 47`.

**XHCI-022** `InputControlContext` has 8 `drop_flags` + `add_flags` dwords, 6 reserved dwords, an 8-dword slot context, and 31 × 8-dword endpoint contexts. Total size is `(2 + 6 + 8 + 31*8) * 4 = 1056` bytes (not page-aligned; callers must place it in DMA memory).

**XHCI-023** TRB constructor functions:
- `make_setup_stage_trb(setup, trt)`: IDT=1 (immediate data); TRT (bits 16-17) = 0/2/3 depending on data-stage presence/direction. **No IOC, no CHAIN** (chaining is applied by the caller when a data stage follows).
- `make_data_stage_trb(phys, len, dir_in)`: DIR_IN if dir_in. **No CHAIN** — chaining is caller-managed.
- `make_status_stage_trb(dir_in)`: DIR_IN if **NOT** dir_in (direction flips for the status stage); IOC=1.
- `make_enable_slot_trb()`: no special flags.
- `make_address_device_trb(input_ctx_phys, slot_id, bsr)`: BSR if bsr (`TRB_BSR = 1 << 9`), slot_id in bits 24-31.
- `make_configure_endpoint_trb(input_ctx_phys, slot_id, deconfigure)`: DC if deconfigure (`TRB_DC = 1 << 9`), slot_id in bits 24-31.
- `make_evaluate_context_trb(ctx_phys, slot_id)`: slot_id in bits 24-31.
- `make_normal_trb(data_phys, len)`: IOC=1, length masked to 17 bits. **No slot_id/endpoint_id params** — those are passed via the doorbell.
- `make_no_op_command_trb()`: IOC=1.

`TRB_DIR_IN = 1 << 16`. `TRB_DC` and `TRB_BSR` **share bit 9** — they are mutually exclusive by TRB type.

---

## xHCI Event Ring (`kernel/src/usb/xhci/event.rs`)

**XHCI-024** The event ring state is stored in `AtomicU64`/`AtomicU32`/`AtomicU16` statics, not in a struct, because the IRQ handler (`xhci_irq_handler`) runs in interrupt context with no `&mut self` access.

**XHCI-025** `XHCI_IRQ_COUNT` is monotonically increasing. `irq_count()` reads it with `Ordering::Relaxed`. It is incremented once per `xhci_irq_handler()` invocation.

**XHCI-026** `set_event_ring_info()` stores the virtual address, physical address, TRB count, and initial dequeue index. These are set once during controller init and never modified thereafter (except `XHCI_ER_DEQUEUE` and `XHCI_ER_CYCLE` which track consumption state).

**XHCI-027** `consume_pending_events()` reads events from the ring until the cycle bit does not match the expected value, then updates the dequeue index and ERDP register. The ERDP register is updated every time, even when no events are consumed, to clear spurious interrupt conditions. The dequeue index wraps via `% er_trb_count` and toggles `expected_cycle` on wraparound.

**XHCI-028** Event dispatch inside `consume_pending_events()`:
- **33 (Command Completion)** → publishes `(slot_id, cc, param)` into `LAST_CMD_STATE` with the seen flag (bit 63) via `Ordering::Release`.
- **34 (Port Status Change)** → pushes the port id (bits 24-31 of param) into the SPSC `PORT_EVENTS` ring (see XHCI-029).
- **32 (Transfer Event)** → publishes `(slot_id, ep_id, cc, remaining)` into `LAST_TRANSFER_STATE` with the seen flag (bit 63).
- **37 (Host Controller)** → traced only.
- Everything is gated by the `usb_trace` feature macro for serial output.

**XHCI-029** **Port-change events use a lock-free SPSC ring**: `PORT_EVENTS: [AtomicU8; 64]` with `PORT_EVENTS_HEAD`/`PORT_EVENTS_TAIL` (`AtomicU64`). The ISR is the only producer, the BSP init path the only consumer (same CPU). A full ring drops the event (the init drain re-reads PORTSC anyway). `take_port_change()` pops one id; `port_change_pending()` reports non-emptiness. This is the basis of **event-driven port detection** — no per-port polling loop.

**XHCI-030** After consuming events, ERDP is updated to the current dequeue position with `ERDP = paddr + dequeue_index * 16 | (1 << 3)`. The `1 << 3` (ERDP_BUSY) bit tells the xHC that the driver has consumed events and the controller may write new ones.

**XHCI-031** IMAN IP (Interrupt Pending) is cleared by writing `(iman & IMAN_IE) | 1` (preserving the IE bit, setting the IP=1 clear bit). This is done both in `drain_pending_and_clear_intr()` and in `consume_pending_events()` to prevent interrupt storms from unacknowledged IP.

**XHCI-032** `xhci_irq_handler()` increments `XHCI_IRQ_COUNT`, calls `idt::verify_integrity()`, clears USBSTS EINT (bit 3) by writing `1 << 3` to OP_USBSTS, then calls `consume_pending_events()`. It does NOT explicitly EOI (the IDT device-interrupt wrapper sends EOI after dispatch).

**XHCI-033** `drain_pending_and_clear_intr()` is a synchronous version of the IRQ handler: it calls `consume_pending_events()`, clears USBSTS EINT, and clears IMAN IP. This is used during init before interrupts are enabled.

**XHCI-034** `last_command_completion()` atomically reads and clears `LAST_CMD_STATE` with `Ordering::AcqRel`. If bit 63 is set, it returns `Some((slot_id, cc, param))`; otherwise `None`. This is a single-consumer API — calling twice on the same completion loses the second read. **`peek_last_command_completion()` is the non-destructive variant** used by wait-loop predicates (it must not consume the event before the caller reads it). `last_transfer_completion()` / `peek_last_transfer_completion()` mirror this for transfer events.

**XHCI-035** `read_event_completion_at(trb_va)` reads a raw event TRB from a known virtual address and returns `(param, completion_code, slot_id, trb_type)`. This is used for diagnostic polling in the IRQ verification path.

---

## xHCI Command Doorbell (`kernel/src/usb/xhci/command.rs`)

**XHCI-036** `ring_doorbell(doorbell_va, slot_id, target)` writes the doorbell register at `doorbell_va + slot_id * 4`. For the command doorbell, slot_id=0, target=0 (`ring_command_doorbell`).

**XHCI-037** `wait_for_completion()` waits with a 5-second deadline via `universal_timer::wait_until_cond` (HLT-based, not a spin). The predicate alternates `consume_pending_events()` and `peek_last_command_completion()`. On timeout returns `Err("command completion timeout")`; on a completion code != 1 (success) returns `Err("command failed")`.

**XHCI-038** All command submission functions follow the same pattern:
1. Build the TRB via a constructor from `memory.rs`
2. `cmd_ring.enqueue(&trb)`
3. `cmd_ring.flush()`
4. `ring_command_doorbell(doorbell_va)`
5. `wait_for_completion()`

Functions: `submit_enable_slot`, `submit_address_device`, `submit_configure_endpoint`, `submit_evaluate_context`, `submit_no_op`.

**XHCI-039** `submit_enable_slot()` **returns the slot id from the command-completion event** — slot IDs are controller-assigned (per xHCI spec), not driver-guessed as `devices.len()+1`.

**XHCI-040** The doorbell write ordering ensures the TRB write is visible to the xHC before the doorbell is rung because `flush()` issues a `SeqCst` fence and `map_mmio` uses NO_CACHE (uncached, strongly-ordered). No additional barrier is needed between `enqueue` and the doorbell write.

---

## xHCI Port Management (`kernel/src/usb/xhci/ports.rs`)

**XHCI-041** `UsbPort` tracks `port_num`, `connected`, `enabled`, `speed`, `resetting`. Initial state: all false/0.

**XHCI-042** `UsbPorts::init_ports()` iterates all ports. For each port:
- If PORTSC_PP (port power) is 0, writes PP=1 and waits **20 ms** (`universal_timer::sleep_ms`) for the power rail to settle.
- Re-reads PORTSC (the pre-power read has a stale CCS); this fresh read is the authoritative power-on state.
- If PORTSC_CCS is set: marks connected, records speed. **USB 2.0 and below (speed != 4/SS) get an explicit reset**; SuperSpeed ports auto-enable on link training (checked via PED).
- Writes back PORTSC with PED masked out to clear RW1C change bits (17-23) without accidentally clearing PED.
- Devices still mid link-training at this point raise a PORT_CHANGE event afterwards — **detection is event-driven** (see XHCI-029), not a poll loop here.

**XHCI-043** `reset_port_by_idx()`:
1. Sets `resetting = true`
2. Writes PORTSC with PR=1, masking out PED and all status bits to avoid RW1C side effects
3. Waits for PR=0 with a 500 ms deadline via `wait_until_cond`
4. After reset completes, checks PED: if enabled, sets `enabled=true` and re-reads speed
5. Sets `resetting = false`

**XHCI-044** PORTSC_PED is an RW1C (Read-Write-1-to-Clear) bit. Writing 1 to it disables the port. All PORTSC writes that intend to modify other bits MUST mask PED out: `portsc & !PORTSC_PED`. The same caution applies to the status change bits (PORTSC_STATUS_BITS).

**XHCI-045** `handle_port_status_change(port_num)` processes connect/disconnect events driven from the SPSC ring:
- Connect: records speed, calls `reset_port_by_idx()`
- Disconnect: clears `connected`, `enabled`, `speed`
- If disconnected but enabled, clears `enabled`
- **Re-reads PORTSC after processing** (the reset may have changed PED and set new change bits); using the stale value would write PED=1 back (RW1C → instant disable) and miss new change bits.

**XHCI-046** `reset_port()` and `port_speed()` are public convenience wrappers. `port_count()` returns the number of ports in the vector.

---

## xHCI Device Management (`kernel/src/usb/xhci/device.rs`)

**XHCI-047** `DeviceSlot` replaces the old `UsbDevice`: `slot_id`, `port_num`, `speed`, `mps`, `icc_phys`/`icc_va` (one shared `InputControlContext` page), `ep0_ring`, `address`, `vendor_id`/`product_id`, `ep_rings: Vec<(dci, TrbRing)>`, `config_value`, `interface_class`/`subclass`/`protocol`, `bulk_in_dci`/`bulk_out_dci` and their MPS values.

**XHCI-048** `DeviceSlotManager` maintains `slots: Vec<DeviceSlot>` and a `next_address` counter starting at 1. The slot id comes from the controller via `submit_enable_slot()`.

**XHCI-049** `enumerate_port()` — full device enumeration, not a stub:
1. `submit_enable_slot()` → controller-assigned slot id
2. Allocate one ICC page, one descriptor page, one EP0 ring (all before any command)
3. `bsr = (speed == SPEED_FS)`. Full-Speed uses BSR (MPS=8, address=0) per USB 2.0 §9.3.1; other speeds use the speed table (LS=8, HS=64, SS=512) and the real address
4. Phase 1 `submit_address_device` (BSR if FS)
5. **BSR path**: read the first 8 descriptor bytes (the 18-byte read is unsafe with unknown MPS), recover the real `bMaxPacketSize0`, re-address with BSR=0 and the real MPS, then fetch the full 18-byte descriptor
6. **Non-BSR path**: fetch the full 18-byte descriptor directly
7. `next_address += 1`, push the slot

**XHCI-050** **The ICC page is reused across phases**: the same `icc_phys`/`icc_va` page is re-zeroed and re-filled for Address Device (BSR and non-BSR) and later for Configure Endpoint. One DMA page per slot, never leaked.

**XHCI-051** `submit_control()` builds a 3-stage control transfer on `ep0_ring`:
- Setup Stage: `TRT` encodes the data stage (0 = no data, 2 = OUT, 3 = IN); `CHAIN` set **only when a data stage follows** (per xHCI §4.11.2.3)
- Data Stage (optional): DIR_IN if IN
- Status Stage: always present, direction **flipped**, IOC=1
- Then `flush()` + doorbell on the default control endpoint (target 1) + `wait_for_transfer`

**XHCI-052** `wait_for_transfer(slot_id, ep_id)` waits with a 5-second deadline via `wait_until_cond`, consuming events and peeking `LAST_TRANSFER_STATE` for a matching slot/ep. Completion codes 1 (success) and 13 (short packet) are accepted.

**XHCI-053** `get_config_descriptor_full()`:
1. Fetch the 9-byte config header, parse `total_length` (reject > 4096)
2. Fetch the full blob, walk `(length, type)` pairs recording interfaces (alt-setting 0 only) and bulk endpoints (DCI = `ep_num*2 + (is_in?1:0)`)
3. **Interface selection**: prefer mass storage class; else first non-zero class; else interface 0
4. Records `config_value`, chosen interface class/subclass/protocol, and bulk IN/OUT DCI + MPS

**XHCI-054** `configure_device()`:
- Sends the USB `SET_CONFIGURATION` control transfer **before** the xHC Configure Endpoint command (xHCI §4.8.1)
- Builds bulk IN/OUT endpoint contexts (type, MPS, dequeue phys, `cerr=3`, `avg_trb_len=3072`)
- Allocates one `TrbRing` per bulk endpoint; **only pushes to `slot.ep_rings` after all allocations succeed** (no partial state)
- `init_icc_for_configure_endpoint()` fills the shared ICC, then `submit_configure_endpoint`

**XHCI-055** `submit_bulk()` enqueues a Normal TRB (IOC) for `data_len` bytes (rejecting > 64 KiB per TRB), flushes, rings the endpoint doorbell, and waits for the matching transfer completion.

---

## Controller Init (`kernel/src/usb/xhci/mod.rs`)

**XHCI-056** `init_all(pci_devices)` iterates all PCI devices, filtering for class=0x0C, subclass=0x03, prog_if=0x30 (xHCI USB controller). For each match it calls `init_controller()` using the shared `kernel_services().dma` allocator, **collecting `Vec<Arc<dyn BlockDevice>>`** for mass-storage devices. A failed controller is logged and skipped; others still initialize.

**XHCI-057** `init_controller()` init sequence:
1. Read BAR0, validate it is memory-mapped
2. `XhciRegisters::new()` maps MMIO
3. Read capability registers (HCSPARAMS1/2, HCCPARAMS1)
4. Parse extended capabilities (USB Legacy Support, Supported Protocol)
5. `controller_reset()` — assert HCRST, wait for self-clear (500 ms)
6. Allocate DCBAA + scratchpad array; write DCBAAP
7. Write CONFIG (max slots)
8. Allocate command TRB ring and write CRCR
9. Allocate ERST and event ring; write ERSTSZ, ERSTBA, ERDP to runtime registers
10. Set event ring globals (`set_event_ring_info`, `set_erdp_register_va`, `set_op_base_va`)
11. `setup_interrupts()` — enable MSI-X (or MSI, or INTx fallback)
12. Arm interrupter (IMAN.IE=1), set IMOD=0
13. Write USBCMD = RUN | INTE | HSEE
14. Wait USBSTS HCH=0 (500 ms, HLT wait)
15. `drain_pending_and_clear_intr()`
16. `UsbPorts::init_ports()` — port power, reset, clear change bits
17. **Event-driven port-detect drain**: drain `PORT_EVENTS` + consume pending events + `sleep_ms(10)` in a loop for a bounded 500 ms window, so devices that raise CSC shortly after power-on still get enumerated
18. `drain_pending_and_clear_intr()`, diagnostic dump (USBSTS, IMAN, PORTSC1, ERDP)
19. `enumerate_initial_ports()` — real slot enumeration into a `DeviceSlotManager`
20. `verify_message_interrupt_delivery()` — No-Op command IRQ verification
21. **Class binding**: for each unconfigured slot, `get_config_descriptor_full()` → `configure_device()`; mass-storage slots get bulk rings and a `UsbMassStorageDevice` (see USB-MSD section)

**XHCI-058** `controller_reset()` sets USBCMD_HCRST and waits until it self-clears (500 ms, HLT wait). During reset the xHCI does not respond to operational register writes.

**XHCI-059** `alloc_dcbaa()` allocates `(max_slots + 1) * 8` bytes rounded up to pages, zeroes them, and returns a `DmaBuffer`. `alloc_scratchpad_array()` allocates the entry array (8 bytes/entry if ac64, else 4) plus one page per scratchpad buffer, filling each entry with its buffer's phys.

**XHCI-060** 64-bit registers (DCBAAP, CRCR, ERSTBA, ERDP) are written as two 32-bit writes when `ac64=true` (low then high 32 bits). This is necessary on 32-bit registers that compose a 64-bit value.

**XHCI-061** `setup_interrupts()` follows this priority:
1. **MSI-X** (preferred): register IDT handler, get MSI-X table info, map non-BAR0 MSI-X BAR via `dma.map_mmio()`, disable MSI if present, call `msix::enable()`. Returns `MsixFallback` (both caps, vector, BSP APIC id) for the delivery-failure fallback.
2. **MSI** (fallback if no MSI-X): register IDT handler, call `msi::enable()`. Returns `None`.
3. **INTx** (last resort): if `interrupt_line != 0`, register IDT handler, call `ioapic::enable_irq()` with ActiveLow/Level trigger. Returns `None`.

**XHCI-062** IMAN.IP clear ordering: MSI-X is enabled *before* the interrupter is armed (IMAN.IE=1) and before the controller is started (USBCMD.RUN=1). This ensures the xHC has a valid interrupt target before interrupts can be generated.

**XHCI-063** `verify_message_interrupt_delivery()`:
1. Record pre-command `irq_count()`
2. Enqueue No-Op command TRB and ring doorbell
3. Wait for IRQ with `wait_until_cond` (100 ms, HLT-based — busy-spinning starves QEMU TCG's device emulation and would report a false failure)
4. **Snapshot IMAN/USBSTS/MSI-X table before polling** — IP=1 with EINT=1 proves the xHC requested an interrupt; if no CPU vector arrived, the fault is below the driver (PCI/QEMU/APIC)
5. Drain events; if the completion arrived but the IRQ did not:
   - Dump LAPIC SVR/TPR for diagnostics
   - If `MsixFallback` available: disable MSI-X, enable **MSI** on the same vector, retry the No-Op, log result
6. If no completion at all — log "stuck"
7. Always drain events after the test

**XHCI-064** `enumerate_initial_ports()` is **no longer a stub**: it builds a `DeviceSlotManager`, iterates enabled+connected ports, and calls `mgr.enumerate_port()` (Enable Slot → Address Device → descriptor fetch) for each.

---

## USB Bulk-Only Transport Mass Storage (`kernel/src/usb/class/mass_storage.rs`)

**USB-MSD-001** BOT signatures: `CBW_SIGNATURE = 0x43425355` ("USBC"), `CSW_SIGNATURE = 0x53425355` ("USBS"). Direction bits: `DIR_IN = 0x80`, `DIR_OUT = 0x00`.

**USB-MSD-002** **The CBW is staged at `CBW_OFFSET = 512` in the shared data page** — never at offset 0. The 31-byte CBW is copied to `data_page_va + 512` and the bulk-OUT transfer reads from `data_page_phys + 512`. This avoids aliasing the CBW and the SCSI data payload in the same page region (the read-cache/write-cache conflict that corrupted early transfers).

**USB-MSD-003** A **dedicated CSW page** (`csw_page_phys`/`csw_page_va`) is allocated per device — the CSW is never overlapped with data buffers, so a device that writes the CSW into the same page as in-flight data cannot corrupt it.

**USB-MSD-004** `do_scsi_command(cdb, data_phys, data_len, dir_in)` is the fixed sequence:
1. Build CBW (signature, incrementing tag, transfer length, direction flag, LUN=0, `b_cbwcb_length=10`)
2. `bot_send_cbw()` (staged at CBW_OFFSET)
3. If `data_len > 0`: data stage on the bulk IN or bulk OUT ring
4. `bot_receive_csw()` into the dedicated CSW page; validates `d_csw_signature == CSW_SIGNATURE` and `b_csw_status == 0` — either failure is a hard error

**USB-MSD-005** SCSI command descriptor blocks: `scsi_read10_cdb` (0x28), `scsi_write10_cdb` (0x2A), `scsi_read_capacity10_cdb` (0x25), `scsi_inquiry_cdb` (0x12, 36 bytes), `scsi_test_unit_ready_cdb` (0x00). All are 16-byte CBW CBs.

**USB-MSD-006** `UsbMassStorageDevice::new()`:
- Allocates the data page and CSW page
- Runs INQUIRY (36 bytes) to capture the 28-char model string (offset 8)
- Runs READ CAPACITY (10) — **rejects any block size != 512**, sets `sector_count = total_blocks + 1`
- Returns `Arc<Self>`

**USB-MSD-007** `UsbMassStorageDevice` implements `filesystems::blockdriver::traits::BlockDevice` (`submit` / `sector_count` / `model_string`), which is why it can flow into the block-device registry (`BLOCK_DEVICES`) and be mounted by `mount_first_partition`. Its `inner` is a `spin::Mutex` for single-slot serialization.

**USB-MSD-008** `submit()` handles `IoBuffer::{Buf, ConstBuf, Phys}`, validates the buffer covers `count * 512` bytes, and performs **one sector at a time** (single-sector READ(10)/WRITE(10) CDBs) through the shared data page, copying between the guest buffer and `data_page_va` around each transfer with a `SeqCst` fence before each WRITE.

---

## USB Class Driver Registry (`kernel/src/usb/class/driver.rs`)

**USB-CDR-001** The registry is a static `Mutex<Vec<&'static dyn UsbClassDriver>>`. Drivers are `Send + Sync` unit structs; there is no dynamic loading — `register_all()` statically registers `MassStorageDriver` and `HidDriver` (both x86_64; the whole `usb` crate is already x86_64-gated in `lib.rs`).
- Location: `kernel/src/usb/class/driver.rs`

**USB-CDR-002** `register_all()` is idempotent, guarded by a `REGISTERED: AtomicBool`. It is invoked lazily from `find_driver()` (not from the boot sequence), so the registry is self-contained.

**USB-CDR-003** `find_driver(iface_class, subclass, protocol)` returns the first driver whose `probe()` matches, or `None`. `bind_slot` (`usb/xhci/mod.rs`) calls it after `configure_device` and hands the winner an `InterfaceResources`; if no driver matches, the slot is left unbound (`Ok(None)`) and the extracted endpoint rings drop (DMA memory is not freed by `TrbRing`'s drop — rings are deliberately leaked to the driver/registry lifetime).

**USB-CDR-004** `InterfaceResources` transfers **ownership** of the endpoint `TrbRing`s from the `DeviceSlot` to the class driver: `bind_slot` removes the bulk-IN/bulk-OUT/interrupt-IN rings by DCI from `slot.ep_rings`. A driver must not keep its own copy of a ring it does not consume.

**USB-CDR-005** `BoundUsbDevice::Block(Arc<dyn BlockDevice>)` devices are returned to `init_all`/`poll` and flow into the block layer; `BoundUsbDevice::Input(u32)` carries the UInputL-owned device id. Input devices are registered with UInputL *inside* `init_interface`, which is safe because `input::init()` runs before USB init in `Kernel::run()`.

**USB-CDR-006** `MassStorageDriver::init_interface` requires both bulk endpoints and wraps the existing `UsbMassStorageDevice::new(...)` — it is a thin adapter and performs blocking SCSI I/O, so it must run on the init path, never interrupt context.

## USB HID Boot Keyboard (`kernel/src/usb/class/hid.rs`)

**USB-HID-001** `HidDriver` probes on `iface_class == CLASS_HID` (3) only. The boot keyboard protocol (subclass 1, protocol 1) needs no report descriptor and no `SET_PROTOCOL`/`SET_IDLE` control transfers, so the class alone is sufficient. This phase binds exactly **one** keyboard; a second HID device is rejected by `init_interface` with an error.

**USB-HID-002** The keyboard is driven by a UInputL `poll` hook (`hid_keyboard_poll`), registered with `CAP_KEYS`. Each poll submits one interrupt-IN read (`device::submit_interrupt`) with a **250 ms** timeout, then diffs the 8-byte boot report (modifier byte + reserved + 6 key usages) against the previous report to emit press (1) / release (0) `InputEvent`s. No key is emitted for usages with no `KeyCode` mapping.

**USB-HID-003** The interrupt-IN endpoint is configured by `configure_device` with `EP_TYPE_INTERRUPT_IN` (7), and the endpoint-context Interval field is written from `DeviceSlot::interrupt_in_interval`, which `usb_interval_to_context()` computes per spec Table 6-12: FS/LS `round_up(bInterval*8)` microframes then `log2` (clamped 3-10); HS/SS `bInterval - 1` (clamped 0-15).

**USB-HID-004** Interrupt-IN completions reuse the same single-slot `LAST_TRANSFER_STATE` atomic as control/bulk (see XHCI-065). `submit_interrupt` waits with the caller-chosen timeout and consumes its own completion. The HID poll and bulk I/O must not run concurrently on the same endpoint pair — today they are serialised because the module run and the input-test loop run on the BSP and never perform concurrent block I/O.

---

## Interrupt State Atomics

**XHCI-065** The following global atomics coordinate between the init path (BSP, no interrupts) and the IRQ handler (interrupt context):
- `XHCI_IRQ_COUNT` (AtomicU64) — incremented by IRQ handler, read by init for delivery verification
- `XHCI_ER_VADDR`, `XHCI_ER_PADDR`, `XHCI_ER_SIZE` (AtomicU64/U32) — set once during init, read by IRQ handler
- `XHCI_ER_DEQUEUE` (AtomicU16) — read/written by both init and IRQ handler; the event ring is single-consumer (only one thread calls `consume_pending_events` at a time)
- `XHCI_ER_CYCLE` (AtomicU32) — same access pattern as `XHCI_ER_DEQUEUE`
- `XHCI_RT_VA` (AtomicU64) — set once during init, read by IRQ handler for IMAN/ERDP writes
- `XHCI_OP_VA` (AtomicU64) — set once during init, read by IRQ handler for USBSTS EINT clear
- `LAST_CMD_STATE` (AtomicU64) — written by `consume_pending_events` (IRQ or poll), read-and-clear by `last_command_completion` (init or command submit path). The seen flag (bit 63) prevents stale reads
- `LAST_TRANSFER_STATE` (AtomicU64) — same protocol for bulk/control/interrupt transfer completions; read-and-clear by `last_transfer_completion`, peeked by `peek_last_transfer_completion`. Shared by the MSD bulk path and the HID interrupt-IN poll hook (USB-HID-004): exactly one transfer may be awaited at a time
- `PORT_EVENTS` + `PORT_EVENTS_HEAD`/`PORT_EVENTS_TAIL` — SPSC port-change ring (see XHCI-029)

**XHCI-066** All atomic accesses use `Ordering::Relaxed` except `LAST_CMD_STATE`/`LAST_TRANSFER_STATE` (Release store in consumer, AcqRel swap in reader) and the `PORT_EVENTS` ring (Acquire/Release on head/tail with release/acquire fences on the payload).

---

## API Contracts

### `DmaAllocator` (`kernel_services().dma`)
- The allocator is a kernel-wide singleton (`services::dma::KernelDma`), `Send + Sync`, with an internal `Mutex`; no per-driver instance exists
- `map_mmio()` consumes virtual address space; overflow is checked and returns `Err`
- All mappings are `NO_CACHE`; `DmaBuffer` carries `phys`/`virt`/`size`

### `TrbRing` (caller must ensure)
- The ring is writable with NO_CACHE semantics (no caching, writes reach the controller)
- `enqueue()` is not re-entrant; the caller must serialise access
- The ring's DMA memory is not touched by the CPU while the xHC may read it (between enqueue and completion)

### Event handling (caller must ensure)
- `consume_pending_events()` is not re-entrant; the IRQ handler and poll path must not run concurrently
- `last_command_completion()` / `last_transfer_completion()` consume the last completion state; polling without consuming will see `None` (use the `peek_*` variants in wait-loop predicates)
- The init sequence must not enable interrupts before `set_event_ring_info()`, `set_erdp_register_va()`, and `set_op_base_va()` are called

### Port operations (caller must ensure)
- Port numbers are 1-indexed per xHCI spec; valid range is `1..=max_ports`
- `handle_port_status_change()` should only be called when a port status change event (type 34) is received, not on every poll

### Mass storage (caller must ensure)
- `UsbMassStorageDevice::new()` performs blocking SCSI I/O and must be called on the init path, not from interrupt context
- The shared data page is not safe for concurrent `submit()` calls without the `inner` mutex (the `BlockDevice::submit` impl locks it)

### Class drivers / HID (caller must ensure)
- `find_driver()` may call `register_all()` on first use; it is not a re-entrancy hazard (guarded by `AtomicBool`)
- `submit_interrupt()` blocks up to the caller-chosen timeout and consumes its transfer completion — do not call it while another transfer on the same `LAST_TRANSFER_STATE` slot is outstanding
- The HID `poll` hook runs while UInputL holds its `DEVICES` lock; the hook must not call `register_device` or otherwise re-enter UInputL core state

---

## Design Notes

- **Why atomic statics instead of a struct**: The IRQ handler cannot access `&self` of the controller struct; it only has a function call. Global atomics provide lock-free access to the event ring state from both the init path and the interrupt handler without requiring a static `Mutex<Controller>`.
- **Why MSI-X before MSI**: MSI-X offers per-vector masking, more entries (typically 2048 vs 32 for MSI), and no PCI config space access to change masks. The fallback to MSI handles QEMU configurations where MSI-X routing is broken but MSI works.
- **Why HLT waits instead of busy-spins**: Under QEMU TCG (emulation without KVM), busy-spinning is non-preemptive — the device emulation thread never gets CPU time to complete doorbell-triggered work. `wait_until_cond`/`sleep_ms` (universal timer) yield to the interrupt/device scheduler.
- **Why PORTSC_PED must be masked on every write**: PORTSC is a RW1C register. Writing 1 to bit 1 (PED) disables the port. Any write that touches PORTSC must mask PED out unless the intent is to disable the port.
- **Why CBW_OFFSET=512**: staging the CBW away from offset 0 in the shared data page avoids an aliasing conflict between the CBW write and SCSI payload reads/writes (USB-MSD-002).
- **Debug aids**: the `usb_trace` cargo feature gates all xHCI event/descriptor serial tracing; `make_demo_drive.py` builds a QEMU demo USB drive image for testing mass storage.
