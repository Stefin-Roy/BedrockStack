
//! CPU slow-mode / performance limiting for Intel processors.
//!
//! Primary mechanism:
//!     HWP (IA32_HWP_REQUEST)
//!
//! Fallback:
//!     IA32_PERF_CTL / EIST
//!
//! Last-resort fallback:
//!     IA32_CLOCK_MODULATION
//!
//! Intended to be called independently on each logical processor.
//!
//! Target:
//!     Intel Core i5-10300H (Comet Lake-H)
//!
//! Important:
//!     HWP expresses performance as processor-specific performance
//!     values. On the Comet Lake family these values correspond to
//!     frequency ratios against the nominal 100 MHz reference clock,
//!     so ratio 8 corresponds to approximately 800 MHz.
//!
//!     HWP is still a request/constraint mechanism, not a hard real-time
//!     clock-frequency guarantee. Thermal, package-power, firmware and
//!     hardware constraints may cause actual frequency to differ.

use core::arch::asm;


// ---------------------------------------------------------------------------
// MSRs
// ---------------------------------------------------------------------------

const IA32_PERF_CTL: u32 = 0x199;
const IA32_PERF_STATUS: u32 = 0x198;

const IA32_CLOCK_MODULATION: u32 = 0x19A;

const IA32_ENERGY_PERF_BIAS: u32 = 0x1B0;

const IA32_PM_ENABLE: u32 = 0x770;
const IA32_HWP_CAPABILITIES: u32 = 0x771;
const IA32_HWP_REQUEST: u32 = 0x774;
const IA32_HWP_STATUS: u32 = 0x777;


// ---------------------------------------------------------------------------
// Target
// ---------------------------------------------------------------------------

/// Desired maximum core ratio.
///
/// 8 * 100 MHz = 800 MHz nominally.
const SLOW_MAX_RATIO: u8 = 8;


// ---------------------------------------------------------------------------
// CPUID feature bits
// ---------------------------------------------------------------------------

const CPUID_ECX_EIST: u32 = 1 << 7;
const CPUID_ECX_TM2: u32 = 1 << 8;

const CPUID_ECX_HYPERVISOR: u32 = 1 << 31;

const CPUID_LEAF6_EAX_HWP: u32 = 1 << 7;
const CPUID_LEAF6_EAX_HWP_EPP: u32 = 1 << 10;


// ---------------------------------------------------------------------------
// HWP field definitions
// ---------------------------------------------------------------------------

const HWP_MIN_SHIFT: u32 = 0;
const HWP_MAX_SHIFT: u32 = 8;
const HWP_EPP_SHIFT: u32 = 24;

const HWP_BYTE_MASK: u64 = 0xff;

const HWP_MIN_MASK: u64 =
    HWP_BYTE_MASK << HWP_MIN_SHIFT;

const HWP_MAX_MASK: u64 =
    HWP_BYTE_MASK << HWP_MAX_SHIFT;

const HWP_EPP_MASK: u64 =
    HWP_BYTE_MASK << HWP_EPP_SHIFT;


// ---------------------------------------------------------------------------
// HWP capabilities
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct HwpCapabilities {
    /// Highest non-guaranteed performance level.
    highest: u8,

    /// Guaranteed performance level.
    guaranteed: u8,

    /// Most efficient performance level.
    efficient: u8,

    /// Lowest performance level software may request.
    lowest: u8,
}

impl HwpCapabilities {
    #[inline]
    fn read() -> Self {
        let raw = unsafe { rdmsr(IA32_HWP_CAPABILITIES) };

        Self {
            // Intel SDM:
            //   [7:0]   Highest
            //   [15:8]  Guaranteed
            //   [23:16] Most Efficient
            //   [31:24] Lowest

            highest: (raw & 0xff) as u8,
            guaranteed: ((raw >> 8) & 0xff) as u8,
            efficient: ((raw >> 16) & 0xff) as u8,
            lowest: ((raw >> 24) & 0xff) as u8,
        }
    }

