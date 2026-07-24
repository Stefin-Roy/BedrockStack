//! MSI/MSI-X capability test suite.
//!
//! Tests the PCI MSI (Message Signalled Interrupts) and MSI-X (MSI with
//! eXtended capabilities) detection and configuration functions.
//!
//! All write tests save and restore the original MSI/MSI-X Message Control
//! register to avoid interfering with other drivers.  MSI tests target the
//! AHCI controller (always present on QEMU Q35).  MSI-X tests scan all PCI
//! devices and use the first one with an MSI-X capability, so adding virtio
//! or NVMe devices automatically exercises those paths.

use core::sync::atomic::{AtomicU32, Ordering};

use framebuffer::Framebuffer;
use crate::drivers::serial::SerialPort;
use crate::pci;
use crate::pci::caps::{self, CAP_MSI, CAP_MSIX};
use crate::pci::PciDevice;
use crate::platform::x86_64_pc::apic;
use super::Module;

static PASS: AtomicU32 = AtomicU32::new(0);
static SKIP: AtomicU32 = AtomicU32::new(0);
static FAIL: AtomicU32 = AtomicU32::new(0);

macro_rules! t {
    ($name:expr, $body:expr) => {
        {
            let mut port = SerialPort::new();
            use core::fmt::Write;
            write!(port, "[MSIXTEST] {:35} ", $name).ok();
            match $body {
                Ok(()) => {
                    write!(port, "PASS\n").ok();
                    PASS.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    if e == "SKIP" {
                        write!(port, "SKIP\n").ok();
                        SKIP.fetch_add(1, Ordering::Relaxed);
                    } else {
                        write!(port, "FAIL: {}\n", e).ok();
                        FAIL.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
    };
}

/// Find the first AHCI controller on the PCI bus.
fn find_ahci() -> Option<&'static PciDevice> {
    pci::devices().iter().find(|d| d.class == 0x01 && d.subclass == 0x06 && d.prog_if == 0x01)
}

/// Find any PCI device that supports MSI-X (e.g. virtio, NVMe).
fn find_msix_device() -> Option<&'static PciDevice> {
    pci::devices().iter().find(|d| caps::has(d, CAP_MSIX))
}

/// Find the first LPC bridge (typically a non-MSI device).
fn find_lpc() -> Option<&'static PciDevice> {
    pci::devices().iter().find(|d| d.class == 0x06 && d.subclass == 0x01)
}

// ── Test cases ───────────────────────────────────────────────────

fn test_find_msi_cap() -> Result<(), &'static str> {
    let dev = find_ahci().ok_or("no AHCI controller")?;
    let cap = caps::find(dev, CAP_MSI).ok_or("MSI cap not found on AHCI")?;
    if cap.id != CAP_MSI { return Err("wrong capability ID"); }
    if cap.offset == 0 { return Err("MSI cap offset is 0"); }
    Ok(())
}

fn test_find_msix_cap() -> Result<(), &'static str> {
    let dev = match find_msix_device() {
        Some(d) => d,
        None => return Err("SKIP"),
    };
    let cap = caps::find(dev, CAP_MSIX).ok_or("MSI-X cap not found despite has()")?;
    if cap.id != CAP_MSIX { return Err("wrong capability ID"); }
    if cap.offset == 0 { return Err("MSI-X cap offset is 0"); }
    Ok(())
}

fn test_msi_cap_info() -> Result<(), &'static str> {
    let dev = find_ahci().ok_or("no AHCI controller")?;
    let cap = caps::find(dev, CAP_MSI).ok_or("MSI cap not found")?;
    let (is_64bit, has_pvm) = pci::msi::cap_info(dev, &cap);
    // ICH9 on Q35 always has 64-bit MSI; per-vector masking depends on rev.
    if !is_64bit { return Err("ICH9 AHCI MSI should be 64-bit"); }
    let mc = caps::read_u16(dev, &cap, 2);
    if is_64bit != ((mc >> 7) & 1 != 0) { return Err("cap_info 64-bit disagrees with hardware"); }
    if has_pvm != ((mc >> 8) & 1 != 0) { return Err("cap_info PVM disagrees with hardware"); }
    Ok(())
}

fn test_msix_table_info() -> Result<(), &'static str> {
    let dev = match find_msix_device() {
        Some(d) => d,
        None => return Err("SKIP"),
    };
    let cap = caps::find(dev, CAP_MSIX).ok_or("MSI-X cap lookup failed")?;
    let info = pci::msix::table_info(dev, &cap);
    if info.table_size == 0 { return Err("MSI-X table size is 0"); }
    if info.bir > 5 { return Err("MSI-X BIR out of range"); }
    if info.pba_bir > 5 { return Err("MSI-X PBA BIR out of range"); }
    Ok(())
}

