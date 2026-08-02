pub mod registers;
pub mod memory;
pub mod command;
pub mod event;
pub mod ports;
pub mod device;
pub mod context;

use alloc::sync::Arc;
use alloc::vec::Vec;
use crate::filesystems::blockdriver::traits::BlockDevice;
use crate::pci::PciDevice;
use crate::services::dma::{DmaAllocator, DmaBuffer};

/// Information retained only while xHCI validates a newly enabled MSI-X
/// route.  If the device raises an event but no LAPIC vector arrives, we can
/// switch the *same* IDT vector to MSI instead of silently running polling
/// only for the rest of the boot.
#[derive(Clone, Copy)]
struct MsixFallback {
    msix_cap: crate::pci::caps::PciCapability,
    msi_cap: crate::pci::caps::PciCapability,
    vector: u8,
    dest_apic_id: u8,
}

/// A Supported Protocol capability parsed from the xHC extended capability
/// list (xHCI §7.6.2).  Retained so hot-plug diagnostics can label a newly
/// attached device with its protocol name and revision.
struct ProtocolCap {
    name: [u8; 20],
    rev_major: u8,
    rev_minor: u8,
    port_offset: u8,
    port_count: u8,
}

/// State retained after init so the idle loop can poll for post-boot port
/// changes and enumerate/detach devices without re-probing the controller.
struct XhciControllerState {
    ports: spin::Mutex<ports::UsbPorts>,
    slots: spin::Mutex<device::DeviceSlotManager>,
    cmd_ring: spin::Mutex<memory::TrbRing>,
    doorbell_va: u64,
    dma: &'static dyn DmaAllocator,
    protocol_caps: Vec<ProtocolCap>,
}

static CONTROLLER: spin::Mutex<Option<XhciControllerState>> = spin::Mutex::new(None);

pub fn init_all(
    pci_devices: &[PciDevice],
) -> Vec<Arc<dyn BlockDevice>> {
    use crate::drivers::serial::SerialPort;
    let dma: &'static dyn DmaAllocator = crate::services::kernel_services().dma;
    let mut usb_block_devices = Vec::new();
    for dev in pci_devices {
        if dev.class == 0x0C && dev.subclass == 0x03 && dev.prog_if == 0x30 {
            SerialPort::puts("[xhci] XHCI controller at ");
            SerialPort::put_u64(dev.bus as u64);
            SerialPort::puts(":");
            SerialPort::put_u64(dev.device as u64);
            SerialPort::puts(":");
            SerialPort::put_u64(dev.function as u64);
            SerialPort::puts("\n");

            match init_controller(dev, dma) {
                Ok(block_devs) => {
                    SerialPort::puts("[xhci] controller ready\n");
                    usb_block_devices.extend(block_devs);
                }
                Err(e) => {
                    SerialPort::puts("[xhci] init failed: ");
                    SerialPort::puts(e);
                    SerialPort::puts("\n");
                }
            }
        }
    }
    usb_block_devices
}

