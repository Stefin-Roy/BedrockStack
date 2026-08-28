//! VT-d unit bringup — register file, root/context/SLPT, fault handling.
//!
//! One `VtDUnit` per DRHD. Each DRHD gets its own root table and own DID
//! (per-DRHD isolation). A shared SLPT could be used, but for true isolation
//! each unit would need its own SLPT; for now we keep a single global domain
//! SLPT and differentiate by DID (IOTLB tagging). All IOVA→phys mappings are
//! broadcast to every enabled unit's invalidation.

use alloc::vec::Vec;
use core::sync::atomic::{Ordering, fence};
use spin::{Once, Mutex};

use crate::acpi::{DmarInfo, Drhd};
use crate::drivers::serial::SerialPort;
use crate::mm::phys_alloc::BitmapAllocator;

use super::qi::{self, QiState};
use super::slpt::{self, Agaw};

// ── Register offsets ───────────────────────────────────────────────
const REG_VER: u64 = 0x00;
const REG_CAP: u64 = 0x08;
const REG_ECAP: u64 = 0x10;
const REG_GCMD: u64 = 0x18;
const REG_GSTS: u64 = 0x1C;
const REG_RTADDR: u64 = 0x20;
const REG_FSTS: u64 = 0x34;
const REG_FECTL: u64 = 0x38;
const REG_FEDATA: u64 = 0x3C;
const REG_FEADDR: u64 = 0x40;
const REG_FEUADDR: u64 = 0x44;

// GCMD/GSTS bits
const GCMD_TE: u32 = 1 << 31;
const GCMD_SRTP: u32 = 1 << 30;
const GSTS_TES: u32 = 1 << 31;
const GSTS_RTPS: u32 = 1 << 30;
const GSTS_QIES: u32 = 1 << 26;

// CAP bits
const CAP_SAGAW_SHIFT: u64 = 8;
const CAP_SAGAW_MASK: u64 = 0x1F << 8;
const CAP_MGAW_SHIFT: u64 = 16;
const CAP_MGAW_MASK: u64 = 0x3F << 16;
const CAP_FRO_SHIFT: u64 = 24;
const CAP_FRO_MASK: u64 = 0x3FF << 24;
const CAP_NFR_SHIFT: u64 = 40;
const CAP_NFR_MASK: u64 = 0xFF << 40;

// ECAP bits (VT-d 11.4.3). Note: bit0 = C (Page-walk Coherency), bit7 = SC (Snoop Control).
// Earlier code mis-labelled C as bit7; fix: keep ECAP_C at bit0 and add SC.
const ECAP_C: u64 = 1 << 0;
const ECAP_SC: u64 = 1 << 7;

// FSTS bits — PPF is RO (derived from FRCD.F), only PFO/IQE (and AFO/ICE/ITE) are RW1C (§10.4.5).
const FSTS_PPF: u32 = 1 << 1;
const FSTS_PFO: u32 = 1 << 0;
const FSTS_IQE: u32 = 1 << 4;
// Detection includes PPF; clearing must exclude PPF (RO).
const FSTS_ALL_FAULT: u32 = FSTS_PPF | FSTS_PFO | FSTS_IQE;
const FSTS_W1C_MASK: u32 = FSTS_PFO | FSTS_IQE;

// VT-d Fault Reason decode (Table 30 DMA + Table 15 IR). Only DMA 01h-0Eh and 20h-2Bh named; unknown -> hex.
fn decode_fault_reason(fr: u8) -> &'static str {
    match fr {
        0x01 => "LRT.2 root not present",
        0x02 => "LCT.2 ctx not present",
        0x03 => "LCT.4 AW/TT/SSPTPTR invalid",
        0x04 => "LGN.1 addr overflow (MGAW/AGAW/HAW)",
        0x05 => "LGN.2 write perm fail",
        0x06 => "LGN.3 read perm fail",
        0x07 => "LSS.1 2nd-stage entry error",
        0x08 => "LRT.1 root entry error",
        0x09 => "LCT.1 ctx entry error",
        0x0A => "LRT.3 root reserved bit",
        0x0B => "LCT.3 ctx reserved bit",
        0x0C => "LSS.2 2nd-stage reserved bit",
        0x0D => "LCT.5 TT blocked",
        0x0E => "LGN.4 intr range blocked",
        0x20 => "IR 20h reserved field in remappable req",
        0x21 => "IR 21h intr index over limit",
        0x22 => "IR 22h IRTE not present",
        0x23 => "IR 23h IRTA access error",
        0x24 => "IR 24h IRTE reserved bit",
        0x25 => "IR 25h compatibility format blocked",
        0x26 => "IR 26h SID verify fail",
        0x27 => "IR 27h PID access error",
        0x28 => "IR 28h PID reserved bit",
        0x29 => "IR 29h invalid MSI in intr range",
        0x2A => "IR 2Ah EIME=0 required",
        0x2B => "IR 2Bh IR disabled but MSI",
        _ => "unknown",
    }
}

// Context entry fields (128-bit, two u64)
// LO: bits 0 = present, 1 = FPD, 2:3=TT, bits 12-63 = SLPT ptr
// HI: bits 0-2 = AW, bits 8-23 = DID
fn ctx_lo(present: bool, slpt_phys: u64) -> u64 {
    let mut v = 0u64;
    if present {
        v |= 1;
    }
    // TT=00 legacy second-stage only.
    v |= slpt_phys & !0xFFF;
    v
}
fn ctx_hi(agaw: Agaw, did: u16) -> u64 {
    let aw = agaw.context_aw_field();
    (aw & 0x7) | ((did as u64) << 8)
}

#[inline]
unsafe fn read32(base: u64, off: u64) -> u32 {
    unsafe { core::ptr::read_volatile((base + off) as *const u32) }
}
#[inline]
unsafe fn write32(base: u64, off: u64, val: u32) {
    unsafe { core::ptr::write_volatile((base + off) as *mut u32, val) }
}
#[inline]
unsafe fn read64(base: u64, off: u64) -> u64 {
    unsafe { core::ptr::read_volatile((base + off) as *const u64) }
}
#[inline]
unsafe fn write64(base: u64, off: u64, val: u64) {
    unsafe { core::ptr::write_volatile((base + off) as *mut u64, val) }
}