fn test_no_msi_on_lpc() -> Result<(), &'static str> {
    let dev = find_lpc().ok_or("no LPC bridge found")?;
    if caps::has(dev, CAP_MSI) { return Err("LPC bridge unexpectedly has MSI"); }
    if caps::has(dev, CAP_MSIX) { return Err("LPC bridge unexpectedly has MSI-X"); }
    Ok(())
}

fn test_msi_enable_disable() -> Result<(), &'static str> {
    let dev = find_ahci().ok_or("no AHCI controller")?;
    let cap = caps::find(dev, CAP_MSI).ok_or("MSI cap not found")?;
    let (is_64bit, _) = pci::msi::cap_info(dev, &cap);

    // Save original MSI registers.
    let saved_mc = caps::read_u16(dev, &cap, 2);
    let saved_ma = caps::read_u32(dev, &cap, 4);
    let saved_mua = if is_64bit { caps::read_u32(dev, &cap, 8) } else { 0 };
    let saved_md = if is_64bit {
        caps::read_u16(dev, &cap, 12)
    } else {
        caps::read_u16(dev, &cap, 8)
    };

    // Enable MSI with a dummy vector and the BSP APIC ID.
    let vector: u8 = 0xFA;
    let apic_id = apic::read_apic_id();
    pci::msi::enable(dev, &cap, vector, apic_id);

    // Verify enable bit is set.
    let mc = caps::read_u16(dev, &cap, 2);
    if mc & 1 == 0 {
        caps::write_u16(dev, &cap, 2, saved_mc);
        return Err("MSI enable bit not set after enable()");
    }

    // Verify Message Address.
    let ma = caps::read_u32(dev, &cap, 4);
    let expected_ma = 0xFEE00000u32 | ((apic_id as u32) << 12);
    if ma != expected_ma {
        caps::write_u16(dev, &cap, 2, saved_mc);
        return Err("MSI Message Address mismatch");
    }

    // Verify Message Data equals the requested vector.
    let md = if is_64bit { caps::read_u16(dev, &cap, 12) } else { caps::read_u16(dev, &cap, 8) };
    if md != vector as u16 {
        caps::write_u16(dev, &cap, 2, saved_mc);
        return Err("MSI Message Data mismatch");
    }

    // For 64-bit MSI, verify Upper Address is cleared to 0.
    if is_64bit {
        let mua = caps::read_u32(dev, &cap, 8);
        if mua != 0 {
            caps::write_u16(dev, &cap, 2, saved_mc);
            return Err("MSI Upper Address should be 0");
        }
    }

    // Disable MSI and verify the enable bit is cleared.
    pci::msi::disable(dev, &cap);
    let mc2 = caps::read_u16(dev, &cap, 2);
    if mc2 & 1 != 0 {
        caps::write_u16(dev, &cap, 2, saved_mc);
        return Err("MSI enable bit still set after disable()");
    }

    // Restore original MSI register state.
    caps::write_u32(dev, &cap, 4, saved_ma);
    if is_64bit {
        caps::write_u32(dev, &cap, 8, saved_mua);
        caps::write_u16(dev, &cap, 12, saved_md);
    } else {
        caps::write_u16(dev, &cap, 8, saved_md);
    }
    caps::write_u16(dev, &cap, 2, saved_mc);
    Ok(())
}

fn test_msix_mc_register() -> Result<(), &'static str> {
    let dev = match find_msix_device() {
        Some(d) => d,
        None => return Err("SKIP"),
    };
    let cap = caps::find(dev, CAP_MSIX).ok_or("MSI-X cap lookup failed")?;

    // Save original MC.
    let saved_mc = caps::read_u16(dev, &cap, 2);

    // Clear Enable (bit 14), set Function Mask (bit 15).
    caps::write_u16(dev, &cap, 2, (saved_mc & !(1 << 14)) | (1 << 15));
    let mc = caps::read_u16(dev, &cap, 2);
    if mc & (1 << 15) == 0 {
        caps::write_u16(dev, &cap, 2, saved_mc);
        return Err("MSI-X Function Mask bit not set");
    }
    if mc & (1 << 14) != 0 {
        caps::write_u16(dev, &cap, 2, saved_mc);
        return Err("MSI-X still enabled after clearing enable bit");
    }

    // Set Enable (bit 14), clear Function Mask (bit 15).
    caps::write_u16(dev, &cap, 2, (saved_mc & !(1 << 15)) | (1 << 14));
    let mc = caps::read_u16(dev, &cap, 2);
    if mc & (1 << 14) == 0 {
        caps::write_u16(dev, &cap, 2, saved_mc);
        return Err("MSI-X enable bit not set after write");
    }
    if mc & (1 << 15) != 0 {
        caps::write_u16(dev, &cap, 2, saved_mc);
        return Err("MSI-X Function Mask still set");
    }

    // Restore.
    caps::write_u16(dev, &cap, 2, saved_mc);
    Ok(())
}