fn init_controller(dev: &PciDevice, dma: &'static dyn DmaAllocator) -> Result<Vec<Arc<dyn BlockDevice>>, &'static str> {
    use crate::drivers::serial::SerialPort;

    let phys_base = match crate::pci::bar::bar(dev, 0) {
        crate::pci::bar::Bar::Memory { addr, .. } => addr,
        _ => return Err("BAR0 not memory-mapped"),
    };

    let regs = registers::XhciRegisters::new(phys_base, dma)?;
    let mmio_va = regs.mmio_base();
    let caplength = regs.cap_length();

    let hcsp1 = registers::HcsParams1::from(regs.read_cap32(0x04));
    let hcsp2 = registers::HcsParams2::from(regs.read_cap32(0x08));
    let hccp1 = registers::HccParams1::from(regs.read_cap32(0x10));

    let max_slots = hcsp1.max_slots();
    let max_ports = hcsp1.max_ports();
    let spbuf_cnt = hcsp2.scratchpad_bufs();
    let ac64 = hccp1.ac64();
    let xecp_off = hccp1.xecp() as u64;
    let ctx_size = if hccp1.csz() { 64 } else { 32 };

    SerialPort::puts("[xhci] CAPLENGTH=0x");
    SerialPort::put_hex(caplength as u64);
    SerialPort::puts(" max_slots=");
    SerialPort::put_u64(max_slots as u64);
    SerialPort::puts(" ports=");
    SerialPort::put_u64(max_ports as u64);
    SerialPort::puts(" spbuf=");
    SerialPort::put_u64(spbuf_cnt as u64);
    SerialPort::puts(" ctx_sz=");
    SerialPort::put_u64(ctx_size as u64);
    SerialPort::puts(" ac64=");
    SerialPort::put_u64(ac64 as u64);
    SerialPort::puts("\n");

    controller_reset(&regs);

    let dcbaa = alloc_dcbaa(dma, max_slots)?;

    if spbuf_cnt > 0 {
        let sp_array = alloc_scratchpad_array(dma, spbuf_cnt, ac64)?;
        unsafe {
            core::ptr::write_volatile(dcbaa.virt as *mut u64, sp_array.phys);
        }
    }

    regs.write_op32(registers::OP_DCBAAP, dcbaa.phys as u32);
    if ac64 {
        regs.write_op32(registers::OP_DCBAAP + 4, (dcbaa.phys >> 32) as u32);
    }

    regs.write_op32(registers::OP_CONFIG, max_slots as u32);

    let mut cmd_ring = memory::TrbRing::new(dma, 4096)?;
    let crcr = cmd_ring.phys as u64 | 1;
    regs.write_op32(registers::OP_CRCR, crcr as u32);
    if ac64 {
        regs.write_op32(registers::OP_CRCR + 4, (crcr >> 32) as u32);
    }

    let erst = registers::Erst::new(dma)?;
    let er_buf = registers::EventRing::new(dma, erst.seg_va)?;

    let rt_va = regs.runtime_va();

    unsafe {
        let erstsz_off = rt_va + 0x28;
        core::ptr::write_volatile(erstsz_off as *mut u32, 1u32);
        let erstba_off = rt_va + 0x30;
        core::ptr::write_volatile(erstba_off as *mut u32, erst.seg_phys as u32);
        if ac64 {
            core::ptr::write_volatile((erstba_off + 4) as *mut u32, (erst.seg_phys >> 32) as u32);
        }
        let erdp_off = rt_va + 0x38;
        let erdp_val = er_buf.phys;
        core::ptr::write_volatile(erdp_off as *mut u32, erdp_val as u32);
        if ac64 {
            core::ptr::write_volatile((erdp_off + 4) as *mut u32, (erdp_val >> 32) as u32);
        }
    }

    event::set_event_ring_info(er_buf.virt, er_buf.phys, er_buf.trb_count as u32, er_buf.dequeue_index);
    event::set_erdp_register_va(rt_va);
    event::set_op_base_va(regs.op_base());

    // Enable MSI-X FIRST so the xHC has a valid interrupt target before
    // the interrupter is armed and the controller starts.
    let msix_fallback = setup_interrupts(dev, rt_va, mmio_va, dma, ac64);

    // Now it is safe to arm the interrupter and start the controller.
    unsafe {
        let iman_off = rt_va + 0x20;
        core::ptr::write_volatile(iman_off as *mut u32, registers::IMAN_IE);
        let imod_off = rt_va + 0x24;
        core::ptr::write_volatile(imod_off as *mut u32, 0u32);
    }

    regs.write_op32(registers::OP_USBCMD,
        registers::USBCMD_RUN | registers::USBCMD_INTE | registers::USBCMD_HSEE);

    {
        use crate::services::universal_timer::{now_ns, wait_until_cond};
        let deadline = now_ns() + 500_000_000;
        let running = wait_until_cond(
            deadline,
            &|| regs.read_op32(registers::OP_USBSTS) & registers::USBSTS_HCH == 0,
        );
        if !running {
            SerialPort::puts("[xhci] start timeout\n");
        }
    }
    SerialPort::puts("[xhci] controller running\n");

    event::drain_pending_and_clear_intr();

    let port_regs = registers::PortRegisterSet::new(mmio_va, caplength);
    let mut usb_ports = ports::UsbPorts::new(max_ports, port_regs);
    usb_ports.init_ports()?;

    // Event-driven port detection: PORT_CHANGE events are queued by the ISR
    // while devices finish link-training.  Drain them (and process any that
    // arrived during the power-on settle) for a bounded window so devices
    // that raise CSC shortly after power-on still get enumerated.
    {
        use crate::services::universal_timer::{now_ns, sleep_ms};
        let detect_deadline = now_ns() + 500_000_000;
        loop {
            while let Some(port_id) = event::take_port_change() {
                if let Err(e) = usb_ports.handle_port_status_change(port_id) {
                    SerialPort::puts("[xhci] port change err: ");
                    SerialPort::puts(e);
                    SerialPort::puts("\n");
                }
            }
            if now_ns() >= detect_deadline {
                break;
            }
            event::drain_pending_and_clear_intr();
            sleep_ms(10);
        }
    }

    event::drain_pending_and_clear_intr();

    {
        use crate::drivers::serial::SerialPort;
        let usbsts = regs.read_op32(registers::OP_USBSTS);
        let iman = unsafe { core::ptr::read_volatile((rt_va + 0x20) as *const u32) };
        let portsc = regs.read_portsc(1);
        let erdp_lo = unsafe { core::ptr::read_volatile((rt_va + 0x38) as *const u32) };
        let erdp_hi = unsafe { core::ptr::read_volatile((rt_va + 0x3C) as *const u32) };
        let erdp = (erdp_lo as u64) | ((erdp_hi as u64) << 32);
        SerialPort::puts("[xhci] dump: USBSTS=0x");
        SerialPort::put_hex(usbsts as u64);
        SerialPort::puts(" IMAN=0x");
        SerialPort::put_hex(iman as u64);
        SerialPort::puts(" PORTSC1=0x");
        SerialPort::put_hex(portsc as u64);
        SerialPort::puts(" ERDP=0x");
        SerialPort::put_hex(erdp);
        SerialPort::puts("\n");
    }

    let mut dev_mgr = enumerate_initial_ports(&mut usb_ports, &mut cmd_ring, dma,
        regs.doorbell_va(), ctx_size, max_slots);

    // Verify message-interrupt delivery with a No-Op command.
    verify_message_interrupt_delivery(&mut cmd_ring, regs.doorbell_va(), &regs, dev, msix_fallback);

    let doorbell_va = regs.doorbell_va();

    // Step 2: full configuration + class driver binding
    let mut block_devices: Vec<Arc<dyn BlockDevice>> = Vec::new();
    for i in 0..dev_mgr.slots.len() {
        let slot = &mut dev_mgr.slots[i];
        if slot.interface_class != 0 || slot.config_value != 0 {
            continue; // already configured
        }
        match bind_slot(slot, &mut cmd_ring, doorbell_va, dma) {
            Ok(Some(dev)) => block_devices.push(dev),
            Ok(None) => {}
            Err(e) => {
                SerialPort::puts("[xhci]  bind failed: ");
                SerialPort::puts(e);
                SerialPort::puts("\n");
            }
        }
    }

    let protocol_caps = if xecp_off != 0 {
        parse_ext_caps(mmio_va, xecp_off)
    } else {
        Vec::new()
    };

    *CONTROLLER.lock() = Some(XhciControllerState {
        ports: spin::Mutex::new(usb_ports),
        slots: spin::Mutex::new(dev_mgr),
        cmd_ring: spin::Mutex::new(cmd_ring),
        doorbell_va,
        dma,
        protocol_caps,
    });

    Ok(block_devices)
}

/// Full configuration + class-driver binding for one enumerated slot.
/// Returns the bound block device, or None if the slot needs no class driver.
fn bind_slot(
    slot: &mut device::DeviceSlot,
    cmd_ring: &mut memory::TrbRing,
    doorbell_va: u64,
    dma: &dyn DmaAllocator,
) -> Result<Option<Arc<dyn BlockDevice>>, &'static str> {
    use crate::drivers::serial::SerialPort;

    device::get_config_descriptor_full(slot, doorbell_va, slot.icc_phys, slot.icc_va)?;

    if slot.interface_class == 0 {
        return Ok(None);
    }

    device::configure_device(slot, cmd_ring, doorbell_va, dma)?;

    // Extract the endpoint rings the class driver needs.  Ring ownership
    // moves into the driver via `InterfaceResources`.
    let mut bulk_out = None;
    let mut bulk_in = None;
    let mut interrupt_in = None;
    let mut i = 0;
    while i < slot.ep_rings.len() {
        let (dci, _) = slot.ep_rings[i];
        if dci == slot.bulk_out_dci && bulk_out.is_none() {
            bulk_out = Some(slot.ep_rings.remove(i));
        } else if dci == slot.bulk_in_dci && bulk_in.is_none() {
            bulk_in = Some(slot.ep_rings.remove(i));
        } else if dci == slot.interrupt_in_dci && interrupt_in.is_none() {
            interrupt_in = Some(slot.ep_rings.remove(i));
        } else {
            i += 1;
        }
    }

    use crate::usb::class::driver::{BoundUsbDevice, EndpointResource, InterfaceResources};
    let to_resource =
        |pair: Option<(u8, memory::TrbRing)>, mps: u16, interval: u8| -> Option<EndpointResource> {
            pair.map(|(dci, ring)| EndpointResource { dci, mps, interval, ring })
        };

    let res = InterfaceResources {
        slot_id: slot.slot_id,
        doorbell_va,
        iface_class: slot.interface_class,
        iface_subclass: slot.interface_subclass,
        iface_protocol: slot.interface_protocol,
        bulk_in: to_resource(bulk_in, slot.bulk_in_mps, 0),
        bulk_out: to_resource(bulk_out, slot.bulk_out_mps, 0),
        interrupt_in: to_resource(interrupt_in, slot.interrupt_in_mps, slot.interrupt_in_interval),
    };

    let driver = match crate::usb::class::driver::find_driver(
        slot.interface_class,
        slot.interface_subclass,
        slot.interface_protocol,
    ) {
        Some(d) => d,
        None => return Ok(None),
    };

    SerialPort::puts("[usbdrv] ");
    SerialPort::puts(driver.name());
    SerialPort::puts(" bind slot=");
    SerialPort::put_u64(slot.slot_id as u64);
    SerialPort::puts("\n");

    match driver.init_interface(res, dma)? {
        BoundUsbDevice::Block(dev) => Ok(Some(dev)),
        BoundUsbDevice::Input(_id) => Ok(None),
    }
}