    /// Returns the smallest legal HWP value which is at least `requested`.
    ///
    /// Example:
    ///
    ///     requested = 8
    ///     lowest    = 8
    ///     -> 8
    ///
    /// If the processor's lowest programmable performance is above 8,
    /// HWP cannot represent an 800 MHz maximum. In that case this returns
    /// the lowest programmable value, and the caller can decide whether
    /// that is acceptable.
    #[inline]
    fn clamp_minimum(&self, requested: u8) -> u8 {
        if requested < self.lowest {
            self.lowest
        } else {
            requested
        }
    }
}


// ---------------------------------------------------------------------------
// Result
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SlowModeResult {
    /// HWP successfully configured with a maximum at or above the target.
    Hwp,

    /// HWP was unavailable, EIST fallback was used.
    Eist,

    /// Neither HWP nor EIST was usable; clock modulation was used.
    ClockModulation,

    /// CPU was not Intel.
    NotIntel,

    /// Execution is under a hypervisor.
    Hypervisor,

    /// No supported mechanism was found.
    Unsupported,

    /// HWP exists but its lowest programmable value is above the
    /// requested target.
    HwpCannotRepresentTarget {
        requested: u8,
        minimum: u8,
    },
}


// ---------------------------------------------------------------------------
// Low-level MSR access
// ---------------------------------------------------------------------------

#[inline(always)]
pub unsafe fn wrmsr(msr: u32, value: u64) {
    let low = value as u32;
    let high = (value >> 32) as u32;

    unsafe {
        asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") low,
            in("edx") high,
            options(nomem, nostack, preserves_flags)
        );
    }
}


#[inline(always)]
pub unsafe fn rdmsr(msr: u32) -> u64 {
    let low: u32;
    let high: u32;

    unsafe {
        asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags)
        );
    }

    (low as u64) | ((high as u64) << 32)
}


// ---------------------------------------------------------------------------
// CPUID
// ---------------------------------------------------------------------------

#[inline(always)]
pub unsafe fn cpuid(
    leaf: u32,
    subleaf: u32,
) -> (u32, u32, u32, u32) {
    let eax: u32;
    let ebx: u32;
    let ecx: u32;
    let edx: u32;

    unsafe {
        asm!(
            "push rbx",
            "cpuid",
            "mov {ebx_out:e}, ebx",
            "pop rbx",

            ebx_out = out(reg) ebx,

            inout("eax") leaf => eax,
            inout("ecx") subleaf => ecx,
            out("edx") edx,

            options(nomem, preserves_flags)
        );
    }

    (eax, ebx, ecx, edx)
}


// ---------------------------------------------------------------------------
// CPU identification
// ---------------------------------------------------------------------------

#[inline]
fn cpuid_max_leaf() -> u32 {
    unsafe { cpuid(0, 0).0 }
}


fn is_intel() -> bool {
    let (_, ebx, ecx, edx) = unsafe {
        cpuid(0, 0)
    };

    // CPUID vendor string is:
    //
    // EBX = "Genu"
    // EDX = "ineI"
    // ECX = "ntel"

    ebx == u32::from_le_bytes(*b"Genu")
        && edx == u32::from_le_bytes(*b"ineI")
        && ecx == u32::from_le_bytes(*b"ntel")
}


fn is_hypervisor() -> bool {
    if cpuid_max_leaf() < 1 {
        return false;
    }

    let (_, _, ecx, _) = unsafe {
        cpuid(1, 0)
    };

    (ecx & CPUID_ECX_HYPERVISOR) != 0
}


// ---------------------------------------------------------------------------
// Family / model identification
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct CpuSignature {
    family: u8,
    model: u8,
    stepping: u8,
}


fn cpu_signature() -> CpuSignature {
    let (eax, _, _, _) = unsafe {
        cpuid(1, 0)
    };

    let stepping = (eax & 0x0f) as u8;
    let model = ((eax >> 4) & 0x0f) as u8;
    let family = ((eax >> 8) & 0x0f) as u8;

    let extended_model = ((eax >> 16) & 0x0f) as u8;
    let extended_family = ((eax >> 20) & 0xff) as u8;

    let family = if family == 0x0f {
        family + extended_family
    } else {
        family
    };

    let model = if family == 0x06 || family == 0x0f {
        model | (extended_model << 4)
    } else {
        model
    };

    CpuSignature {
        family,
        model,
        stepping,
    }
}


