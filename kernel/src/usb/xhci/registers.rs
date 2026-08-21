use crate::services::dma::DmaAllocator;

pub const USBCMD_RUN: u32 = 1 << 0;
pub const USBCMD_HCRST: u32 = 1 << 1;
pub const USBCMD_INTE: u32 = 1 << 2;
pub const USBCMD_HSEE: u32 = 1 << 3;
pub const USBCMD_LHCRST: u32 = 1 << 7;
pub const USBCMD_CSS: u32 = 1 << 8;
pub const USBCMD_CRS: u32 = 1 << 9;
pub const USBCMD_EWE: u32 = 1 << 10;
pub const USBCMD_EUSE: u32 = 1 << 11;

pub const USBSTS_HCH: u32 = 1 << 0;
pub const USBSTS_HSE: u32 = 1 << 2;
pub const USBSTS_EINT: u32 = 1 << 3;
pub const USBSTS_PCD: u32 = 1 << 4;
pub const USBSTS_SSS: u32 = 1 << 8;
pub const USBSTS_RSS: u32 = 1 << 9;
pub const USBSTS_SRE: u32 = 1 << 10;
pub const USBSTS_CNR: u32 = 1 << 11;
pub const USBSTS_HCE: u32 = 1 << 12;

pub const OP_USBCMD: u32 = 0x00;
pub const OP_USBSTS: u32 = 0x04;
pub const OP_PAGESIZE: u32 = 0x08;
pub const OP_DNCTRL: u32 = 0x14;
pub const OP_CRCR: u32 = 0x18;
pub const OP_DCBAAP: u32 = 0x30;
pub const OP_CONFIG: u32 = 0x38;

pub const IMAN_IE: u32 = 1 << 1;

pub const PORTSC_CCS: u32 = 1 << 0;
pub const PORTSC_PED: u32 = 1 << 1;
pub const PORTSC_OCA: u32 = 1 << 3;
pub const PORTSC_PR: u32 = 1 << 4;
pub const PORTSC_PLS_MASK: u32 = 0xF << 5;
pub const PORTSC_PP: u32 = 1 << 9;
pub const PORTSC_SPEED_SHIFT: u32 = 10;
pub const PORTSC_SPEED_MASK: u32 = 0xF << 10;
pub const PORTSC_LWS: u32 = 1 << 11;
pub const PORTSC_CSC: u32 = 1 << 17;
pub const PORTSC_PEC: u32 = 1 << 18;
pub const PORTSC_WRC: u32 = 1 << 19;
pub const PORTSC_OCC: u32 = 1 << 20;
pub const PORTSC_PRC: u32 = 1 << 21;
pub const PORTSC_PLC: u32 = 1 << 22;
pub const PORTSC_CEC: u32 = 1 << 23;

pub const PORTSC_STATUS_BITS: u32 =
    PORTSC_CSC | PORTSC_PEC | PORTSC_WRC | PORTSC_OCC | PORTSC_PRC | PORTSC_PLC | PORTSC_CEC;

const DEFAULT_MMIO_SIZE: u64 = 0x10000;
const MMIO_SIZE: u64 = DEFAULT_MMIO_SIZE;

pub struct XhciRegisters {
    mmio_va: u64,
    caplength: u8,
    op_base: u64,
    doorbell_va: u64,
    runtime_va: u64,
}