/// Poll the retained xHCI controller for queued port-change events and
/// enumerate or detach devices accordingly.  Returns any newly bound block
/// devices so the caller can register them with the VFS/block layer.
/// Called from the idle loop on the BSP.
pub fn poll() -> Vec<Arc<dyn BlockDevice>> {
    use crate::drivers::serial::SerialPort;

    let mut new_devices = Vec::new();
    let guard = CONTROLLER.lock();
    let ctrl = match guard.as_ref() {
        Some(c) => c,
        None => return new_devices,
    };

    if !event::port_change_pending() {
        return new_devices;
    }

    while let Some(port_id) = event::take_port_change() {
        {
            let mut ports = ctrl.ports.lock();
            if let Err(e) = ports.handle_port_status_change(port_id) {
                SerialPort::puts("[xhci] port change err: ");
                SerialPort::puts(e);
                SerialPort::puts("\n");
            }
        }

        let port_state = {
            let ports = ctrl.ports.lock();
            ports.ports.iter().find(|p| p.port_num == port_id).map(|p| (p.connected, p.enabled, p.speed))
        };

        match port_state {
            // Disconnect: tear down the slot if one exists.
            Some((false, _, _)) => {
                let mut cmd = ctrl.cmd_ring.lock();
                let mut slots = ctrl.slots.lock();
                if let Some(idx) = slots.slots.iter().position(|s| s.port_num == port_id) {
                    let slot_id = slots.slots[idx].slot_id;
                    match command::submit_disable_slot(&mut cmd, ctrl.doorbell_va, slot_id) {
                        Ok(()) => {
                            SerialPort::puts("[xhci] port ");
                            SerialPort::put_u64(port_id as u64);
                            SerialPort::puts(": slot ");
                            SerialPort::put_u64(slot_id as u64);
                            SerialPort::puts(" disabled\n");
                        }
                        Err(e) => {
                            SerialPort::puts("[xhci] port ");
                            SerialPort::put_u64(port_id as u64);
                            SerialPort::puts(": disable slot failed: ");
                            SerialPort::puts(e);
                            SerialPort::puts("\n");
                        }
                    }
                    slots.slots.remove(idx);
                }
            }
            // Connect: enumerate + bind, unless the port already has a slot
            // (e.g. a link-state change on an already-attached device).
            Some((true, true, speed)) => {
                let (name, major, minor) = match protocol_cap_for_port(ctrl, port_id) {
                    Some(cap) => {
                        let n = cap.name.iter().position(|&b| b == 0).unwrap_or(cap.name.len());
                        let name = core::str::from_utf8(&cap.name[..n]).unwrap_or("?");
                        (name, cap.rev_major, cap.rev_minor)
                    }
                    None => ("?", 0, 0),
                };
                SerialPort::puts("[xhci] port ");
                SerialPort::put_u64(port_id as u64);
                SerialPort::puts(": connect (");
                SerialPort::puts(name);
                SerialPort::puts(" v");
                SerialPort::put_u64(major as u64);
                SerialPort::puts(".");
                SerialPort::put_u64(minor as u64);
                SerialPort::puts(")\n");

                let mut cmd = ctrl.cmd_ring.lock();
                let mut slots = ctrl.slots.lock();
                if slots.slots.iter().any(|s| s.port_num == port_id) {
                    continue;
                }
                if let Err(e) = slots.enumerate_port(&mut cmd, ctrl.doorbell_va, ctrl.dma, port_id, speed) {
                    SerialPort::puts("[xhci]  port ");
                    SerialPort::put_u64(port_id as u64);
                    SerialPort::puts(" enum failed: ");
                    SerialPort::puts(e);
                    SerialPort::puts("\n");
                    continue;
                }
                let slot = slots.slots.last_mut().expect("enumerated slot missing");
                match bind_slot(slot, &mut cmd, ctrl.doorbell_va, ctrl.dma) {
                    Ok(Some(dev)) => new_devices.push(dev),
                    Ok(None) => {}
                    Err(e) => {
                        SerialPort::puts("[xhci]  bind failed: ");
                        SerialPort::puts(e);
                        SerialPort::puts("\n");
                    }
                }
            }
            _ => {}
        }
    }
    new_devices
}