pub struct VtDUnit {
    pub segment: u16,
    pub reg_base_phys: u64,
    pub reg_va: u64,
    pub ver: u32,
    pub cap: u64,
    pub ecap: u64,
    pub gcmd: u32,
    pub agaw: Agaw,
    pub sagaw: u64, // raw SAGAW bits 12:8 for global intersection
    pub mgaw: u8,
    pub haw: u8,
    pub fro: u64,
    pub nfr: u8,
    pub root_phys: u64,
    pub qi: QiState,
    pub did: u16,
    pub enabled: bool,
    pub include_pci_all: bool,
    pub sc: bool, // ECAP.SC snoop-control
    pub coherency: bool, // ECAP.C page-walk coherency
}

impl VtDUnit {
    pub fn new(drhd: &Drhd, reg_va: u64, did: u16, haw: u8) -> Self {
        let cap = unsafe { read64(reg_va, REG_CAP) };
        let ecap = unsafe { read64(reg_va, REG_ECAP) };
        let ver = unsafe { read32(reg_va, REG_VER) };
        let mgaw = ((cap & CAP_MGAW_MASK) >> CAP_MGAW_SHIFT) as u8 + 1;
        let fro = ((cap & CAP_FRO_MASK) >> CAP_FRO_SHIFT) * 16;
        let nfr = (((cap & CAP_NFR_MASK) >> CAP_NFR_SHIFT) as u8) + 1;
        let sagaw = (cap & CAP_SAGAW_MASK) >> CAP_SAGAW_SHIFT;
        // Choose AGAW purely from SAGAW (spec 11.4.2). MGAW/HAW limit effective
        // IOVA width (capped in Domain::new) but do not gate AW code support.
        // QEMU and bare-metal report SAGAW=48-only with MGAW=HAW=39; gating on
        // mgaw>=48 would incorrectly force 39 and cause FR=03h (AW not supported).
        let supports_48 = (sagaw & (1 << 2)) != 0;
        let supports_39 = (sagaw & (1 << 1)) != 0;
        let agaw = if supports_48 {
            Agaw::Level4
        } else if supports_39 {
            Agaw::Level3
        } else if sagaw == 0 {
            // No SAGAW (SSTS=0 or broken firmware): best-effort fallback using MGAW/HAW.
            if mgaw >= 48 && haw >= 48 { Agaw::Level4 } else { Agaw::Level3 }
        } else {
            // SAGAW reports only 30-bit etc. not handled (Levels 5): fallback to 39 and let global intersection abort.
            Agaw::Level3
        };
        let qi = QiState::new(reg_va, ecap);
        let sc = (ecap & ECAP_SC) != 0;
        let coherency = (ecap & ECAP_C) != 0;
        SerialPort::puts("[iommu] DRHD seg=");
        SerialPort::put_hex(drhd.segment as u64);
        SerialPort::puts(" reg_phys=");
        SerialPort::put_hex(drhd.register_base);
        SerialPort::puts(" reg_va=");
        SerialPort::put_hex(reg_va);
        SerialPort::puts(" ver=");
        SerialPort::put_hex(ver as u64);
        SerialPort::puts(" cap=");
        SerialPort::put_hex(cap);
        SerialPort::puts(" ecap=");
        SerialPort::put_hex(ecap);
        SerialPort::puts(" sagaw=");
        SerialPort::put_hex(sagaw);
        SerialPort::puts(" (39=");
        SerialPort::put_u64(if supports_39 {1} else {0});
        SerialPort::puts(" 48=");
        SerialPort::put_u64(if supports_48 {1} else {0});
        SerialPort::puts(") sc=");
        SerialPort::put_u64(sc as u64);
        SerialPort::puts(" c=");
        SerialPort::put_u64(coherency as u64);
        SerialPort::puts(" mgaw=");
        SerialPort::put_u64(mgaw as u64);
        SerialPort::puts(" haw=");
        SerialPort::put_u64(haw as u64);
        SerialPort::puts(" agaw=");
        SerialPort::put_u64(match agaw { Agaw::Level3 => 39, Agaw::Level4 => 48 });
        SerialPort::puts(" nfr=");
        SerialPort::put_u64(nfr as u64);
        SerialPort::puts(" did=");
        SerialPort::put_u64(did as u64);
        SerialPort::puts("\n");

        VtDUnit {
            segment: drhd.segment,
            reg_base_phys: drhd.register_base,
            reg_va,
            ver,
            cap,
            ecap,
            gcmd: 0,
            agaw,
            sagaw,
            mgaw,
            haw,
            fro,
            nfr,
            root_phys: 0,
            qi,
            did,
            enabled: false,
            include_pci_all: drhd.include_pci_all,
            sc,
            coherency,
        }
    }

    /// Allocate root + context tables for this unit.
    /// If `include_pci_all` the root is populated for all 256 buses,
    /// otherwise only for buses enumerated in `drhd` DeviceScope.
    pub fn alloc_tables(
        &mut self,
        drhd: &Drhd,
        alloc: &mut BitmapAllocator,
        slpt_root_phys: u64,
    ) -> Result<(), &'static str> {
        let root_phys = alloc.alloc().ok_or("IOMMU root OOM")?;
        let root_va = crate::mm::layout::to_physmap(root_phys);
        unsafe { core::ptr::write_bytes(root_va as *mut u8, 0, 4096) };
        self.root_phys = root_phys;

        // For global scope we alias a single context table across all buses to save memory.
        // For scoped units we allocate per-bus tables only for buses in scope.
        // Collect set of buses to program.
        let mut buses_to_program: [bool; 256] = [false; 256];
        if drhd.include_pci_all {
            for b in 0..256 { buses_to_program[b] = true; }
        } else {
            // Enumerate DeviceScope: Type 01 endpoint @ start_bus, Type 02 sub-hierarchy @ start_bus
            // For simplicity we include start_bus for each entry. Full sub-hierarchy decode
            // (secondary bus traversal) not needed for QEMU which uses include_all.
            for ds in &drhd.devices {
                buses_to_program[ds.start_bus_number as usize] = true;
                // Also handle path depth >1: include intermediate buses? include path hops?
                // Path is pairs (dev,func) starting at start_bus; we don't know secondary buses,
                // so conservatively also include start_bus+1 if path len>2? Skip for now.
                if ds.path.len() > 1 {
                    // Best-effort: also program bus derived from first hop if enumerable
                    // (no secondary bus register access at boot, so skip)
                }
            }
            // If no buses enumerated (firmware bug), still program bus 0 to avoid dead unit
            let any = buses_to_program.iter().any(|&v| v);
            if !any {
                SerialPort::puts("[iommu] DRHD non-include_all with 0 scopes, mapping bus0 fallback\n");
                buses_to_program[0] = true;
            }
        }

