//! Per-CPU CPU-feature enablement (CR4 bits gated on runtime CPUID).
//!
//! SMEP and SMAP are enabled here rather than in the boot stubs so the UEFI
//! and Multiboot2 paths share one runtime-gated implementation, and APs pick
//! them up in [`crate::arch::x86_64::trampoline::ap_entry64`] — the
//! trampoline's hardcoded CR4 value stays untouched.
//!
//! SMAP is now safe to enable: every kernel dereference of user memory is
//! fenced by the `UserAccess` guard opened for the whole of
//! `syscall_dispatch` (`arch/x86_64/syscall.rs`). Validation walks read page
//! tables through the physmap (supervisor PTEs) and are unaffected.

use core::arch::asm;
use spin::Once;

/// CPUID leaf 7 EBX feature bits.
const CPUID_EBX_SMEP: u32 = 1 << 7;
const CPUID_EBX_SMAP: u32 = 1 << 20;
/// CR4.SMEP — supervisor-mode execution prevention.
const CR4_SMEP: u64 = 1 << 20;
/// CR4.SMAP — supervisor-mode access prevention.
const CR4_SMAP: u64 = 1 << 21;

fn cpuid_leaf7_ebx() -> Option<u32> {
    let max = core::arch::x86_64::__cpuid(0).eax;
    if max < 7 {
        return None;
    }
    Some(core::arch::x86_64::__cpuid_count(7, 0).ebx)
}

/// True when this CPU supports SMEP (CPUID.(EAX=7,ECX=0):EBX[7]).
fn has_smep() -> bool {
    cpuid_leaf7_ebx().is_some_and(|ebx| ebx & CPUID_EBX_SMEP != 0)
}/// True when this CPU supports SMAP (CPUID.(EAX=7,ECX=0):EBX[20]).
fn has_smap() -> bool {
    cpuid_leaf7_ebx().is_some_and(|ebx| ebx & CPUID_EBX_SMAP != 0)
}

/// Read CR4 of the calling CPU.
#[inline]
fn read_cr4() -> u64 {
    let mut cr4: u64;
    unsafe { asm!("mov {0}, cr4", out(reg) cr4, options(nomem, nostack)) };
    cr4
}

/// Set `bit` in CR4 if absent. No-op when already set.
#[inline]
unsafe fn set_cr4_bit(bit: u64) {
    let mut cr4 = read_cr4();
    if cr4 & bit != 0 {
        return;
    }
    cr4 |= bit;
    unsafe { asm!("mov cr4, {0}", in(reg) cr4, options(nomem, nostack)) };
}

/// Enable SMEP on the calling CPU when supported; no-op otherwise.
///
/// Idempotent — safe to call from both BSP init and every AP entry.
pub fn enable_smep() {
    if !has_smep() {
        return;
    }
    unsafe { set_cr4_bit(CR4_SMEP) };
}

/// Set once on the BSP: true iff this boot runs with CR4.SMAP. `STAC`/`CLAC`
/// raise #UD on CPUs lacking SMAP support, so every fence site must consult
/// this before emitting them.
static SMAP_ACTIVE: Once<bool> = Once::new();

/// True when STAC/CLAC may be executed on this boot.
pub fn smap_active() -> bool {
    SMAP_ACTIVE.get().copied().unwrap_or(false)
}

/// Enable SMAP on the calling CPU when supported; no-op otherwise.
///
/// Every user-pointer deref must be inside an active `UserAccess` guard
/// before this bit can be set — which holds unconditionally today because
/// all such derefs live under `syscall_dispatch`.
///
/// Idempotent — safe to call from both BSP init and every AP entry.
pub fn enable_smap() {
    if !has_smap() {
        return;
    }
    unsafe { set_cr4_bit(CR4_SMAP) };
    SMAP_ACTIVE.call_once(|| true);
}

// ── Protection Keys (PKU) ────────────────────────────────────────────
//
// PTE key tag bits 59..62 select one of 16 keys; the per-CPU PKRU register
// holds two rights bits per key (WD = write-disable, AD = access-disable).
// BedrockOS convention: PKRU is *task* state (`Task.pkru`, default 0 =
// everything accessible), loaded on context switch and forced to 0 for the
// duration of a syscall so the kernel can never lock itself out of a user
// buffer it must copy (keys therefore bind only against pure user execution
// — a deliberate, documented v1 semantic).

const CPUID_ECX_PKU: u32 = 1 << 3;
const CPUID_ECX_OSPKE: u32 = 1 << 4;
/// CR4.PKE — protection-key enable.
const CR4_PKE: u64 = 1 << 22;

/// Set once by [`enable_pke`] on the BSP; true iff this CPU runs with PKE on.
static PKU_ACTIVE: Once<bool> = Once::new();

fn cpuid_leaf7_ecx() -> Option<u32> {
    let max = core::arch::x86_64::__cpuid(0).eax;
    if max < 7 {
        return None;
    }
    Some(core::arch::x86_64::__cpuid_count(7, 0).ecx)
}

/// True when the CPU supports protection keys *and* RDPKRU/WRPKRU (OSPKE).
fn has_pku() -> bool {
    match cpuid_leaf7_ecx() {
        Some(ecx) => ecx & CPUID_ECX_PKU != 0 && ecx & CPUID_ECX_OSPKE != 0,
        None => false,
    }
}

/// Enable CR4.PKE on the calling CPU when supported. Idempotent; call from
/// BSP paging setup and every AP entry before anything applies a PKRU.
pub fn enable_pke() {
    if !has_pku() {
        return;
    }
    unsafe { set_cr4_bit(CR4_PKE) };
    PKU_ACTIVE.call_once(|| true);
}

/// True when WRPKRU/RDPKRU may be executed on this boot.
pub fn pku_active() -> bool {
    PKU_ACTIVE.get().copied().unwrap_or(false)
}

/// Write PKRU (EAX = value, ECX = EDX = 0 required by the instruction).
#[inline]
unsafe fn wrpkru(v: u32) {
    unsafe {
        asm!(
            "wrpkru",
            in("eax") v,
            in("ecx") 0,
            in("edx") 0,
            options(nomem, nostack)
        )
    };
}

/// Apply `pkru` when PKU is active; no-op otherwise (and cheap enough to call
/// unconditionally from the scheduler).
pub fn pku_apply(pkru: u32) {
    if pku_active() {
        unsafe { wrpkru(pkru) };
    }
}

/// Force all-keys-accessible for kernel work (syscall entry).
pub fn pku_enter() {
    if pku_active() {
        unsafe { wrpkru(0) };
    }
}