fn protocol_cap_for_port<'a>(ctrl: &'a XhciControllerState, port_num: u8) -> Option<&'a ProtocolCap> {
    ctrl.protocol_caps
        .iter()
        .find(|c| port_num >= c.port_offset && port_num < c.port_offset + c.port_count)
}

fn verify_message_interrupt_delivery(
    cmd_ring: &mut memory::TrbRing,
    doorbell_va: u64,
    _regs: &registers::XhciRegisters,
    dev: &PciDevice,
    msix_fallback: Option<MsixFallback>,
) {
    use crate::drivers::serial::SerialPort;
    use crate::usb::xhci::event;

    let before = event::irq_count();

    // Enqueue a No-Op command TRB and ring the doorbell.
    let trb = memory::make_no_op_command_trb();
    cmd_ring.enqueue(&trb);
    cmd_ring.flush();
    command::ring_command_doorbell(doorbell_va);

    // Do not busy-spin here.  Under QEMU TCG that can starve device
    // emulation, delaying the command completion (and its MSI/MSI-X write)
    // until after this diagnostic has already reported a false failure.
    // HLT yields to the interrupt/device scheduler and wakes on the LAPIC
    // timer or the xHCI message interrupt.  The universal timer guarantees
    // the halt cannot sleep past the deadline even if the MSI-X route never
    // raises a device IRQ.
    use crate::services::universal_timer::{now_ns, wait_until_cond};
    let deadline = now_ns() + 100_000_000;
    let irq_fired = wait_until_cond(deadline, &|| event::irq_count() != before);

    // Snapshot the controller before polling/acknowledging the event.  IP=1
    // together with USBSTS.EINT=1 proves that the xHC requested an interrupt;
    // if no CPU vector arrived, the fault is below the driver (PCI/QEMU/APIC).
    let iman_before_poll = unsafe { core::ptr::read_volatile((_regs.runtime_va() + 0x20) as *const u32) };
    let usbsts_before_poll = _regs.read_op32(registers::OP_USBSTS);
    SerialPort::puts("[xhci] irq snapshot: IF=");
    SerialPort::put_u64(crate::arch::CurrentArch::are_interrupts_enabled() as u64);
    SerialPort::puts(" USBSTS=0x");
    SerialPort::put_hex(usbsts_before_poll as u64);
    SerialPort::puts(" IMAN=0x");
    SerialPort::put_hex(iman_before_poll as u64);
    // Read MSI-X table entry directly from the BAR to see if our writes stuck.
    let diag_addr = crate::pci::msix::diag_read_addr();
    let diag_data = crate::pci::msix::diag_read_data();
    let diag_vc = crate::pci::msix::diag_read_vc();
    let diag_pba = crate::pci::msix::diag_read_pba();
    fn print_diag(name: &str, val: Option<u64>) {
        SerialPort::puts(name);
        SerialPort::puts("=");
        match val {
            Some(v) => {
                SerialPort::puts("0x");
                SerialPort::put_hex(v);
            }
            None => SerialPort::puts("unset"),
        }
    }
    print_diag(" tbl_addr", diag_addr);
    print_diag(" data", diag_data.map(|v| v as u64));
    print_diag(" vc", diag_vc.map(|v| v as u64));
    print_diag(" pba", diag_pba.map(|v| v as u64));
    SerialPort::puts("\n");

    // Fallback: poll the event ring so we don't leave a dangling completion.
    event::drain_pending_and_clear_intr();

    // Check if the completion arrived via the event ring at all.
    let completed = event::last_command_completion().is_some();

    SerialPort::puts("[xhci] message interrupt delivery: irq=");
    SerialPort::put_u64(event::irq_count() - before);
    if irq_fired {
        SerialPort::puts(" (via interrupt)");
    } else if completed {
        // The timeout has already elapsed.  This is not a delayed delivery:
        // the xHC requested an interrupt but no CPU vector was dispatched.
        SerialPort::puts(" (NOT delivered; completion recovered by poll)");
        // Extra diagnostics for the failed delivery.
        let lapic_base = crate::platform::x86_64_pc::apic::lapic_base();
        if lapic_base != 0 {
            let svr = unsafe { core::ptr::read_volatile((lapic_base + 0xF0) as *const u32) };
            let tpr = unsafe { core::ptr::read_volatile((lapic_base + 0x80) as *const u32) };
            SerialPort::puts("[xhci] lapic_diag: SVR=0x");
            SerialPort::put_hex(svr as u64);
            SerialPort::puts(" TPR=0x");
            SerialPort::put_hex(tpr as u64);
            SerialPort::puts("\n");
        }

        if let Some(fallback) = msix_fallback {
            // MSI-X is configured and the xHC has asserted IP/EINT, so this
            // is a real delivery failure rather than a slow command.  QEMU
            // configurations with a broken MSI-X route still implement MSI;
            // keep the controller interrupt driven by using that route.
            crate::pci::msix::disable(dev, &fallback.msix_cap);
            crate::pci::msi::enable(
                dev,
                &fallback.msi_cap,
                fallback.vector,
                fallback.dest_apic_id,
            );
            SerialPort::puts("[xhci] MSI-X delivery failed; retrying with MSI\n");

            let msi_before = event::irq_count();
            cmd_ring.enqueue(&memory::make_no_op_command_trb());
            cmd_ring.flush();
            command::ring_command_doorbell(doorbell_va);
            let msi_deadline = now_ns() + 100_000_000;
            wait_until_cond(msi_deadline, &|| event::irq_count() != msi_before);
            SerialPort::puts("[xhci] MSI fallback delivery: irq=");
            SerialPort::put_u64(event::irq_count() - msi_before);
            SerialPort::puts("\n");
            event::drain_pending_and_clear_intr();
        }
    } else {
        SerialPort::puts(" (no completion - stuck)");
    }
    SerialPort::puts("\n");
}