fn test_msi_inactive_on_boot() -> Result<(), &'static str> {
    let dev = find_ahci().ok_or("no AHCI controller")?;
    let cap = caps::find(dev, CAP_MSI).ok_or("MSI cap not found")?;
    let mc = caps::read_u16(dev, &cap, 2);
    if mc & 1 != 0 { return Err("MSI already enabled at boot"); }
    Ok(())
}

fn test_msix_inactive_on_boot() -> Result<(), &'static str> {
    let dev = match find_msix_device() {
        Some(d) => d,
        None => return Err("SKIP"),
    };
    let cap = caps::find(dev, CAP_MSIX).ok_or("MSI-X cap lookup failed")?;
    let mc = caps::read_u16(dev, &cap, 2);
    if mc & (1 << 14) != 0 { return Err("MSI-X already enabled at boot"); }
    Ok(())
}

fn test_msix_table_size_sane() -> Result<(), &'static str> {
    let dev = match find_msix_device() {
        Some(d) => d,
        None => return Err("SKIP"),
    };
    let cap = caps::find(dev, CAP_MSIX).ok_or("MSI-X cap lookup failed")?;
    let info = pci::msix::table_info(dev, &cap);
    // ICH9 supports up to 6 ports; the table has at least 2 entries.
    if info.table_size < 1 { return Err("MSI-X table size < 1"); }
    if info.table_size > 8 { return Err("MSI-X table size implausible (>8)"); }
    Ok(())
}

fn test_msi_cap_enumeration_consistency() -> Result<(), &'static str> {
    let dev = find_ahci().ok_or("no AHCI controller")?;
    let all_caps = caps::all(dev);
    if !all_caps.iter().any(|c| c.id == CAP_MSI) { return Err("cap all() didn't list MSI"); }
    let msi_from_find = caps::find(dev, CAP_MSI).map(|c| c.offset);
    let msi_from_all = all_caps.iter().find(|c| c.id == CAP_MSI).map(|c| c.offset);
    if msi_from_find != msi_from_all { return Err("MSI cap offset mismatch"); }
    // MSI-X: verify consistency on any device that has it.
    if let Some(msix_dev) = find_msix_device() {
        let msix_find = caps::find(msix_dev, CAP_MSIX).ok_or("has() but find() failed")?;
        let msix_all_caps = caps::all(msix_dev);
        let msix_from_all = msix_all_caps.iter().find(|c| c.id == CAP_MSIX);
        if msix_from_all.map(|c| c.offset) != Some(msix_find.offset) {
            return Err("MSI-X cap offset mismatch");
        }
    }
    Ok(())
}

// ── Struct + Module impl ─────────────────────────────────────────

pub struct MsixTest;

impl Module for MsixTest {
    fn name(&self) -> &str { "msix_test" }

    fn version(&self) -> &str { "0.1.0" }

    fn init(&self, _display: &mut Framebuffer) -> Result<(), &'static str> {
        SerialPort::puts("[MSIXTEST] === MSI/MSI-X Test Suite ===\n");

        t!("find_msi_cap", test_find_msi_cap());
        t!("find_msix_cap", test_find_msix_cap());
        t!("msi_cap_info", test_msi_cap_info());
        t!("msix_table_info", test_msix_table_info());
        t!("no_msi_on_lpc", test_no_msi_on_lpc());
        t!("msi_enable_disable", test_msi_enable_disable());
        t!("msix_mc_register", test_msix_mc_register());
        t!("msi_inactive_boot", test_msi_inactive_on_boot());
        t!("msix_inactive_boot", test_msix_inactive_on_boot());
        t!("msix_table_size", test_msix_table_size_sane());
        t!("cap_enumeration", test_msi_cap_enumeration_consistency());

        let p = PASS.load(Ordering::Relaxed);
        let s = SKIP.load(Ordering::Relaxed);
        let f = FAIL.load(Ordering::Relaxed);
        let mut port = SerialPort::new();
        use core::fmt::Write;
        write!(port, "[MSIXTEST] done: {}/{} passed", p, p + f).ok();
        if s > 0 { write!(port, " ({} skipped)", s).ok(); }
        if f > 0 { write!(port, " ({} FAILED)", f).ok(); }
        write!(port, "\n").ok();

        if f > 0 { Err("MSI/MSI-X tests failed") } else { Ok(()) }
    }
}