        // Allocate one shared context table for all programmed buses (alias). For true per-bus isolation
        // we'd allocate distinct pages, but aliasing keeps DID shared and saves memory while still
        // respecting bus scoping via root presence.
        let ctx_phys = alloc.alloc().ok_or("IOMMU ctx OOM")?;
        let ctx_va = crate::mm::layout::to_physmap(ctx_phys);
        unsafe { core::ptr::write_bytes(ctx_va as *mut u8, 0, 4096) };

        // Fill context entries: per-DRHD DID, present for all devfn.
        let did = self.did;
        // DID 0 is reserved when CM=1 (spec §9.3); we start at 1, so ensure non-zero.
        let effective_did = if did == 0 { 1 } else { did };
        self.did = effective_did;
        for devfn in 0..256 {
            let lo = ctx_lo(true, slpt_root_phys);
            let hi = ctx_hi(self.agaw, effective_did);
            unsafe {
                let ptr = ctx_va as *mut u64;
                *ptr.add(devfn * 2) = lo;
                *ptr.add(devfn * 2 + 1) = hi;
            }
        }
        // Install root entries
        for bus in 0..256 {
            if !buses_to_program[bus] {
                continue;
            }
            let re_off = bus * 16;
            unsafe {
                let rptr = root_va as *mut u64;
                let idx = (re_off / 8) as usize;
                *rptr.add(idx) = (ctx_phys & !0xFFF) | 1;
                *rptr.add(idx + 1) = 0;
            }
        }
        fence(Ordering::SeqCst);
        SerialPort::puts("[iommu] alloc_tables did=");
        SerialPort::put_u64(effective_did as u64);
        SerialPort::puts(" include_all=");
        SerialPort::put_u64(if drhd.include_pci_all {1} else {0});
        SerialPort::puts(" buses prog=");
        let cnt = buses_to_program.iter().filter(|&&v| v).count();
        SerialPort::put_u64(cnt as u64);
        SerialPort::puts("\n");
        Ok(())
    }

    pub fn set_root(&mut self) -> bool {
        let rtaddr_val = (self.root_phys & !0xFFF) | 0; // TTM: 00 legacy
        unsafe {
            write64(self.reg_va, REG_RTADDR, rtaddr_val);
            fence(Ordering::SeqCst);
            // GCMD is write-only; read GSTS to preserve active controls (e.g. QIE) per §10.4.4.
            let mut gcmd = read32(self.reg_va, REG_GSTS);
            gcmd |= GCMD_SRTP;
            write32(self.reg_va, REG_GCMD, gcmd);
        }
        let mut polls = 0u32;
        while polls < 1_000_000 {
            let gsts = unsafe { read32(self.reg_va, REG_GSTS) };
            if gsts & GSTS_RTPS != 0 {
                SerialPort::puts("[iommu] RTPS set did=");
                SerialPort::put_u64(self.did as u64);
                SerialPort::puts("\n");
                return true;
            }
            core::hint::spin_loop();
            polls += 1;
        }
        SerialPort::puts("[iommu] SRTP timeout did=");
        SerialPort::put_u64(self.did as u64);
        SerialPort::puts("\n");
        false
    }

    pub fn enable_qi(&mut self, alloc: &mut BitmapAllocator) -> bool {
        if self.qi.has_qi {
            return qi::init_qi(&mut self.qi, alloc);
        }
        false
    }

    pub fn enable_translation(&mut self) -> bool {
        unsafe {
            // GCMD write-only: derive new value from GSTS per §10.4.4 to avoid clearing QIE etc.
            let mut gcmd = read32(self.reg_va, REG_GSTS);
            gcmd |= GCMD_TE;
            write32(self.reg_va, REG_GCMD, gcmd);
            fence(Ordering::SeqCst);
        }
        let mut polls = 0u32;
        while polls < 1_000_000 {
            let gsts = unsafe { read32(self.reg_va, REG_GSTS) };
            if gsts & GSTS_TES != 0 {
                SerialPort::puts("[iommu] TE enabled did=");
                SerialPort::put_u64(self.did as u64);
                SerialPort::puts("\n");
                self.enabled = true;
                return true;
            }
            core::hint::spin_loop();
            polls += 1;
        }
        SerialPort::puts("[iommu] TE timeout did=");
        SerialPort::put_u64(self.did as u64);
        SerialPort::puts("\n");
        false
    }

    #[allow(dead_code)]
    pub fn disable_translation(&mut self) {
        unsafe {
            // GCMD write-only: read GSTS to preserve other bits (§10.4.4).
            let mut gcmd = read32(self.reg_va, REG_GSTS);
            gcmd &= !GCMD_TE;
            write32(self.reg_va, REG_GCMD, gcmd);
        }
        let mut polls = 0u32;
        while polls < 1_000_000 {
            let gsts = unsafe { read32(self.reg_va, REG_GSTS) };
            if gsts & GSTS_TES == 0 {
                break;
            }
            core::hint::spin_loop();
            polls += 1;
        }
        self.enabled = false;
    }

    /// Global context+iotlb invalidate after map.
    /// Spec §6.5.1: when QIES=1 register-based CCMD/IOTLB invalidation is illegal — must use QI only.
    pub fn invalidate_all(&mut self) -> bool {
        // Check if QI is actually enabled (GSTS.QIES). has_qi+queue_size only means capability, not enabled.
        let qies = unsafe { read32(self.reg_va, REG_GSTS) } & GSTS_QIES != 0;
        let use_qi = self.qi.has_qi && self.qi.queue_size > 0 && qies;
        let ok = if use_qi {
            // QI path: CC (global) → IOTLB (global) → WAIT (fence+SW) for completion ordering (§6.5.2.12, §6.5.4).
            // Never fallback to CCMD when QIES=1 — that is architecturally illegal and QEMU reports
            // "Queued Invalidation enabled, should not use register-based invalidation".
            let a = qi::qi_invalidate_context(&mut self.qi);
            if !a {
                SerialPort::puts("[iommu] QI CC failed did=");
                SerialPort::put_u64(self.did as u64);
                SerialPort::puts(" (no CCMD fallback when QIES=1)\n");
                // FSTS.IQE already cleared inside qi_submit; just report failure.
                false
            } else {
                let b = qi::qi_invalidate_iotlb(&mut self.qi, self.did);
                if !b {
                    SerialPort::puts("[iommu] QI IOTLB failed did=");
                    SerialPort::put_u64(self.did as u64);
                    SerialPort::puts("\n");
                    false
                } else {
                    // Wait ensures prior CC+IOTLB are globally observable before DMA (§6.5.2.9, §6.5.2.12).
                    // Also satisfies QI disable quiesce: head==tail && last_type==5.
                    let w = qi::qi_invalidate_wait(&mut self.qi);
                    if !w {
                        SerialPort::puts("[iommu] QI WAIT failed did=");
                        SerialPort::put_u64(self.did as u64);
                        SerialPort::puts("\n");
                        false
                    } else {
                        true
                    }
                }
            }
        } else {
            // Register-based fallback only when QI not enabled or not supported.
            qi::reg_invalidate_context_global(self.reg_va) && qi::reg_invalidate_iotlb_global(self.reg_va)
        };
        if !ok {
            SerialPort::puts("[iommu] invalidate failed did=");
            SerialPort::put_u64(self.did as u64);
            SerialPort::puts("\n");
        }
        ok
    }

    /// Fault status handling — drain FRCD registers. W1C.
    pub fn drain_faults(&self) {
        let fsts = unsafe { read32(self.reg_va, REG_FSTS) };
        if fsts & FSTS_ALL_FAULT == 0 {
            return;
        }
        SerialPort::puts("[iommu] fault FSTS=");
        SerialPort::put_hex(fsts as u64);
        SerialPort::puts(" did=");
        SerialPort::put_u64(self.did as u64);
        SerialPort::puts(" fro=");
        SerialPort::put_hex(self.fro);
        SerialPort::puts(" nfr=");
        SerialPort::put_u64(self.nfr as u64);
        SerialPort::puts("\n");
        // FRCD: each 16B, low qword (rec_off+0) holds fault info/address [63:12]; high qword (rec_off+8) bit 63 (=bit127) is F (RW1C).
        for i in 0..(self.nfr as u64) {
            let rec_off = self.fro + i * 16;
            let low = unsafe { read64(self.reg_va, rec_off) };
            let high = unsafe { read64(self.reg_va, rec_off + 8) };
            let fault = (high >> 63) & 1;
            if fault == 0 {
                continue;
            }
            SerialPort::puts("  FRCD[");
            SerialPort::put_u64(i);
            SerialPort::puts("]= lo=");
            SerialPort::put_hex(low);
            SerialPort::puts(" hi=");
            SerialPort::put_hex(high);
            SerialPort::puts("\n");
            // Decode fields per spec Fig 11-15 (11.4.7.6)
            let fr = ((high >> 32) & 0xFF) as u8;
            let sid = (high & 0xFFFF) as u16;
            let fi = low & !0xFFFu64;
            let t1 = (high >> 62) & 1;
            let t2 = (high >> 28) & 1;
            let at = (high >> 60) & 3;
            let sid_bus = ((sid >> 8) & 0xFF) as u8;
            let sid_dev = ((sid >> 3) & 0x1F) as u8;
            let sid_func = (sid & 0x7) as u8;
            SerialPort::puts("    decoded: SID=");
            SerialPort::put_hex(sid as u64);
            SerialPort::puts(" (");
            SerialPort::put_u64(sid_bus as u64);
            SerialPort::puts(":");
            SerialPort::put_u64(sid_dev as u64);
            SerialPort::puts(".");
            SerialPort::put_u64(sid_func as u64);
            SerialPort::puts(") FR=");
            SerialPort::put_hex(fr as u64);
            SerialPort::puts(" ");
            SerialPort::puts(decode_fault_reason(fr));
            SerialPort::puts(" FI=");
            SerialPort::put_hex(fi);
            SerialPort::puts(" AT=");
            SerialPort::put_u64(at);
            SerialPort::puts(" T=");
            SerialPort::put_u64(t1);
            SerialPort::put_u64(t2);
            SerialPort::puts("\n");
            // Clear F by writing 1 to bit 63 of the upper qword (bit 127 overall); F is RW1C.
            unsafe {
                // Spec §10.4.8 (§11.4.8): FRCD.F at bit 127 is RW1C
                write64(self.reg_va, rec_off + 8, high | (1u64 << 63));
                fence(Ordering::SeqCst);
            }
        }
        // Clear FSTS W1C bits that are set. PPF is RO (derived from FRCD.F) — do not attempt to clear by writing 1.
        let to_clear = fsts & FSTS_W1C_MASK;
        unsafe {
            if to_clear != 0 {
                write32(self.reg_va, REG_FSTS, to_clear);
                fence(Ordering::SeqCst);
            }
        }
    }

    /// Mask/unmask fault interrupt via FECTL bit 31 IM.
    pub fn set_fault_intr(&self, enable: bool) {
        let mut fectl = unsafe { read32(self.reg_va, REG_FECTL) };
        if enable {
            fectl &= !(1 << 31);
        } else {
            fectl |= 1 << 31;
        }
        unsafe { write32(self.reg_va, REG_FECTL, fectl) };
    }

    /// Program fault interrupt as MSI to LAPIC. Handles xAPIC and x2APIC.
    pub fn program_fault_msi(&self, vector: u8, dest_apic: u32) {
        let fe_data = vector as u32; // delivery fixed, vector
        // Standard MSI address is 0xFEE00000 | dest[7:0]<<12 (xAPIC). Upper 32 bits (FEUADDR) must be 0 for
        // non-remapped MSI (base is 0x00000000_FEE00000). x2APIC destinations >255 require VT-d IR, not FEUADDR.
        let fe_addr = 0xFEE00000u32 | ((dest_apic & 0xFF) << 12);
        unsafe {
            write32(self.reg_va, REG_FEDATA, fe_data);
            write32(self.reg_va, REG_FEADDR, fe_addr);
            write32(self.reg_va, REG_FEUADDR, 0);
        }
        SerialPort::puts("[iommu] fault MSI vec=");
        SerialPort::put_u64(vector as u64);
        SerialPort::puts(" dest=");
        SerialPort::put_u64(dest_apic as u64);
        SerialPort::puts(" did=");
        SerialPort::put_u64(self.did as u64);
        SerialPort::puts("\n");
    }
}

