//! Formal kernel command line / bootargs.
//!
//! Single source of truth for Multiboot2 tag 1 (`boot command line`) and the
//! UEFI fallback. No heap before `init` – the raw string from the low
//! Multiboot2 info buffer (identity-mapped only before `switch_to_higher_half`)
//! is copied into a kernel-resident stash (`BOOTARGS_BUF`, like `RSDP_BUF`).
//! After that every consumer uses `get()` / `contains_word()`.
//!
//! Parsing is whitespace-split exact-word, no alloc, no case folding.
//! `nokaslr` is checked via `is_nokaslr()` (also accepts `-nokaslr` for
//! GRUB compatibility).

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const BUF_SIZE: usize = 512;

static mut BOOTARGS_BUF: [u8; BUF_SIZE] = [0u8; BUF_SIZE];
static BOOTARGS_LEN: AtomicUsize = AtomicUsize::new(0);
static BOOTARGS_INIT: AtomicBool = AtomicBool::new(false);

/// Copy `raw` (already NUL-trimmed, may be non-UTF8) into the stash and
/// mark initialized. Called once from `rust_entry_mb2` while `info` is still
/// identity-mapped, and from `kernel_main` UEFI path as empty.
unsafe fn stash(raw: &[u8]) -> &'static str {
    let take = core::cmp::min(raw.len(), BUF_SIZE);
    unsafe {
        core::ptr::copy_nonoverlapping(
            raw.as_ptr(),
            core::ptr::addr_of_mut!(BOOTARGS_BUF) as *mut u8,
            take,
        );
        BOOTARGS_LEN.store(take, Ordering::Release);
        BOOTARGS_INIT.store(true, Ordering::Release);
        let s = core::slice::from_raw_parts(core::ptr::addr_of!(BOOTARGS_BUF) as *const u8, take);
        // Non-UTF8 bytes are lossy-replaced by trimming to valid prefix – keep simple: from_utf8 lossy fallback to empty on error.
        core::str::from_utf8(s).unwrap_or("")
    }
}

/// Initialize from a Multiboot2 cmdline slice (already trimmed at NUL).
/// Single-threaded BSP, pre-SMP. Safe to call multiple times – second is no-op.
pub unsafe fn init_from_slice(raw: &[u8]) {
    if BOOTARGS_INIT.load(Ordering::Acquire) {
        return;
    }
    unsafe { stash(raw); }
}

/// Initialize as empty (UEFI / RISC-V fallback). No cmdline delivered.
pub fn init_empty() {
    if BOOTARGS_INIT.swap(true, Ordering::SeqCst) {
        return;
    }
    BOOTARGS_LEN.store(0, Ordering::Release);
}

/// Initialize from a `'static` str (used by mb2 path after stash).
pub fn init_str(s: &'static str) {
    if BOOTARGS_INIT.load(Ordering::Acquire) {
        return;
    }
    let b = s.as_bytes();
    let take = core::cmp::min(b.len(), BUF_SIZE);
    unsafe {
        core::ptr::copy_nonoverlapping(
            b.as_ptr(),
            core::ptr::addr_of_mut!(BOOTARGS_BUF) as *mut u8,
            take,
        );
        BOOTARGS_LEN.store(take, Ordering::Release);
        BOOTARGS_INIT.store(true, Ordering::Release);
    }
}

/// Raw stashed bytes (may be empty). Returns empty slice before init.
fn as_bytes() -> &'static [u8] {
    let len = BOOTARGS_LEN.load(Ordering::Acquire);
    if len == 0 {
        return b"";
    }
    unsafe { core::slice::from_raw_parts(core::ptr::addr_of!(BOOTARGS_BUF) as *const u8, len) }
}

/// Get the stashed cmdline as `&'static str`. `None` before any `init_*`, `Some("")` if empty.
pub fn get() -> Option<&'static str> {
    if !BOOTARGS_INIT.load(Ordering::Acquire) {
        return None;
    }
    let b = as_bytes();
    Some(core::str::from_utf8(b).unwrap_or(""))
}

/// Length of stashed cmdline (bytes).
pub fn len() -> usize {
    BOOTARGS_LEN.load(Ordering::Acquire)
}

/// True if initialized (even if empty).
pub fn is_init() -> bool {
    BOOTARGS_INIT.load(Ordering::Acquire)
}

/// Whitespace-split exact word match, no alloc. Matches `word` or `"-{word}"`.
pub fn contains_word(word: &str) -> bool {
    let b = as_bytes();
    if b.is_empty() || word.is_empty() {
        return false;
    }
    let w = word.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        while i < b.len() && (b[i] == b' ' || b[i] == b'\t' || b[i] == b'\n' || b[i] == b'\r') {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let start = i;
        while i < b.len() && b[i] != b' ' && b[i] != b'\t' && b[i] != b'\n' && b[i] != b'\r' {
            i += 1;
        }
        let token = &b[start..i];
        if token == w {
            return true;
        }
        if token.len() == w.len() + 1 && token[0] == b'-' && &token[1..] == w {
            return true;
        }
        if token.len() == w.len() + 2 && token[0] == b'-' && token[1] == b'-' && &token[2..] == w {
            return true;
        }
    }
    false
}