fn parse_ext_caps(mmio_va: u64, xecp_off: u64) -> Vec<ProtocolCap> {
    use crate::drivers::serial::SerialPort;
    let mut caps = Vec::new();
    let mut off = xecp_off;
    loop {
        let cap_id = registers::read_cap_id(mmio_va, off);
        let cap_next = registers::read_cap_next(mmio_va, off);
        if cap_id == 0 && cap_next == 0 {
            break;
        }
        match cap_id {
            1 => {
                SerialPort::puts("[xhci] USB Legacy Support\n");
            }
            2 => {
                let name = registers::read_protocol_string(mmio_va, off);
                let nul_pos = name.iter().position(|&c| c == 0).unwrap_or(20);
                let name_str = core::str::from_utf8(&name[..nul_pos]).unwrap_or("?");
                // xHCI Supported Protocol Capability layout:
                //   +2: Minor Revision, +3: Major Revision, +4: name string,
                //   +8: Port Offset, +9: Port Count.
                let port_info = registers::read_cap_data32(mmio_va, off, 8);
                let comp_port_off = (port_info & 0xFF) as u8;
                let comp_port_cnt = ((port_info >> 8) & 0xFF) as u8;
                let rev_major = registers::read_cap_id(mmio_va, off + 3);
                let rev_minor = registers::read_cap_id(mmio_va, off + 2);
                SerialPort::puts("[xhci] ");
                SerialPort::puts(name_str);
                SerialPort::puts(" v");
                SerialPort::put_u64(rev_major as u64);
                SerialPort::puts(".");
                SerialPort::put_u64(rev_minor as u64);
                SerialPort::puts(" ports ");
                SerialPort::put_u64(comp_port_off as u64);
                SerialPort::puts("-");
                SerialPort::put_u64((comp_port_off + comp_port_cnt - 1) as u64);
                SerialPort::puts("\n");
                caps.push(ProtocolCap {
                    name,
                    rev_major,
                    rev_minor,
                    port_offset: comp_port_off,
                    port_count: comp_port_cnt,
                });
            }
            _ => {}
        }
        if cap_next == 0 {
            break;
        }
        off = cap_next as u64;
    }
    caps
}

