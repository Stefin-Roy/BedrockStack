//! Arch introspection — CPU features, paging, syscall MSRs.
//!
//! RO data lives under `/sys/arch` (pure snapshot), RW/control under `/kernel/arch`.
//! x86_64 only; riscv64 exposes minimal satp.

use alloc::sync::Arc;
use alloc::vec::Vec;
#[cfg(target_arch = "x86_64")]
use alloc::vec;
#[cfg(target_arch = "x86_64")]
use super::super::schema::{self, Field, Schema, Value};
#[cfg(target_arch = "riscv64")]
use super::super::schema::{self, Schema, Value};
use super::super::{Object, ObjectKind, UnispaceError};

pub fn register() -> Result<(), UnispaceError> {
    #[cfg(target_arch = "x86_64")]
    {
        crate::unispace::connect("/sys/arch/cpufeat", Arc::new(CpufeatObject))?;
        crate::unispace::connect("/sys/arch/paging", Arc::new(PagingObject))?;
        crate::unispace::connect("/kernel/arch/syscall", Arc::new(SyscallObject))?;
        crate::unispace::connect("/kernel/arch/gdt", Arc::new(GdtObject))?;
        crate::unispace::connect("/sys/arch/lapic", Arc::new(LapicObject))?;
    }
    #[cfg(target_arch = "riscv64")]
    {
        crate::unispace::connect("/sys/arch/satp", Arc::new(SatpObject))?;
    }
    Ok(())
}

#[cfg(target_arch = "x86_64")]
static CPUFEAT_SCHEMA: Schema = Schema::Struct(&[
    Field { name: "pku", ty: &schema::SCHEMA_BOOL },
    Field { name: "pge", ty: &schema::SCHEMA_BOOL },
    Field { name: "pcide", ty: &schema::SCHEMA_BOOL },
    Field { name: "invpcid", ty: &schema::SCHEMA_BOOL },
    Field { name: "xsave", ty: &schema::SCHEMA_BOOL },
    Field { name: "smap", ty: &schema::SCHEMA_BOOL },
]);

#[cfg(target_arch = "x86_64")]
struct CpufeatObject;
#[cfg(target_arch = "x86_64")]
impl Object for CpufeatObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &CPUFEAT_SCHEMA }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        // Use cpufeat detection if available, else best-effort via cpuid.
        let pku = detect_pku();
        let pge = detect_pge();
        let pcide = detect_pcide();
        let invpcid = detect_invpcid();
        let xsave = detect_xsave();
        let smap = detect_smap();
        let v = Value::Struct(vec![
            Value::Bool(pku),
            Value::Bool(pge),
            Value::Bool(pcide),
            Value::Bool(invpcid),
            Value::Bool(xsave),
            Value::Bool(smap),
        ]);
        schema::encode_value(&v, &CPUFEAT_SCHEMA, out)
    }
}

#[cfg(target_arch = "x86_64")]
fn detect_pku() -> bool {
    let max = core::arch::x86_64::__cpuid(0).eax;
    if max < 7 { return false; }
    let res = core::arch::x86_64::__cpuid_count(7, 0);
    (res.ecx & (1 << 3)) != 0
}
#[cfg(target_arch = "x86_64")]
fn detect_pge() -> bool {
    let res = core::arch::x86_64::__cpuid(1);
    (res.edx & (1 << 13)) != 0
}
#[cfg(target_arch = "x86_64")]
fn detect_pcide() -> bool {
    let max = core::arch::x86_64::__cpuid(0).eax;
    if max < 7 { return false; }
    let res = core::arch::x86_64::__cpuid_count(7, 0);
    (res.ebx & (1 << 17)) != 0
}
#[cfg(target_arch = "x86_64")]
fn detect_invpcid() -> bool {
    let max = core::arch::x86_64::__cpuid(0).eax;
    if max < 7 { return false; }
    let res = core::arch::x86_64::__cpuid_count(7, 0);
    (res.ebx & (1 << 10)) != 0
}
#[cfg(target_arch = "x86_64")]
fn detect_xsave() -> bool {
    let res = core::arch::x86_64::__cpuid(1);
    (res.ecx & (1 << 26)) != 0
}
#[cfg(target_arch = "x86_64")]
fn detect_smap() -> bool {
    let max = core::arch::x86_64::__cpuid(0).eax;
    if max < 7 { return false; }
    let res = core::arch::x86_64::__cpuid_count(7, 0);
    (res.ebx & (1 << 20)) != 0
}

#[cfg(target_arch = "x86_64")]
static PAGING_SCHEMA: Schema = Schema::Struct(&[
    Field { name: "kernel_vma_base", ty: &schema::SCHEMA_U64 },
    Field { name: "physmap_base", ty: &schema::SCHEMA_U64 },
    Field { name: "physmap_size", ty: &schema::SCHEMA_U64 },
    Field { name: "kaslr_offset", ty: &schema::SCHEMA_U64 },
    Field { name: "cr3", ty: &schema::SCHEMA_U64 },
]);