// ── Global IOMMU domain state ──────────────────────────────────

pub struct Domain {
    pub slpt_root_phys: u64,
    pub agaw: Agaw,
    pub haw: u8,
    pub next_iova: u64,
    pub iova_limit: u64,
    pub allow_snp: bool, // ECAP.SC global
}

impl Domain {
    pub fn new(agaw: Agaw, slpt_root_phys: u64, mgaw: u8, haw: u8, allow_snp: bool) -> Self {
        let bits = match agaw { Agaw::Level3 => 39, Agaw::Level4 => 48 };
        // IOVA limit is AGAW, capped by both HAW and MGAW (spec §3.7: min(MGAW,AGAW,HAW)).
        // For QEMU/bare-metal SAGAW=48, MGAW=HAW=39 => limit 512G, still holds FB/RMRR below 4G.
        let mut limit = if bits == 39 { 1u64 << 39 } else { 1u64 << 48 };
        if (haw as u64) < bits {
            limit = core::cmp::min(limit, 1u64 << (haw as u64));
        }
        if (mgaw as u64) < bits {
            limit = core::cmp::min(limit, 1u64 << (mgaw as u64));
        }
        Domain {
            slpt_root_phys,
            agaw,
            haw,
            next_iova: 0x1000,
            iova_limit: limit,
            allow_snp,
        }
    }

