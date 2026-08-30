//! Fault diagnostic dump — register, stack, code & page-table output.
//!
//! Replaces bare `fault_halt` handlers.  Uses the tiny disassembler in
//! `super::disasm`.  Self-contained with no dependency on scheduler /
//! VCPU / service infra.

use core::arch::asm;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use x86_64::structures::idt::InterruptStackFrame;

use crate::drivers::serial::{dump_put_hex, dump_puts};
use crate::smp::{MAX_CPUS, current_cpu_id};

// ── x86-64 PTE flag constants ───────────────────────────────────────

const PTE_PRESENT: u64 = 1 << 0;
const PTE_WRITABLE: u64 = 1 << 1;
const PTE_USER: u64 = 1 << 2;
const PTE_NO_EXEC: u64 = 1 << 63;

// ── Per-CPU re-entrancy guard ──────────────────────────────────────

static DUMP_IN_PROGRESS: [AtomicBool; MAX_CPUS] = [
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
];

pub fn is_dump_in_progress() -> bool {
    let cpu = current_cpu_id() as usize;
    if cpu < MAX_CPUS {
        DUMP_IN_PROGRESS[cpu].load(Ordering::Relaxed)
    } else {
        false
    }
}

pub fn dump_in_progress_snapshot() -> [bool; MAX_CPUS] {
    let mut out = [false; MAX_CPUS];
    for i in 0..MAX_CPUS {
        out[i] = DUMP_IN_PROGRESS[i].load(Ordering::Relaxed);
    }
    out
}

// ── Page-fault recovery during dump ────────────────────────────────
// Per-CPU recovery slots — checked by PF handler when DUMP_IN_PROGRESS. The
// old single-global slot raced when two CPUs dumped concurrent page-table walks.

pub static PF_RECOVERY_RIP: [AtomicU64; MAX_CPUS] = [
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
];
pub static PF_RECOVERY_HAPPENED: [AtomicBool; MAX_CPUS] = [
    AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false),
    AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false),
    AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false),
    AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false),
];
pub static PF_FAULT_ADDR: [AtomicU64; MAX_CPUS] = [
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
];
pub static PF_ERROR_CODE: [AtomicU64; MAX_CPUS] = [
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
];
// Legacy singletons for any external reader that hasn't migrated — mirrors CPU0 slot.
pub static PF_RECOVERY_RIP_LEGACY: AtomicU64 = AtomicU64::new(0);
pub static PF_RECOVERY_HAPPENED_LEGACY: AtomicBool = AtomicBool::new(false);

// ── SMP global freeze + per-CPU frozen snapshots ───────────────────
// Freeze protocol: first CPU to fault/NMI wins global owner via CAS on
// GLOBAL_ACTIVE, broadcasts NMI to freeze peers, waits ~10 ms (TSC), then
// prints its own verbose dump + per-CPU snapshots captured by peers' NMI
// handlers. Peers that see GLOBAL_ACTIVE && !owner just snapshot + halt.

static GLOBAL_ACTIVE: AtomicBool = AtomicBool::new(false);
static GLOBAL_OWNER: AtomicU64 = AtomicU64::new(0); // cpu+1, 0 = none

static FROZEN_VALID: [AtomicBool; MAX_CPUS] = [
    AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false),
    AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false),
    AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false),
    AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false),
];
static FROZEN_RIP: [AtomicU64; MAX_CPUS] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static FROZEN_RSP: [AtomicU64; MAX_CPUS] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static FROZEN_RFLAGS: [AtomicU64; MAX_CPUS] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static FROZEN_CS: [AtomicU64; MAX_CPUS] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static FROZEN_SS: [AtomicU64; MAX_CPUS] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static FROZEN_CR0: [AtomicU64; MAX_CPUS] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static FROZEN_CR2: [AtomicU64; MAX_CPUS] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static FROZEN_CR3: [AtomicU64; MAX_CPUS] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static FROZEN_CR4: [AtomicU64; MAX_CPUS] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static FROZEN_EFER: [AtomicU64; MAX_CPUS] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static FROZEN_STAR: [AtomicU64; MAX_CPUS] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static FROZEN_LSTAR: [AtomicU64; MAX_CPUS] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static FROZEN_CSTAR: [AtomicU64; MAX_CPUS] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static FROZEN_FMASK: [AtomicU64; MAX_CPUS] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static FROZEN_FS: [AtomicU64; MAX_CPUS] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static FROZEN_GS: [AtomicU64; MAX_CPUS] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static FROZEN_KGS: [AtomicU64; MAX_CPUS] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];

pub fn is_global_dump_active() -> bool { GLOBAL_ACTIVE.load(Ordering::SeqCst) }
pub fn global_dump_owner() -> Option<usize> {
    let v = GLOBAL_OWNER.load(Ordering::SeqCst);
    if v == 0 { None } else { Some((v - 1) as usize) }
}
pub fn is_dump_owner(cpu: usize) -> bool { GLOBAL_OWNER.load(Ordering::SeqCst) == (cpu as u64 + 1) }
fn try_claim_global(cpu: usize) -> bool {
    if GLOBAL_ACTIVE.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
        GLOBAL_OWNER.store(cpu as u64 + 1, Ordering::SeqCst);
        true
    } else { false }
}
#[allow(dead_code)]
fn is_frozen_valid(cpu: usize) -> bool { if cpu < MAX_CPUS { FROZEN_VALID[cpu].load(Ordering::Relaxed) } else { false } }

fn record_frozen_snapshot(cpu: usize, frame: &InterruptStackFrame, cr0: u64, cr2: u64, cr3: u64, cr4: u64) {
    if cpu >= MAX_CPUS { return; }
    if FROZEN_VALID[cpu].load(Ordering::Relaxed) { return; }
    FROZEN_RIP[cpu].store(frame.instruction_pointer.as_u64(), Ordering::Relaxed);
    FROZEN_RSP[cpu].store(frame.stack_pointer.as_u64(), Ordering::Relaxed);
    FROZEN_RFLAGS[cpu].store(frame.cpu_flags.bits(), Ordering::Relaxed);
    FROZEN_CS[cpu].store(frame.code_segment.0 as u64, Ordering::Relaxed);
    FROZEN_SS[cpu].store(frame.stack_segment.0 as u64, Ordering::Relaxed);
    FROZEN_CR0[cpu].store(cr0, Ordering::Relaxed);
    FROZEN_CR2[cpu].store(cr2, Ordering::Relaxed);
    FROZEN_CR3[cpu].store(cr3, Ordering::Relaxed);
    FROZEN_CR4[cpu].store(cr4, Ordering::Relaxed);
    unsafe {
        let efer = read_msr(0xC0000080);
        let star = read_msr(0xC0000081);
        let lstar = read_msr(0xC0000082);
        let cstar = read_msr(0xC0000083);
        let fmask = read_msr(0xC0000084);
        let fs = read_msr(0xC0000100);
        let gs = read_msr(0xC0000101);
        let kgs = read_msr(0xC0000102);
        FROZEN_EFER[cpu].store(efer, Ordering::Relaxed);
        FROZEN_STAR[cpu].store(star, Ordering::Relaxed);
        FROZEN_LSTAR[cpu].store(lstar, Ordering::Relaxed);
        FROZEN_CSTAR[cpu].store(cstar, Ordering::Relaxed);
        FROZEN_FMASK[cpu].store(fmask, Ordering::Relaxed);
        FROZEN_FS[cpu].store(fs, Ordering::Relaxed);
        FROZEN_GS[cpu].store(gs, Ordering::Relaxed);
        FROZEN_KGS[cpu].store(kgs, Ordering::Relaxed);
    }
    FROZEN_VALID[cpu].store(true, Ordering::SeqCst);
}

fn clear_global_dump_state() {
    GLOBAL_ACTIVE.store(false, Ordering::SeqCst);
    GLOBAL_OWNER.store(0, Ordering::SeqCst);
    for i in 0..MAX_CPUS { FROZEN_VALID[i].store(false, Ordering::Relaxed); }
}

#[allow(dead_code)]
pub fn clear_dump_state_for_continue() { clear_global_dump_state(); }