/// Convenience: `nokaslr` present (also `-nokaslr`).
pub fn is_nokaslr() -> bool {
    contains_word("nokaslr")
}

/// `nosmp` disables Application Processor bring-up and keeps the kernel on
/// the BSP. This is a bring-up escape hatch for isolating scheduler and AP
/// startup failures.
pub fn is_nosmp() -> bool {
    contains_word("nosmp") || contains_word("no_smp")
}

/// Optional upper bound on the number of CPUs brought online. `maxcpus=N`
/// and `max_cpus=N` include the BSP, so `maxcpus=1` is equivalent to
/// `nosmp`. Invalid, zero, and overflowing values are ignored.
pub fn max_cpus() -> Option<usize> {
    let bytes = as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        while i < bytes.len()
            && (bytes[i] == b' ' || bytes[i] == b'\t' || bytes[i] == b'\n' || bytes[i] == b'\r')
        {
            i += 1;
        }
        let start = i;
        while i < bytes.len()
            && bytes[i] != b' '
            && bytes[i] != b'\t'
            && bytes[i] != b'\n'
            && bytes[i] != b'\r'
        {
            i += 1;
        }
        let token = &bytes[start..i];
        let prefixes: [&[u8]; 4] = [b"maxcpus=", b"-maxcpus=", b"max_cpus=", b"-max_cpus="];
        for prefix in prefixes {
            if token.len() <= prefix.len() || !token.starts_with(prefix) {
                continue;
            }
            let mut value = 0usize;
            let mut valid = true;
            for &digit in &token[prefix.len()..] {
                if !digit.is_ascii_digit() {
                    valid = false;
                    break;
                }
                value = match value.checked_mul(10).and_then(|v| v.checked_add((digit - b'0') as usize)) {
                    Some(v) => v,
                    None => {
                        valid = false;
                        break;
                    }
                };
            }
            if valid && value != 0 {
                return Some(value);
            }
        }
    }
    None
}

/// `noiommu` disables the VT-d IOMMU (also `-noiommu`). Without this flag
/// the IOMMU is always on when DMAR is present (opt-out, not opt-in).
pub fn is_noiommu() -> bool {
    contains_word("noiommu")
}

/// `nobgrt` disables BGRT parsing (also `-nobgrt`). When present the
/// ACPI BGRT table is ignored and `AcpiSubsystem.bgrt` stays `None`.
pub fn is_nobgrt() -> bool {
    contains_word("nobgrt")
}

/// `nobootanim` disables the boot animation (also `-nobootanim`).
/// When present no BGRT logo, hex, track or stage text is drawn.
pub fn is_nobootanim() -> bool {
    contains_word("nobootanim")
}

/// `iommu_verify` gates extra IOMMU verification before enabling VT-d
/// (also `verifyiommu`, `verify_iommu`, `iommuverify`). When present the
/// framebuffer + RMRR identity maps are re-validated via SLPT translate and
/// any mismatch aborts IOMMU enable (fallback to `noiommu` path). Without
/// this flag the IOMMU enables best-effort. No-serial verification gate.
pub fn is_iommu_verify() -> bool {
    contains_word("iommu_verify")
        || contains_word("verifyiommu")
        || contains_word("verify_iommu")
        || contains_word("iommuverify")
        || contains_word("verify")
}

/// `nochime` suppresses the INIT startup chime (also `-nochime`).
/// When present the userspace `INIT` must not play `/B/EFI/BEDROCK/STARTUP.WAV`
/// and the kernel dumps the boot log to `/EFI/BEDROCK/boot.log` on the ESP
/// right before `INIT` is launched.
pub fn is_nochime() -> bool {
    contains_word("nochime")
}

/// `nocpuslowrepeat` disables the 100 ms periodic `cpu_slow` re-application
/// (also `-nocpuslowrepeat`, `--nocpuslowrepeat`). When present the kernel
/// still performs the initial `cpu_slow` MSR programming on BSP + APs, but
/// does not re-arm the 100 ms periodic timer that re-enforces the limit
/// against firmware overwrites.
pub fn is_nocpuslowrepeat() -> bool {
    contains_word("nocpuslowrepeat")
}

/// `nowatchdog` disables the NMI watchdog (also `-nowatchdog`,
/// `--nowatchdog`, `no_watchdog`, `nowdog`). When present the kernel skips
/// `watchdog::init()` entirely (no PMU/PIT/soft arming, no NMI handler
/// pet). The F9 hotkey still works via the soft path only if the watchdog
/// is armed, so with `nowatchdog` F9 is also inert. Default is watchdog
/// enabled; pass `nowatchdog` to silence it (e.g. for bring-up where the
/// 3 s NMI fires before userspace is ready).
pub fn is_nowatchdog() -> bool {
    contains_word("nowatchdog")
        || contains_word("no_watchdog")
        || contains_word("nowdog")
        || contains_word("disable_watchdog")
        || contains_word("nowd")
}

/// Explicit opt-in `watchdog` (also `-watchdog`). When present it forces
/// the watchdog on even if a future default changes. Currently a no-op
/// alias that returns true when the plain word `watchdog` appears; the
/// disable check above takes precedence.
pub fn is_watchdog() -> bool {
    contains_word("watchdog")
}
