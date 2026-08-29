//! NVMe driver — split module.
//!
//! Implements `StorageDriver` probe for class 01/08/02 and exposes per-namespace
//! `BlockDevice`. Uses PRP (not SGL), MSI-X with 2 vectors (admin + I/O), single
//! I/O queue pair (qid 1) shared across namespaces. Falls back to polling if
//! MSI-X not available or vectors exhausted.
//!
//! Invariants: no `.unwrap`/`.expect` on device/disk data; all fallible paths
//! return `Result<_, &'static str>`.

#![allow(dead_code, unused_variables, unused_mut, unused_assignments, unused_comparisons)]

pub mod queue;
pub mod regs;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::filesystems::blockdriver::driver::StorageDriver;
use crate::filesystems::blockdriver::traits::{BlockDevice, IoBuffer, IoCompletions, IoRequest};
use crate::pci::PciDevice;
use crate::services::dma::{DmaAllocator, DmaBuffer};

use queue::{NvmeCqEntry, NvmeQueue};
use regs::{NvmeBar, *};

fn nvme_trace_enabled() -> bool {
    cfg!(feature = "nvme_trace")
}
#[allow(unused_macros)]
macro_rules! nvme_log {
    ($($arg:tt)*) => {
        if nvme_trace_enabled() {
            crate::drivers::serial::SerialPort::puts("[nvme] ");
            crate::drivers::serial::SerialPort::puts(&alloc::format!($($arg)*));
            crate::drivers::serial::SerialPort::puts("\n");
        }
    };
}

fn puts(s: &str) {
    crate::drivers::serial::SerialPort::puts(s);
}
fn put_hex(v: u64) {
    crate::drivers::serial::SerialPort::put_hex(v);
}
fn put_u64(v: u64) {
    crate::drivers::serial::SerialPort::put_u64(v);
}

fn cache_flush_line(addr: *const u8) {
    static CHECKED: AtomicBool = AtomicBool::new(false);
    static HAS_OPT: AtomicBool = AtomicBool::new(false);
    if !CHECKED.load(Ordering::Relaxed) {
        let res = core::arch::x86_64::__cpuid(7);
        HAS_OPT.store((res.ebx >> 23) & 1 == 1, Ordering::Relaxed);
        CHECKED.store(true, Ordering::Relaxed);
    }
    if HAS_OPT.load(Ordering::Relaxed) {
        unsafe { core::arch::asm!("clflushopt [{}]", in(reg) addr, options(nostack, preserves_flags)) };
    } else {
        unsafe { core::arch::asm!("clflush [{}]", in(reg) addr, options(nostack, preserves_flags)) };
    }
}

// ── IRQ handling ─────────────────────────────────────────────────

struct CtrlPtr(*const NvmeController);
unsafe impl Send for CtrlPtr {}
unsafe impl Sync for CtrlPtr {}

static IRQ_CTRLS: crate::filesystems::vfs::irq::IrqMutex<Vec<CtrlPtr>> =
    crate::filesystems::vfs::irq::IrqMutex::new(Vec::new());

fn handle_nvme_irq() {
    crate::arch::x86_64::idt::verify_integrity();
    let ctrls = IRQ_CTRLS.lock();
    for cptr in ctrls.iter() {
        if cptr.0.is_null() {
            continue;
        }
        // Defensive: low pointer indicates stale/recycled heap – skip.
        const HEAP_VA_MIN: u64 = 0xFFFF_8000_0000_0000;
        if (cptr.0 as u64) < HEAP_VA_MIN {
            continue;
        }
        let ctrl = unsafe { &*cptr.0 };
        // Validate queue BAs before deref – poll_cq already guards but double-check.
        // Use try_lock to avoid deadlock if IRQ interrupts a thread holding the queue lock
        // (both use PreemptMutex which disables preemption but not IRQs, so IRQ can preempt
        // a thread holding the lock on the same CPU and would otherwise spin forever).
        let admin_ok = {
            if let Some(admin) = ctrl.admin.try_lock() {
                if admin.depth == 0 || admin.cq_buf.virt < HEAP_VA_MIN {
                    false
                } else if let Some(entry) = admin.poll_cq() {
                    let _ = entry;
                    ctrl.admin_irq.store(1, Ordering::Release);
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };
        let io_ok = {
            if let Some(io) = ctrl.io.try_lock() {
                if io.depth == 0 || io.cq_buf.virt < HEAP_VA_MIN {
                    false
                } else if let Some(entry) = io.poll_cq() {
                    let _ = entry;
                    ctrl.io_irq.store(1, Ordering::Release);
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };
        // Only set generic flag if at least one queue made progress, or keep
        // semantic of always setting? Keep always but only if queues valid.
        if admin_ok || io_ok {
            ctrl.irq_fired.store(1, Ordering::Release);
        } else {
            // Still set for polling fallback, but only if ctrl looks sane.
            // Avoid spurious wake on completely stale ctrl – skip.
            // We set only if at least one queue was valid (depth>0).
            // Use try_lock to avoid deadlock on same CPU.
            let admin_depth = ctrl.admin.try_lock().map(|a| a.depth).unwrap_or(0);
            let io_depth = ctrl.io.try_lock().map(|i| i.depth).unwrap_or(0);
            if admin_depth != 0 || io_depth != 0 {
                ctrl.irq_fired.store(1, Ordering::Release);
            }
        }
    }
}

// ── Controller ───────────────────────────────────────────────────

struct NvmeController {
    bar: NvmeBar,
    cap: u64,
    vs: u32,
    dstrd: u8,
    admin: crate::sync::PreemptMutex<NvmeQueue>,
    io: crate::sync::PreemptMutex<NvmeQueue>,
    admin_irq: AtomicU32,
    io_irq: AtomicU32,
    irq_fired: AtomicU32,
    admin_vector: Option<u8>,
    io_vector: Option<u8>,
    prp_list: crate::sync::PreemptMutex<Option<DmaBuffer>>,
    model: [u8; 40],
    // keep DmaBuffers alive: prp_list is only one, but queue bufs are inside queues
}

unsafe impl Send for NvmeController {}
unsafe impl Sync for NvmeController {}

// ── Helpers for admin submission ─────────────────────────────────

fn admin_submit(
    ctrl: &NvmeController,
    opcode: u8,
    nsid: u32,
    prp1: u64,
    prp2: u64,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
) -> Result<NvmeCqEntry, &'static str> {
    let mut admin = ctrl.admin.lock();
    let cid = admin.sq_tail;
    let sq_idx = admin.next_sq_tail();
    // must not overflow depth
    let sq_ptr = admin.sq_entry_ptr(sq_idx);
    let cdw0 = opcode as u32 | ((cid as u32) << 16);
    // flags: FUSE 0, PSCT? 0
    unsafe {
        (*sq_ptr).cdw0 = cdw0;
        (*sq_ptr).nsid = nsid;
        (*sq_ptr).rsvd = 0;
        (*sq_ptr).mptr = 0;
        (*sq_ptr).prp1 = prp1;
        (*sq_ptr).prp2 = prp2;
        (*sq_ptr).cdw10 = cdw10;
        (*sq_ptr).cdw11 = cdw11;
        (*sq_ptr).cdw12 = cdw12;
        (*sq_ptr).cdw13 = 0;
        (*sq_ptr).cdw14 = 0;
        (*sq_ptr).cdw15 = 0;
        core::arch::asm!("mfence", options(nostack, preserves_flags));
    }
    admin.ring_sq();
    drop(admin);

    // Wait for completion — use try_lock in poll so wait can timeout even if lock contended.
    use crate::services::universal_timer::{now_ns, wait_until_cond};
    let deadline = now_ns() + 5_000_000_000;
    let done = || {
        // Check via IRQ flag or direct CQ poll
        if let Some(admin) = ctrl.admin.try_lock() {
            if let Some(_e) = admin.poll_cq() {
                return true;
            }
        } else {
            // If admin queue is locked (e.g., IRQ handler), treat as not ready and let outer loop recheck deadline.
            return false;
        }
        // also check irq flag which may have been set by interrupt
        if ctrl.irq_fired.load(Ordering::Acquire) != 0 {
            // re-check poll
            if let Some(admin2) = ctrl.admin.try_lock() {
                if admin2.poll_cq().is_some() {
                    return true;
                }
            }
        }
        false
    };
    let _ = wait_until_cond(deadline, &done);
    // Now try to consume
    let mut admin = ctrl.admin.lock();
    if let Some(entry) = admin.poll_cq() {
        if (entry.status & 1) != if admin.cq_phase { 1 } else { 0 } {
            // phase mismatch shouldn't happen if poll succeeded
        }
        // check CID matches
        if entry.cid != cid {
            // Wrong CID, but still consume? For simplicity we consume and check status
            // In real hardware, out-of-order completions possible, but QEMU is in-order
        }
        let status = entry.status;
        admin.advance_cq();
        ctrl.admin_irq.store(0, Ordering::Release);
        ctrl.irq_fired.store(0, Ordering::Release);
        if let Err(e) = queue::cq_status_to_result(status) {
            let (sct, sc, dnr) = queue::cq_status_decode(status);
            let name = queue::cq_status_name(sct, sc);
            puts("[nvme] admin cq error status=0x");
            put_hex(status as u64);
            puts(" sct=");
            put_u64(sct as u64);
            puts(" sc=0x");
            put_hex(sc as u64);
            puts(" dnr=");
            put_u64(dnr as u64);
            puts(" (");
            puts(name);
            puts(") cid=");
            put_u64(cid as u64);
            puts("\n");
            return Err(e);
        }
        Ok(entry)
    } else {
        puts("[nvme] admin timeout cid=");
        put_u64(cid as u64);
        puts("\n");
        Err("NVMe admin timeout")
    }
}

fn identify_controller(ctrl: &NvmeController, buf: &DmaBuffer) -> Result<(), &'static str> {
    let cns = queue::CNS_CONTROLLER;
    admin_submit(ctrl, queue::OPCODE_IDENTIFY, 0, buf.phys, 0, cns, 0, 0)?;
    Ok(())
}
fn identify_namespace(ctrl: &NvmeController, nsid: u32, buf: &DmaBuffer) -> Result<(), &'static str> {
    let cns = queue::CNS_NAMESPACE;
    admin_submit(ctrl, queue::OPCODE_IDENTIFY, nsid, buf.phys, 0, cns, 0, 0)?;
    Ok(())
}

fn create_io_cq(ctrl: &NvmeController, qid: u16, queue: &NvmeQueue, iv: u16, ien: bool) -> Result<(), &'static str> {
    let prp1 = queue.cq_buf.phys;
    let qsize = (queue.depth as u32 - 1) & 0xFFFF;
    let qid_u = qid as u32 & 0xFFFF;
    let cdw10 = qid_u | (qsize << 16);
    let cdw11 = 1 | if ien { 1 << 1 } else { 0 } | ((iv as u32) << 16);
    admin_submit(ctrl, queue::OPCODE_CREATE_IO_CQ, 0, prp1, 0, cdw10, cdw11, 0)?;
    Ok(())
}
fn create_io_sq(ctrl: &NvmeController, qid: u16, queue: &NvmeQueue, cqid: u16) -> Result<(), &'static str> {
    let prp1 = queue.sq_buf.phys;
    let qsize = (queue.depth as u32 - 1) & 0xFFFF;
    let qid_u = qid as u32 & 0xFFFF;
    let cdw10 = qid_u | (qsize << 16);
    // CDW11: CQID in bits 31:16, QPRIO 2:1 =0, PC bit0 =1. Previous code put CQID in low 16 overlapping PC, causing CQID=0 for any even CQID and "Completion Queue Invalid" (0x8201).
    let cdw11 = ((cqid as u32) << 16) | (1 << 0);
    admin_submit(ctrl, queue::OPCODE_CREATE_IO_SQ, 0, prp1, 0, cdw10, cdw11, 0)?;
    Ok(())
}

