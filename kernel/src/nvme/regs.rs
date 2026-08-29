//! NVMe BAR0 register map (NVMe Base spec 2.4, Figure 35)

use core::ptr::{read_volatile, write_volatile};

#[derive(Clone, Copy)]
pub struct NvmeBar {
    pub vaddr: u64,
}

impl NvmeBar {
    pub fn r32(self, off: u32) -> u32 {
        unsafe { read_volatile((self.vaddr + off as u64) as *const u32) }
    }
    pub fn w32(self, off: u32, v: u32) {
        unsafe { write_volatile((self.vaddr + off as u64) as *mut u32, v) }
    }
    pub fn r64(self, off: u32) -> u64 {
        unsafe { read_volatile((self.vaddr + off as u64) as *const u64) }
    }
    pub fn w64(self, off: u32, v: u64) {
        unsafe { write_volatile((self.vaddr + off as u64) as *mut u64, v) }
    }
}

// Register offsets
pub const REG_CAP: u32 = 0x00; // 8 bytes
pub const REG_VS: u32 = 0x08; // 4
pub const REG_INTMS: u32 = 0x0C;
pub const REG_INTMC: u32 = 0x10;
pub const REG_CC: u32 = 0x14;
pub const REG_CSTS: u32 = 0x1C;
pub const REG_NSSR: u32 = 0x20;
pub const REG_AQA: u32 = 0x24;
pub const REG_ASQ: u32 = 0x28; // 8
pub const REG_ACQ: u32 = 0x30; // 8
pub const REG_CMBLOC: u32 = 0x38;
pub const REG_CMBSZ: u32 = 0x3C;

pub const DOORBELL_BASE: u32 = 0x1000;

// CAP helpers
#[inline]
pub fn cap_mqes(cap: u64) -> u16 {
    (cap & 0xFFFF) as u16
}
#[inline]
pub fn cap_cqr(cap: u64) -> bool {
    (cap >> 16) & 1 != 0
}
#[inline]
pub fn cap_to(cap: u64) -> u8 {
    ((cap >> 24) & 0xFF) as u8
}
#[inline]
pub fn cap_dstrd(cap: u64) -> u8 {
    ((cap >> 32) & 0xF) as u8
}
#[inline]
pub fn cap_mpsmin(cap: u64) -> u8 {
    ((cap >> 48) & 0xF) as u8
}
#[inline]
pub fn cap_mpsmax(cap: u64) -> u8 {
    ((cap >> 52) & 0xF) as u8
}

// CC bits
pub const CC_EN: u32 = 1 << 0;
pub const CC_CSS_NVM: u32 = 0 << 4; //bits 6:4
pub const CC_MPS_SHIFT: u32 = 7;
pub const CC_AMS_RR: u32 = 0 << 11;
pub const CC_SHN_NONE: u32 = 0 << 14;
pub const CC_SHN_NORMAL: u32 = 1 << 14;
pub const CC_IOSQES_SHIFT: u32 = 16;
pub const CC_IOCQES_SHIFT: u32 = 20;
pub const CC_CRIME: u32 = 1 << 24; // controller ready independent of media

// CSTS bits
pub const CSTS_RDY: u32 = 1 << 0;
pub const CSTS_CFS: u32 = 1 << 1;
pub const CSTS_SHST_MASK: u32 = 0x3 << 2;
pub const CSTS_SHST_NORMAL: u32 = 0 << 2;
pub const CSTS_NSSRO: u32 = 1 << 4;

// AQA
#[inline]
pub fn aqa_asqs(depth: u16) -> u32 {
    ((depth as u32) - 1) & 0xFFF
}
#[inline]
pub fn aqa_acqs(depth: u16) -> u32 {
    (((depth as u32) - 1) & 0xFFF) << 16
}
#[inline]
pub fn aqa_make(sq_depth: u16, cq_depth: u16) -> u32 {
    aqa_asqs(sq_depth) | aqa_acqs(cq_depth)
}

// Doorbell calculation
#[inline]
pub fn doorbell_sq(bar: NvmeBar, qid: u16, dstrd: u8) -> u64 {
    let stride = 4u64 << dstrd;
    bar.vaddr + DOORBELL_BASE as u64 + (2u64 * qid as u64) * stride
}
#[inline]
pub fn doorbell_cq(bar: NvmeBar, qid: u16, dstrd: u8) -> u64 {
    let stride = 4u64 << dstrd;
    bar.vaddr + DOORBELL_BASE as u64 + (2u64 * qid as u64 + 1) * stride
}

pub fn cc_value() -> u32 {
    // MPS=0 (4K), IOSQES=6 (64), IOCQES=4 (16)
    CC_EN | CC_CSS_NVM | CC_AMS_RR | CC_SHN_NONE | (6 << CC_IOSQES_SHIFT) | (4 << CC_IOCQES_SHIFT)
}
pub fn cc_disable_value() -> u32 {
    // keep IOSQES/IOCQES/MPS so CC=0 with same sizes
    (6 << CC_IOSQES_SHIFT) | (4 << CC_IOCQES_SHIFT)
}