// ---------------------------------------------------------------------------
// Feature detection
// ---------------------------------------------------------------------------

fn has_eist() -> bool {
    if cpuid_max_leaf() < 1 {
        return false;
    }

    let (_, _, ecx, _) = unsafe {
        cpuid(1, 0)
    };

    (ecx & CPUID_ECX_EIST) != 0
}


fn has_tm2() -> bool {
    if cpuid_max_leaf() < 1 {
        return false;
    }

    let (_, _, ecx, _) = unsafe {
        cpuid(1, 0)
    };

    (ecx & CPUID_ECX_TM2) != 0
}


fn has_hwp() -> bool {
    if cpuid_max_leaf() < 6 {
        return false;
    }

    let (eax, _, _, _) = unsafe {
        cpuid(6, 0)
    };

    (eax & CPUID_LEAF6_EAX_HWP) != 0
}


fn has_hwp_epp() -> bool {
    if cpuid_max_leaf() < 6 {
        return false;
    }

    let (eax, _, _, _) = unsafe {
        cpuid(6, 0)
    };

    (eax & CPUID_LEAF6_EAX_HWP_EPP) != 0
}





// ---------------------------------------------------------------------------
// HWP enable state
// ---------------------------------------------------------------------------

#[inline]
fn hwp_enabled() -> bool {
    // IA32_PM_ENABLE[0].
    //
    // This MSR exists only when HWP is supported.
    //
    // Do not blindly write this from slow mode. HWP is normally enabled
    // by firmware/OS policy. If it is disabled, we can explicitly enable
    // it because this bit is architecturally defined as the HWP enable.
    unsafe {
        (rdmsr(IA32_PM_ENABLE) & 1) != 0
    }
}


#[inline]
unsafe fn enable_hwp() {
    let mut value = unsafe { rdmsr(IA32_PM_ENABLE) };

    value |= 1;

    unsafe { wrmsr(IA32_PM_ENABLE, value) };
}


// ---------------------------------------------------------------------------
// HWP programming
// ---------------------------------------------------------------------------

/// Program an HWP maximum-performance constraint.
///
/// This deliberately preserves:
///
///     minimum
///     desired
///     EPP
///     reserved/high bits
///
/// Only Maximum_Performance is modified.
unsafe fn hwp_set_maximum(maximum: u8) {
    let mut request = unsafe { rdmsr(IA32_HWP_REQUEST) };

    request &= !HWP_MAX_MASK;
    request |= (maximum as u64) << HWP_MAX_SHIFT;

    unsafe { wrmsr(IA32_HWP_REQUEST, request) };
}


/// Set HWP minimum.
///
/// Do not normally use this for a "maximum 800 MHz" policy.
unsafe fn hwp_set_minimum(minimum: u8) {
    let mut request = unsafe { rdmsr(IA32_HWP_REQUEST) };

    request &= !HWP_MIN_MASK;
    request |= (minimum as u64) << HWP_MIN_SHIFT;

    unsafe { wrmsr(IA32_HWP_REQUEST, request) };
}


/// Set EPP while preserving all other HWP request fields.
///
/// EPP:
///
///     0x00 = performance-oriented
///     0xFF = energy-oriented
///
/// EPP is a preference, NOT a frequency limit.
unsafe fn hwp_set_epp(epp: u8) {
    let mut request = unsafe { rdmsr(IA32_HWP_REQUEST) };

    request &= !HWP_EPP_MASK;
    request |= (epp as u64) << HWP_EPP_SHIFT;

    unsafe { wrmsr(IA32_HWP_REQUEST, request) };
}


// ---------------------------------------------------------------------------
// EIST fallback
// ---------------------------------------------------------------------------

/// Program IA32_PERF_CTL's target ratio.
///
/// IA32_PERF_CTL bits 15:8 contain the target ratio on the relevant
/// Intel processors.
///
/// This is a fallback for systems without HWP.
///
/// It does NOT provide the same autonomous min/max constraint semantics
/// as HWP.
unsafe fn eist_set_ratio(ratio: u8) {
    let mut perf_ctl = unsafe { rdmsr(IA32_PERF_CTL) };

    // Preserve everything except the target P-state field.
    perf_ctl &= !0x0000_ff00u64;

    perf_ctl |= (ratio as u64) << 8;

    unsafe { wrmsr(IA32_PERF_CTL, perf_ctl) };
}