fn set_num_queues(ctrl: &NvmeController, num_cq: u16, num_sq: u16) -> Result<(u16, u16), &'static str> {
    // Feature ID 07h Number of Queues. CDW11: NCQR (31:16) | NSQR (15:00) — 0's based. Request 1 each => 0.
    let cdw10 = 0x07u32;
    let ncqr = if num_cq == 0 { 0 } else { num_cq - 1 };
    let nsqr = if num_sq == 0 { 0 } else { num_sq - 1 };
    let cdw11 = ((ncqr as u32) << 16) | (nsqr as u32);
    let entry = admin_submit(ctrl, queue::OPCODE_SET_FEATURES, 0, 0, 0, cdw10, cdw11, 0)?;
    let cdw0 = entry.cdw0;
    let nsqa = (cdw0 & 0xFFFF) as u16;
    let ncqa = ((cdw0 >> 16) & 0xFFFF) as u16;
    // Convert 0's based to count (add 1), but controller may return 0's based; spec says allocated queues are 0's based.
    // Return counts (1..).
    Ok((ncqa.wrapping_add(1), nsqa.wrapping_add(1)))
}

// ── PRP builder ──────────────────────────────────────────────────

fn build_prp(
    dma: &dyn DmaAllocator,
    buf_vaddr: u64,
    size: usize,
    prp_list_buf: Option<&DmaBuffer>,
) -> Result<(u64, u64), &'static str> {
    if size == 0 {
        return Ok((0, 0));
    }
    let offset = (buf_vaddr & 0xFFF) as usize;
    let first_page_len = (4096 - offset).min(size);
    let remaining = size - first_page_len;
    let prp1 = dma
        .virt_to_phys(buf_vaddr)
        .ok_or("PRP translate fail")?
        | (offset as u64);
    if remaining == 0 {
        return Ok((prp1, 0));
    }
    if remaining <= 4096 {
        let second_va = (buf_vaddr & !0xFFF) + 4096;
        let prp2 = dma
            .virt_to_phys(second_va)
            .ok_or("PRP translate fail")?;
        return Ok((prp1, prp2));
    }
    let prp_list = prp_list_buf.ok_or("PRP list required but not allocated")?;
    let total_pages = (size + offset).div_ceil(4096);
    if total_pages > 512 {
        return Err("PRP list too large (>512 entries)");
    }
    let pages_needed = total_pages - 1;
    let list_virt = prp_list.virt;
    unsafe { core::ptr::write_bytes(list_virt as *mut u8, 0, 4096) };
    for i in 0..pages_needed {
        let page_pa = dma
            .virt_to_phys((buf_vaddr & !0xFFF) + 4096 + i as u64 * 4096)
            .ok_or("PRP translate fail")?;
        unsafe {
            let entry_ptr = (list_virt + i as u64 * 8) as *mut u64;
            write_volatile(entry_ptr, page_pa);
        }
    }
    let prp2 = prp_list.phys;
    unsafe { core::arch::asm!("mfence", options(nostack, preserves_flags)) };
    Ok((prp1, prp2))
}

fn build_prp_phys(pa: u64, size: usize, prp_list_buf: Option<&DmaBuffer>) -> Result<(u64, u64), &'static str> {
    if size == 0 {
        return Ok((0, 0));
    }
    let offset = (pa & 0xFFF) as usize;
    let first_page_len = (4096 - offset).min(size);
    let remaining = size - first_page_len;
    if remaining == 0 {
        return Ok((pa, 0));
    }
    if remaining <= 4096 {
        let second_pa = (pa & !0xFFF) + 4096;
        return Ok((pa, second_pa));
    }
    let prp_list = prp_list_buf.ok_or("PRP list required")?;
    let total_pages = (size + offset).div_ceil(4096);
    if total_pages > 512 {
        return Err("PRP Phys too large");
    }
    let list_virt = prp_list.virt;
    unsafe { core::ptr::write_bytes(list_virt as *mut u8, 0, 4096) };
    for i in 0..(total_pages - 1) {
        let page_pa = (pa & !0xFFF) + 4096 + i as u64 * 4096;
        unsafe {
            let entry_ptr = (list_virt + i as u64 * 8) as *mut u64;
            write_volatile(entry_ptr, page_pa);
        }
    }
    unsafe { core::arch::asm!("mfence", options(nostack, preserves_flags)) };
    Ok((pa, prp_list.phys))
}