/// Called from NMI handler on follower CPUs when global freeze is active.
/// Snapshots the interrupted frame and halts forever (no return).
/// No serial output here — owner will dump all snapshots sequentially to avoid
/// interleaved UART bytes when multiple followers NMI at once.
pub fn frozen_follower_halt(frame: &InterruptStackFrame) -> ! {
    let cpu = current_cpu_id() as usize;
    if cpu < MAX_CPUS && !FROZEN_VALID[cpu].load(Ordering::Relaxed) {
        let cr0: u64; let cr2: u64; let cr3: u64; let cr4: u64;
        unsafe {
            asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack));
            asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack));
            asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack));
            asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack));
        }
        record_frozen_snapshot(cpu, frame, cr0, cr2, cr3, cr4);
    }
    loop { unsafe { asm!("cli", "hlt", options(nomem, nostack)) }; }
}

fn broadcast_freeze_and_wait() {
    #[cfg(target_arch = "x86_64")]
    {
        // Broadcast NMI to all other CPUs — NMI bypasses IF and IST ensures safe stack.
        crate::platform::x86_64_pc::apic::send_nmi_all_except_self();
        // Wait ~10 ms for peers to snapshot (TSC if calibrated, else spin).
        let start = crate::platform::x86_64_pc::apic::tsc_now_ns();
        if start != 0 && crate::platform::x86_64_pc::apic::tsc_hz() != 0 {
            let deadline = start + 10_000_000;
            while crate::platform::x86_64_pc::apic::tsc_now_ns() < deadline {
                core::hint::spin_loop();
            }
        } else {
            for _ in 0..500_000 { core::hint::spin_loop(); }
        }
    }
}

// ── Helpers for all-CPU verbose dump ─────────────────────────────

fn dump_msrs_snapshot(w: &mut impl Write, cpu: usize) {
    let efer = FROZEN_EFER[cpu].load(Ordering::Relaxed);
    let star = FROZEN_STAR[cpu].load(Ordering::Relaxed);
    let lstar = FROZEN_LSTAR[cpu].load(Ordering::Relaxed);
    let cstar = FROZEN_CSTAR[cpu].load(Ordering::Relaxed);
    let fmask = FROZEN_FMASK[cpu].load(Ordering::Relaxed);
    let fs = FROZEN_FS[cpu].load(Ordering::Relaxed);
    let gs = FROZEN_GS[cpu].load(Ordering::Relaxed);
    let kgs = FROZEN_KGS[cpu].load(Ordering::Relaxed);
    let _ = writeln!(w, "EFER        = {:#018x}", efer);
    let _ = writeln!(w, "  SCE ={}   System Call Extensions", (efer >> 0) & 1);
    let _ = writeln!(w, "  LME ={}   Long Mode Enable", (efer >> 8) & 1);
    let _ = writeln!(w, "  LMA ={}   Long Mode Active", (efer >> 10) & 1);
    let _ = writeln!(w, "  NXE ={}   No-Execute Enable", (efer >> 11) & 1);
    let _ = writeln!(w, "  SVME={}   SVM Enable", (efer >> 12) & 1);
    let _ = writeln!(w, "  LMSLE={}  Long Mode Segment Limit", (efer >> 13) & 1);
    let _ = writeln!(w, "  FFXSR={}  Fast FXSAVE/FXRSTOR", (efer >> 14) & 1);
    let _ = writeln!(w, "  TCE ={}   Translation Cache Extension", (efer >> 15) & 1);
    let _ = writeln!(w, "STAR        = {:#018x}", star);
    let _ = writeln!(w, "LSTAR       = {:#018x}", lstar);
    let _ = writeln!(w, "CSTAR       = {:#018x}", cstar);
    let _ = writeln!(w, "FMASK       = {:#018x}  (IF={})", fmask, if fmask & 0x200 != 0 { "masked" } else { "unmasked" });
    let _ = writeln!(w, "FS_BASE     = {:#018x}", fs);
    let _ = writeln!(w, "GS_BASE     = {:#018x}", gs);
    let _ = writeln!(w, "KERNEL_GS_BASE = {:#018x}", kgs);
}

fn dump_frozen_cpu(w: &mut impl Write, cpu: usize) {
    if !FROZEN_VALID[cpu].load(Ordering::Relaxed) { return; }
    let rip = FROZEN_RIP[cpu].load(Ordering::Relaxed);
    let rsp = FROZEN_RSP[cpu].load(Ordering::Relaxed);
    let rfl = FROZEN_RFLAGS[cpu].load(Ordering::Relaxed);
    let cs = FROZEN_CS[cpu].load(Ordering::Relaxed);
    let ss = FROZEN_SS[cpu].load(Ordering::Relaxed);
    let cr0 = FROZEN_CR0[cpu].load(Ordering::Relaxed);
    let cr2 = FROZEN_CR2[cpu].load(Ordering::Relaxed);
    let cr3 = FROZEN_CR3[cpu].load(Ordering::Relaxed);
    let cr4 = FROZEN_CR4[cpu].load(Ordering::Relaxed);
    let _ = writeln!(w);
    let _ = writeln!(w, "--- CPU {} frozen snapshot (via NMI) ---", cpu);
    let kaslr = crate::mm::layout::kaslr_offset();
    let _ = writeln!(w, "KASLR slide = {:#x}  (image base {:#018x})", kaslr, kernel_image_base());
    match krel(rip) {
        Some(off) => { let _ = writeln!(w, "RIP  = {:#018x}  (kernel+{:#x})  <-- frozen", rip, off); },
        None => { let _ = writeln!(w, "RIP  = {:#018x}  <-- frozen", rip); },
    }
    let _ = writeln!(w, "CS   = {:#018x}", cs);
    let _ = writeln!(w, "RFLAGS = {:#018x}", rfl);
    write_rflags(w, rfl);
    let cpl = cs & 3;
    if cpl == 3 {
        let _ = writeln!(w, "SS   = {:#018x}  (user)", ss);
        let _ = writeln!(w, "RSP  = {:#018x}  (user, frozen)", rsp);
    } else {
        let _ = writeln!(w, "RSP  = {:#018x}  <-- frozen", rsp);
    }
    let _ = writeln!(w);
    let _ = writeln!(w, "CR0 = {:#018x}", cr0);
    write_cr0_flags(w, cr0);
    let _ = writeln!(w, "CR2 = {:#018x}", cr2);
    let cr3_asid = cr3 & 0xFFF;
    let cr3_phys = cr3 & 0x000F_FFFF_FFFF_F000;
    let _ = writeln!(w, "CR3 = {:#018x}", cr3);
    if cr3_asid != 0 { let _ = writeln!(w, "      phys={:#x}  ASID/PCID={:#x}", cr3_phys, cr3_asid); } else { let _ = writeln!(w, "      phys={:#x}", cr3_phys); }
    let _ = writeln!(w, "CR4 = {:#018x}", cr4);
    write_cr4_flags(w, cr4);
    let _ = writeln!(w);
    let _ = writeln!(w, "--- MSRs (CPU {}) ---", cpu);
    dump_msrs_snapshot(w, cpu);
    let _ = writeln!(w);
    let if_flag = (rfl >> 9) & 1;
    let _ = writeln!(w, "Interrupts: {}", if if_flag != 0 { "enabled (IF=1)" } else { "disabled (IF=0)" });
    let _ = writeln!(w);
    dump_fault_stack(w, rsp, cr3);
    let _ = writeln!(w);
    dump_code_bytes(w, rip, cr3);
    let _ = writeln!(w);
}

fn dump_all_frozen_cpus(w: &mut impl Write, owner: usize) {
    let n = crate::smp::cpu_count() as usize;
    let limit = n.min(MAX_CPUS);
    let mut any = false;
    for cpu in 0..limit {
        if cpu == owner { continue; }
        if FROZEN_VALID[cpu].load(Ordering::Relaxed) {
            dump_frozen_cpu(w, cpu);
            any = true;
        }
    }
    if !any {
        let _ = writeln!(w, "--- Other CPUs ---");
        let _ = writeln!(w, "(no frozen snapshots — single CPU or peers not yet frozen)");
        let _ = writeln!(w);
        // Fallback: show SMP state
        let snap = crate::smp::smp_snapshot();
        if snap.len() > 1 {
            let _ = writeln!(w, "SMP snapshot ({} CPUs):", snap.len());
            for (id, apic, is_bsp, state, stack_top, has_task, preempt, ticks) in snap {
                let _ = writeln!(w, " CPU{} apic={} bsp={} state={} stack_top={:#x} has_task={} preempt={} ticks={}", id, apic, is_bsp, state, stack_top, has_task, preempt, ticks);
            }
            let _ = writeln!(w);
        }
    } else {
        // Also show SMP summary after detailed per-CPU dumps
        let snap = crate::smp::smp_snapshot();
        let _ = writeln!(w, "--- SMP summary ---");
        for (id, apic, is_bsp, state, stack_top, has_task, preempt, ticks) in snap {
            let _ = writeln!(w, " CPU{} apic={} bsp={} state={} stack_top={:#x} has_task={} preempt={} ticks={}", id, apic, is_bsp, state, stack_top, has_task, preempt, ticks);
        }
        let _ = writeln!(w);
    }
}