impl XhciRegisters {
    pub fn new(phys_base: u64, dma: &dyn DmaAllocator) -> Result<Self, &'static str> {
        Self::new_with_size(phys_base, dma, DEFAULT_MMIO_SIZE)
    }

    pub fn new_with_size(
        phys_base: u64,
        dma: &dyn DmaAllocator,
        mut mmio_size: u64,
    ) -> Result<Self, &'static str> {
        // Real xHCI BARs may exceed 64 KiB when extended caps spill past 0x10000.
        // Probe callers should pass a sized value; fallback to default if zero.
        if mmio_size == 0 {
            mmio_size = DEFAULT_MMIO_SIZE;
        }
        // Round up to page size for VMM.
        mmio_size = (mmio_size + 0xFFF) & !0xFFF;
        let mmio_va = dma.map_mmio(phys_base, mmio_size)?;

        let caplength = unsafe { core::ptr::read_volatile(mmio_va as *const u8) };
        let op_base = mmio_va + caplength as u64;

        let rts_off_val = Self::read32_inner(mmio_va, 0x18) & !0x1F;
        let runtime_off = if rts_off_val != 0 {
            rts_off_val
        } else {
            0x8000
        };
        let runtime_va = mmio_va + runtime_off as u64;

        let dboff = Self::read32_inner(mmio_va, 0x14) & !0x3;
        let doorbell_va = mmio_va + dboff as u64;

        Ok(XhciRegisters {
            mmio_va,
            caplength,
            op_base,
            doorbell_va,
            runtime_va,
        })
    }

    pub fn mmio_base(&self) -> u64 {
        self.mmio_va
    }
    pub fn cap_length(&self) -> u8 {
        self.caplength
    }
    pub fn op_base(&self) -> u64 {
        self.op_base
    }
    pub fn doorbell_va(&self) -> u64 {
        self.doorbell_va
    }
    pub fn runtime_va(&self) -> u64 {
        self.runtime_va
    }

    fn read32_inner(base: u64, offset: u64) -> u32 {
        unsafe { core::ptr::read_volatile((base + offset) as *const u32) }
    }

    fn write32_inner(base: u64, offset: u64, val: u32) {
        unsafe { core::ptr::write_volatile((base + offset) as *mut u32, val) }
    }

    pub fn read_cap32(&self, offset: u16) -> u32 {
        Self::read32_inner(self.mmio_va, offset as u64)
    }

    pub fn read_op32(&self, offset: u32) -> u32 {
        Self::read32_inner(self.op_base, offset as u64)
    }

    pub fn write_op32(&self, offset: u32, val: u32) {
        Self::write32_inner(self.op_base, offset as u64, val);
    }

    pub fn read_portsc(&self, port_num: u8) -> u32 {
        let off = self.caplength as u64 + 0x400 + (port_num as u64 - 1) * 0x10;
        Self::read32_inner(self.mmio_va, off)
    }

    pub fn write_portsc(&self, port_num: u8, val: u32) {
        let off = self.caplength as u64 + 0x400 + (port_num as u64 - 1) * 0x10;
        Self::write32_inner(self.mmio_va, off, val);
    }
}

pub fn read_cap_id(base: u64, offset: u64) -> u8 {
    unsafe { core::ptr::read_volatile((base + offset) as *const u8) }
}

pub fn read_cap_next(base: u64, offset: u64) -> u8 {
    unsafe { core::ptr::read_volatile((base + offset + 1) as *const u8) }
}

pub fn read_cap_data32(base: u64, offset: u64, reg: u16) -> u32 {
    unsafe { core::ptr::read_volatile((base + offset + reg as u64) as *const u32) }
}

pub fn read_protocol_string(base: u64, offset: u64) -> [u8; 20] {
    let mut buf = [0u8; 20];
    for (i, chunk) in buf.chunks_exact_mut(4).enumerate() {
        let word =
            unsafe { core::ptr::read_volatile((base + offset + 4 + (i as u64) * 4) as *const u32) };
        chunk.copy_from_slice(&word.to_le_bytes());
    }
    buf
}

pub struct HcsParams1(u32);

impl HcsParams1 {
    pub fn from(raw: u32) -> Self {
        HcsParams1(raw)
    }
    pub fn max_slots(&self) -> u8 {
        (self.0 & 0xFF) as u8
    }
    pub fn max_intrs(&self) -> u16 {
        ((self.0 >> 8) & 0x7FF) as u16
    }
    pub fn max_ports(&self) -> u8 {
        ((self.0 >> 24) & 0xFF) as u8
    }
}