// ── Namespace BlockDevice ───────────────────────────────────────

struct NvmeNamespace {
    ctrl: Arc<NvmeController>,
    nsid: u32,
    nlb: u64,
    lba_shift: u8,
    model: [u8; 40],
    submit_lock: crate::sync::PreemptMutex<()>,
}

impl BlockDevice for NvmeNamespace {
    fn submit(&self, reqs: &[IoRequest]) -> Result<IoCompletions, &'static str> {
        let _guard = self.submit_lock.lock();
        if reqs.is_empty() {
            return Ok(IoCompletions { completed: 0, errors: 0 });
        }
        // For simplicity, handle one request at a time. Batch is rare for our VFS (CachedDevice batches)
        // But we allow batch of same direction.
        let is_write = reqs[0].is_write;
        for r in reqs.iter() {
            if r.is_write != is_write {
                return Err("NVMe mixed batch not supported");
            }
        }
        // Need to ensure queue depth
        let mut completed = 0u32;
        let mut errors = 0u32;

        for req in reqs.iter() {
            // Validate count
            if req.count == 0 {
                completed += 1;
                continue;
            }
            let bytes = (req.count as usize) * self.sector_size();
            let (buf_addr, buf_len, is_phys) = match &req.buffer {
                IoBuffer::Buf(b) => (b.as_ptr() as u64, b.len(), false),
                IoBuffer::ConstBuf(b) => (b.as_ptr() as u64, b.len(), false),
                IoBuffer::Phys(pa, sz) => (*pa, *sz, true),
            };
            if buf_len < bytes {
                errors += 1;
                continue;
            }
            // Build PRP
            let prp_guard = self.ctrl.prp_list.lock();
            let prp_list_ref = prp_guard.as_ref();
            let (prp1, prp2) = if is_phys {
                match build_prp_phys(buf_addr, bytes, prp_list_ref) {
                    Ok(v) => v,
                    Err(e) => {
                        errors += 1;
                        if nvme_trace_enabled() {
                            puts("[nvme] prp phys fail: ");
                            puts(e);
                            puts("\n");
                        }
                        continue;
                    }
                }
            } else {
                match build_prp(crate::services::kernel_services().dma, buf_addr, bytes, prp_list_ref) {
                    Ok(v) => v,
                    Err(e) => {
                        errors += 1;
                        if nvme_trace_enabled() {
                            puts("[nvme] prp fail: ");
                            puts(e);
                            puts("\n");
                        }
                        continue;
                    }
                }
            };
            // Cache flush for writes
            if is_write && !is_phys {
                let end = buf_addr + bytes as u64;
                let mut cl = buf_addr & !63u64;
                while cl < end {
                    cache_flush_line(cl as *const u8);
                    cl += 64;
                }
                unsafe { core::arch::asm!("mfence", options(nostack, preserves_flags)) };
            }
            // Submit to IO queue
            let res = {
                let mut io = self.ctrl.io.lock();
                let cid = io.sq_tail;
                let sq_idx = io.next_sq_tail();
                let sq_ptr = io.sq_entry_ptr(sq_idx);
                let opcode = if is_write { queue::OPCODE_IO_WRITE } else { queue::OPCODE_IO_READ };
                let cdw0 = opcode as u32 | ((cid as u32) << 16);
                let slba_low = (req.lba & 0xFFFF_FFFF) as u32;
                let slba_high = (req.lba >> 32) as u32;
                let nlb = (req.count - 1) as u32 & 0xFFFF;
                unsafe {
                    (*sq_ptr).cdw0 = cdw0;
                    (*sq_ptr).nsid = self.nsid;
                    (*sq_ptr).rsvd = 0;
                    (*sq_ptr).mptr = 0;
                    (*sq_ptr).prp1 = prp1;
                    (*sq_ptr).prp2 = prp2;
                    (*sq_ptr).cdw10 = slba_low;
                    (*sq_ptr).cdw11 = slba_high;
                    (*sq_ptr).cdw12 = nlb;
                    (*sq_ptr).cdw13 = 0;
                    (*sq_ptr).cdw14 = 0;
                    (*sq_ptr).cdw15 = 0;
                    core::arch::asm!("mfence", options(nostack, preserves_flags));
                }
                io.ring_sq();
                drop(io);
                drop(prp_guard);
                // Wait for completion
                use crate::services::universal_timer::{now_ns, wait_until_cond};
                let deadline = now_ns() + 5_000_000_000;
                let ctrl_clone = self.ctrl.clone();
                let done = || {
                    if let Some(io) = ctrl_clone.io.try_lock() {
                        if let Some(entry) = io.poll_cq() {
                            if entry.cid == cid {
                                return true;
                            }
                            // If different CID, still check phase mismatch? But we assume in-order.
                            // If poll shows entry with different CID, not ours yet; continue waiting
                            // unless we check irq_fired
                            let _ = entry;
                            return false;
                        }
                    } else {
                        return false;
                    }
                    if ctrl_clone.irq_fired.load(Ordering::Acquire) != 0 {
                        if let Some(io2) = ctrl_clone.io.try_lock() {
                            if let Some(e) = io2.poll_cq() {
                                return e.cid == cid;
                            }
                        }
                    }
                    false
                };
                let _ = wait_until_cond(deadline, &done);
                // Consume
                let mut io = self.ctrl.io.lock();
                if let Some(entry) = io.poll_cq() {
                    if entry.cid != cid {
                        // Mismatch - treat as error, but still advance to avoid stuck
                        if nvme_trace_enabled() {
                            puts("[nvme] cid mismatch io expected ");
                            put_u64(cid as u64);
                            puts(" got ");
                            put_u64(entry.cid as u64);
                            puts("\n");
                        }
                        io.advance_cq();
                        self.ctrl.irq_fired.store(0, Ordering::Release);
                        Err("NVMe CID mismatch")
                    } else {
                        let status = entry.status;
                        io.advance_cq();
                        self.ctrl.irq_fired.store(0, Ordering::Release);
                        match queue::cq_status_to_result(status) {
                            Ok(()) => Ok(()),
                            Err(e) => {
                                if nvme_trace_enabled() {
                                    puts("[nvme] io cq error status=0x");
                                    put_hex(status as u64);
                                    puts(" slba=");
                                    put_u64(req.lba);
                                    puts(" nlb=");
                                    put_u64(req.count as u64);
                                    puts("\n");
                                }
                                Err(e)
                            }
                        }
                    }
                } else {
                    // timeout
                    if nvme_trace_enabled() {
                        puts("[nvme] io timeout slba=");
                        put_u64(req.lba);
                        puts("\n");
                    }
                    Err("NVMe IO timeout")
                }
            };
            match res {
                Ok(()) => {
                    if !is_write && !is_phys {
                        let end = buf_addr + bytes as u64;
                        let mut cl = buf_addr & !63u64;
                        while cl < end {
                            cache_flush_line(cl as *const u8);
                            cl += 64;
                        }
                        unsafe { core::arch::asm!("mfence", options(nostack, preserves_flags)) };
                    }
                    completed += 1;
                }
                Err(_) => errors += 1,
            }
        }
        Ok(IoCompletions { completed, errors })
    }

    fn sector_count(&self) -> u64 {
        self.nlb
    }
    fn sector_size(&self) -> usize {
        1usize << self.lba_shift
    }
    fn model_string(&self) -> &str {
        let t = core::str::from_utf8(&self.model).unwrap_or("(bad utf8)");
        t.trim_end_matches(char::from(0)).trim()
    }
    fn sync(&self) -> Result<(), &'static str> {
        // Flush command: use admin or IO? Use IO flush on this NSID via admin queue? Spec: Flush is I/O command via I/O queue.
        // We'll issue via IO queue with opcode FLUSH, prp 0, slba 0.
        let _guard = self.submit_lock.lock();
        let mut io = self.ctrl.io.lock();
        let cid = io.sq_tail;
        let sq_idx = io.next_sq_tail();
        let sq_ptr = io.sq_entry_ptr(sq_idx);
        let cdw0 = queue::OPCODE_IO_FLUSH as u32 | ((cid as u32) << 16);
        unsafe {
            (*sq_ptr).cdw0 = cdw0;
            (*sq_ptr).nsid = self.nsid;
            (*sq_ptr).rsvd = 0;
            (*sq_ptr).mptr = 0;
            (*sq_ptr).prp1 = 0;
            (*sq_ptr).prp2 = 0;
            (*sq_ptr).cdw10 = 0;
            (*sq_ptr).cdw11 = 0;
            (*sq_ptr).cdw12 = 0;
            (*sq_ptr).cdw13 = 0;
            (*sq_ptr).cdw14 = 0;
            (*sq_ptr).cdw15 = 0;
            core::arch::asm!("mfence", options(nostack, preserves_flags));
        }
        io.ring_sq();
        drop(io);
        use crate::services::universal_timer::{now_ns, wait_until_cond};
        let deadline = now_ns() + 5_000_000_000;
        let ctrl_clone = self.ctrl.clone();
        let done = || {
            if let Some(io) = ctrl_clone.io.try_lock() {
                if let Some(entry) = io.poll_cq() {
                    return entry.cid == cid;
                }
            }
            false
        };
        let _ = wait_until_cond(deadline, &done);
        let mut io = self.ctrl.io.lock();
        if let Some(entry) = io.poll_cq() {
            if entry.cid != cid {
                io.advance_cq();
                return Err("NVMe flush cid mismatch");
            }
            let status = entry.status;
            io.advance_cq();
            queue::cq_status_to_result(status)
        } else {
            Err("NVMe flush timeout")
        }
    }
}