fn dump_smp_overview(w: &mut impl Write) {
    let n = crate::smp::cpu_count() as usize;
    let _ = writeln!(w, "--- SMP overview ---");
    let _ = writeln!(w, "Online CPUs: {}", n);
    let snap = crate::smp::smp_snapshot();
    for (id, apic, is_bsp, state, stack_top, has_task, preempt, ticks) in snap {
        let _ = writeln!(w, " CPU{} apic={} bsp={} state={} stack_top={:#x} has_task={} preempt={} ticks={}", id, apic, is_bsp, state, stack_top, has_task, preempt, ticks);
    }
    let _ = writeln!(w);
}

// ── Exception name ──────────────────────────────────────────────────

fn exception_name(vector: u8) -> &'static str {
    match vector {
        0 => "#DE",
        1 => "#DB",
        2 => "#NMI",
        3 => "#BP",
        4 => "#OF",
        5 => "#BR",
        6 => "#UD",
        7 => "#NM",
        8 => "#DF",
        9 => "#COP",
        10 => "#TS",
        11 => "#NP",
        12 => "#SS",
        13 => "#GP",
        14 => "#PF",
        16 => "#MF",
        17 => "#AC",
        18 => "#MC",
        19 => "#XM",
        20 => "#VE",
        _ => "??",
    }
}

// ── Null writer (pre-scan instruction lengths without output) ───────

struct NullWrite;
impl Write for NullWrite {
    fn write_str(&mut self, _: &str) -> core::fmt::Result {
        Ok(())
    }
}

// ── Lock-free raw serial writer (bypasses spinlock during dump) ────
// Mirrors to panic screen when it is ready — so the same `writeln!` stream
// appears on COM1 and on VRAM without duplicating format logic.

struct DumpWriter;

impl Write for DumpWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        dump_puts(s);
        // Mirror to VRAM if fatal screen was claimed (no-op otherwise, no lock).
        #[cfg(target_arch = "x86_64")]
        {
            // Avoid invoking display code on non-x86 (riscv has no panic screen).
            if crate::kerneldump::screen::is_ready() {
                crate::kerneldump::screen::panic_puts(s);
            }
        }
        Ok(())
    }
}

/// Combined writer that also ensures screen is mirrored (alias).
pub struct PanicWriter;

impl Write for PanicWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        dump_puts(s);
        #[cfg(target_arch = "x86_64")]
        if crate::kerneldump::screen::is_ready() {
            crate::kerneldump::screen::panic_puts(s);
        }
        Ok(())
    }
}

// ── PF-safe volatile memory reader ──────────────────────────────────
// Uses inline asm to set a recovery target before the read.  If a page
// fault fires, the handler (in idt.rs) modifies the interrupt frame's
// RIP to jump to the recovery label, which sets the happened flag and
// continues.  The caller then sees `PF_RECOVERY_HAPPENED` and returns
// `None` instead of faulting again.

unsafe fn pf_read_volatile(ptr: *const u64) -> Option<u64> {
    if ptr.is_null() {
        return None;
    }
    let cpu = current_cpu_id() as usize;
    let idx = if cpu < MAX_CPUS { cpu } else { 0 };
    // Initialize result so the recovery path has a defined value
    // (the asm output is not written on the PF recovery path).
    let mut result: u64 = 0;

    unsafe {
        let rip_slot = &PF_RECOVERY_RIP[idx] as *const AtomicU64 as *const u64 as *mut u64;
        let happ_slot = &PF_RECOVERY_HAPPENED[idx] as *const AtomicBool as *const u8 as *mut u8;

        asm!(
            // Set recovery target → label 20
            "lea rax, [20f + rip]",
            "mov [{rip_slot}], rax",
            "mov byte ptr [{happ_slot}], 0",
            // The actual read (may PF)
            "mov {result}, [{ptr}]",
            // No PF — clear recovery target and skip recovery
            "mov qword ptr [{rip_slot}], 0",
            "jmp 30f",
            // Recovery point: PF handler will set RIP here
            "20:",
            "mov qword ptr [{rip_slot}], 0",
            "mov byte ptr [{happ_slot}], 1",
            // Continue
            "30:",
            result = inlateout(reg) result,
            ptr = in(reg) ptr,
            rip_slot = in(reg) rip_slot,
            happ_slot = in(reg) happ_slot,
            out("rax") _,
            options(nostack),
        );
    }

    if PF_RECOVERY_HAPPENED[idx].load(Ordering::Relaxed) {
        PF_RECOVERY_HAPPENED[idx].store(false, Ordering::Relaxed);
        None
    } else {
        Some(result)
    }
}

// ── Safe memory probes (page-table frames via the private physmap) ──
// After `init_physmap` arms the DIRECT_MAP, low physical RAM is reachable only
// through `PHYS_MAP_BASE` — the identity window covers just the trampoline /
// bootstack / APIC / framebuffer.  So every page-table frame and the final
// data page must be deref'd at `to_physmap(phys)`.  Before the physmap is live
// `to_physmap` returns the identity value, matching the VMM walkers.

/// Virtual deref address for a page-table frame / physical data page.
fn frame_va(phys: u64) -> u64 {
    crate::mm::layout::to_physmap(phys)
}

// ── KASLR-aware kernel-relative addressing ──────────────────────────
//
// The image is slid by `layout::kaslr_offset()` at boot, so raw RIP/stack
// values don't match the unslid ELF on disk. Every address that lands inside
// the running image is annotated as `k+0xOFFSET` — offset from the *linked*
// base (`KERNEL_VMA_BASE`), which is exactly what nm/addr2line/objdump expect.
// The slide itself is printed in the header so a dump is self-describing.

/// Runtime base of the kernel image (linked base minus the KASLR slide).
fn kernel_image_base() -> u64 {
    crate::mm::layout::KERNEL_VMA_BASE.wrapping_sub(crate::mm::layout::kaslr_offset())
}

/// Kernel-relative offset of `addr` (from the linked base) when `addr` lies
/// inside the running image; `None` for anything else (physmap, stacks, user).
fn krel(addr: u64) -> Option<u64> {
    let base = kernel_image_base();
    let off = addr.wrapping_sub(base);
    if off < crate::mm::layout::KERNEL_IMAGE_SIZE {
        Some(off)
    } else {
        None
    }
}

/// `  (k+0x1234a0)` suffix, or empty when `addr` isn't inside the image.
fn ksuffix(addr: u64) -> &'static str {
    // Formatting into a fixed buffer would need a lock-free writer; the
    // annotation alone is enough to recover the slide offline, so keep the
    // stack rows clean and annotate only via the header + RIP line below.
    if krel(addr).is_some() {
        " (k)"
    } else {
        ""
    }
}

fn probe_read_quad(cr3: u64, addr: u64) -> Option<u64> {
    let ext = (addr as i64) >> 47;
    if ext != 0 && ext != -1 {
        return None;
    }

    let pml4_phys = cr3 & 0x000F_FFFF_FFFF_F000;

    unsafe {
        let pml4_entry =
            pf_read_volatile(frame_va(pml4_phys + (addr >> 39 & 0x1FF) * 8) as *const u64)?;
        if pml4_entry & PTE_PRESENT == 0 {
            return None;
        }

        let pdp_phys = pml4_entry & 0x000F_FFFF_FFFF_F000;
        let pdp_entry =
            pf_read_volatile(frame_va(pdp_phys + (addr >> 30 & 0x1FF) * 8) as *const u64)?;
        if pdp_entry & PTE_PRESENT == 0 {
            return None;
        }
        if pdp_entry & (1 << 7) != 0 {
            let page = pdp_entry & 0x000F_FFC0_0000_0000;
            return pf_read_volatile(frame_va(page | (addr & 0x3FFF_FFFF)) as *const u64);
        }

        let pd_phys = pdp_entry & 0x000F_FFFF_FFFF_F000;
        let pd_entry =
            pf_read_volatile(frame_va(pd_phys + (addr >> 21 & 0x1FF) * 8) as *const u64)?;
        if pd_entry & PTE_PRESENT == 0 {
            return None;
        }
        if pd_entry & (1 << 7) != 0 {
            let page = pd_entry & 0x000F_FFFF_FE00_0000;
            return pf_read_volatile(frame_va(page | (addr & 0x1F_FFFF)) as *const u64);
        }

        let pt_phys = pd_entry & 0x000F_FFFF_FFFF_F000;
        let pte = pf_read_volatile(frame_va(pt_phys + (addr >> 12 & 0x1FF) * 8) as *const u64)?;
        if pte & PTE_PRESENT == 0 {
            return None;
        }

        let page = pte & 0x000F_FFFF_FFFF_F000;
        pf_read_volatile(frame_va(page | (addr & 0xFFF)) as *const u64)
    }
}