pub struct HcsParams2(u32);

impl HcsParams2 {
    pub fn from(raw: u32) -> Self {
        HcsParams2(raw)
    }
    pub fn erst_max(&self) -> u8 {
        ((self.0 >> 4) & 0xF) as u8
    }
    pub fn scratchpad_bufs(&self) -> u16 {
        // HCSPARAMS2: bits 31:27 = Max Scratchpad Buffers Lo, bits 26:22 = Hi.
        // The register value is the Hi field shifted above the Lo field.
        let lo = (self.0 >> 27) & 0x1F;
        let hi = (self.0 >> 22) & 0x1F;
        ((hi as u16) << 5) | lo as u16
    }
}

pub struct HccParams1(u32);

impl HccParams1 {
    pub fn from(raw: u32) -> Self {
        HccParams1(raw)
    }
    pub fn ac64(&self) -> bool {
        self.0 & (1 << 0) != 0
    }
    pub fn csz(&self) -> bool {
        self.0 & (1 << 2) != 0
    }
    pub fn xecp(&self) -> u16 {
        ((self.0 >> 16) as u16) << 2
    }
}

pub struct PortRegisterSet {
    mmio_va: u64,
    caplength: u64,
}

impl PortRegisterSet {
    pub fn new(mmio_va: u64, caplength: u8) -> Self {
        PortRegisterSet {
            mmio_va,
            caplength: caplength as u64,
        }
    }

    fn port_off(&self, port_num: u8) -> u64 {
        self.caplength + 0x400 + (port_num as u64 - 1) * 0x10
    }

    pub fn read_portsc(&self, port_num: u8) -> u32 {
        let off = self.port_off(port_num);
        unsafe { core::ptr::read_volatile((self.mmio_va + off) as *const u32) }
    }

    pub fn write_portsc(&self, port_num: u8, val: u32) {
        let off = self.port_off(port_num);
        unsafe { core::ptr::write_volatile((self.mmio_va + off) as *mut u32, val) }
    }
}

pub struct Erst {
    pub seg_phys: u64,
    pub seg_va: u64,
}

impl Erst {
    pub fn new(dma: &dyn DmaAllocator) -> Result<Self, &'static str> {
        let buf = dma.alloc_page().ok_or("OOM for ERST")?;
        unsafe { core::ptr::write_bytes(buf.virt as *mut u8, 0, 16) }
        Ok(Erst {
            seg_phys: buf.phys,
            seg_va: buf.virt,
        })
    }
}

pub struct EventRing {
    pub phys: u64,
    pub virt: u64,
    pub trb_count: u16,
    pub dequeue_index: u16,
}

impl EventRing {
    pub fn new(dma: &dyn DmaAllocator, erst_seg_va: u64) -> Result<Self, &'static str> {
        let trb_count = 256;
        let bytes = (trb_count as usize) * 16;
        let pages = (bytes + 4095) / 4096;
        let buf = dma.alloc_contiguous(pages).ok_or("OOM for event ring")?;
        unsafe { core::ptr::write_bytes(buf.virt as *mut u8, 0, buf.size) };

        let seg_ptr = erst_seg_va as *mut u8;
        unsafe {
            core::ptr::write_volatile(seg_ptr as *mut u32, buf.phys as u32);
            core::ptr::write_volatile((seg_ptr.add(4)) as *mut u32, (buf.phys >> 32) as u32);
            core::ptr::write_volatile((seg_ptr.add(8)) as *mut u32, trb_count as u32);
            core::ptr::write_volatile((seg_ptr.add(12)) as *mut u32, 0);
        }

        Ok(EventRing {
            phys: buf.phys,
            virt: buf.virt,
            trb_count,
            dequeue_index: 0,
        })
    }
}