// ── Init helpers ───────────────────────────────────────────────

fn wait_csts(bar: NvmeBar, want_rdy: bool, timeout_ms: u64) -> bool {
    use crate::services::universal_timer::{now_ns, wait_until_cond};
    let deadline = now_ns() + timeout_ms * 1_000_000;
    wait_until_cond(deadline, &|| ((bar.r32(REG_CSTS) & CSTS_RDY) != 0) == want_rdy)
}

fn init_controller_inner(
    dev: &PciDevice,
    dma: &dyn DmaAllocator,
) -> Result<Vec<Arc<dyn BlockDevice>>, &'static str> {
    crate::pci::enable_device(dev);
    // Decode BAR0
    let bar_paddr = match crate::pci::bar::bar(dev, 0) {
        crate::pci::bar::Bar::Memory { addr, .. } => addr,
        _ => {
            puts("[nvme] BAR0 not memory\n");
            return Err("NVMe BAR0 not memory");
        }
    };
    let bar_size = crate::pci::bar::bar_size(dev, 0).unwrap_or(0x4000);
    let bar_size_aligned = (bar_size + 0xFFF) & !0xFFF;
    let bar_va = dma.map_mmio(bar_paddr, bar_size_aligned)?;
    let bar = NvmeBar { vaddr: bar_va };

    let cap = bar.r64(REG_CAP);
    let vs = bar.r32(REG_VS);
    let cc = bar.r32(REG_CC);
    puts("[nvme] ctrl ");
    put_u64(dev.bus as u64);
    puts(":");
    put_u64(dev.device as u64);
    puts(":");
    put_u64(dev.function as u64);
    puts(" cap=0x");
    put_hex(cap);
    puts(" vs=0x");
    put_hex(vs as u64);
    puts(" cc=0x");
    put_hex(cc as u64);
    puts("\n");
    if cap == 0 || cap == 0xFFFF_FFFF_FFFF_FFFF {
        return Err("NVMe CAP invalid");
    }
    if bar.r32(REG_CSTS) == 0xFFFF_FFFF {
        return Err("NVMe CSTS invalid");
    }

    let mqes = regs::cap_mqes(cap);
    let dstrd = regs::cap_dstrd(cap);
    let to = regs::cap_to(cap);
    let mpsmin = regs::cap_mpsmin(cap);
    let mpsmax = regs::cap_mpsmax(cap);
    puts("[nvme] mqes=");
    put_u64(mqes as u64);
    puts(" dstrd=");
    put_u64(dstrd as u64);
    puts(" to=");
    put_u64(to as u64);
    puts(" mpsmin=");
    put_u64(mpsmin as u64);
    puts(" mpsmax=");
    put_u64(mpsmax as u64);
    puts("\n");
    if mpsmin > 0 {
        puts("[nvme] WARN: mpsmin>0 but we use 4K\n");
    }
    let _ = mpsmax;

    // Disable if enabled
    if cc & CC_EN != 0 {
        puts("[nvme] disabling controller\n");
        bar.w32(REG_CC, cc & !CC_EN);
        let timeout = (to as u64) * 500 + 2000; // spec TO in 500ms units, plus margin
        if !wait_csts(bar, false, timeout) {
            puts("[nvme] disable timeout\n");
            return Err("NVMe disable timeout");
        }
    }
    // Check CFS
    if bar.r32(REG_CSTS) & CSTS_CFS != 0 {
        puts("[nvme] controller fatal status\n");
        return Err("NVMe CFS set");
    }

    // MSI-X setup before admin queue (so admin interrupts work)
    let mut admin_vector: Option<u8> = None;
    let mut io_vector: Option<u8> = None;
    let mut msix_cap: Option<crate::pci::caps::PciCapability> = None;
    let mut _msix_bar_va: u64 = 0;

    // Try MSI-X first
    if let Some(cap_msix) = crate::pci::caps::find(dev, crate::pci::caps::CAP_MSIX) {
        let info = crate::pci::msix::table_info(dev, &cap_msix);
        if info.table_size >= 2 {
            // Map MSI-X table BAR if not BAR0
            let (table_bar_va, pba_bar_va) = if info.bir == 0 && info.pba_bir == 0 {
                (bar_va, bar_va)
            } else {
                // Need to map other BARs
                let mut t_va = bar_va;
                let mut p_va = bar_va;
                if info.bir != 0 {
                    match crate::pci::bar::bar(dev, info.bir) {
                        crate::pci::bar::Bar::Memory { addr, .. } => {
                            let sz = crate::pci::bar::bar_size(dev, info.bir).unwrap_or(0x1000);
                            let sz_a = (sz + 0xFFF) & !0xFFF;
                            match dma.map_mmio(addr, sz_a) {
                                Ok(va) => t_va = va,
                                Err(_) => t_va = 0,
                            }
                        }
                        _ => t_va = 0,
                    }
                }
                if info.pba_bir != info.bir {
                    if info.pba_bir == 0 {
                        p_va = bar_va;
                    } else {
                        match crate::pci::bar::bar(dev, info.pba_bir) {
                            crate::pci::bar::Bar::Memory { addr, .. } => {
                                let sz = crate::pci::bar::bar_size(dev, info.pba_bir).unwrap_or(0x1000);
                                let sz_a = (sz + 0xFFF) & !0xFFF;
                                match dma.map_mmio(addr, sz_a) {
                                    Ok(va) => p_va = va,
                                    Err(_) => p_va = 0,
                                }
                            }
                            _ => p_va = 0,
                        }
                    }
                } else {
                    p_va = t_va;
                }
                (t_va, p_va)
            };
            if table_bar_va != 0 && pba_bar_va != 0 {
                // Allocate 2 vectors
                let v1 = crate::arch::x86_64::idt::register_device_handler(handle_nvme_irq);
                let v2 = crate::arch::x86_64::idt::register_device_handler(handle_nvme_irq);
                if let (Some(v_a), Some(v_b)) = (v1, v2) {
                    let apic_id = crate::platform::x86_64_pc::apic::read_apic_id();
                    // Enable MSI-X with 2 entries using first vector, then reprogram second
                    crate::pci::msix::enable(dev, &cap_msix, table_bar_va, pba_bar_va, 2, v_a, apic_id);
                    // Reprogram second entry to v_b
                    crate::pci::msix::program_entry(dev, &cap_msix, table_bar_va, 1, v_b, apic_id);
                    // Disable MSI if present (mutually exclusive)
                    if let Some(msi_cap) = crate::pci::caps::find(dev, crate::pci::caps::CAP_MSI) {
                        crate::pci::msi::disable(dev, &msi_cap);
                    }
                    admin_vector = Some(v_a);
                    io_vector = Some(v_b);
                    msix_cap = Some(cap_msix);
                    _msix_bar_va = table_bar_va;
                    puts("[nvme] MSI-X 2 vectors admin=");
                    put_u64(v_a as u64);
                    puts(" io=");
                    put_u64(v_b as u64);
                    puts("\n");
                    // Mask INTMS? For MSI-X, INTMS/INTMC are undefined; but clear mask
                    bar.w32(REG_INTMC, 0xFFFF_FFFF);
                } else {
                    if let Some(v) = v1 {
                        crate::arch::x86_64::idt::unregister_device_handler(v);
                    }
                    if let Some(v) = v2 {
                        crate::arch::x86_64::idt::unregister_device_handler(v);
                    }
                    puts("[nvme] MSI-X vector alloc failed, fallback\n");
                }
            }
        } else if info.table_size == 1 {
            // Only 1 entry, use single vector for both
            let v = crate::arch::x86_64::idt::register_device_handler(handle_nvme_irq);
            if let Some(v_a) = v {
                let apic_id = crate::platform::x86_64_pc::apic::read_apic_id();
                let table_bar_va = if info.bir == 0 { bar_va } else { 0 };
                let pba_bar_va = table_bar_va;
                if table_bar_va != 0 {
                    crate::pci::msix::enable(dev, &cap_msix, table_bar_va, pba_bar_va, 1, v_a, apic_id);
                    admin_vector = Some(v_a);
                    io_vector = Some(v_a);
                    msix_cap = Some(cap_msix);
                    _msix_bar_va = table_bar_va;
                    puts("[nvme] MSI-X 1 vector ");
                    put_u64(v_a as u64);
                    puts("\n");
                    bar.w32(REG_INTMC, 0xFFFF_FFFF);
                }
            }
        }
    }
    if admin_vector.is_none() {
        // Try MSI
        if let Some(msi_cap) = crate::pci::caps::find(dev, crate::pci::caps::CAP_MSI) {
            if let Some(v) = crate::arch::x86_64::idt::register_device_handler(handle_nvme_irq) {
                let apic_id = crate::platform::x86_64_pc::apic::read_apic_id();
                crate::pci::msi::enable(dev, &msi_cap, v, apic_id);
                admin_vector = Some(v);
                io_vector = Some(v);
                puts("[nvme] MSI vector ");
                put_u64(v as u64);
                puts("\n");
                bar.w32(REG_INTMC, 0xFFFF_FFFF);
            }
        }
    }
    if admin_vector.is_none() {
        puts("[nvme] polling mode (no MSI-X/MSI)\n");
        // Unmask interrupts via INTMC for polling? Actually polling doesn't need interrupts, but INTMS mask should be set?
        // For INTx, we would enable IOAPIC, but we skip and poll.
    } else {
        // Unmask all
        if msix_cap.is_none() {
            // For MSI/INTx, ensure INTMC unmasks
            // MSI uses direct APIC, no IOAPIC needed
        }
    }

    // Allocate admin queue
    let admin_depth: u16 = core::cmp::min(mqes as u16 + 1, 32);
    let admin_sq_db = regs::doorbell_sq(bar, 0, dstrd);
    let admin_cq_db = regs::doorbell_cq(bar, 0, dstrd);
    let admin_q = NvmeQueue::new(0, admin_depth, dma, admin_sq_db, admin_cq_db).ok_or("OOM admin queue")?;
    // Keep copies of phys for register programming
    let admin_sq_phys = admin_q.sq_buf.phys;
    let admin_cq_phys = admin_q.cq_buf.phys;

    puts("[nvme] admin queue depth=");
    put_u64(admin_depth as u64);
    puts(" sq=0x");
    put_hex(admin_sq_phys);
    puts(" cq=0x");
    put_hex(admin_cq_phys);
    puts("\n");

    // Program AQA, ASQ, ACQ
    bar.w32(REG_AQA, regs::aqa_make(admin_depth, admin_depth));
    bar.w64(REG_ASQ, admin_sq_phys);
    bar.w64(REG_ACQ, admin_cq_phys);

    // Enable controller
    let cc_val = regs::cc_value(); // includes EN
    bar.w32(REG_CC, cc_val);
    let timeout = (to as u64) * 500 + 2000;
    if !wait_csts(bar, true, timeout) {
        puts("[nvme] enable timeout\n");
        dma.free(&admin_q.sq_buf);
        dma.free(&admin_q.cq_buf);
        if let Some(cap) = msix_cap.as_ref() {
            crate::pci::msix::disable(dev, cap);
        } else if admin_vector.is_some() {
            if let Some(msi_cap) = crate::pci::caps::find(dev, crate::pci::caps::CAP_MSI) {
                crate::pci::msi::disable(dev, &msi_cap);
            }
        }
        if let Some(v) = admin_vector {
            crate::arch::x86_64::idt::unregister_device_handler(v);
        }
        if let Some(v) = io_vector {
            if Some(v) != admin_vector {
                crate::arch::x86_64::idt::unregister_device_handler(v);
            }
        }
        return Err("NVMe enable timeout");
    }
    if bar.r32(REG_CSTS) & CSTS_CFS != 0 {
        puts("[nvme] CFS after enable\n");
        bar.w32(REG_CC, bar.r32(REG_CC) & !CC_EN);
        let _ = wait_csts(bar, false, 2000);
        dma.free(&admin_q.sq_buf);
        dma.free(&admin_q.cq_buf);
        if let Some(cap) = msix_cap.as_ref() {
            crate::pci::msix::disable(dev, cap);
        } else if admin_vector.is_some() {
            if let Some(msi_cap) = crate::pci::caps::find(dev, crate::pci::caps::CAP_MSI) {
                crate::pci::msi::disable(dev, &msi_cap);
            }
        }
        if let Some(v) = admin_vector {
            crate::arch::x86_64::idt::unregister_device_handler(v);
        }
        if let Some(v) = io_vector {
            if Some(v) != admin_vector {
                crate::arch::x86_64::idt::unregister_device_handler(v);
            }
        }
        return Err("NVMe CFS after enable");
    }
    puts("[nvme] controller ready\n");
    if vs != 0 {
        puts("[nvme] vs=");
        put_hex(vs as u64);
        puts(" cap=");
        put_hex(cap);
        puts("\n");
    }

    // Build controller object early to use admin_submit — cleanup vectors/DMA/CC on OOM.
    let prp_list_buf = match dma.alloc_page() {
        Some(b) => b,
        None => {
            dma.free(&admin_q.sq_buf);
            dma.free(&admin_q.cq_buf);
            if let Some(cap) = msix_cap.as_ref() {
                crate::pci::msix::disable(dev, cap);
            } else if admin_vector.is_some() {
                if let Some(msi_cap) = crate::pci::caps::find(dev, crate::pci::caps::CAP_MSI) {
                    crate::pci::msi::disable(dev, &msi_cap);
                }
            }
            if let Some(v) = admin_vector {
                crate::arch::x86_64::idt::unregister_device_handler(v);
            }
            if let Some(v) = io_vector {
                if Some(v) != admin_vector {
                    crate::arch::x86_64::idt::unregister_device_handler(v);
                }
            }
            bar.w32(REG_CC, bar.r32(REG_CC) & !CC_EN);
            let _ = wait_csts(bar, false, 2000);
            return Err("OOM prp list");
        }
    };
    // Placeholder IO queue: allocate minimal 1-page each for sq/cq until real IO queue is created
    let placeholder_sq = match dma.alloc_page() {
        Some(b) => b,
        None => {
            dma.free(&prp_list_buf);
            dma.free(&admin_q.sq_buf);
            dma.free(&admin_q.cq_buf);
            if let Some(cap) = msix_cap.as_ref() {
                crate::pci::msix::disable(dev, cap);
            } else if admin_vector.is_some() {
                if let Some(msi_cap) = crate::pci::caps::find(dev, crate::pci::caps::CAP_MSI) {
                    crate::pci::msi::disable(dev, &msi_cap);
                }
            }
            if let Some(v) = admin_vector {
                crate::arch::x86_64::idt::unregister_device_handler(v);
            }
            if let Some(v) = io_vector {
                if Some(v) != admin_vector {
                    crate::arch::x86_64::idt::unregister_device_handler(v);
                }
            }
            bar.w32(REG_CC, bar.r32(REG_CC) & !CC_EN);
            let _ = wait_csts(bar, false, 2000);
            return Err("OOM placeholder sq");
        }
    };
    let placeholder_cq = match dma.alloc_page() {
        Some(b) => b,
        None => {
            dma.free(&placeholder_sq);
            dma.free(&prp_list_buf);
            dma.free(&admin_q.sq_buf);
            dma.free(&admin_q.cq_buf);
            if let Some(cap) = msix_cap.as_ref() {
                crate::pci::msix::disable(dev, cap);
            } else if admin_vector.is_some() {
                if let Some(msi_cap) = crate::pci::caps::find(dev, crate::pci::caps::CAP_MSI) {
                    crate::pci::msi::disable(dev, &msi_cap);
                }
            }
            if let Some(v) = admin_vector {
                crate::arch::x86_64::idt::unregister_device_handler(v);
            }
            if let Some(v) = io_vector {
                if Some(v) != admin_vector {
                    crate::arch::x86_64::idt::unregister_device_handler(v);
                }
            }
            bar.w32(REG_CC, bar.r32(REG_CC) & !CC_EN);
            let _ = wait_csts(bar, false, 2000);
            return Err("OOM placeholder cq");
        }
    };
    let placeholder_io = NvmeQueue {
        qid: 1,
        depth: 1,
        sq_buf: placeholder_sq,
        cq_buf: placeholder_cq,
        sq_tail: 0,
        cq_head: 0,
        cq_phase: true,
        db_sq: 0,
        db_cq: 0,
    };
    let ctrl = Arc::new(NvmeController {
        bar,
        cap,
        vs,
        dstrd,
        admin: crate::sync::PreemptMutex::new(admin_q),
        io: crate::sync::PreemptMutex::new(placeholder_io),
        admin_irq: AtomicU32::new(0),
        io_irq: AtomicU32::new(0),
        irq_fired: AtomicU32::new(0),
        admin_vector,
        io_vector,
        prp_list: crate::sync::PreemptMutex::new(Some(prp_list_buf)),
        model: [0u8; 40],
    });

    // Identify controller — IRQ registration is deferred until IO queues are fully
    // created. Early push left a dangling raw pointer if identify/Create IO CQ
    // failed and the Arc was dropped (UAF fault at 0x171000e).
    let ident_buf = match dma.alloc_page() {
        Some(b) => b,
        None => {
            {
                let admin = ctrl.admin.lock();
                dma.free(&admin.sq_buf);
                dma.free(&admin.cq_buf);
            }
            {
                let io = ctrl.io.lock();
                dma.free(&io.sq_buf);
                dma.free(&io.cq_buf);
            }
            if let Some(pb) = ctrl.prp_list.lock().take() {
                dma.free(&pb);
            }
            if let Some(cap) = msix_cap.as_ref() {
                crate::pci::msix::disable(dev, cap);
            } else if admin_vector.is_some() {
                if let Some(msi_cap) = crate::pci::caps::find(dev, crate::pci::caps::CAP_MSI) {
                    crate::pci::msi::disable(dev, &msi_cap);
                }
            }
            if let Some(v) = admin_vector {
                crate::arch::x86_64::idt::unregister_device_handler(v);
            }
            if let Some(v) = io_vector {
                if Some(v) != admin_vector {
                    crate::arch::x86_64::idt::unregister_device_handler(v);
                }
            }
            bar.w32(REG_CC, bar.r32(REG_CC) & !CC_EN);
            let _ = wait_csts(bar, false, 2000);
            return Err("OOM ident");
        }
    };
    if let Err(e) = identify_controller(&ctrl, &ident_buf) {
        dma.free(&ident_buf);
        {
            let admin = ctrl.admin.lock();
            dma.free(&admin.sq_buf);
            dma.free(&admin.cq_buf);
        }
        {
            let io = ctrl.io.lock();
            dma.free(&io.sq_buf);
            dma.free(&io.cq_buf);
        }
        if let Some(pb) = ctrl.prp_list.lock().take() {
            dma.free(&pb);
        }
        if let Some(cap) = msix_cap.as_ref() {
            crate::pci::msix::disable(dev, cap);
        } else if admin_vector.is_some() {
            if let Some(msi_cap) = crate::pci::caps::find(dev, crate::pci::caps::CAP_MSI) {
                crate::pci::msi::disable(dev, &msi_cap);
            }
        }
        if let Some(v) = admin_vector {
            crate::arch::x86_64::idt::unregister_device_handler(v);
        }
        if let Some(v) = io_vector {
            if Some(v) != admin_vector {
                crate::arch::x86_64::idt::unregister_device_handler(v);
            }
        }
        bar.w32(REG_CC, bar.r32(REG_CC) & !CC_EN);
        let _ = wait_csts(bar, false, 2000);
        puts("[nvme] identify failed: ");
        puts(e);
        puts("\n");
        return Err(e);
    }
    // Parse
    let ident_virt = ident_buf.virt;
    let nn: u32;
    let mn: [u8; 40];
    unsafe {
        nn = read_volatile((ident_virt + 0x204) as *const u32);
        let mut m = [0u8; 40];
        for i in 0..40 {
            m[i] = read_volatile((ident_virt + 0x18 + i as u64) as *const u8);
        }
        mn = m;
    }
    puts("[nvme] NN=");
    put_u64(nn as u64);
    puts(" MN='");
    for &b in mn.iter() {
        if b == 0 { break; }
        crate::drivers::serial::SerialPort::putc(b);
    }
    puts("'\n");
    if nn == 0 {
        dma.free(&ident_buf);
        {
            let admin = ctrl.admin.lock();
            dma.free(&admin.sq_buf);
            dma.free(&admin.cq_buf);
        }
        {
            let io = ctrl.io.lock();
            dma.free(&io.sq_buf);
            dma.free(&io.cq_buf);
        }
        if let Some(pb) = ctrl.prp_list.lock().take() {
            dma.free(&pb);
        }
        if let Some(cap) = msix_cap.as_ref() {
            crate::pci::msix::disable(dev, cap);
        } else if admin_vector.is_some() {
            if let Some(msi_cap) = crate::pci::caps::find(dev, crate::pci::caps::CAP_MSI) {
                crate::pci::msi::disable(dev, &msi_cap);
            }
        }
        if let Some(v) = admin_vector {
            crate::arch::x86_64::idt::unregister_device_handler(v);
        }
        if let Some(v) = io_vector {
            if Some(v) != admin_vector {
                crate::arch::x86_64::idt::unregister_device_handler(v);
            }
        }
        bar.w32(REG_CC, bar.r32(REG_CC) & !CC_EN);
        let _ = wait_csts(bar, false, 2000);
        return Err("NVMe NN=0");
    }
    if nn > 32 {
        puts("[nvme] NN large, capping scan to 32\n");
    }
    let scan_nn = core::cmp::min(nn, 32);

    // Set Number of Queues (Feature 07h) — must be before any IO queue creation per spec 5.2.30.1.5.
    // Request 1 IO CQ + 1 IO SQ (0's based 0). Log allocated counts; if it fails we proceed anyway
    // since some controllers (QEMU) may have default allocation.
    match set_num_queues(&ctrl, 1, 1) {
        Ok((ncqa, nsqa)) => {
            puts("[nvme] Number of Queues allocated: CQ=");
            put_u64(ncqa as u64);
            puts(" SQ=");
            put_u64(nsqa as u64);
            puts("\n");
        }
        Err(e) => {
            puts("[nvme] Set Number of Queues failed: ");
            puts(e);
            puts(" (proceeding anyway)\n");
        }
    }

    // Create IO queues before scanning namespaces: need 1 IO queue
    let io_depth: u16 = core::cmp::min(mqes as u16 + 1, 64);
    let io_qid: u16 = 1;
    let io_sq_db = regs::doorbell_sq(bar, io_qid, dstrd);
    let io_cq_db = regs::doorbell_cq(bar, io_qid, dstrd);
    // Determine IV and IEN: for MSI-X 2 vectors, IV=1 (second entry), IEN=1; for 1 vector or MSI, IV=0; for polling, IEN=0
    let (iv, ien) = match (admin_vector, io_vector) {
        (Some(a), Some(b)) if a != b => (1, true), // 2 distinct MSI-X vectors
        (Some(_), Some(_)) => (0, true),          // shared MSI-X or MSI
        _ => (0, false),                          // polling
    };
    // Create IO CQ first — with OOM and CQ-error cleanup, plus polling fallback.
    let io_queue = match NvmeQueue::new(io_qid, io_depth, dma, io_sq_db, io_cq_db) {
        Some(q) => q,
        None => {
            dma.free(&ident_buf);
            {
                let admin = ctrl.admin.lock();
                dma.free(&admin.sq_buf);
                dma.free(&admin.cq_buf);
            }
            {
                let io = ctrl.io.lock();
                dma.free(&io.sq_buf);
                dma.free(&io.cq_buf);
            }
            if let Some(pb) = ctrl.prp_list.lock().take() {
                dma.free(&pb);
            }
            if let Some(cap) = msix_cap.as_ref() {
                crate::pci::msix::disable(dev, cap);
            } else if admin_vector.is_some() {
                if let Some(msi_cap) = crate::pci::caps::find(dev, crate::pci::caps::CAP_MSI) {
                    crate::pci::msi::disable(dev, &msi_cap);
                }
            }
            if let Some(v) = admin_vector {
                crate::arch::x86_64::idt::unregister_device_handler(v);
            }
            if let Some(v) = io_vector {
                if Some(v) != admin_vector {
                    crate::arch::x86_64::idt::unregister_device_handler(v);
                }
            }
            bar.w32(REG_CC, bar.r32(REG_CC) & !CC_EN);
            let _ = wait_csts(bar, false, 2000);
            return Err("OOM io queue");
        }
    };
    // Need to create CQ before SQ via admin — try MSI-X vector first, fall back to polling.
    let mut cq_iv = iv;
    let mut cq_ien = ien;
    if let Err(e) = create_io_cq(&ctrl, io_qid, &io_queue, cq_iv, cq_ien) {
        let (sct, sc, _d) = queue::cq_status_decode(0x8201); // placeholder, real status already logged in admin_submit
        puts("[nvme] IO CQ create failed with iv=");
        put_u64(cq_iv as u64);
        puts(" ien=");
        put_u64(cq_ien as u64);
        puts(" err=");
        puts(e);
        puts(" — trying polling fallback\n");
        let _ = (sct, sc);
        if cq_iv != 0 || cq_ien {
            cq_iv = 0;
            cq_ien = false;
            if let Err(e2) = create_io_cq(&ctrl, io_qid, &io_queue, cq_iv, cq_ien) {
                puts("[nvme] IO CQ polling fallback also failed: ");
                puts(e2);
                puts("\n");
                dma.free(&io_queue.sq_buf);
                dma.free(&io_queue.cq_buf);
                dma.free(&ident_buf);
                {
                    let admin = ctrl.admin.lock();
                    dma.free(&admin.sq_buf);
                    dma.free(&admin.cq_buf);
                }
                {
                    let io = ctrl.io.lock();
                    dma.free(&io.sq_buf);
                    dma.free(&io.cq_buf);
                }
                if let Some(pb) = ctrl.prp_list.lock().take() {
                    dma.free(&pb);
                }
                if let Some(cap) = msix_cap.as_ref() {
                    crate::pci::msix::disable(dev, cap);
                } else if admin_vector.is_some() {
                    if let Some(msi_cap) = crate::pci::caps::find(dev, crate::pci::caps::CAP_MSI) {
                        crate::pci::msi::disable(dev, &msi_cap);
                    }
                }
                if let Some(v) = admin_vector {
                    crate::arch::x86_64::idt::unregister_device_handler(v);
                }
                if let Some(v) = io_vector {
                    if Some(v) != admin_vector {
                        crate::arch::x86_64::idt::unregister_device_handler(v);
                    }
                }
                bar.w32(REG_CC, bar.r32(REG_CC) & !CC_EN);
                let _ = wait_csts(bar, false, 2000);
                return Err(e2);
            } else {
                puts("[nvme] IO CQ polling fallback succeeded\n");
            }
        } else {
            dma.free(&io_queue.sq_buf);
            dma.free(&io_queue.cq_buf);
            dma.free(&ident_buf);
            {
                let admin = ctrl.admin.lock();
                dma.free(&admin.sq_buf);
                dma.free(&admin.cq_buf);
            }
            {
                let io = ctrl.io.lock();
                dma.free(&io.sq_buf);
                dma.free(&io.cq_buf);
            }
            if let Some(pb) = ctrl.prp_list.lock().take() {
                dma.free(&pb);
            }
            if let Some(cap) = msix_cap.as_ref() {
                crate::pci::msix::disable(dev, cap);
            } else if admin_vector.is_some() {
                if let Some(msi_cap) = crate::pci::caps::find(dev, crate::pci::caps::CAP_MSI) {
                    crate::pci::msi::disable(dev, &msi_cap);
                }
            }
            if let Some(v) = admin_vector {
                crate::arch::x86_64::idt::unregister_device_handler(v);
            }
            if let Some(v) = io_vector {
                if Some(v) != admin_vector {
                    crate::arch::x86_64::idt::unregister_device_handler(v);
                }
            }
            bar.w32(REG_CC, bar.r32(REG_CC) & !CC_EN);
            let _ = wait_csts(bar, false, 2000);
            return Err(e);
        }
    }
    puts("[nvme] IO CQ created qid=1 depth=");
    put_u64(io_depth as u64);
    puts(" iv=");
    put_u64(cq_iv as u64);
    puts(" ien=");
    put_u64(cq_ien as u64);
    puts("\n");
    if let Err(e) = create_io_sq(&ctrl, io_qid, &io_queue, io_qid) {
        puts("[nvme] IO SQ create failed: ");
        puts(e);
        puts("\n");
        dma.free(&io_queue.sq_buf);
        dma.free(&io_queue.cq_buf);
        dma.free(&ident_buf);
        {
            let admin = ctrl.admin.lock();
            dma.free(&admin.sq_buf);
            dma.free(&admin.cq_buf);
        }
        {
            let io = ctrl.io.lock();
            dma.free(&io.sq_buf);
            dma.free(&io.cq_buf);
        }
        if let Some(pb) = ctrl.prp_list.lock().take() {
            dma.free(&pb);
        }
        if let Some(cap) = msix_cap.as_ref() {
            crate::pci::msix::disable(dev, cap);
        } else if admin_vector.is_some() {
            if let Some(msi_cap) = crate::pci::caps::find(dev, crate::pci::caps::CAP_MSI) {
                crate::pci::msi::disable(dev, &msi_cap);
            }
        }
        if let Some(v) = admin_vector {
            crate::arch::x86_64::idt::unregister_device_handler(v);
        }
        if let Some(v) = io_vector {
            if Some(v) != admin_vector {
                crate::arch::x86_64::idt::unregister_device_handler(v);
            }
        }
        bar.w32(REG_CC, bar.r32(REG_CC) & !CC_EN);
        let _ = wait_csts(bar, false, 2000);
        return Err(e);
    }
    puts("[nvme] IO SQ created qid=1 cqid=1\n");

    // Replace placeholder IO queue with real one — free placeholder DMA and register IRQ.
    {
        let mut io = ctrl.io.lock();
        dma.free(&io.sq_buf);
        dma.free(&io.cq_buf);
        *io = io_queue;
    }
    // Defer IRQ registration until after queues are fully ready: prevents UAF if earlier steps failed.
    IRQ_CTRLS.lock().push(CtrlPtr(Arc::as_ptr(&ctrl) as *const NvmeController));

    // Now scan namespaces — free each ns_buf after use, handle OOM with full teardown.
    let mut devices: Vec<Arc<dyn BlockDevice>> = Vec::new();
    for nsid in 1..=scan_nn {
        let ns_buf = match dma.alloc_page() {
            Some(b) => b,
            None => {
                puts("[nvme] OOM ns ident\n");
                // Teardown already-registered IRQ and controller resources.
                {
                    let mut ctrls = IRQ_CTRLS.lock();
                    ctrls.retain(|p| p.0 != Arc::as_ptr(&ctrl) as *const NvmeController);
                }
                dma.free(&ident_buf);
                {
                    let admin = ctrl.admin.lock();
                    dma.free(&admin.sq_buf);
                    dma.free(&admin.cq_buf);
                }
                {
                    let io = ctrl.io.lock();
                    dma.free(&io.sq_buf);
                    dma.free(&io.cq_buf);
                }
                if let Some(pb) = ctrl.prp_list.lock().take() {
                    dma.free(&pb);
                }
                if let Some(cap) = msix_cap.as_ref() {
                    crate::pci::msix::disable(dev, cap);
                } else if admin_vector.is_some() {
                    if let Some(msi_cap) = crate::pci::caps::find(dev, crate::pci::caps::CAP_MSI) {
                        crate::pci::msi::disable(dev, &msi_cap);
                    }
                }
                if let Some(v) = admin_vector {
                    crate::arch::x86_64::idt::unregister_device_handler(v);
                }
                if let Some(v) = io_vector {
                    if Some(v) != admin_vector {
                        crate::arch::x86_64::idt::unregister_device_handler(v);
                    }
                }
                bar.w32(REG_CC, bar.r32(REG_CC) & !CC_EN);
                let _ = wait_csts(bar, false, 2000);
                return Err("OOM ns ident");
            }
        };
        match identify_namespace(&ctrl, nsid, &ns_buf) {
            Ok(()) => {
                // Parse NSZE, LBAF
                let (nsze, lbads): (u64, u8) = unsafe {
                    let nsze = read_volatile((ns_buf.virt + 0x00) as *const u64);
                    let _nlba = read_volatile((ns_buf.virt + 0x19) as *const u8);
                    let flbas = read_volatile((ns_buf.virt + 0x1A) as *const u8);
                    let lbaf_idx = (flbas & 0x0F) as usize;
                    if lbaf_idx >= 16 {
                        (0, 0)
                    } else {
                        let lbaf_off = 0x80 + lbaf_idx * 4;
                        let lbads = read_volatile((ns_buf.virt + lbaf_off as u64 + 2) as *const u8);
                        (nsze, lbads)
                    }
                };
                dma.free(&ns_buf);
                if nsze == 0 {
                    puts("[nvme] ns ");
                    put_u64(nsid as u64);
                    puts(" empty, skip\n");
                    continue;
                }
                let lbads_v = if lbads == 0 { 9 } else { lbads }; // default 512 if zero (some controllers report 0 for invalid)
                // Validate lbads 9..15? Allow 9 (512), 12 (4096), etc. If lbads <9 or >15, clamp to 9
                let lba_shift = if lbads_v < 9 || lbads_v > 15 { 9 } else { lbads_v };
                let sector_size = 1u64 << lba_shift;
                puts("[nvme] ns ");
                put_u64(nsid as u64);
                puts(" nsze=");
                put_u64(nsze);
                puts(" lbads=");
                put_u64(lba_shift as u64);
                puts(" (");
                put_u64(sector_size);
                puts(" B)\n");
                // Model for namespace = controller MN + nsid
                let model = mn;
                // Could append nsid
                let ns = Arc::new(NvmeNamespace {
                    ctrl: ctrl.clone(),
                    nsid,
                    nlb: nsze,
                    lba_shift,
                    model,
                    submit_lock: crate::sync::PreemptMutex::new(()),
                });
                devices.push(ns as Arc<dyn BlockDevice>);
            }
            Err(e) => {
                dma.free(&ns_buf);
                puts("[nvme] identify ns ");
                put_u64(nsid as u64);
                puts(" failed: ");
                puts(e);
                puts("\n");
                // If first NS fails, try next? QEMU returns error for non-existent NS after NN.
                continue;
            }
        }
    }

    if devices.is_empty() {
        // No namespaces → teardown IRQ registration and controller.
        {
            let mut ctrls = IRQ_CTRLS.lock();
            ctrls.retain(|p| p.0 != Arc::as_ptr(&ctrl) as *const NvmeController);
        }
        dma.free(&ident_buf);
        {
            let admin = ctrl.admin.lock();
            dma.free(&admin.sq_buf);
            dma.free(&admin.cq_buf);
        }
        {
            let io = ctrl.io.lock();
            dma.free(&io.sq_buf);
            dma.free(&io.cq_buf);
        }
        if let Some(pb) = ctrl.prp_list.lock().take() {
            dma.free(&pb);
        }
        if let Some(cap) = msix_cap.as_ref() {
            crate::pci::msix::disable(dev, cap);
        } else if admin_vector.is_some() {
            if let Some(msi_cap) = crate::pci::caps::find(dev, crate::pci::caps::CAP_MSI) {
                crate::pci::msi::disable(dev, &msi_cap);
            }
        }
        if let Some(v) = admin_vector {
            crate::arch::x86_64::idt::unregister_device_handler(v);
        }
        if let Some(v) = io_vector {
            if Some(v) != admin_vector {
                crate::arch::x86_64::idt::unregister_device_handler(v);
            }
        }
        bar.w32(REG_CC, bar.r32(REG_CC) & !CC_EN);
        let _ = wait_csts(bar, false, 2000);
        return Err("NVMe no namespaces found");
    }
    puts("[nvme] ");
    put_u64(devices.len() as u64);
    puts(" namespace(s) ready\n");
    dma.free(&ident_buf);

    Ok(devices)
}

// ── StorageDriver impl ─────────────────────────────────────────

pub struct NvmeDriver;

impl StorageDriver for NvmeDriver {
    fn name(&self) -> &str {
        "nvme"
    }
    fn probe(&self, dev: &PciDevice) -> bool {
        dev.class == 0x01 && dev.subclass == 0x08 && (dev.prog_if == 0x02 || dev.prog_if == 0x00)
    }
    fn init_controller(
        &self,
        dev: &PciDevice,
        dma: &dyn DmaAllocator,
    ) -> Result<Vec<Arc<dyn BlockDevice>>, &'static str> {
        init_controller_inner(dev, dma)
    }
}