// ---------------------------------------------------------------------------
// Clock modulation fallback
// ---------------------------------------------------------------------------

/// Disable software-controlled clock modulation.
///
/// This restores the normal clock path without touching unrelated
/// reserved/configuration bits.
unsafe fn clock_modulation_disable() {
    let mut value = unsafe { rdmsr(IA32_CLOCK_MODULATION) };

    value &= !(1u64 << 4);

    unsafe { wrmsr(IA32_CLOCK_MODULATION, value) };
}


/// Configure software-controlled clock modulation.
///
/// `duty_encoding` is:
///
///     001 = 12.5%
///     010 = 25%
///     011 = 37.5%
///     100 = 50%
///     101 = 63.5%
///     110 = 75%
///     111 = 87.5%
///
/// This is deliberately separate from HWP/EIST because clock modulation
/// is not a P-state. It gates clock delivery and therefore changes
/// effective throughput rather than selecting a genuine 800 MHz
/// operating point.
unsafe fn clock_modulation_enable(duty_encoding: u8) {
    let mut value = unsafe { rdmsr(IA32_CLOCK_MODULATION) };

    value &= !0x1e;
    value |= ((duty_encoding as u64) & 0x07) << 1;
    value |= 1 << 4;

    unsafe { wrmsr(IA32_CLOCK_MODULATION, value) };
}


// ---------------------------------------------------------------------------
// Energy Performance Bias
// ---------------------------------------------------------------------------

/// Set IA32_ENERGY_PERF_BIAS to maximum energy preference.
///
/// This is a preference only. It does not establish a frequency cap.
///
/// This function is intentionally separate because HWP EPP supersedes
/// much of the practical usefulness of EPB when HWP is active.
unsafe fn set_energy_perf_bias_max_savings() {
    unsafe { wrmsr(IA32_ENERGY_PERF_BIAS, 0x0f) };
}


// ---------------------------------------------------------------------------
// HWP slow-mode implementation
// ---------------------------------------------------------------------------

unsafe fn enable_hwp_slow_mode() -> SlowModeResult {
    let caps = HwpCapabilities::read();

    let requested = SLOW_MAX_RATIO;

    //
    // The requested value must be legal.
    //
    // Intel defines Lowest_Performance as the lowest performance level
    // software can program into IA32_HWP_REQUEST.
    //
    // Therefore:
    //
    //     requested < lowest
    //
    // means that an exact HWP representation of the requested target
    // does not exist.
    //
    let clamped = caps.clamp_minimum(requested);
    if clamped != requested {
        return SlowModeResult::HwpCannotRepresentTarget {
            requested,
            minimum: clamped,
        };
    }

    //
    // Do not blindly destroy the existing HWP request.
    //
    // Preserve:
    //
    //     Minimum
    //     Desired
    //     EPP
    //     Activity Window
    //     Package-selection / valid fields
    //     reserved bits
    //
    // and modify only Maximum.
    //
    unsafe { hwp_set_maximum(clamped) };

    //
    // An EPP of FF encourages energy-efficient operation.
    //
    // It is deliberately a preference, not part of the frequency limit.
    //
    if has_hwp_epp() {
        unsafe { hwp_set_epp(0xff) };
    }

    SlowModeResult::Hwp
}


// ---------------------------------------------------------------------------
// Public slow-mode entry point
// ---------------------------------------------------------------------------

