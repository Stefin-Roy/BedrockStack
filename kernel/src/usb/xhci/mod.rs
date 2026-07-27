pub mod registers;
pub mod memory;
pub mod command;
pub mod event;
pub mod ports;
pub mod device;

use crate::pci::PciDevice;
use crate::usb::dma::{DmaBuffer, UsbDmaAllocator};

pub fn init_all(
    pci_devices: &[PciDevice],
    root: u64,
    alloc: *mut crate::mm::phys_alloc::BitmapAllocator,
) {
    use crate::drivers::serial::SerialPort;
    for dev in pci_devices {
        if dev.class == 0x0C && dev.subclass == 0x03 && dev.prog_if == 0x30 {
            SerialPort::puts("[xhci] XHCI controller at ");
            SerialPort::put_u64(dev.bus as u64);
            SerialPort::puts(":");
            SerialPort::put_u64(dev.device as u64);
            SerialPort::puts(":");
            SerialPort::put_u64(dev.function as u64);
            SerialPort::puts("\n");

            let mut dma = UsbDmaAllocator::new(root, alloc);
            match init_controller(dev, &mut dma) {
                Ok(()) => SerialPort::puts("[xhci] controller ready\n"),
                Err(e) => {
                    SerialPort::puts("[xhci] init failed: ");
                    SerialPort::puts(e);
                    SerialPort::puts("\n");
                }
            }
        }
    }
}

fn init_controller(dev: &PciDevice, dma: &mut UsbDmaAllocator) -> Result<(), &'static str> {
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

    if xecp_off != 0 {
        parse_ext_caps(mmio_va, xecp_off);
    }

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

    let cmd_ring = memory::TrbRing::new(dma, 4096)?;
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
    setup_interrupts(dev, rt_va, mmio_va, dma, ac64);

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
        use crate::platform::x86_64_pc::apic::ApicTimeout;
        let mut timeout = ApicTimeout::new(500);
        loop {
            if regs.read_op32(registers::OP_USBSTS) & registers::USBSTS_HCH == 0 {
                break;
            }
            if timeout.expired() {
                SerialPort::puts("[xhci] start timeout\n");
                break;
            }
            core::hint::spin_loop();
        }
    }
    SerialPort::puts("[xhci] controller running\n");

    event::drain_pending_and_clear_intr();

    let port_regs = registers::PortRegisterSet::new(mmio_va, caplength);
    let mut usb_ports = ports::UsbPorts::new(max_ports, port_regs);
    usb_ports.init_ports()?;

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

    let _ = enumerate_initial_ports(&mut usb_ports, &cmd_ring, dma,
        regs.doorbell_va(), er_buf.virt, er_buf.trb_count as u32, max_slots);

    Ok(())
}

fn parse_ext_caps(mmio_va: u64, xecp_off: u64) {
    use crate::drivers::serial::SerialPort;
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
                let port_info = registers::read_cap_data32(mmio_va, off, 8);
                let comp_port_off = (port_info >> 24) as u8;
                let comp_port_cnt = (port_info >> 8) as u8;
                let rev = registers::read_cap_data32(mmio_va, off, 4);
                let rev_major = rev & 0xFF;
                let rev_minor = (rev >> 8) & 0xFF;
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
            }
            _ => {}
        }
        if cap_next == 0 {
            break;
        }
        off = cap_next as u64;
    }
}

fn controller_reset(regs: &registers::XhciRegisters) {
    use crate::drivers::serial::SerialPort;
    use crate::platform::x86_64_pc::apic::ApicTimeout;
    let cmd = regs.read_op32(registers::OP_USBCMD);
    regs.write_op32(registers::OP_USBCMD, cmd | registers::USBCMD_HCRST);
    let mut timeout = ApicTimeout::new(500);
    loop {
        if regs.read_op32(registers::OP_USBCMD) & registers::USBCMD_HCRST == 0 {
            break;
        }
        if timeout.expired() {
            SerialPort::puts("[xhci] reset timeout\n");
            break;
        }
        core::hint::spin_loop();
    }
}

fn alloc_dcbaa(dma: &mut UsbDmaAllocator, max_slots: u8) -> Result<DmaBuffer, &'static str> {
    let bytes = (max_slots as usize + 1) * 8;
    let pages = (bytes + 4095) / 4096;
    let buf = dma.alloc_contiguous(pages).ok_or("OOM for DCBAA")?;
    unsafe { core::ptr::write_bytes(buf.virt as *mut u8, 0, buf.size) };
    Ok(buf)
}

fn alloc_scratchpad_array(
    dma: &mut UsbDmaAllocator,
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

fn setup_interrupts(dev: &PciDevice, rt_va: u64, mmio_va: u64, dma: &mut UsbDmaAllocator, _ac64: bool) {
    use crate::arch::x86_64::idt;
    use crate::pci::caps;
    use crate::drivers::serial::SerialPort;

    let bsp_apic_id = unsafe {
        let lapic = crate::platform::x86_64_pc::apic::lapic_base();
        core::ptr::read_volatile((lapic as *const u32).add(0x20 / 4)) >> 24
    } as u8;

    let caps_list = caps::all(dev);
    let msix_cap = caps_list.iter().find(|c| c.id == caps::CAP_MSIX);
    let msi_cap = caps_list.iter().find(|c| c.id == caps::CAP_MSI);

    // Try MSI first (avoids MADT table BAR writes that may corrupt regs at 0x3000)
    if let Some(ref cap) = msi_cap {
        if let Some(vector) = idt::register_device_handler(event::xhci_irq_handler) {
            crate::pci::msi::enable(dev, cap, vector, bsp_apic_id);
            crate::drivers::serial::SerialPort::puts("[xhci] MSI enabled\n");
            return;
        }
    }

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
                        return;
                    }
                };
                match dma.map_mmio(phys, 0x10000) {
                    Ok(va) => va,
                    Err(e) => {
                        SerialPort::puts("[xhci] MSI-X BAR map: ");
                        SerialPort::puts(e);
                        SerialPort::puts("\n");
                        return;
                    }
                }
            };
            crate::pci::msix::program_entry(dev, cap, bar_va, 0, vector, bsp_apic_id);
            crate::pci::msix::enable(dev, cap, bar_va, 1, vector, bsp_apic_id);
            SerialPort::puts("[xhci] MSI-X enabled\n");
        }
        return;
    }

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
}

fn enumerate_initial_ports(
    usb_ports: &mut ports::UsbPorts,
    _cmd_ring: &memory::TrbRing,
    _dma: &mut UsbDmaAllocator,
    _doorbell_va: u64,
    _er_vaddr: u64,
    _er_trb_count: u32,
    _max_slots: u8,
) -> Result<(), &'static str> {
    use crate::drivers::serial::SerialPort;
    for port in &usb_ports.ports {
        if port.enabled && port.connected {
            SerialPort::puts("[xhci]  port ");
            SerialPort::put_u64(port.port_num as u64);
            SerialPort::puts(": device ready\n");
        }
    }
    Ok(())
}
