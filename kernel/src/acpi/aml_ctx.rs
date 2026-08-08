//! AML interpreter bootstrap: parse the DSDT + all SSDTs into a persistent
//! `aml::AmlContext` and decode `\_S5` from it.
//!
//! Only table parsing and the `\_S5` evaluation happen at boot. The device
//! `_INI` sweep (`initialize_objects`) is part of the boot path; the vendored
//! `aml` crate carries loop-progress hardening so it can no longer hang on the
//! old QEMU q35 DSDT.

use alloc::boxed::Box;
use ::aml::{value::Args, AmlContext, AmlError, AmlName, AmlValue, DebugVerbosity};

use super::handler::AmlHandler;
use super::tables::SdtEntry;

fn sig(s: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*s)
}

/// Raw bytes of a mapped SDT (header + body), sliced past the 36-byte SDT
/// header so the AML parser sees a bare term list. The mapping lives for the
/// whole kernel lifetime, so this is exposed as `'static`.
fn table_body(e: &SdtEntry) -> &'static [u8] {
    let len = e.length as usize;
    let raw = unsafe { core::slice::from_raw_parts(e.vaddr as *const u8, len) };
    &raw[36..len]
}

/// Create a persistent AML interpreter from the DSDT (via the RSDT/XSDT walk,
/// or the FADT pointer as fallback) and every SSDT. A DSDT parse failure is
/// fatal to the interpreter (the caller logs it loudly); a broken SSDT is
/// logged and skipped so one bad table does not kill the namespace.
pub fn init_aml_ctx(entries: &[SdtEntry], dsdt_fallback: u64) -> Result<AmlContext, AmlError> {
    let mut ctx = AmlContext::new(Box::new(AmlHandler), DebugVerbosity::None);

    if let Some(e) = entries.iter().find(|e| e.signature == sig(b"DSDT")) {
        ctx.parse_table(table_body(e))?;
    } else if dsdt_fallback != 0 {
        let raw = super::tables::map_sdt_bytes(dsdt_fallback, b"DSDT")
            .ok_or(AmlError::MalformedStream)?;
        ctx.parse_table(&raw[36..])?;
    } else {
        return Err(AmlError::MalformedStream);
    }

    for e in entries.iter().filter(|e| e.signature == sig(b"SSDT")) {
        if let Err(err) = ctx.parse_table(table_body(e)) {
            log::error!("ACPI: SSDT at 0x{:x} failed to parse: {:?}", e.phys_addr, err);
        }
    }
    Ok(ctx)
}

/// Decode the `\_S5` soft-off package and return SLP_TYPa (masked to the 3-bit
/// field). Returns `None` when `\_S5` is absent or not statically decodable.
pub fn s5_slp_typa(ctx: &mut AmlContext) -> Option<u8> {
    let path = AmlName::from_str("\\_S5").ok()?;
    let value = ctx.invoke_method(&path, Args::EMPTY).ok()?;
    let slp_typa = match value {
        AmlValue::Package(pkg) => match pkg.first() {
            Some(AmlValue::Integer(v)) => *v,
            _ => return None,
        },
        AmlValue::Integer(v) => v,
        _ => return None,
    };
    Some((slp_typa & 0x7) as u8)
}