fn controller_reset(regs: &registers::XhciRegisters) {
    use crate::drivers::serial::SerialPort;
    let cmd = regs.read_op32(registers::OP_USBCMD);
    regs.write_op32(registers::OP_USBCMD, cmd | registers::USBCMD_HCRST);
    use crate::services::universal_timer::{now_ns, wait_until_cond};
    let deadline = now_ns() + 500_000_000;
    let done = wait_until_cond(
        deadline,
        &|| regs.read_op32(registers::OP_USBCMD) & registers::USBCMD_HCRST == 0,
    );
    if !done {
        SerialPort::puts("[xhci] reset timeout\n");
    }
}

fn alloc_dcbaa(dma: &dyn DmaAllocator, max_slots: u8) -> Result<DmaBuffer, &'static str> {
    let bytes = (max_slots as usize + 1) * 8;
    let pages = (bytes + 4095) / 4096;
    let buf = dma.alloc_contiguous(pages).ok_or("OOM for DCBAA")?;
    unsafe { core::ptr::write_bytes(buf.virt as *mut u8, 0, buf.size) };
    Ok(buf)
}

fn alloc_scratchpad_array(
    dma: &dyn DmaAllocator,
    spbuf_cnt: u16,
    ac64: bool,
) -> Result<DmaBuffer, &'static str> {
    let entry_size = if ac64 { 8usize } else { 4 };
    let total = spbuf_cnt as usize * entry_size;
    let pages = (total + 4095) / 4096;
    let buf = dma.alloc_contiguous(pages).ok_or("OOM for scratchpad array")?;
    unsafe { core::ptr::write_bytes(buf.virt as *mut u8, 0, buf.size) };
    for i in 0..spbuf_cnt as usize {
        let scratch = dma.alloc_page().ok_or("OOM for scratchpad buffer")?;
        let entry_va = buf.virt + (i * entry_size) as u64;
        if ac64 {
            unsafe { core::ptr::write_volatile(entry_va as *mut u64, scratch.phys); }
        } else {
            unsafe { core::ptr::write_volatile(entry_va as *mut u32, scratch.phys as u32); }
        }
    }
    Ok(buf)
}