    pub fn alloc_iova(&mut self, size: u64, rmrrs: &[crate::acpi::Rmrr]) -> Option<u64> {
        self.alloc_iova_with_limit(size, self.iova_limit, rmrrs)
    }

    /// Allocate an IOVA strictly below `limit` (exclusive), for 32-bit-only
    /// devices.  Uses the same bump+RMRR-skip logic as [`Self::alloc_iova`]
    /// but caps the window at `limit` — if the bump cursor has already
    /// advanced past `limit` there is no low IOVA left to give out.
    pub fn alloc_iova_below(
        &mut self,
        size: u64,
        limit: u64,
        rmrrs: &[crate::acpi::Rmrr],
    ) -> Option<u64> {
        if limit > self.iova_limit {
            return None;
        }
        self.alloc_iova_with_limit(size, limit, rmrrs)
    }

    fn alloc_iova_with_limit(
        &mut self,
        size: u64,
        limit: u64,
        rmrrs: &[crate::acpi::Rmrr],
    ) -> Option<u64> {
        if size == 0 {
            return None;
        }
        let pages = size.checked_add(0xFFF)? & !0xFFF;
        let mut candidate = self.next_iova.checked_add(0xFFF)? & !0xFFF;
        let max_attempts = 64;
        let mut attempts = 0;
        while attempts < max_attempts {
            if candidate + pages > limit || candidate + pages < candidate {
                return None;
            }
            let mut overlaps = false;
            for r in rmrrs {
                // Page-align RMRR identically to init's identity-map rounding (base down, limit up).
                let r_start = r.base_address & !0xFFF;
                let Some(r_end) = (r.limit_address | 0xFFF).checked_add(1) else {
                    return None;
                };
                let Some(candidate_end) = candidate.checked_add(pages) else {
                    return None;
                };
                if candidate < r_end && r_start < candidate_end {
                    candidate = r_end.checked_add(0xFFF).map(|v| v & !0xFFF)?;
                    overlaps = true;
                    break;
                }
            }
            if !overlaps {
                if candidate == 0 {
                    candidate = 0x1000;
                    attempts += 1;
                    continue;
                }
                self.next_iova = candidate + pages;
                return Some(candidate);
            }
            attempts += 1;
        }
        None
    }
}

pub struct IommuState {
    pub units: Vec<VtDUnit>,
    pub domain: Domain,
    pub rmrrs: Vec<crate::acpi::Rmrr>,
    pub lock: Mutex<()>,
}

static IOMMU_STATE: Once<Mutex<IommuState>> = Once::new();
static IOMMU_ENABLED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn is_enabled() -> bool {
    IOMMU_ENABLED.load(Ordering::Acquire)
}