// ── Stack dump ──────────────────────────────────────────────────────

fn dump_fault_stack(w: &mut impl Write, rsp: u64, cr3: u64) {
    let _ = writeln!(w, "--- Stack Dump (up to 32 quadwords from RSP) ---");

    for row in 0..8 {
        let base = rsp.wrapping_add(row as u64 * 32);
        let mut any_valid = false;
        let _ = write!(w, "  {:#018x}:", base);
        for col in 0..4 {
            let addr = base.wrapping_add(col as u64 * 8);
            match probe_read_quad(cr3, addr) {
                Some(val) => {
                    // `(k)` marks values inside the slid kernel image —
                    // subtract the header's KASLR slide to get the
                    // link-time address for nm/addr2line.
                    let _ = write!(w, "  {:#018x}{}", val, ksuffix(val));
                    any_valid = true;
                }
                None => {
                    let _ = write!(w, "  ________________");
                }
            }
        }
        let _ = writeln!(w);
        if !any_valid {
            break;
        }
    }
}

// ── Code disassembly ────────────────────────────────────────────────

fn dump_code_bytes(w: &mut impl Write, rip: u64, cr3: u64) {
    let _ = writeln!(w, "--- Code (instructions around RIP) ---");

    let start_addr = rip.saturating_sub(32) & !7;
    let mut buf = [0u8; 64];
    let mut valid = 0usize;
    for i in 0..8 {
        let addr = start_addr.wrapping_add(i as u64 * 8);
        if let Some(val) = probe_read_quad(cr3, addr) {
            buf[valid..][..8].copy_from_slice(&val.to_le_bytes());
        } else {
            buf[valid..][..8].fill(0xCC);
        }
        valid += 8;
    }

    if valid == 0 {
        let _ = writeln!(w, "  (no code readable)");
        return;
    }

    // Single-pass: find which instruction contains RIP and record
    // its index and the exact offset.  No 512-byte insn_offsets array.
    let mut offset = 0usize;
    let mut num_insns = 0usize;
    let mut rip_idx = 0usize;
    let mut found_rip = false;

    while offset < valid && num_insns < 64 {
        let addr = start_addr.wrapping_add(offset as u64);
        let len = super::disasm::disasm_one(addr, &buf[offset..], &mut NullWrite).unwrap_or(0);
        let len = if len == 0 { 1 } else { len };

        if !found_rip && rip >= addr && rip < addr.wrapping_add(len as u64) {
            rip_idx = num_insns;
            found_rip = true;
        }

        offset += len;
        num_insns += 1;
    }

    if !found_rip {
        rip_idx = 0;
    }

    let start_idx = rip_idx.saturating_sub(4);
    let end_idx = (rip_idx + 5).min(num_insns);

    // Second pass: re-decode within the window, emitting output
    offset = 0;
    num_insns = 0;
    while offset < valid && num_insns < end_idx {
        let addr = start_addr.wrapping_add(offset as u64);
        let len = super::disasm::disasm_one(addr, &buf[offset..], &mut NullWrite).unwrap_or(0);
        let len = if len == 0 { 1 } else { len };

        if num_insns >= start_idx {
            // Show the link-time (unslid) address too so the line can be
            // pasted straight into objdump --start-address.
            if let Some(off) = krel(addr) {
                let _ = write!(w, "  {:#018x} (k+{:#x}):", addr, off);
            } else {
                let _ = write!(w, "  {:#018x}:", addr);
            }

            for j in 0..len {
                let _ = write!(w, " {:02x}", buf[offset + j]);
            }
            let pad_len = (25usize).saturating_sub(len * 3);
            for _ in 0..pad_len {
                let _ = write!(w, " ");
            }

            super::disasm::disasm_one(addr, &buf[offset..], w);

            if num_insns == rip_idx {
                let _ = write!(w, "  <-- RIP");
            }
            let _ = writeln!(w);
        }

        offset += len;
        num_insns += 1;
    }
}

// ── Error-code decoder ──────────────────────────────────────────────

fn dump_error_code(w: &mut impl Write, vector: u8, code: u64) {
    match vector {
        14 => {
            let p = (code >> 0) & 1;
            let wr = (code >> 1) & 1;
            let us = (code >> 2) & 1;
            let rsv = (code >> 3) & 1;
            let id = (code >> 4) & 1;
            let pk = (code >> 5) & 1;
            let ss = (code >> 6) & 1;
            let _sgx = (code >> 15) & 1;

            let _ = writeln!(w, "--- Page Fault Error Code ({:#x}) ---", code);
            let _ = writeln!(
                w,
                "  P    = {}  {}",
                p,
                if p != 0 {
                    "Protection violation"
                } else {
                    "Not present"
                }
            );
            let _ = writeln!(
                w,
                "  W/R  = {}  {}",
                wr,
                if wr != 0 {
                    "Write access"
                } else {
                    "Read access"
                }
            );
            let _ = writeln!(
                w,
                "  U/S  = {}  {}",
                us,
                if us != 0 {
                    "User mode"
                } else {
                    "Supervisor mode"
                }
            );
            let _ = writeln!(w, "  RSVD = {}", rsv);
            let _ = writeln!(
                w,
                "  I/D  = {}  {}",
                id,
                if id != 0 {
                    "Instruction fetch"
                } else {
                    "Data access"
                }
            );
            let _ = writeln!(w, "  PK   = {}", pk);
            let _ = writeln!(w, "  SS   = {}", ss);
        }
        10 | 11 | 12 | 13 => {
            let _ = writeln!(w, "Error code: {:#x}", code);
            let ext = (code >> 0) & 1;
            let table = (code >> 1) & 3;
            let index = (code >> 3) & 0x1FFF;
            let table_name = ["GDT", "IDT", "LDT", "IDT"][table as usize];
            let _ = writeln!(
                w,
                "  External : {}",
                if ext != 0 {
                    "Yes (event sourced externally)"
                } else {
                    "No"
                }
            );
            let _ = writeln!(
                w,
                "  Table    : {} ({})",
                table_name,
                match table {
                    0 => "GDT",
                    1 => "IDT",
                    2 => "LDT",
                    _ => "IDT",
                }
            );
            let _ = writeln!(w, "  Selector : {:#05x} (index {})", index << 3, index);
        }
        _ => {
            let _ = writeln!(w, "Error code: {:#x}", code);
        }
    }
}

// ── CPUID identification ────────────────────────────────────────────

