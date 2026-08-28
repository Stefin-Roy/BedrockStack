//! Machine Check Architecture (MCA) — probe + pretty-print.
//!
//! Model-specific but architectural MSRs are safe behind CPUID MCE (EDX[7]).
//! All MSR accesses are via `rdmsr`; callers must ensure CR4.MCE is set and
//! CPUID says MCE exists. `dump_mca` is lock-free and uses only `dump_puts`.

use core::arch::asm;
use core::fmt::Write;

use crate::drivers::serial::{dump_put_hex, dump_puts};

const IA32_MCG_CAP: u32 = 0x179;
const IA32_MCG_STATUS: u32 = 0x17A;
const IA32_MCG_CTL: u32 = 0x17B;
const IA32_MCG_EXT_CTL: u32 = 0x4D0; // LMCE
const IA32_MCI_CTL_BASE: u32 = 0x400;
const IA32_MCI_STATUS_BASE: u32 = 0x401;
const IA32_MCI_ADDR_BASE: u32 = 0x402;
const IA32_MCI_MISC_BASE: u32 = 0x403;

#[inline]
unsafe fn read_msr(msr: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe { asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nomem, nostack)) };
    ((hi as u64) << 32) | (lo as u64)
}

#[inline]
unsafe fn write_msr(msr: u32, val: u64) {
    let lo = val as u32;
    let hi = (val >> 32) as u32;
    unsafe { asm!("wrmsr", in("ecx") msr, in("eax") lo, in("edx") hi, options(nomem, nostack)) };
}

fn has_mce() -> bool {
    let res = core::arch::x86_64::__cpuid(1);
    (res.edx & (1 << 7)) != 0
}

/// Enable MCA reporting: set MCG_CTL and each MCi_CTL to all-1s when
/// MCG_CAP[MCG_CTL_P] is set. Idempotent; no-op on CPUs without MCE or
/// with 0 banks (QEMU without -mce=on). This covers the hypervisor fallback
/// where MCG_CAP reads zero.
pub fn enable_mca() {
    if !has_mce() {
        return;
    }
    unsafe {
        let cap = read_msr(IA32_MCG_CAP);
        let count = (cap & 0xFF) as usize;
        if count == 0 {
            return;
        }
        let ctl_p = (cap >> 8) & 1 != 0;
        if ctl_p {
            write_msr(IA32_MCG_CTL, !0u64);
        }
        let n = count.min(32);
        for i in 0..n {
            let ctl = IA32_MCI_CTL_BASE + (i as u32) * 4;
            write_msr(ctl, !0u64);
        }
    }
}

/// Lock-free MCA dump to serial (which panicscreen will mirror).
///
/// Called from #MC handler before `dump_full_fault`. Also callable from
/// non-#MC fatal paths for diagnostics.
pub fn dump_mca<W: Write>(w: &mut W) {
    if !has_mce() {
        let _ = writeln!(w, "--- Machine Check (MCA) ---");
        let _ = writeln!(w, "  CPUID MCE=0 — no MCA banks");
        return;
    }
    unsafe {
        let cap = read_msr(IA32_MCG_CAP);
        let count = (cap & 0xFF) as usize;
        let ctl_p = (cap >> 8) & 1 != 0;
        let ext_p = (cap >> 9) & 1 != 0;
        let cmci_p = (cap >> 10) & 1 != 0;
        let tes_p = (cap >> 11) & 1 != 0;

        let status = read_msr(IA32_MCG_STATUS);
        let ripv = (status >> 0) & 1;
        let eipv = (status >> 1) & 1;
        let mcip = (status >> 2) & 1;
        let lmce = if ext_p { (status >> 3) & 1 } else { 0 };

        let _ = writeln!(w, "--- Machine Check (MCA) ---");
        let _ = writeln!(w, "MCG_CAP    = {:#018x}  banks={} ctl_p={} ext_p={} cmci_p={} tes_p={}", cap, count, ctl_p as u8, ext_p as u8, cmci_p as u8, tes_p as u8);
        let _ = writeln!(w, "MCG_STATUS = {:#018x}  RIPV={} EIPV={} MCIP={} LMCE={}", status, ripv, eipv, mcip, lmce);
        if ctl_p {
            let mcg_ctl = read_msr(IA32_MCG_CTL);
            let _ = writeln!(w, "MCG_CTL    = {:#018x}", mcg_ctl);
        }
        if ext_p {
            // LMCE opt: IA32_MCG_EXT_CTL bit0 = LMCE_EN. Read if implemented.
            let ext = read_msr(IA32_MCG_EXT_CTL);
            let _ = writeln!(w, "MCG_EXT_CTL= {:#018x}  LMCE_EN={}", ext, ext & 1);
        }

        let n = count.min(32);
        if n == 0 {
            let _ = writeln!(w, "  (no MCA banks reported)");
            return;
        }
        for i in 0..n {
            let st = read_msr(IA32_MCI_STATUS_BASE + (i as u32) * 4);
            let valid = (st >> 63) & 1 != 0;
            if !valid {
                // Only print non-zero banks in minimal mode? Print valid==0 as one-liner?
                continue;
            }
            let over = (st >> 62) & 1;
            let uc = (st >> 61) & 1;
            let en = (st >> 60) & 1;
            let misci = 0; // placeholder
            let addrv = (st >> 58) & 1;
            let miscv = (st >> 59) & 1;
            let pcc = (st >> 57) & 1;
            let s = (st >> 56) & 1;
            let ar = (st >> 55) & 1;
            let mca_code = st & 0xFFFF;
            let mca_model = (st >> 16) & 0xFFFF;

            let _ = writeln!(w, "  MC{} STATUS={:#018x}  VAL=1 UC={} EN={} PCC={} S={} AR={} ADDRV={} MISCV={} OVER={}  code={:#06x} model={:#06x}",
                i, st, uc, en, pcc, s, ar, addrv, miscv, over, mca_code, mca_model);

            if addrv != 0 {
                let addr = read_msr(IA32_MCI_ADDR_BASE + (i as u32) * 4);
                let _ = writeln!(w, "      ADDR={:#018x}", addr);
            }
            if miscv != 0 {
                let misc = read_msr(IA32_MCI_MISC_BASE + (i as u32) * 4);
                let _ = writeln!(w, "      MISC={:#018x}", misc);
            }
            // Also show CTL for completeness
            let ctl = read_msr(IA32_MCI_CTL_BASE + (i as u32) * 4);
            let _ = writeln!(w, "      CTL ={:#018x}", ctl);
            let _ = misci; // avoid warn
        }
        // For completeness, list invalid banks as absent
        let mut any_valid = false;
        for i in 0..n {
            let st = read_msr(IA32_MCI_STATUS_BASE + (i as u32) * 4);
            if (st >> 63) & 1 != 0 { any_valid = true; break; }
        }
        if !any_valid {
            let _ = writeln!(w, "  (no banks with VAL=1 — spurious or already cleared)");
        }
    }
}

/// Serial-only variant used when caller has no writer (e.g. early #MC).
pub fn dump_mca_serial() {
    struct S;
    impl Write for S {
        fn write_str(&mut self, s: &str) -> core::fmt::Result { dump_puts(s); Ok(()) }
    }
    let mut s = S;
    dump_mca(&mut s);
    // also hex dump of banks via serial direct? Already done.
    let _ = dump_put_hex(0); // hush
}