#[cfg(target_arch = "x86_64")]
struct PagingObject;
#[cfg(target_arch = "x86_64")]
impl Object for PagingObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &PAGING_SCHEMA }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let cr3: u64;
        unsafe { core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack)) };
        let v = Value::Struct(vec![
            Value::U64(crate::mm::layout::KERNEL_VMA_BASE),
            Value::U64(crate::mm::layout::PHYS_MAP_BASE),
            Value::U64(crate::mm::layout::physmap_end()),
            Value::U64(crate::mm::layout::kaslr_offset()),
            Value::U64(cr3),
        ]);
        schema::encode_value(&v, &PAGING_SCHEMA, out)
    }
}

#[cfg(target_arch = "x86_64")]
static SYSCALL_SCHEMA: Schema = Schema::Struct(&[
    Field { name: "lstar", ty: &schema::SCHEMA_U64 },
    Field { name: "cstar", ty: &schema::SCHEMA_U64 },
    Field { name: "fmask", ty: &schema::SCHEMA_U64 },
    Field { name: "star", ty: &schema::SCHEMA_U64 },
    Field { name: "efer", ty: &schema::SCHEMA_U64 },
]);

#[cfg(target_arch = "x86_64")]
struct SyscallObject;
#[cfg(target_arch = "x86_64")]
impl Object for SyscallObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &SYSCALL_SCHEMA }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let lstar = rdmsr(0xC0000082);
        let cstar = rdmsr(0xC0000083);
        let fmask = rdmsr(0xC0000084);
        let star = rdmsr(0xC0000081);
        let efer = rdmsr(0xC0000080);
        let v = Value::Struct(vec![Value::U64(lstar), Value::U64(cstar), Value::U64(fmask), Value::U64(star), Value::U64(efer)]);
        schema::encode_value(&v, &SYSCALL_SCHEMA, out)
    }
}

#[cfg(target_arch = "x86_64")]
fn rdmsr(msr: u32) -> u64 {
    let low: u32; let high: u32;
    unsafe { core::arch::asm!("rdmsr", in("ecx") msr, out("eax") low, out("edx") high, options(nomem, nostack)) };
    (low as u64) | ((high as u64) << 32)
}

#[cfg(target_arch = "x86_64")]
static GDT_SCHEMA: Schema = Schema::Struct(&[
    Field { name: "gdt_base", ty: &schema::SCHEMA_U64 },
    Field { name: "gdt_limit", ty: &schema::SCHEMA_U32 },
]);

#[cfg(target_arch = "x86_64")]
struct GdtObject;
#[cfg(target_arch = "x86_64")]
impl Object for GdtObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &GDT_SCHEMA }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let mut gdtr: [u8; 10] = [0; 10];
        unsafe { core::arch::asm!("sgdt [{}]", in(reg) gdtr.as_mut_ptr(), options(nostack)) };
        let limit = u16::from_le_bytes([gdtr[0], gdtr[1]]) as u32;
        let base = u64::from_le_bytes([gdtr[2], gdtr[3], gdtr[4], gdtr[5], gdtr[6], gdtr[7], gdtr[8], gdtr[9]]);
        let v = Value::Struct(vec![Value::U64(base), Value::U64(limit as u64)]);
        schema::encode_value(&v, &GDT_SCHEMA, out)
    }
}

#[cfg(target_arch = "x86_64")]
static LAPIC_SCHEMA: Schema = Schema::Struct(&[
    Field { name: "lapic_base", ty: &schema::SCHEMA_U64 },
    Field { name: "apic_id", ty: &schema::SCHEMA_U32 },
]);

#[cfg(target_arch = "x86_64")]
struct LapicObject;
#[cfg(target_arch = "x86_64")]
impl Object for LapicObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &LAPIC_SCHEMA }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let base = crate::platform::x86_64_pc::apic::lapic_base();
        let id = crate::platform::x86_64_pc::apic::read_full_apic_id();
        let v = Value::Struct(vec![Value::U64(base), Value::U64(id as u64)]);
        schema::encode_value(&v, &LAPIC_SCHEMA, out)
    }
}

#[cfg(target_arch = "riscv64")]
struct SatpObject;
#[cfg(target_arch = "riscv64")]
impl Object for SatpObject {
    fn kind(&self) -> ObjectKind { ObjectKind::Service }
    fn value_schema(&self) -> &'static Schema { &schema::SCHEMA_U64 }
    fn read_value(&self, out: &mut Vec<u8>, _max: usize) -> Result<(), UnispaceError> {
        let satp: u64;
        unsafe { core::arch::asm!("csrr {}, satp", out(reg) satp) };
        schema::encode_value(&Value::U64(satp), &schema::SCHEMA_U64, out)
    }
}