/// Initialize IOMMU after ACPI tables and before DMA services.
/// Returns true if at least one unit successfully enabled TE.
/// `fb_range` is `(phys_base, size_bytes)` for the GOP scanout buffer; when
/// `Some` it is identity-mapped before TE so the iGPU display engine can
/// continue DMA-reading `fb_phys` as IOVA. `None`/zero skips FB mapping.
/// Gated by `noiommu` at callsite (`services/x86_64`); this also bails if
/// `noiommu` is set. Verification of FB/RMRR identity is additionally gated
/// behind `iommu_verify` (`bootargs::is_iommu_verify`) — failure aborts to
/// fallback noiommu (returns false, caller uses unprotected DMA).
pub fn init(
    dmar: &DmarInfo,
    _root: u64,
    alloc_ptr: *mut BitmapAllocator,
    fb_range: Option<(u64, u64)>,
) -> bool {
    // `noiommu` is opt-out master gate — even direct calls must respect it.
    if crate::bootargs::is_noiommu() {
        SerialPort::puts("[iommu] noiommu set — skip init\n");
        return false;
    }
    if dmar.drhds.is_empty() {
        SerialPort::puts("[iommu] no DRHD\n");
        return false;
    }
    let alloc = unsafe { &mut *alloc_ptr };
    let haw: u8 = if dmar.host_address_width == 0 || dmar.host_address_width > 64 {
        // QEMU may report 0; default to 39 (512G) — matches default mgaw
        39
    } else {
        dmar.host_address_width + 1
    };

    let mut units: Vec<VtDUnit> = Vec::new();
    for (idx, drhd) in dmar.drhds.iter().enumerate() {
        let reg_phys = drhd.register_base;
        // Map 32 KiB, not one page: the FRCD fault-record array sits at
        // CAP.FRO*16 (10-bit offset) and legally extends past 4 KiB on units
        // with large queues. drain_faults indexes it through this single
        // mapping, so size for the architectural maximum
        // (1023*16 + 256*16 < 0x8000).
        let reg_va = match crate::acpi::try_map_device_mmio(
            reg_phys & !0xFFF,
            0x8000,
            crate::mm::vmm::PageFlags::READ
                | crate::mm::vmm::PageFlags::WRITE
                | crate::mm::vmm::PageFlags::NO_CACHE,
        ) {
            Ok(v) => v + (reg_phys & 0xFFF),
            Err(e) => {
                SerialPort::puts("[iommu] map reg failed ");
                SerialPort::put_hex(reg_phys);
                SerialPort::puts(" err=");
                SerialPort::puts(e);
                SerialPort::puts("\n");
                continue;
            }
        };
        // DID: start at 1 (0 reserved when CM=1). Per-DRHD isolation.
        let did = (idx as u16).wrapping_add(1);
        let unit = VtDUnit::new(drhd, reg_va, did, haw);
        units.push(unit);
    }
    if units.is_empty() {
        SerialPort::puts("[iommu] no units mapped\n");
        return false;
    }
    // Decide domain AGAW as intersection of SAGAW across units (highest common).
    // MGAW/HAW limit effective IOVA width via Domain::new (min cap), not AW code.
    let mut common_sagaw = units[0].sagaw;
    for u in &units[1..] {
        common_sagaw &= u.sagaw;
    }
    let global_agaw = if (common_sagaw & (1 << 2)) != 0 {
        Agaw::Level4
    } else if (common_sagaw & (1 << 1)) != 0 {
        Agaw::Level3
    } else if common_sagaw == 0 {
        // No common SAGAW (e.g., only 30-bit or mismatched): try best-effort fallback using per-unit agaw intersection
        let mut fallback = units[0].agaw;
        for u in &units[1..] {
            fallback = match (fallback, u.agaw) {
                (Agaw::Level3, _) | (_, Agaw::Level3) => Agaw::Level3,
                _ => Agaw::Level4,
            };
        }
        SerialPort::puts("[iommu] WARN: no common SAGAW (common=");
        SerialPort::put_hex(common_sagaw);
        SerialPort::puts(") fallback agaw=");
        SerialPort::put_u64(match fallback { Agaw::Level3=>39, Agaw::Level4=>48 });
        SerialPort::puts("\n");
        fallback
    } else {
        SerialPort::puts("[iommu] no common AGAW (common_sagaw=");
        SerialPort::put_hex(common_sagaw);
        SerialPort::puts(") => abort IOMMU\n");
        return false;
    };
    let min_haw = units.iter().map(|u| u.haw).min().unwrap();
    let min_mgaw = units.iter().map(|u| u.mgaw).min().unwrap();
    SerialPort::puts("[iommu] global agaw=");
    SerialPort::put_u64(match global_agaw { Agaw::Level3=>39, Agaw::Level4=>48 });
    SerialPort::puts(" common_sagaw=");
    SerialPort::put_hex(common_sagaw);
    SerialPort::puts(" min_haw=");
    SerialPort::put_u64(min_haw as u64);
    SerialPort::puts(" min_mgaw=");
    SerialPort::put_u64(min_mgaw as u64);
    SerialPort::puts("\n");
    // SNP (snoop) requires ECAP.SC=1 on every unit; QEMU reports SC=0 (ecap 0xf00f0a),
    // so SNP is reserved(0) even for leaf. Use global conservative value.
    let allow_snp = units.iter().all(|u| u.sc);
    if !allow_snp {
        SerialPort::puts("[iommu] SC=0 on at least one unit => leaf SNP=0 (reserve)\n");
    }
    // Allocate SLPT root
    let slpt_root = match alloc.alloc() {
        Some(p) => p,
        None => {
            SerialPort::puts("[iommu] slpt root OOM\n");
            return false;
        }
    };
    let slpt_va = crate::mm::layout::to_physmap(slpt_root);
    unsafe { core::ptr::write_bytes(slpt_va as *mut u8, 0, 4096) };
    let mut domain = Domain::new(global_agaw, slpt_root, min_mgaw, min_haw, allow_snp);
    // Effective RMRR set for IOVA allocation — starts as firmware RMRRs and will also reserve the GOP framebuffer
    // (instead of bumping next_iova past high MMIO which would exhaust 32-bit IOVA space for alloc_iova_below).
    let mut effective_rmrrs: Vec<crate::acpi::Rmrr> = dmar.rmrrs.clone();

    // Identity-map RMRR ranges into SLPT BEFORE enabling translation.
    for rmrr in &dmar.rmrrs {
        let base = rmrr.base_address & !0xFFF;
        let limit_inclusive = rmrr.limit_address;
        // size = (limit|0xFFF)+1 - base per spec p8-11 inclusive
        let end_exclusive = (limit_inclusive | 0xFFF).wrapping_add(1);
        if end_exclusive <= base || end_exclusive > domain.iova_limit {
            SerialPort::puts("[iommu] RMRR range invalid/over limit skip\n");
            continue;
        }
        let size = end_exclusive - base;
        SerialPort::puts("[iommu] RMRR identity base=");
        SerialPort::put_hex(base);
        SerialPort::puts(" limit=");
        SerialPort::put_hex(limit_inclusive);
        SerialPort::puts(" size=");
        SerialPort::put_hex(size);
        SerialPort::puts("\n");
        let pages = size / 4096;
        for p in 0..pages {
            let addr = base + p * 4096;
            if let Err(e) = slpt::map_4k(slpt_root, alloc, addr, addr, global_agaw, allow_snp) {
                SerialPort::puts("[iommu] RMRR map fail ");
                SerialPort::put_hex(addr);
                SerialPort::puts(" err=");
                SerialPort::puts(e);
                SerialPort::puts("\n");
            }
        }
        if domain.next_iova <= end_exclusive && end_exclusive < domain.iova_limit {
            domain.next_iova = (end_exclusive + 0xFFF) & !0xFFF;
        }
    }

    // Identity-map GOP framebuffer (display scanout) BEFORE enabling translation.
    // Without this the iGPU display engine DMAs `fb_phys` as IOVA and faults -> stripes
    // (user report: machine-art display with IOMMU on, clean with noiommu). `fb_range`
    // is passed from `Kernel::fb_phys` + stride*height*bpp. This is not RMRR (firmware
    // does not list FB as RMRR), so we synthesize the identity. `noiommu` already
    // bailed above; this block only runs when IOMMU is actually enabling.
    if let Some((fb_base_raw, fb_size)) = fb_range {
        if fb_size != 0 && fb_base_raw != 0 {
            let fb_base = fb_base_raw & !0xFFF;
            let fb_end_raw = fb_base_raw.checked_add(fb_size).unwrap_or(0);
            // round exclusive end up to 4K (fb_size may not be page-aligned)
            let fb_end_excl = (fb_end_raw + 0xFFF) & !0xFFF;
            if fb_end_excl <= fb_base || fb_base >= domain.iova_limit || fb_end_excl > domain.iova_limit {
                SerialPort::puts("[iommu] FB range invalid/over IOVA limit (");
                SerialPort::put_hex(fb_base);
                SerialPort::puts(" + ");
                SerialPort::put_hex(fb_size);
                SerialPort::puts(" end=");
                SerialPort::put_hex(fb_end_excl);
                SerialPort::puts(" limit=");
                SerialPort::put_hex(domain.iova_limit);
                SerialPort::puts(") -> abort IOMMU, fallback to noiommu\n");
                return false;
            }
            let fb_pages = (fb_end_excl - fb_base) / 4096;
            SerialPort::puts("[iommu] FB identity base=");
            SerialPort::put_hex(fb_base);
            SerialPort::puts(" size=");
            SerialPort::put_hex(fb_size);
            SerialPort::puts(" pages=");
            SerialPort::put_u64(fb_pages);
            SerialPort::puts("\n");
            let mut fb_map_failed = false;
            for p in 0..fb_pages {
                let addr = fb_base + p * 4096;
                // Skip if already identity-mapped by overlapping RMRR (avoid Already mapped)
                if let Some(pa) = slpt::translate(slpt_root, addr, global_agaw) {
                    if pa == addr {
                        continue;
                    }
                    // Stale mismatch — treat as error (verify will also catch)
                    SerialPort::puts("[iommu] FB overlap mismatch addr=");
                    SerialPort::put_hex(addr);
                    SerialPort::puts(" pa=");
                    SerialPort::put_hex(pa);
                    SerialPort::puts("\n");
                    if crate::bootargs::is_iommu_verify() {
                        return false;
                    }
                    // without verify, clear and re-map? attempt overwrite via translate fixup below
                    // fall through to map attempt which will hit Already mapped -> need explicit overwrite
                    // So skip duplicate mismatch silently for best-effort
                    continue;
                }
                if let Err(e) = slpt::map_4k(slpt_root, alloc, addr, addr, global_agaw, allow_snp) {
                    // If Already mapped due to RMRR overlap, ignore; else fail
                    if e == "IOMMU SLPT: iova already mapped" {
                        continue;
                    }
                    SerialPort::puts("[iommu] FB map fail ");
                    SerialPort::put_hex(addr);
                    SerialPort::puts(" err=");
                    SerialPort::puts(e);
                    SerialPort::puts("\n");
                    fb_map_failed = true;
                    break;
                }
            }
            if fb_map_failed {
                SerialPort::puts("[iommu] FB map failed -> abort IOMMU, fallback to noiommu\n");
                return false;
            }
            // Reserve FB range for IOVA allocation without bumping next_iova past high MMIO.
            // Bumping to 0xE0000000+ would exhaust 32-bit IOVA space for alloc_iova_below.
            // Instead treat FB as an RMRR-like reserved range (page-aligned) checked in alloc_iova_with_limit.
            effective_rmrrs.push(crate::acpi::Rmrr {
                segment: 0,
                base_address: fb_base,
                limit_address: fb_end_excl - 1,
                devices: alloc::vec::Vec::new(),
            });
            SerialPort::puts("[iommu] FB reserved for IOVA allocator base=");
            SerialPort::put_hex(fb_base);
            SerialPort::puts(" end=");
            SerialPort::put_hex(fb_end_excl);
            SerialPort::puts("\n");
            // Gated verification: when `iommu_verify` bootarg present, re-walk and ensure identity
            if crate::bootargs::is_iommu_verify() {
                let mut verify_ok = true;
                for p in 0..fb_pages {
                    let addr = fb_base + p * 4096;
                    match slpt::translate(slpt_root, addr, global_agaw) {
                        Some(pa) if pa == addr => {},
                        Some(pa) => {
                            SerialPort::puts("[iommu] verify FB mismatch IOVA=");
                            SerialPort::put_hex(addr);
                            SerialPort::puts(" -> ");
                            SerialPort::put_hex(pa);
                            SerialPort::puts("\n");
                            verify_ok = false;
                            break;
                        },
                        None => {
                            SerialPort::puts("[iommu] verify FB missing IOVA=");
                            SerialPort::put_hex(addr);
                            SerialPort::puts("\n");
                            verify_ok = false;
                            break;
                        }
                    }
                }
                if !verify_ok {
                    SerialPort::puts("[iommu] verify FB failed -> abort IOMMU, fallback to noiommu\n");
                    return false;
                }
                // Also verify RMRRs still identity
                for rmrr in &dmar.rmrrs {
                    let base = rmrr.base_address & !0xFFF;
                    let limit_inc = rmrr.limit_address;
                    let end = (limit_inc | 0xFFF).wrapping_add(1);
                    if end <= base || end > domain.iova_limit { continue; }
                    let pages = (end - base)/4096;
                    for p in 0..pages {
                        let addr = base + p*4096;
                        match slpt::translate(slpt_root, addr, global_agaw) {
                            Some(pa) if pa == addr => {},
                            _ => {
                                SerialPort::puts("[iommu] verify RMRR fail addr=");
                                SerialPort::put_hex(addr);
                                SerialPort::puts("\n");
                                verify_ok = false;
                                break;
                            }
                        }
                    }
                    if !verify_ok { break; }
                }
                if !verify_ok {
                    SerialPort::puts("[iommu] verify RMRR failed -> abort IOMMU\n");
                    return false;
                }
                SerialPort::puts("[iommu] verify FB+RMRR identity ok\n");
            }
        }
    }

    // For each unit: alloc tables (root/context) pointing to SLPT, set RTADDR, enable QI
    // Use domain (global) AGAW for context entries; unit-local AGAW may be wider than domain and would mis-walk.
    for (idx, unit) in units.iter_mut().enumerate() {
        unit.agaw = global_agaw;
        let drhd = &dmar.drhds[idx];
        if let Err(e) = unit.alloc_tables(drhd, alloc, slpt_root) {
            SerialPort::puts("[iommu] alloc_tables fail ");
            SerialPort::puts(e);
            SerialPort::puts("\n");
            continue;
        }
        if !unit.set_root() {
            SerialPort::puts("[iommu] set_root fail did=");
            SerialPort::put_u64(unit.did as u64);
            SerialPort::puts("\n");
            continue;
        }
        let _ = unit.enable_qi(alloc);
        let _ = unit.invalidate_all();
    }

    // Now enable translation on all units
    let mut ok_count = 0;
    for unit in &mut units {
        if unit.root_phys == 0 {
            continue;
        }
        if unit.enable_translation() {
            unit.drain_faults();
            ok_count += 1;
        }
    }

    if ok_count == 0 {
        SerialPort::puts("[iommu] no unit enabled TE — IOMMU disabled\n");
        return false;
    }

    // Early fault window: display engine DMAs continuously; if FB/RMRR identity was wrong
    // or any early DMA faults, it will appear within microseconds after TE.
    // On any fault, fallback fully to noiommu so USB/storage (xHCI) does not hang
    // waiting for faulting DMA. Previously we tried per-unit fallback but that
    // left the include_all unit enabled and hung at xHCI init on this ASUS.
    {
        let mut any_fault = false;
        for _ in 0..200_000 {
            for u in &units {
                let fsts = unsafe { read32(u.reg_va, REG_FSTS) };
                if fsts & FSTS_ALL_FAULT != 0 {
                    any_fault = true;
                    break;
                }
            }
            if any_fault {
                break;
            }
            core::hint::spin_loop();
        }
        if any_fault {
            SerialPort::puts("[iommu] early faults after TE -> full fallback to noiommu, dumping FRCD\n");
            for u in &mut units {
                u.drain_faults();
                if u.enabled {
                    u.disable_translation();
                }
            }
            return false;
        }
    }

    SerialPort::puts("[iommu] enabled units=");
    SerialPort::put_u64(ok_count as u64);
    SerialPort::puts("/");
    SerialPort::put_u64(units.len() as u64);
    SerialPort::puts(" agaw=");
    SerialPort::put_u64(match global_agaw { Agaw::Level3=>39, Agaw::Level4=>48 });
    SerialPort::puts(" mgaw=");
    SerialPort::put_u64(min_mgaw as u64);
    SerialPort::puts(" haw=");
    SerialPort::put_u64(min_haw as u64);
    SerialPort::puts(" allow_snp=");
    SerialPort::put_u64(if allow_snp {1} else {0});
    SerialPort::puts(" slpt=");
    SerialPort::put_hex(slpt_root);
    SerialPort::puts("\n");

    let state = IommuState {
        units,
        domain,
        rmrrs: effective_rmrrs,
        lock: Mutex::new(()),
    };
    IOMMU_STATE.call_once(|| Mutex::new(state));
    IOMMU_ENABLED.store(true, Ordering::Release);
    true
}