/// Configure the current logical processor for CPU slow mode.
///
/// Policy:
///
/// 1. Reject non-Intel CPUs.
/// 2. Refuse to modify CPUs running under a detected hypervisor.
/// 3. Prefer HWP.
/// 4. If HWP is supported but disabled, enable it.
/// 5. Program HWP Maximum_Performance = 8 (800 MHz ratio).
/// 6. Preserve the other HWP request fields.
/// 7. Use EPP=FF as an additional energy-saving preference.
/// 8. If HWP is unavailable, fall back to IA32_PERF_CTL.
/// 9. If EIST is unavailable, optionally fall back to clock modulation.
///
/// This function does NOT combine HWP + EIST + clock modulation.
///
/// Each mechanism is an independent policy mechanism and stacking them
/// makes behavior difficult to reason about.
///
/// Call this once on every logical processor.
pub unsafe fn enable_cpu_slow_mode() -> SlowModeResult {
    // -----------------------------------------------------------------------
    // Vendor
    // -----------------------------------------------------------------------

    if !is_intel() {
        return SlowModeResult::NotIntel;
    }


    // -----------------------------------------------------------------------
    // Hypervisor
    // -----------------------------------------------------------------------
    //
    // Do not touch physical-performance MSRs when CPUID says we are
    // virtualized. A hypervisor may virtualize these registers and may
    // interpret writes completely differently.
    //

    if is_hypervisor() {
        return SlowModeResult::Hypervisor;
    }


    // -----------------------------------------------------------------------
    // HWP
    // -----------------------------------------------------------------------

    if has_hwp() {
        //
        // HWP is architecturally enabled through IA32_PM_ENABLE[0].
        //
        // Normally firmware or the OS has already enabled this before
        // exposing HWP. Enabling it here is useful for a standalone
        // bootloader/kernel environment.
        //

        if !hwp_enabled() {
            unsafe { enable_hwp() };
        }

        let result = unsafe { enable_hwp_slow_mode() };

        match result {
            SlowModeResult::Hwp => {
                //
                // Do not also touch IA32_PERF_CTL or clock modulation.
                //
                // HWP is now the authority for this logical processor.
                //

                return SlowModeResult::Hwp;
            }

            SlowModeResult::HwpCannotRepresentTarget { .. } => {
                //
                // Do not silently claim that we achieved 800 MHz.
                //
                // If the HWP performance domain does not contain ratio 8,
                // the request cannot be represented as an HWP maximum.
                //
                return result;
            }

            _ => {}
        }
    }


    // -----------------------------------------------------------------------
    // EIST / IA32_PERF_CTL fallback
    // -----------------------------------------------------------------------

    if has_eist() {
        //
        // Request ratio 8.
        //
        // On the Comet Lake family this corresponds to approximately
        // 800 MHz with the nominal 100 MHz reference clock.
        //

        unsafe { eist_set_ratio(SLOW_MAX_RATIO) };

        //
        // Energy Performance Bias is a preference, so it can be applied
        // in the non-HWP fallback path.
        //

        unsafe { set_energy_perf_bias_max_savings() };

        return SlowModeResult::Eist;
    }


    // -----------------------------------------------------------------------
    // Last-resort clock modulation
    // -----------------------------------------------------------------------
    //
    // Clock modulation does NOT create an 800 MHz P-state.
    //
    // It reduces effective clock delivery by periodically stopping the
    // core clock.
    //
    // We therefore do not pretend this is equivalent to an 800 MHz
    // frequency cap.
    //
    // The least aggressive legal duty cycle is 87.5%, but that doesn't
    // help us achieve 800 MHz from a known base frequency.
    //
    // This fallback is therefore intentionally conservative and should
    // only be enabled if your policy explicitly wants throttling rather
    // than a true P-state request.
    //

    if has_tm2() {
        //
        // 25% duty cycle:
        //
        //     encoding 010
        //     register bits 3:1 = 010
        //
        // This is a substantial throughput reduction but is NOT an
        // 800 MHz frequency setting.
        //

        unsafe { clock_modulation_enable(0b010) };

        return SlowModeResult::ClockModulation;
    }


    SlowModeResult::Unsupported
}


// ---------------------------------------------------------------------------
// Optional explicit helpers
// ---------------------------------------------------------------------------