fn setup_interrupts(
    dev: &PciDevice,
    rt_va: u64,
    mmio_va: u64,
    dma: &dyn DmaAllocator,
    _ac64: bool,
) -> Option<MsixFallback> {
    use crate::arch::x86_64::idt;
    use crate::pci::caps;
    use crate::drivers::serial::SerialPort;

    let bsp_apic_id = unsafe {
        let lapic = crate::platform::x86_64_pc::apic::lapic_base();
        core::ptr::read_volatile((lapic as *const u32).add(0x20 / 4)) >> 24
    } as u8;

    let caps_list = caps::all(dev);
    SerialPort::puts("[xhci] caps:");
    for c in caps_list.iter() {
        SerialPort::puts(" id=");
        SerialPort::put_u64(c.id as u64);
        SerialPort::puts("@0x");
        SerialPort::put_hex(c.offset as u64);
    }
    SerialPort::puts("\n");
    let msix_cap = caps_list.iter().find(|c| c.id == caps::CAP_MSIX);
    let msi_cap = caps_list.iter().find(|c| c.id == caps::CAP_MSI);

    // Try MSI-X first (more capable: per-vector masking, more entries).
    // Falls back to MSI if MSI-X fails.
    if let Some(cap) = msix_cap {
        if let Some(vector) = idt::register_device_handler(event::xhci_irq_handler) {
            let info = crate::pci::msix::table_info(dev, cap);
            SerialPort::puts("[xhci] MSI-X BIR=");
            SerialPort::put_u64(info.bir as u64);
            SerialPort::puts(" table_offset=0x");
            SerialPort::put_hex(info.table_offset);
            SerialPort::puts(" entries=");
            SerialPort::put_u64(info.table_size as u64);
            SerialPort::puts("\n");

            let bar_va = if info.bir == 0 {
                mmio_va
            } else {
                let phys = match crate::pci::bar::bar(dev, info.bir) {
                    crate::pci::bar::Bar::Memory { addr, .. } => addr,
                    _ => {
                        SerialPort::puts("[xhci] MSI-X BAR not memory\n");
                        return None;
                    }
                };
                match dma.map_mmio(phys, 0x10000) {
                    Ok(va) => va,
                    Err(e) => {
                        SerialPort::puts("[xhci] MSI-X BAR map: ");
                        SerialPort::puts(e);
                        SerialPort::puts("\n");
                        return None;
                    }
                }
            };

            // If MSI is also present, disable it to avoid conflicts.
            if let Some(ref msi) = msi_cap {
                crate::pci::msi::disable(dev, msi);
            }

            crate::pci::msix::enable(dev, cap, bar_va, bar_va, 1, vector, bsp_apic_id);
            SerialPort::puts("[xhci] MSI-X enabled\n");

            return msi_cap.copied().map(|msi_cap| MsixFallback {
                msix_cap: *cap,
                msi_cap,
                vector,
                dest_apic_id: bsp_apic_id,
            });
        }
    }

    // Fallback: MSI
    if let Some(ref cap) = msi_cap {
        if let Some(vector) = idt::register_device_handler(event::xhci_irq_handler) {
            crate::pci::msi::enable(dev, cap, vector, bsp_apic_id);
            crate::drivers::serial::SerialPort::puts("[xhci] MSI enabled\n");
            return None;
        }
    }

    SerialPort::puts("[xhci] INTx fallback: interrupt_line=0x");
    SerialPort::put_hex(dev.interrupt_line as u64);
    let cmd = crate::pci::ecam::read_u16(0, dev.bus, dev.device, dev.function, 0x04);
    SerialPort::puts(" CMD=0x");
    SerialPort::put_hex(cmd as u64);
    SerialPort::puts("\n");

    if dev.interrupt_line != 0 {
        if let Some(vector) = idt::register_device_handler(event::xhci_irq_handler) {
            if crate::platform::x86_64_pc::ioapic::enable_irq(
                dev.interrupt_line as u32,
                crate::acpi::Polarity::ActiveLow,
                crate::acpi::TriggerMode::Level,
            ).is_some() {
                unsafe {
                    core::ptr::write_volatile((rt_va + 0x20) as *mut u32, registers::IMAN_IE);
                }
                crate::drivers::serial::SerialPort::puts("[xhci] INTX enabled\n");
            } else {
                idt::unregister_device_handler(vector);
            }
        }
    }
    None
}

fn enumerate_initial_ports(
    usb_ports: &mut ports::UsbPorts,
    cmd_ring: &mut memory::TrbRing,
    dma: &dyn DmaAllocator,
    doorbell_va: u64,
    ctx_size: u8,
    max_slots: u8,
) -> device::DeviceSlotManager {
    use crate::usb::xhci::device::DeviceSlotManager;
    let mut mgr = DeviceSlotManager::new(ctx_size, max_slots);
    for port in &usb_ports.ports {
        if port.enabled && port.connected {
            if let Err(e) = mgr.enumerate_port(cmd_ring, doorbell_va, dma, port.port_num, port.speed) {
                use crate::drivers::serial::SerialPort;
                SerialPort::puts("[xhci]  port ");
                SerialPort::put_u64(port.port_num as u64);
                SerialPort::puts(" enum failed: ");
                SerialPort::puts(e);
                SerialPort::puts("\n");
            }
        }
    }
    mgr
}
