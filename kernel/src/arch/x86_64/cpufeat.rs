//! Per-CPU CPU-feature enablement (CR4 bits gated on runtime CPUID).
//!
//! SMEP is enabled here rather than in the boot stubs so the UEFI and
//! Multiboot2 paths share one runtime-gated implementation, and APs pick it
//! up in [`crate::arch::x86_64::trampoline::ap_entry64`] — the trampoline's
//! hardcoded CR4 value stays untouched.
//!
//! SMAP is deliberately NOT enabled: every syscall copy loop derefs user
//! buffers from ring 0 without `stac`/`clac` fencing yet.  Enabling SMAP
//! before those sites are wrapped would fault on the first legitimate
//! user-pointer access.

use core::arch::asm;

/// CPUID leaf 7 EBX feature bits.
const CPUID_EBX_SMEP: u32 = 1 << 7;
/// CR4.SMEP — supervisor-mode execution prevention.
const CR4_SMEP: u64 = 1 << 20;

/// True when this CPU supports SMEP (CPUID.(EAX=7,ECX=0):EBX[7]).
fn has_smep() -> bool {
    let max = core::arch::x86_64::__cpuid(0).eax;
    if max < 7 {
        return false;
    }
    core::arch::x86_64::__cpuid_count(7, 0).ebx & CPUID_EBX_SMEP != 0
}

/// Enable SMEP on the calling CPU when supported; no-op otherwise.
///
/// Idempotent — safe to call from both BSP init and every AP entry.
pub fn enable_smep() {
    if !has_smep() {
        return;
    }
    let mut cr4: u64;
    unsafe { asm!("mov {0}, cr4", out(reg) cr4, options(nomem, nostack)) };
    if cr4 & CR4_SMEP != 0 {
        return;
    }
    cr4 |= CR4_SMEP;
    unsafe { asm!("mov cr4, {0}", in(reg) cr4, options(nomem, nostack)) };
}
