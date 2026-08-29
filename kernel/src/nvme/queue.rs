//! NVMe queue definitions

use core::ptr::{read_volatile, write_volatile};

use crate::services::dma::{DmaAllocator, DmaBuffer};

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NvmeSqEntry {
    pub cdw0: u32,
    pub nsid: u32,
    pub rsvd: u64,
    pub mptr: u64,
    pub prp1: u64,
    pub prp2: u64,
    pub cdw10: u32,
    pub cdw11: u32,
    pub cdw12: u32,
    pub cdw13: u32,
    pub cdw14: u32,
    pub cdw15: u32,
}
const _: () = assert!(core::mem::size_of::<NvmeSqEntry>() == 64);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NvmeCqEntry {
    pub cdw0: u32,
    pub rsvd: u32,
    pub sq_head: u16,
    pub sq_id: u16,
    pub cid: u16,
    pub status: u16,
}
const _: () = assert!(core::mem::size_of::<NvmeCqEntry>() == 16);

pub struct NvmeQueue {
    pub qid: u16,
    pub depth: u16,
    pub sq_buf: DmaBuffer,
    pub cq_buf: DmaBuffer,
    pub sq_tail: u16,
    pub cq_head: u16,
    pub cq_phase: bool,
    pub db_sq: u64,
    pub db_cq: u64,
}

impl NvmeQueue {
    pub fn new(
        qid: u16,
        depth: u16,
        dma: &dyn DmaAllocator,
        db_sq: u64,
        db_cq: u64,
    ) -> Option<Self> {
        // Each entry 64 / 16 bytes. Depth entries need depth*size bytes, rounded to pages.
        let sq_bytes = depth as usize * 64;
        let cq_bytes = depth as usize * 16;
        let sq_pages = sq_bytes.div_ceil(4096);
        let cq_pages = cq_bytes.div_ceil(4096);
        let sq_buf = dma.alloc_contiguous(sq_pages)?;
        let cq_buf = dma.alloc_contiguous(cq_pages)?;
        // already zeroed by dma
        Some(NvmeQueue {
            qid,
            depth,
            sq_buf,
            cq_buf,
            sq_tail: 0,
            cq_head: 0,
            cq_phase: true,
            db_sq,
            db_cq,
        })
    }

    pub fn sq_entry_ptr(&self, idx: u16) -> *mut NvmeSqEntry {
        (self.sq_buf.virt + idx as u64 * 64) as *mut NvmeSqEntry
    }
    pub fn cq_entry_ptr(&self, idx: u16) -> *const NvmeCqEntry {
        (self.cq_buf.virt + idx as u64 * 16) as *const NvmeCqEntry
    }

    /// Write SQ tail doorbell
    pub fn ring_sq(&mut self) {
        unsafe {
            core::arch::asm!("mfence", options(nostack, preserves_flags));
            write_volatile(self.db_sq as *mut u32, self.sq_tail as u32);
        }
    }
    /// Write CQ head doorbell
    pub fn ring_cq(&mut self) {
        unsafe {
            write_volatile(self.db_cq as *mut u32, self.cq_head as u32);
        }
    }

    /// Poll for completion of `cid` at current cq_head. Returns Some(status) if ready.
    /// Caller must hold lock and manage cq_head/phase.
    pub fn poll_cq(&self) -> Option<NvmeCqEntry> {
        if self.depth == 0 || self.cq_buf.virt == 0 || self.cq_buf.size == 0 {
            return None;
        }
        // Defensive: low VA (phys) or non-canonical indicates stale/recycled queue.
        // DMA arena lives at high canonical VA (>= KERNEL_VMA_BASE-0x90000000). Never deref low pages.
        const DMA_VA_MIN: u64 = 0xFFFF_8000_0000_0000;
        if self.cq_buf.virt < DMA_VA_MIN {
            return None;
        }
        if (self.cq_head as usize) >= self.depth as usize {
            return None;
        }
        let ptr = self.cq_entry_ptr(self.cq_head);
        if (ptr as u64) < DMA_VA_MIN {
            return None;
        }
        let status = unsafe { read_volatile(&(*ptr).status) };
        let phase = (status & 1) != 0;
        if phase != self.cq_phase {
            return None;
        }
        // Ensure entry is fully visible before reading rest
        unsafe { core::arch::asm!("lfence", options(nostack, preserves_flags)) };
        let entry = unsafe { read_volatile(ptr) };
        Some(entry)
    }

    pub fn advance_cq(&mut self) {
        self.cq_head = self.cq_head.wrapping_add(1);
        if self.cq_head >= self.depth {
            self.cq_head = 0;
            self.cq_phase = !self.cq_phase;
        }
        self.ring_cq();
    }

    pub fn next_sq_tail(&mut self) -> u16 {
        let t = self.sq_tail;
        self.sq_tail = self.sq_tail.wrapping_add(1);
        if self.sq_tail >= self.depth {
            self.sq_tail = 0;
        }
        t
    }
}