pub fn with_state<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut IommuState) -> R,
{
    let m = IOMMU_STATE.get()?;
    let mut guard = m.lock();
    Some(f(&mut guard))
}

pub fn with_units<F>(f: F)
where
    F: Fn(&VtDUnit),
{
    if let Some(m) = IOMMU_STATE.get() {
        let guard = m.lock();
        for u in &guard.units {
            f(u);
        }
    }
}

/// Global helper called from IDT fault handler.
/// Uses try_lock to avoid deadlock if fault occurs while holding IommuState lock.
/// If lock is contended, faults remain pending for idle poll.
pub fn fault_handler() {
    let Some(m) = IOMMU_STATE.get() else { return };
    // Use try_lock: spin::Mutex::try_lock is available. Avoid deadlock in IRQ.
    let Some(guard) = m.try_lock() else {
        // Contended — leave for idle poll
        return;
    };
    for u in &guard.units {
        u.drain_faults();
    }
}

/// Map a phys range to a newly allocated IOVA. Returns IOVA or None.
pub fn map_phys_to_iova(phys: u64, size: u64, alloc: &mut BitmapAllocator) -> Option<u64> {
    map_phys_to_iova_inner(phys, size, alloc, None)
}

/// Map `phys` into IOVA strictly below `limit` (exclusive) — for 32-bit-only
/// devices.  Returns `None` when no low IOVA window remains.
pub fn map_phys_to_iova_below(
    phys: u64,
    size: u64,
    alloc: &mut BitmapAllocator,
    limit: u64,
) -> Option<u64> {
    map_phys_to_iova_inner(phys, size, alloc, Some(limit))
}