/// Force the current HWP logical processor to use the slow ratio for both
/// minimum and maximum.
///
/// This is substantially more aggressive than simply setting Maximum=8.
///
/// Use only if you specifically want the HWP request to pin its requested
/// operating point rather than merely cap the maximum.
pub unsafe fn force_hwp_slow_mode() -> SlowModeResult {
    if !is_intel() || is_hypervisor() {
        return if !is_intel() {
            SlowModeResult::NotIntel
        } else {
            SlowModeResult::Hypervisor
        };
    }

    if !has_hwp() {
        return SlowModeResult::Unsupported;
    }

    if !hwp_enabled() {
        unsafe { enable_hwp() };
    }

    let caps = HwpCapabilities::read();

    let clamped = caps.clamp_minimum(SLOW_MAX_RATIO);
    if clamped != SLOW_MAX_RATIO {
        return SlowModeResult::HwpCannotRepresentTarget {
            requested: SLOW_MAX_RATIO,
            minimum: clamped,
        };
    }

    //
    // Minimum = 8
    // Maximum = 8
    //
    // Desired remains untouched.
    //
    unsafe {
        hwp_set_minimum(clamped);
        hwp_set_maximum(clamped);
    }

    if has_hwp_epp() {
        unsafe { hwp_set_epp(0xff) };
    }

    SlowModeResult::Hwp
}


/// Remove the software HWP maximum imposed by this module.
///
/// The correct value for "unrestricted" is the processor's Highest_Performance
/// capability, not 0.
///
/// This preserves minimum, desired, EPP, and other fields.
pub unsafe fn clear_hwp_slow_max() {
    if !has_hwp() {
        return;
    }

    let caps = HwpCapabilities::read();

    unsafe { hwp_set_maximum(caps.highest) };
}


/// Restore normal software clock modulation.
///
/// This does not alter HWP or IA32_PERF_CTL.
pub unsafe fn clear_cpu_clock_modulation() {
    unsafe { clock_modulation_disable() };
}


// ---------------------------------------------------------------------------
// Diagnostic accessors
// ---------------------------------------------------------------------------

/// Read the current HWP capabilities of this logical processor.
///
/// Useful for boot diagnostics.
pub unsafe fn read_hwp_capabilities() -> Option<HwpCapabilitiesRaw> {
    if !has_hwp() {
        return None;
    }

    let caps = HwpCapabilities::read();

    Some(HwpCapabilitiesRaw {
        highest: caps.highest,
        guaranteed: caps.guaranteed,
        efficient: caps.efficient,
        lowest: caps.lowest,
    })
}


#[derive(Clone, Copy)]
pub struct HwpCapabilitiesRaw {
    pub highest: u8,
    pub guaranteed: u8,
    pub efficient: u8,
    pub lowest: u8,
}


/// Read the current HWP request for diagnostics.
pub unsafe fn read_hwp_request() -> Option<u64> {
    if !has_hwp() {
        return None;
    }

    Some(unsafe { rdmsr(IA32_HWP_REQUEST) })
}


/// Read the current HWP status.
pub unsafe fn read_hwp_status() -> Option<u64> {
    if !has_hwp() {
        return None;
    }

    Some(unsafe { rdmsr(IA32_HWP_STATUS) })
}


/// Read current IA32_PERF_CTL.
pub unsafe fn read_perf_ctl() -> u64 {
    unsafe { rdmsr(IA32_PERF_CTL) }
}


/// Read current IA32_PERF_STATUS.
pub unsafe fn read_perf_status() -> u64 {
    unsafe { rdmsr(IA32_PERF_STATUS) }
}


/// Read current clock modulation state.
pub unsafe fn read_clock_modulation() -> u64 {
    unsafe { rdmsr(IA32_CLOCK_MODULATION) }
}


/// Read current CPU signature.
pub fn cpu_info() -> (u8, u8, u8) {
    let sig = cpu_signature();

    (sig.family, sig.model, sig.stepping)
}


// ---------------------------------------------------------------------------
// Optional compile-time assertion for the intended target.
// ---------------------------------------------------------------------------

/// Comet Lake client/mobile is family 6, model 165 (0xA5).
///
/// This is informational only. Do not use it to gate HWP itself because
/// the implementation is deliberately generic to Intel processors.
///
/// The i5-10300H is expected to report family 6 / model 0xA5.
pub fn is_expected_comet_lake_h() -> bool {
    let sig = cpu_signature();

    sig.family == 6 && sig.model == 0xA5
}