// Admin opcodes
pub const OPCODE_CREATE_IO_CQ: u8 = 0x05;
pub const OPCODE_CREATE_IO_SQ: u8 = 0x01;
pub const OPCODE_DELETE_IO_CQ: u8 = 0x04;
pub const OPCODE_DELETE_IO_SQ: u8 = 0x00;
pub const OPCODE_IDENTIFY: u8 = 0x06;
pub const OPCODE_GET_LOG_PAGE: u8 = 0x02;
pub const OPCODE_SET_FEATURES: u8 = 0x09;
pub const OPCODE_GET_FEATURES: u8 = 0x0A;
pub const OPCODE_FLUSH: u8 = 0x00; // I/O flush opcode 0x00

// I/O opcodes
pub const OPCODE_IO_READ: u8 = 0x02;
pub const OPCODE_IO_WRITE: u8 = 0x01;
pub const OPCODE_IO_FLUSH: u8 = 0x00;

pub const CNS_CONTROLLER: u32 = 0x01;
pub const CNS_NAMESPACE: u32 = 0x00;
pub const CNS_ACTIVE_NS_LIST: u32 = 0x02;

// Status codes
#[inline]
pub fn cq_status_to_result(status: u16) -> Result<(), &'static str> {
    let sc = (status >> 1) & 0xFF;
    let sct = (status >> 9) & 0x7;
    if sc == 0 && sct == 0 {
        Ok(())
    } else {
        // Keep generic; caller logs detailed SC/SCT via decode helper.
        Err("NVMe CQ error")
    }
}

#[inline]
pub fn cq_status_decode(status: u16) -> (u8, u8, bool) {
    let sc = ((status >> 1) & 0xFF) as u8;
    let sct = ((status >> 9) & 0x7) as u8;
    let dnr = (status >> 14) & 1 != 0;
    (sct, sc, dnr)
}

pub fn cq_status_name(sct: u8, sc: u8) -> &'static str {
    match (sct, sc) {
        (0, 0x00) => "Success",
        (0, 0x01) => "Invalid Command Opcode",
        (0, 0x02) => "Invalid Field in Command",
        (0, 0x03) => "Command ID Conflict",
        (0, 0x04) => "Data Transfer Error",
        (0, 0x05) => "Commands Aborted due to Power Loss",
        (0, 0x06) => "Internal Device Error",
        (0, 0x07) => "Command Abort Requested",
        (0, 0x08) => "Command Aborted due to SQ Deletion",
        (0, 0x09) => "Command Aborted due to Failed Fused Command",
        (0, 0x0A) => "Command Aborted due to Missing Fused Command",
        (0, 0x0B) => "Invalid Namespace or Format",
        (0, 0x0C) => "Command Sequence Error",
        (0, 0x0D) => "Invalid SGL Segment Descriptor",
        (0, 0x0E) => "Invalid Number of SGL Descriptors",
        (0, 0x0F) => "Data SGL Length Invalid",
        (0, 0x10) => "Metadata SGL Length Invalid",
        (0, 0x11) => "SGL Descriptor Type Invalid",
        (0, 0x13) => "Invalid Use of Controller Memory Buffer",
        (0, 0x14) => "PRP Offset Invalid",
        (0, 0x15) => "Atomic Write Unit Exceeded",
        (0, 0x16) => "Operation Denied",
        (0, 0x1C) => "Invalid Protection Information",
        (1, 0x00) => "Completion Queue Invalid",
        (1, 0x01) => "Invalid Queue Identifier",
        (1, 0x02) => "Maximum Queue Size Exceeded",
        (1, 0x03) => "Abort Command Limit Exceeded",
        (1, 0x05) => "Asynchronous Event Request Limit Exceeded",
        (1, 0x06) => "Invalid Firmware Slot",
        (1, 0x07) => "Invalid Firmware Image",
        (1, 0x08) => "Invalid Interrupt Vector",
        (1, 0x09) => "Invalid Log Page",
        (1, 0x0A) => "Invalid Format",
        (1, 0x0B) => "Firmware Activation Requires Conventional Reset",
        (1, 0x0C) => "Invalid Queue Deletion",
        (1, 0x0D) => "Feature Identifier Not Saveable",
        (1, 0x0E) => "Feature Not Changeable",
        (1, 0x0F) => "Feature Not Namespace Specific",
        (1, 0x10) => "Firmware Activation Requires NVM Subsystem Reset",
        (1, 0x11) => "Firmware Activation Requires Controller Level Reset",
        (1, 0x12) => "Firmware Activation Requires Maximum Time Violation",
        (1, 0x13) => "Firmware Activation Prohibited",
        (1, 0x14) => "Overlapping Range",
        (1, 0x1A) => "Namespace Not Ready",
        (1, 0x1B) => "Conflicting Attributes",
        (1, 0x1C) => "Invalid Protection Information (cmd spec)",
        (1, 0x1D) => "Attempted Write to Read Only Range",
        _ => "Unknown",
    }
}
