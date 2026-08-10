//! Boot-time SMP work dispatcher (x86_64 only).
//!
//! Lets the BSP fan the device-sweep work (per-storage-controller probing)
//! out across the idle APs while they are still parked in `proc::ap_main`'s
//! pre-scheduler wait loop.  A single shared FIFO is drained by every online
//! CPU; `run_parallel` blocks until every posted job has *completed*, so no
//! job is ever stranded when the scheduler takes over.
//!
//! Jobs are plain `FnOnce` closures.  Every piece of shared state they touch
//! (the DMA allocator, ioapic, obj tables, serial, the DMA VA cursor) is already
//! lock-guarded, and the MMIO/DMA mappings they create land in the shared
//! kernel-root page tables every CPU runs under — page-table mutation there is
//! serialized by the VMM-wide `PAGE_TABLE` lock, so concurrent jobs cannot race
//! the intermediate-table allocator and the mappings are visible cross-CPU.

use core::sync::atomic::{AtomicUsize, Ordering};
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::services::irqsafe::IrqLock;
use crate::services::lockorder;
use crate::services::universal_timer::{now_ns, wait_until_cond};

/// A job closure.  `Send`: it may run on any AP's stack.
pub type Job = Box<dyn FnOnce() + Send>;

static QUEUE: IrqLock<VecDeque<Job>> = IrqLock::with_order(VecDeque::new(), lockorder::BOOT_WORK);
static PENDING: AtomicUsize = AtomicUsize::new(0);

/// Push `jobs` onto the shared queue and nudge every AP to drain it.
///
/// Must be called from the BSP with the queue drained (one parallel phase at a
/// time); `run_parallel` enforces this.
fn post(jobs: Vec<Job>) {
    if jobs.is_empty() {
        return;
    }
    let n = jobs.len();
    {
        let mut q = QUEUE.lock();
        debug_assert!(q.is_empty(), "smp:work: post onto a non-empty queue");
        for job in jobs {
            q.push_back(job);
        }
        // Set the counter under the same lock that guards pops, so a draining
        // CPU can never decrement a counter that does not yet include the job.
        PENDING.store(n, Ordering::Release);
    }
    // Wake the APs out of their pre-scheduler halt so they pick up the jobs.
    crate::platform::x86_64_pc::apic::send_ipi_all_except_self(
        crate::platform::x86_64_pc::apic::IPI_SCHED,
    );
}

/// Run every job currently queued, in FIFO order.  Called from each AP's
/// pre-scheduler loop (and by the BSP once it joins the drain).  Returns the
/// number of jobs run.
pub fn drain(_cpu: u32) -> usize {
    let mut ran = 0;
    loop {
        let job = QUEUE.lock().pop_front();
        match job {
            Some(job) => {
                ran += 1;
                job();
                // Completion is observed only after the job has fully run, so
                // `run_parallel` can never return while work is still live.
                PENDING.fetch_sub(1, Ordering::Release);
            }
            None => return ran,
        }
    }
}

/// Run `jobs` to completion, fanning them out across the idle APs.
///
/// The BSP posts the jobs, waits for an AP to demonstrably start draining (so
/// it cannot vacuum the whole queue before the wake-IPI lands), then joins the
/// drain itself.  On a uniprocessor (or without APs) the jobs just run inline.
pub fn run_parallel(jobs: Vec<Job>) {
    if jobs.is_empty() {
        return;
    }
    if crate::smp::cpu_count() <= 1 {
        for job in jobs {
            job();
        }
        return;
    }
    let total = jobs.len();
    post(jobs);

    // Wait until at least one job has completed — proof the APs are draining —
    // before the BSP joins, otherwise the BSP would grab every job and the
    // sweep would run serially.
    wait_until_cond(now_ns() + 1_000_000_000, &|| {
        PENDING.load(Ordering::Acquire) < total
    });

    // Join the drain.  Jobs have their own internal timeouts, so keep waiting
    // (in bounded HLT slices) until everything is done rather than risk
    // returning early with a stranded job.
    loop {
        drain(crate::smp::current_cpu_id());
        if PENDING.load(Ordering::Acquire) == 0 {
            return;
        }
        let deadline = now_ns() + 100_000_000;
        wait_until_cond(deadline, &|| PENDING.load(Ordering::Acquire) == 0);
    }
}