fn write_cpuid_info(w: &mut impl Write) {
    let mut vendor = [0u8; 12];
    let mut eax_1: u32 = 0;
    let mut ecx_1: u32 = 0;
    let mut edx_1: u32 = 0;
    let mut edx_8: u32 = 0;
    let mut _ecx_7: u32 = 0;
    let mut ebx_7: u32 = 0;

    unsafe {
        asm!("push rbx", "mov eax, 0", "cpuid", "mov [{v}], ebx", "mov [{v}+4], edx", "mov [{v}+8], ecx", "pop rbx",
             v = in(reg) vendor.as_mut_ptr(),
             out("eax") _, out("ecx") _, out("edx") _);

        asm!("push rbx", "mov eax, 1", "cpuid", "mov {0:e}, eax", "mov {1:e}, ecx", "mov {2:e}, edx", "pop rbx",
             out(reg) eax_1, out(reg) ecx_1, out(reg) edx_1);

        asm!("push rbx", "mov eax, 7", "xor ecx, ecx", "cpuid", "mov {0:e}, ebx", "mov {1:e}, ecx", "pop rbx",
             out(reg) ebx_7, out(reg) _ecx_7);

        asm!("push rbx", "mov eax, 0x80000001", "cpuid", "mov {0:e}, edx", "pop rbx",
             out(reg) edx_8,
             out("eax") _, out("ecx") _);
    }

    let stepping = eax_1 & 0xF;
    let model = ((eax_1 >> 4) & 0xF) | ((eax_1 >> 12) & 0xF0);
    let family = ((eax_1 >> 8) & 0xF)
        + if (eax_1 >> 8) & 0xF == 0xF {
            (eax_1 >> 20) & 0xFF
        } else {
            0
        };
    let v = core::str::from_utf8(&vendor).unwrap_or("unknown");

    let _ = writeln!(
        w,
        "CPUID: {}  Family {}  Model {}  Stepping {}",
        v, family, model, stepping
    );

    let _ = write!(w, "Features:");
    macro_rules! feat {
        ($cond:expr, $name:expr) => {
            if $cond {
                let _ = write!(w, " {}", $name);
            }
        };
    }
    feat!((edx_1 >> 25) & 1 != 0, "sse");
    feat!((edx_1 >> 26) & 1 != 0, "sse2");
    feat!((ecx_1 >> 0) & 1 != 0, "sse3");
    feat!((ecx_1 >> 9) & 1 != 0, "ssse3");
    feat!((ecx_1 >> 19) & 1 != 0, "sse4.1");
    feat!((ecx_1 >> 20) & 1 != 0, "sse4.2");
    feat!((ecx_1 >> 28) & 1 != 0, "avx");
    feat!((ecx_1 >> 26) & 1 != 0, "xsave");
    feat!((edx_8 >> 11) & 1 != 0, "syscall");
    feat!((edx_8 >> 20) & 1 != 0, "nx");
    feat!((edx_8 >> 27) & 1 != 0, "rdtscp");
    feat!((ebx_7 >> 7) & 1 != 0, "smep");
    feat!((ebx_7 >> 20) & 1 != 0, "smap");
    feat!((ebx_7 >> 0) & 1 != 0, "fsgsbase");
    let _ = writeln!(w);
}

// ── Important MSRs ──────────────────────────────────────────────────

unsafe fn read_msr(msr: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    // SAFETY: caller guarantees MSR number is valid.
    unsafe {
        asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi);
    }
    ((hi as u64) << 32) | (lo as u64)
}

fn dump_msrs(w: &mut impl Write) {
    let _ = writeln!(w, "--- Important MSRs ---");
    unsafe {
        let efer = read_msr(0xC0000080);
        let star = read_msr(0xC0000081);
        let lstar = read_msr(0xC0000082);
        let cstar = read_msr(0xC0000083);
        let fmask = read_msr(0xC0000084);
        let fs_base = read_msr(0xC0000100);
        let gs_base = read_msr(0xC0000101);
        let kernel_gs_base = read_msr(0xC0000102);

        let _ = writeln!(w, "EFER        = {:#018x}", efer);
        let _ = writeln!(w, "  SCE ={}   System Call Extensions", (efer >> 0) & 1);
        let _ = writeln!(w, "  LME ={}   Long Mode Enable", (efer >> 8) & 1);
        let _ = writeln!(w, "  LMA ={}   Long Mode Active", (efer >> 10) & 1);
        let _ = writeln!(w, "  NXE ={}   No-Execute Enable", (efer >> 11) & 1);
        let _ = writeln!(w, "  SVME={}   SVM Enable", (efer >> 12) & 1);
        let _ = writeln!(w, "  LMSLE={}  Long Mode Segment Limit", (efer >> 13) & 1);
        let _ = writeln!(w, "  FFXSR={}  Fast FXSAVE/FXRSTOR", (efer >> 14) & 1);
        let _ = writeln!(
            w,
            "  TCE ={}   Translation Cache Extension",
            (efer >> 15) & 1
        );
        let _ = writeln!(w, "STAR        = {:#018x}", star);
        let _ = writeln!(w, "LSTAR       = {:#018x}", lstar);
        let _ = writeln!(w, "CSTAR       = {:#018x}", cstar);
        let _ = writeln!(
            w,
            "FMASK       = {:#018x}  (IF={})",
            fmask,
            if fmask & 0x200 != 0 {
                "masked"
            } else {
                "unmasked"
            }
        );
        let _ = writeln!(w, "FS_BASE     = {:#018x}", fs_base);
        let _ = writeln!(w, "GS_BASE     = {:#018x}", gs_base);
        let _ = writeln!(w, "KERNEL_GS_BASE = {:#018x}", kernel_gs_base);
    }
}

// ── CR0 / CR4 flag decoders ───────────────────────────────────────

fn write_cr0_flags(w: &mut impl Write, cr0: u64) {
    let flags = [
        ("PE    ", 0, "Protected mode"),
        ("MP    ", 1, "Monitor co-processor"),
        ("EM    ", 2, "Emulation"),
        ("TS    ", 3, "Task switched"),
        ("NE    ", 5, "Numeric error"),
        ("WP    ", 16, "Write protect"),
        ("AM    ", 18, "Alignment mask"),
        ("NW    ", 29, "Not write-through"),
        ("CD    ", 30, "Cache disable"),
        ("PG    ", 31, "Paging"),
    ];
    for &(name, bit, desc) in &flags {
        let v = (cr0 >> bit) & 1;
        let _ = writeln!(
            w,
            "      {} = {}  {}",
            name,
            v,
            if v != 0 { desc } else { "" }
        );
    }
}

fn write_cr4_flags(w: &mut impl Write, cr4: u64) {
    let flags = [
        ("VME        ", 0, "VM Extensions"),
        ("PVI        ", 1, "Protected-mode VM"),
        ("TSD        ", 2, "Time-stamp disable"),
        ("DE         ", 3, "Debugging extensions"),
        ("PSE        ", 4, "Page size extensions"),
        ("PAE        ", 5, "Physical address extension"),
        ("MCE        ", 6, "Machine check enable"),
        ("PGE        ", 7, "Page global enable"),
        ("PCE        ", 8, "Performance counter enable"),
        ("OSFXSR     ", 9, "FXSAVE/FXRSTOR"),
        ("OSXMMEXCPT ", 10, "SSE unmasked exceptions"),
        ("UMIP       ", 11, "UMIP"),
        ("LA57       ", 12, "57-bit VA"),
        ("VMXE       ", 13, "VMX enable"),
        ("SMXE       ", 14, "SMX enable"),
        ("FSGSBASE   ", 16, "FS/GS base access"),
        ("PCIDE      ", 17, "PCID enable"),
        ("OSXSAVE    ", 18, "XSAVE"),
        ("SMEP       ", 20, "SMEP"),
        ("SMAP       ", 21, "SMAP"),
        ("PKE        ", 22, "Protection key"),
        ("CET        ", 23, "CET"),
        ("PKS        ", 24, "Protection key supervisor"),
    ];
    for &(name, bit, desc) in &flags {
        let v = (cr4 >> bit) & 1;
        let _ = writeln!(
            w,
            "      {} = {}  {}",
            name,
            v,
            if v != 0 { desc } else { "" }
        );
    }
}

// ── RFLAGS decoder ─────────────────────────────────────────────────

fn write_rflags(w: &mut impl Write, rflags: u64) {
    let flags = [
        ("CF", 0, "Carry"),
        ("PF", 2, "Parity"),
        ("AF", 4, "Adjust"),
        ("ZF", 6, "Zero"),
        ("SF", 7, "Sign"),
        ("TF", 8, "Trap (single-step)"),
        ("IF", 9, "Interrupt enable"),
        ("DF", 10, "Direction"),
        ("OF", 11, "Overflow"),
        ("NT", 14, "Nested task"),
        ("RF", 16, "Resume"),
        ("VM", 17, "Virtual-8086 mode"),
        ("AC", 18, "Alignment check"),
        ("VIF", 19, "Virtual interrupt"),
        ("VIP", 20, "Virtual interrupt pending"),
        ("ID", 21, "ID flag"),
    ];
    let iopl = (rflags >> 12) & 3;
    for &(name, bit, desc) in &flags {
        if bit == 12 {
            continue;
        }
        let v = (rflags >> bit) & 1;
        let _ = writeln!(
            w,
            "      {:4} = {}  {}",
            name,
            v,
            if v != 0 { desc } else { "" }
        );
    }
    let _ = writeln!(w, "      IOPL = {}  I/O privilege level {}", iopl, iopl);
}