fn map_phys_to_iova_inner(
    phys: u64,
    size: u64,
    alloc: &mut BitmapAllocator,
    limit: Option<u64>,
) -> Option<u64> {
    if size == 0 {
        return None;
    }
    // Support arbitrary phys alignment: map the spanned pages and return offset IOVA.
    let page_offset = phys & 0xFFF;
    let total_size = size.checked_add(page_offset)?;
    let phys_aligned = phys & !0xFFF;
    let state_mutex = IOMMU_STATE.get()?;
    let mut state = state_mutex.lock();
    let rmrrs_snapshot = state.rmrrs.clone();
    let prev_next_iova = state.domain.next_iova;
    let agaw = state.domain.agaw;
    let slpt_root = state.domain.slpt_root_phys;
    let allow_snp = state.domain.allow_snp;
    let iova = match limit {
        Some(lim) => state.domain.alloc_iova_below(total_size, lim, &rmrrs_snapshot)?,
        None => state.domain.alloc_iova(total_size, &rmrrs_snapshot)?,
    };
    let pages = (total_size.checked_add(0xFFF)? & !0xFFF) / 4096;
    for p in 0..pages {
        let i = iova.checked_add(p.checked_mul(4096)?)?;
        let pa = phys_aligned.checked_add(p.checked_mul(4096)?)?;
        if let Err(e) = slpt::map_4k(slpt_root, alloc, i, pa, agaw, allow_snp) {
            SerialPort::puts("[iommu] map_phys fail ");
            SerialPort::put_hex(i);
            SerialPort::puts("->");
            SerialPort::put_hex(pa);
            SerialPort::puts(" err=");
            SerialPort::puts(e);
            SerialPort::puts("\n");
            // Rollback: unmap pages already successfully mapped in this call and restore bump allocator.
            for rb in 0..p {
                let rb_iova = iova.checked_add((rb as u64).checked_mul(4096)?).unwrap_or(0);
                let _ = slpt::unmap_4k(slpt_root, rb_iova, agaw);
            }
            state.domain.next_iova = prev_next_iova;
            return None;
        }
    }
    for unit in &mut state.units {
        if unit.enabled {
            let _ = unit.invalidate_all();
        }
    }
    fence(Ordering::SeqCst);
    Some(iova + page_offset)
}

pub fn translate_iova(iova: u64) -> Option<u64> {
    let m = IOMMU_STATE.get()?;
    let guard = m.lock();
    slpt::translate(guard.domain.slpt_root_phys, iova & !0xFFF, guard.domain.agaw)
        .map(|p| (p & !0xFFF) | (iova & 0xFFF))
}

pub fn is_present() -> bool {
    IOMMU_STATE.get().is_some()
}

pub fn program_fault_msi(vector: u8, apic_id: u32) {
    if let Some(m) = IOMMU_STATE.get() {
        let guard = m.lock();
        for u in &guard.units {
            u.program_fault_msi(vector, apic_id);
            u.set_fault_intr(true);
        }
    }
}

pub fn has_pending_faults() -> bool {
    let Some(m) = IOMMU_STATE.get() else { return false };
    let guard = m.lock();
    for u in &guard.units {
        let fsts = unsafe { read32(u.reg_va, REG_FSTS) };
        if fsts & FSTS_ALL_FAULT != 0 {
            return true;
        }
    }
    false
}

pub fn disable_all() {
    let Some(m) = IOMMU_STATE.get() else { return };
    let mut guard = m.lock();
    for u in &mut guard.units {
        if u.enabled {
            u.disable_translation();
            u.drain_faults();
        }
    }
    IOMMU_ENABLED.store(false, Ordering::Release);
    SerialPort::puts("[iommu] globally disabled due to faults -> fallback to noiommu\n");
}