// ── Page-table walk ────────────────────────────────────────────────

fn write_pte_entry(w: &mut impl Write, label: &str, idx: usize, entry: u64) {
    let _ = write!(w, "  {}[{}] = {:#018x}", label, idx, entry);
    if entry & PTE_PRESENT != 0 {
        let _ = write!(w, " P");
    } else {
        let _ = write!(w, " .");
    }
    if entry & PTE_WRITABLE != 0 {
        let _ = write!(w, " W");
    } else {
        let _ = write!(w, " .");
    }
    if entry & PTE_USER != 0 {
        let _ = write!(w, " U");
    } else {
        let _ = write!(w, " .");
    }
    if entry & (1 << 5) != 0 {
        let _ = write!(w, " A");
    } else {
        let _ = write!(w, " .");
    }
    if entry & (1 << 6) != 0 {
        let _ = write!(w, " D");
    } else {
        let _ = write!(w, " .");
    }
    if entry & (1 << 8) != 0 {
        let _ = write!(w, " G");
    }
    if entry & (1 << 7) != 0 {
        let _ = write!(w, " PS");
    }
    if entry & PTE_NO_EXEC != 0 {
        let _ = write!(w, " NX");
    } else {
        let _ = write!(w, " X");
    }
    let phys = entry & 0x000F_FFFF_FFFF_F000;
    let _ = writeln!(w, "   phys={:#x}", phys);
}

fn dump_page_walk(w: &mut impl Write, cr3: u64, vaddr: u64) {
    let _ = writeln!(w, "--- Page Table Walk for {:#018x} ---", vaddr);

    let pml4_phys = cr3 & 0x000F_FFFF_FFFF_F000;

    let idx4 = ((vaddr >> 39) & 0x1FF) as usize;
    let e4 =
        match unsafe { pf_read_volatile(frame_va(pml4_phys + (idx4 as u64) * 8) as *const u64) } {
            Some(v) => v,
            None => {
                let _ = writeln!(w, "  (PML4 unreadable)");
                return;
            }
        };
    write_pte_entry(w, "PML4", idx4, e4);
    if e4 & PTE_PRESENT == 0 {
        return;
    }

    let pdp_phys = e4 & 0x000F_FFFF_FFFF_F000;
    let idx3 = ((vaddr >> 30) & 0x1FF) as usize;
    let e3 = match unsafe { pf_read_volatile(frame_va(pdp_phys + (idx3 as u64) * 8) as *const u64) }
    {
        Some(v) => v,
        None => {
            let _ = writeln!(w, "  (PDP unreadable)");
            return;
        }
    };
    write_pte_entry(w, "PDP", idx3, e3);
    if e3 & PTE_PRESENT == 0 {
        return;
    }
    if e3 & (1 << 7) != 0 {
        let phys = (e3 & 0x000F_FFC0_0000_0000) | (vaddr & 0x3FFF_FFFF);
        let _ = writeln!(w, "  -> 1 GiB huge page  phys={:#x}", phys);
        return;
    }

    let pd_phys = e3 & 0x000F_FFFF_FFFF_F000;
    let idx2 = ((vaddr >> 21) & 0x1FF) as usize;
    let e2 = match unsafe { pf_read_volatile(frame_va(pd_phys + (idx2 as u64) * 8) as *const u64) }
    {
        Some(v) => v,
        None => {
            let _ = writeln!(w, "  (PD unreadable)");
            return;
        }
    };
    write_pte_entry(w, " PD", idx2, e2);
    if e2 & PTE_PRESENT == 0 {
        return;
    }
    if e2 & (1 << 7) != 0 {
        let phys = (e2 & 0x000F_FFFF_FE00_0000) | (vaddr & 0x1F_FFFF);
        let _ = writeln!(w, "  -> 2 MiB huge page  phys={:#x}", phys);
        return;
    }

    let pt_phys = e2 & 0x000F_FFFF_FFFF_F000;
    let idx1 = ((vaddr >> 12) & 0x1FF) as usize;
    let e1 = match unsafe { pf_read_volatile(frame_va(pt_phys + (idx1 as u64) * 8) as *const u64) }
    {
        Some(v) => v,
        None => {
            let _ = writeln!(w, "  (PT unreadable)");
            return;
        }
    };
    write_pte_entry(w, " PT", idx1, e1);
    if e1 & PTE_PRESENT == 0 {
        let _ = writeln!(w, "  -> unmapped");
        return;
    }

    let phys = (e1 & 0x000F_FFFF_FFFF_F000) | (vaddr & 0xFFF);
    let _ = writeln!(w, "  -> phys={:#x}", phys);
}

// ── NMI live dump (hung RIP, not handler RIP) ──────────────────────

/// Live NMI dump — prints the *interrupted* frame's hung RIP/RSP, not the
/// handler's.  Called from `watchdog::nmi_handler` with the CPU-pushed
/// `InterruptStackFrame`.  Mirrors `dump_full_fault` sections but **returns**
/// unless `watchdog_cont` is off, in which case it halts (default).  The
/// `context` string (e.g. "watchdog timeout" or "PS/2 F9 hotkey") is printed
/// before the standard header so the cause is obvious.
pub fn dump_nmi_full_fault(frame: &InterruptStackFrame, vector: u8, context: &str) {
    let cpu = current_cpu_id() as usize;
    // Follower NMI during global freeze — snapshot + halt without verbose dump.
    if is_global_dump_active() && !is_dump_owner(cpu) {
        frozen_follower_halt(frame);
    }
    // Per-CPU guard — NMI can nest (NMI inside NMI) until iret, so use SeqCst
    // swap. Nested NMI while already dumping on this CPU just halts.
    if cpu >= MAX_CPUS || DUMP_IN_PROGRESS[cpu].swap(true, Ordering::SeqCst) {
        dump_puts("\n[DUMP] Nested NMI (#");
        dump_put_hex(vector as u64);
        dump_puts(") while dumping -- halting\n");
        #[cfg(target_arch = "x86_64")]
        if crate::kerneldump::screen::is_ready() {
            crate::kerneldump::screen::panic_puts("\n[DUMP] Nested NMI while dumping -- halting\n");
        }
        loop {
            unsafe { asm!("cli", "hlt", options(nomem, nostack)) };
        }
    }
    // Claim global freeze if we are first NMI dumper, then freeze peers.
    let is_owner = try_claim_global(cpu);
    if is_owner {
        broadcast_freeze_and_wait();
    }

    #[cfg(target_arch = "x86_64")]
    {
        if !crate::kerneldump::screen::is_ready() {
            let _ = crate::kerneldump::screen::panic_screen_init();
        }
    }

    let fault_rip = frame.instruction_pointer.as_u64();
    let fault_rsp = frame.stack_pointer.as_u64();
    let fault_cs = frame.code_segment.0 as u64;
    let fault_rfl = frame.cpu_flags.bits();
    let fault_ss = frame.stack_segment.0 as u64;

    let cr0: u64;
    let cr2: u64;
    let cr3: u64;
    let cr4: u64;
    unsafe {
        asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack));
        asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack));
        asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack));
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack));
    }

    let mut w = DumpWriter;

    let _ = writeln!(w);
    let _ = writeln!(w, "[NMI] CPU{} context: {}", cpu, context);
    let _ = writeln!(w, "*** NMI ON CPU {} (APIC {}) ***", cpu, crate::smp::per_cpu_by_id(cpu as u32).apic_id);
    match vector {
        2 => {
            let _ = writeln!(w, "==== NON-MASKABLE INTERRUPT (#NMI 2) CPU{} {:=>25}", cpu, "");
        }
        _ => {
            let _ = writeln!(
                w,
                "==== {} (#{} ) CPU{} {} {:=>45}",
                exception_name(vector),
                vector,
                cpu,
                context,
                ""
            );
        }
    }

    // ── Kernel stage (what it was doing) ─────────────────────
    let _ = writeln!(w, "--- Kernel Stage ---");
    let _ = writeln!(w, "Stage: {} ({})", crate::stage::as_str(), crate::stage::bootanim_str());
    let _ = writeln!(w);

    if vector == 18 {
        #[cfg(target_arch = "x86_64")]
        {
            crate::arch::x86_64::mca::dump_mca(&mut w);
            let _ = writeln!(w);
        }
    }

    write_cpuid_info(&mut w);
    let _ = writeln!(w);

    let _ = writeln!(w, "--- Interrupt Frame (hung, not handler) ---");
    let kaslr = crate::mm::layout::kaslr_offset();
    let _ = writeln!(
        w,
        "KASLR slide = {:#x}  (image base {:#018x}; symbolize as addr - slide)",
        kaslr,
        kernel_image_base()
    );
    match krel(fault_rip) {
        Some(off) => {
            let _ = writeln!(w, "RIP  = {:#018x}  (kernel+{:#x})  <-- hung", fault_rip, off);
        }
        None => {
            let _ = writeln!(w, "RIP  = {:#018x}  <-- hung", fault_rip);
        }
    }
    let _ = writeln!(w, "CS   = {:#018x}", fault_cs);
    let _ = writeln!(w, "RFLAGS = {:#018x}", fault_rfl);
    write_rflags(&mut w, fault_rfl);
    let cpl = fault_cs & 3;
    if cpl == 3 {
        let _ = writeln!(w, "SS   = {:#018x}  (saved by CPU on CPL change)", fault_ss);
        let _ = writeln!(w, "RSP  = {:#018x}  (original user RSP, hung)", fault_rsp);
    } else {
        let _ = writeln!(w, "RSP  = {:#018x}  <-- hung", fault_rsp);
    }
    let _ = writeln!(w);

    let _ = writeln!(w, "--- Control Registers ---");
    let _ = writeln!(w, "CR0 = {:#018x}", cr0);
    write_cr0_flags(&mut w, cr0);
    let _ = writeln!(w, "CR2 = {:#018x}", cr2);
    let cr3_asid = cr3 & 0xFFF;
    let cr3_phys = cr3 & 0x000F_FFFF_FFFF_F000;
    let _ = writeln!(w, "CR3 = {:#018x}", cr3);
    if cr3_asid != 0 {
        let _ = writeln!(w, "      phys={:#x}  ASID/PCID={:#x}", cr3_phys, cr3_asid);
    } else {
        let _ = writeln!(w, "      phys={:#x}", cr3_phys);
    }
    let _ = writeln!(w, "CR4 = {:#018x}", cr4);
    write_cr4_flags(&mut w, cr4);
    let _ = writeln!(w);

    dump_msrs(&mut w);
    let _ = writeln!(w);

    let if_flag = (fault_rfl >> 9) & 1;
    let _ = writeln!(
        w,
        "Interrupts: {}",
        if if_flag != 0 {
            "enabled (IF=1)"
        } else {
            "disabled (IF=0)"
        }
    );
    let _ = writeln!(w, "NMI reason: {}", context);
    let _ = writeln!(w);

    dump_fault_stack(&mut w, fault_rsp, cr3);
    let _ = writeln!(w);

    dump_code_bytes(&mut w, fault_rip, cr3);
    let _ = writeln!(w);

    // ── SMP: other CPUs frozen via NMI ───────────────────────────────
    let _ = writeln!(w, "--- SMP: other CPUs (frozen via NMI) ---");
    dump_all_frozen_cpus(&mut w, cpu);
    dump_smp_overview(&mut w);

    let _ = writeln!(w, "================================================");

    // Default: halt (watchdog_cont continues). F9 caller already handles its
    // own policy (always continue), but this path is shared — watchdog_cont
    // controls halt. The nmi_handler also does its own halt post-dump.
    #[cfg(not(feature = "watchdog_cont"))]
    {
        // If this was watchdog timeout we halt; hotkey path would have
        // returned before reaching here? Actually hotkey also lands here —
        // we must not halt hotkey when watchdog_cont is off. Check context.
        if context.contains("watchdog") {
            // Halting path keeps GLOBAL_ACTIVE true so followers stay frozen
            DUMP_IN_PROGRESS[cpu].store(false, Ordering::SeqCst);
            loop {
                unsafe { asm!("cli", "hlt", options(nomem, nostack)) };
            }
        } else {
            // Hotkey: always continue — clear global so next hotkey can freeze again
            DUMP_IN_PROGRESS[cpu].store(false, Ordering::SeqCst);
            clear_global_dump_state();
        }
    }
    #[cfg(feature = "watchdog_cont")]
    {
        DUMP_IN_PROGRESS[cpu].store(false, Ordering::SeqCst);
        clear_global_dump_state();
    }
}

// ── Non-exception fatal dump ───────────────────────────────────────

/// Fatal dump for a condition detected outside any exception frame (e.g. a
/// TLB-shootdown timeout in normal kernel context).  Synthesizes an interrupt
/// stack frame from the live register state so every standard dump section
/// (registers, CRs, MSRs, stack, code bytes) still applies.
///
/// Vector `0xFE` is reserved for these synthetic dumps (`exception_name`
/// renders it as "??"); the caller's context string is printed before the
/// standard header.
pub fn dump_fatal(context: &str) -> ! {
    crate::drivers::serial::SerialPort::puts("\n[DUMP] fatal: ");
    crate::drivers::serial::SerialPort::puts(context);
    crate::drivers::serial::SerialPort::puts("\n");

    let rip: u64;
    let rsp: u64;
    let rflags: u64;
    let cs: u16;
    let ss: u16;
    // pushfq/popfq are balanced, so `nostack` stays honest; `nomem` holds
    // because nothing here dereferences memory.
    unsafe {
        core::arch::asm!(
            "lea {0}, [rip]",
            "mov {1}, rsp",
            "pushfq",
            "pop {2}",
            "mov {3:x}, cs",
            "mov {4:x}, ss",
            out(reg) rip,
            lateout(reg) rsp,
            lateout(reg) rflags,
            lateout(reg) cs,
            lateout(reg) ss,
            options(nomem, nostack)
        );
    }
    let frame = InterruptStackFrame::new(
        x86_64::VirtAddr::new(rip),
        x86_64::structures::gdt::SegmentSelector(cs),
        x86_64::registers::rflags::RFlags::from_bits_truncate(rflags),
        x86_64::VirtAddr::new(rsp),
        x86_64::structures::gdt::SegmentSelector(ss),
    );
    dump_full_fault(&frame, 0, 0xFE)
}

// ── Main fault-dump orchestrator ───────────────────────────────────

pub fn dump_full_fault(frame: &InterruptStackFrame, error_code: u64, vector: u8) -> ! {
    let cpu = current_cpu_id() as usize;

    // ── Follower check: another CPU already owns global dump ─────
    if is_global_dump_active() {
        if let Some(owner) = global_dump_owner() {
            if owner != cpu {
                // Capture our faulting frame as frozen snapshot then halt without verbose interleave.
                let cr0: u64; let cr2: u64; let cr3: u64; let cr4: u64;
                unsafe {
                    asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack));
                    asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack));
                    asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack));
                    asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack));
                }
                if cpu < MAX_CPUS && !FROZEN_VALID[cpu].load(Ordering::Relaxed) {
                    record_frozen_snapshot(cpu, frame, cr0, cr2, cr3, cr4);
                }
                dump_puts("\n[DUMP] CPU ");
                dump_put_hex(cpu as u64);
                dump_puts(" concurrent fault #");
                dump_put_hex(vector as u64);
                dump_puts(" while CPU ");
                dump_put_hex(owner as u64);
                dump_puts(" dumping -- frozen & halting\n");
                loop { unsafe { asm!("cli", "hlt", options(nomem, nostack)) }; }
            }
        }
    }

    // ── Per-CPU re-entrancy guard ────────────────────────────────
    if cpu >= MAX_CPUS || DUMP_IN_PROGRESS[cpu].swap(true, Ordering::SeqCst) {
        dump_puts("\n[DUMP] Nested fault (#");
        dump_put_hex(vector as u64);
        dump_puts(") while dumping -- halting\n");
        #[cfg(target_arch = "x86_64")]
        if crate::kerneldump::screen::is_ready() {
            crate::kerneldump::screen::panic_puts("\n[DUMP] Nested fault while dumping -- halting\n");
        }
        loop {
            unsafe {
                asm!("cli", "hlt", options(nomem, nostack));
            }
        }
    }

    // ── Claim global freeze (first fault wins, others become followers) ─
    let is_owner = try_claim_global(cpu);
    if is_owner {
        broadcast_freeze_and_wait();
    } else {
        // Lost race after per-CPU guard — another CPU just claimed global; become follower.
        let owner = global_dump_owner().unwrap_or(0);
        let cr0: u64; let cr2: u64; let cr3: u64; let cr4: u64;
        unsafe {
            asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack));
            asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack));
            asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack));
            asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack));
        }
        if cpu < MAX_CPUS && !FROZEN_VALID[cpu].load(Ordering::Relaxed) {
            record_frozen_snapshot(cpu, frame, cr0, cr2, cr3, cr4);
        }
        dump_puts("\n[DUMP] CPU ");
        dump_put_hex(cpu as u64);
        dump_puts(" lost global race to CPU ");
        dump_put_hex(owner as u64);
        dump_puts(" -- frozen & halting\n");
        loop { unsafe { asm!("cli", "hlt", options(nomem, nostack)) }; }
    }

    // ── Claim panic screen ASAP (first fault wins VRAM) ──────────
    #[cfg(target_arch = "x86_64")]
    {
        // Try to claim VRAM on any fatal dump (not just #MC) — provides
        // "blue-screen" for all aborts while remaining no-op if no fb.
        // `panic_screen_init` is idempotent w.r.t. claim; it clears screen
        // to red only once (first caller). Subsequent calls see `is_ready()`.
        if !crate::kerneldump::screen::is_ready() {
            let _ = crate::kerneldump::screen::panic_screen_init();
        }
        // Even if we just claimed, ensure title line is printed immediately
        // via serial+screen before heavy decode. Mirrored via DumpWriter now.
        if crate::kerneldump::screen::is_ready() {
            // This raw put ensures banner is visible before any MSR reads that
            // could fault again. Use direct panic_puts for immediate VRAM flush.
            // The DumpWriter mirroring will duplicate header again below; that's fine.
        }
    }

    // ── Extract frame values ──────────────────────────────────────
    let fault_rip = frame.instruction_pointer.as_u64();
    let fault_rsp = frame.stack_pointer.as_u64();
    let fault_cs = frame.code_segment.0 as u64;
    let fault_rfl = frame.cpu_flags.bits();
    let fault_ss = frame.stack_segment.0 as u64;

    // ── Read control registers ────────────────────────────────────
    let cr0: u64;
    let cr2: u64;
    let cr3: u64;
    let cr4: u64;
    unsafe {
        asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack));
        asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack));
        asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack));
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack));
    }

    let mut w = DumpWriter;

    // ── Header ─────────────────────────────────────────────────────
    let _ = writeln!(w);
    let _ = writeln!(w, "*** FAULT ON CPU {} (APIC {}) ***", cpu, crate::smp::per_cpu_by_id(cpu as u32).apic_id);
    match vector {
        14 => {
            let _ = writeln!(w, "==== PAGE FAULT (#14) CPU{} {:=>34}", cpu, "");
        }
        13 => {
            let _ = writeln!(w, "==== GENERAL PROTECTION (#13) CPU{} {:=>23}", cpu, "");
        }
        6 => {
            let _ = writeln!(w, "==== INVALID OPCODE (#6) CPU{} {:=>30}", cpu, "");
        }
        0 => {
            let _ = writeln!(w, "==== DIVIDE ERROR (#0) CPU{} {:=>31}", cpu, "");
        }
        8 => {
            let _ = writeln!(w, "==== DOUBLE FAULT (#8) CPU{} {:=>31}", cpu, "");
        }
        18 => {
            let _ = writeln!(w, "==== MACHINE CHECK (#MC 18) CPU{} {:=>27}", cpu, "");
        }
        _ => {
            let _ = writeln!(
                w,
                "==== {} (#{}) CPU{} {:=>45}",
                exception_name(vector),
                vector,
                cpu,
                ""
            );
        }
    }

    // ── Kernel stage (what it was doing) ─────────────────────
    let _ = writeln!(w, "--- Kernel Stage ---");
    let _ = writeln!(w, "Stage: {} ({})", crate::stage::as_str(), crate::stage::bootanim_str());
    let _ = writeln!(w);

    // ── Machine Check MCA decode (vector 18) ──────────────────────
    if vector == 18 {
        #[cfg(target_arch = "x86_64")]
        {
            crate::arch::x86_64::mca::dump_mca(&mut w);
            let _ = writeln!(w);
        }
    }

    // ── Error code and fault address ──────────────────────────────
    if vector == 14 || (error_code != 0 && matches!(vector, 10..=14)) {
        dump_error_code(&mut w, vector, error_code);
        let _ = writeln!(w);
        if vector == 14 {
            let _ = writeln!(w, "CR2 (fault address): {:#018x}", cr2);
            let _ = writeln!(w);
        }
    }

    // ── CPU info ─────────────────────────────────────────────────
    write_cpuid_info(&mut w);
    let _ = writeln!(w);

    // ── Interrupt frame ───────────────────────────────────────────
    let _ = writeln!(w, "--- Interrupt Frame ---");
    let kaslr = crate::mm::layout::kaslr_offset();
    let _ = writeln!(
        w,
        "KASLR slide = {:#x}  (image base {:#018x}; symbolize as addr - slide)",
        kaslr,
        kernel_image_base()
    );
    match krel(fault_rip) {
        Some(off) => {
            let _ = writeln!(
                w,
                "RIP  = {:#018x}  (kernel+{:#x})",
                fault_rip, off
            );
        }
        None => {
            let _ = writeln!(w, "RIP  = {:#018x}", fault_rip);
        }
    }
    let _ = writeln!(w, "CS   = {:#018x}", fault_cs);
    let _ = writeln!(w, "RFLAGS = {:#018x}", fault_rfl);
    write_rflags(&mut w, fault_rfl);
    let cpl = fault_cs & 3;
    if cpl == 3 {
        let _ = writeln!(w, "SS   = {:#018x}  (saved by CPU on CPL change)", fault_ss);
        let _ = writeln!(w, "RSP  = {:#018x}  (original user RSP)", fault_rsp);
    } else {
        let _ = writeln!(w, "RSP  = {:#018x}", fault_rsp);
    }
    let _ = writeln!(w);

    // ── Control registers ─────────────────────────────────────────
    let _ = writeln!(w, "--- Control Registers ---");
    let _ = writeln!(w, "CR0 = {:#018x}", cr0);
    write_cr0_flags(&mut w, cr0);
    let _ = writeln!(w, "CR2 = {:#018x}", cr2);
    let cr3_asid = cr3 & 0xFFF;
    let cr3_phys = cr3 & 0x000F_FFFF_FFFF_F000;
    let _ = writeln!(w, "CR3 = {:#018x}", cr3);
    if cr3_asid != 0 {
        let _ = writeln!(w, "      phys={:#x}  ASID/PCID={:#x}", cr3_phys, cr3_asid);
    } else {
        let _ = writeln!(w, "      phys={:#x}", cr3_phys);
    }
    let _ = writeln!(w, "CR4 = {:#018x}", cr4);
    write_cr4_flags(&mut w, cr4);
    let _ = writeln!(w);

    // ── MSRs ──────────────────────────────────────────────────────
    dump_msrs(&mut w);
    let _ = writeln!(w);

    // ── RFLAGS summary ────────────────────────────────────────────
    let if_flag = (fault_rfl >> 9) & 1;
    let _ = writeln!(
        w,
        "Interrupts: {}",
        if if_flag != 0 {
            "enabled (IF=1)"
        } else {
            "disabled (IF=0)"
        }
    );
    let _ = writeln!(w);

    // ── Page-table walk (page faults only) ────────────────────────
    if vector == 14 {
        dump_page_walk(&mut w, cr3, cr2);
        let _ = writeln!(w);
    }

    // ── Stack dump ────────────────────────────────────────────────
    dump_fault_stack(&mut w, fault_rsp, cr3);
    let _ = writeln!(w);

    // ── Code disassembly ──────────────────────────────────────────
    dump_code_bytes(&mut w, fault_rip, cr3);
    let _ = writeln!(w);

    // ── SMP: other CPUs frozen via NMI ───────────────────────────────
    let _ = writeln!(w, "--- SMP: other CPUs (frozen via NMI) ---");
    dump_all_frozen_cpus(&mut w, cpu);
    dump_smp_overview(&mut w);

    // ── Footer ────────────────────────────────────────────────────
    let _ = writeln!(w, "================================================");

    loop {
        unsafe {
            asm!("cli", "hlt", options(nomem, nostack));
        }
    }
}
