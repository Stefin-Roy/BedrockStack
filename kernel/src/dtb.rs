#![cfg(target_arch = "riscv64")]

use crate::boot::{MemoryRegion, MemoryRegionKind};
use crate::drivers::serial::SerialPort;

const FDT_MAGIC: u32 = 0xD00DFEED;
const FDT_BEGIN_NODE: u32 = 0x00000001;
const FDT_END_NODE: u32 = 0x00000002;
const FDT_PROP: u32 = 0x00000003;
const FDT_END: u32 = 0x00000009;

const MAX_MEMORY_REGIONS: usize = 8;

/// Mutable boot-time buffer for parsed memory regions.
///
/// Written only during single-threaded boot before any reader is reached;
/// the raw `UnsafeCell` preserves the old `static mut` semantics.
struct DtbMemoryRegions(core::cell::UnsafeCell<[MemoryRegion; MAX_MEMORY_REGIONS]>);

// Safety: writes happen only on the boot hart before the buffer is ever read.
unsafe impl Sync for DtbMemoryRegions {}

impl DtbMemoryRegions {
    const fn new() -> Self {
        DtbMemoryRegions(core::cell::UnsafeCell::new(unsafe { core::mem::zeroed() }))
    }

    fn get(&self) -> *mut [MemoryRegion; MAX_MEMORY_REGIONS] {
        self.0.get()
    }
}

static DTB_MEMORY_REGIONS: DtbMemoryRegions = DtbMemoryRegions::new();

struct FdtHeader {
    magic: u32,
    total_size: u32,
    off_dt_struct: u32,
    off_dt_strings: u32,
}

fn read_be_u32(ptr: *const u8) -> u32 {
    unsafe {
        let b0 = core::ptr::read_volatile(ptr) as u32;
        let b1 = core::ptr::read_volatile(ptr.add(1)) as u32;
        let b2 = core::ptr::read_volatile(ptr.add(2)) as u32;
        let b3 = core::ptr::read_volatile(ptr.add(3)) as u32;
        (b0 << 24) | (b1 << 16) | (b2 << 8) | b3
    }
}

fn read_be_n(ptr: *const u8, cells: u32) -> u64 {
    let mut val: u64 = 0;
    for i in 0..cells {
        let word = read_be_u32(unsafe { ptr.add(i as usize * 4) });
        val = (val << 32) | word as u64;
    }
    val
}

fn fdt_parse_header(dtb: *const u8) -> Option<FdtHeader> {
    if dtb.is_null() {
        return None;
    }
    let magic = read_be_u32(dtb);
    if magic != FDT_MAGIC {
        return None;
    }
    let total_size = read_be_u32(unsafe { dtb.add(4) });
    if total_size < 16 {
        return None;
    }
    let off_dt_struct = read_be_u32(unsafe { dtb.add(8) });
    let off_dt_strings = read_be_u32(unsafe { dtb.add(12) });
    if off_dt_struct as u64 + 4 > total_size as u64 || off_dt_strings as u64 + 4 > total_size as u64 {
        return None;
    }
    Some(FdtHeader {
        magic,
        total_size,
        off_dt_struct,
        off_dt_strings,
    })
}

fn dtb_off(hdr: &FdtHeader, dtb: *const u8, pos: *const u8) -> usize {
    (pos as usize).wrapping_sub(dtb as usize)
}

fn in_bounds(hdr: &FdtHeader, dtb: *const u8, pos: *const u8, add: usize) -> bool {
    let off = dtb_off(hdr, dtb, pos);
    off < hdr.total_size as usize && add <= hdr.total_size as usize - off
}

fn fdt_string(hdr: &FdtHeader, dtb: *const u8, nameoff: u32) -> *const u8 {
    let off = hdr.off_dt_strings as usize + nameoff as usize;
    if off >= hdr.total_size as usize {
        return core::ptr::null();
    }
    unsafe { dtb.add(off) }
}

fn fdt_str_eq(ptr: *const u8, expected: &[u8]) -> bool {
    if ptr.is_null() {
        return false;
    }
    unsafe {
        for (i, &c) in expected.iter().enumerate() {
            if core::ptr::read_volatile(ptr.add(i)) != c {
                return false;
            }
        }
        core::ptr::read_volatile(ptr.add(expected.len())) == 0
    }
}

fn align_ptr(p: *const u8) -> *const u8 {
    let addr = p as usize;
    ((addr + 3) & !3) as *const u8
}

fn skip_name(hdr: &FdtHeader, dtb: *const u8, mut pos: *const u8) -> *const u8 {
    let start_off = dtb_off(hdr, dtb, pos);
    let max_off = hdr.total_size as usize;
    while dtb_off(hdr, dtb, pos) < max_off {
        if unsafe { core::ptr::read_volatile(pos) } == 0 {
            let next = unsafe { pos.add(1) };
            if dtb_off(hdr, dtb, next) < max_off {
                return next;
            }
            return core::ptr::null();
        }
        pos = unsafe { pos.add(1) };
    }
    core::ptr::null()
}

fn skip_prop(hdr: &FdtHeader, dtb: *const u8, mut pos: *const u8) -> *const u8 {
    if !in_bounds(hdr, dtb, pos, 8) {
        return core::ptr::null();
    }
    let len = read_be_u32(pos);
    pos = unsafe { pos.add(8) };
    let padded = (len + 3) & !3;
    if !in_bounds(hdr, dtb, pos, padded as usize) {
        return core::ptr::null();
    }
    unsafe { pos.add(padded as usize) }
}

fn fallback_memory() -> &'static [MemoryRegion] {
    static FALLBACK: [MemoryRegion; 3] = [
        MemoryRegion { base: 0x80050000, size: 0x0FFB0000, kind: MemoryRegionKind::Usable },
        MemoryRegion { base: 0x00100000, size: 0x00001000, kind: MemoryRegionKind::Reserved },
        MemoryRegion { base: 0x80000000, size: 0x00050000, kind: MemoryRegionKind::Reserved },
    ];
    &FALLBACK
}

fn walk_dtb<F>(dtb: *const u8, node_match: &[u8], prop_match: &[u8], mut callback: F) -> bool
where
    F: FnMut(u64, u64),
{
    let hdr = match fdt_parse_header(dtb) {
        Some(h) => h,
        None => return false,
    };

    let struct_base = if in_bounds(&hdr, dtb, dtb, hdr.off_dt_struct as usize) {
        unsafe { dtb.add(hdr.off_dt_struct as usize) }
    } else {
        return false;
    };
    let mut pos = if in_bounds(&hdr, dtb, struct_base, 4) { unsafe { struct_base.add(4) } } else { return false };
    pos = skip_name(&hdr, dtb, pos);
    if pos.is_null() { return false; }
    pos = align_ptr(pos);

    let mut addr_cells: u32 = 2;
    let mut size_cells: u32 = 2;

    loop {
        if !in_bounds(&hdr, dtb, pos, 4) { return false; }
        let token = read_be_u32(pos);
        match token {
            FDT_PROP => {
                if !in_bounds(&hdr, dtb, pos, 12) { return false; }
                pos = unsafe { pos.add(4) };
                let len = read_be_u32(pos);
                pos = unsafe { pos.add(4) };
                let nameoff = read_be_u32(pos);
                pos = unsafe { pos.add(4) };
                let name_ptr = fdt_string(&hdr, dtb, nameoff);
                let val_ptr = pos;
                if fdt_str_eq(name_ptr, b"#address-cells") && len >= 4 {
                    if in_bounds(&hdr, dtb, val_ptr, 4) {
                        addr_cells = read_be_u32(val_ptr);
                    }
                } else if fdt_str_eq(name_ptr, b"#size-cells") && len >= 4 {
                    if in_bounds(&hdr, dtb, val_ptr, 4) {
                        size_cells = read_be_u32(val_ptr);
                    }
                }
                let padded = (len + 3) & !3;
                if !in_bounds(&hdr, dtb, pos, padded as usize) { return false; }
                pos = unsafe { pos.add(padded as usize) };
            }
            FDT_BEGIN_NODE => break,
            FDT_END_NODE => break,
            FDT_END => break,
            _ => {
                if !in_bounds(&hdr, dtb, pos, 4) { return false; }
                pos = unsafe { pos.add(4) };
            }
        }
    }

    let mut depth: u32 = 1;
    let mut in_target = false;

    loop {
        if !in_bounds(&hdr, dtb, pos, 4) { return false; }
        let token = read_be_u32(pos);
        pos = unsafe { pos.add(4) };
        match token {
            FDT_BEGIN_NODE => {
                depth += 1;
                let node_name = pos;
                pos = skip_name(&hdr, dtb, pos);
                if pos.is_null() { return false; }
                pos = align_ptr(pos);
                if depth == 2 {
                    in_target = fdt_str_eq(node_name, node_match);
                }
            }
            FDT_END_NODE => {
                depth -= 1;
                if depth == 1 {
                    in_target = false;
                }
            }
            FDT_PROP if in_target => {
                if !in_bounds(&hdr, dtb, pos, 8) { return false; }
                let len = read_be_u32(pos);
                pos = unsafe { pos.add(4) };
                let nameoff = read_be_u32(pos);
                pos = unsafe { pos.add(4) };
                let name_ptr = fdt_string(&hdr, dtb, nameoff);
                let val_ptr = pos;
                if fdt_str_eq(name_ptr, prop_match) {
                    let mut offset = 0usize;
                    while (offset as u32) < len {
                        if !in_bounds(&hdr, dtb, unsafe { val_ptr.add(offset) }, (addr_cells + size_cells) as usize * 4) {
                            return false;
                        }
                        let addr = read_be_n(unsafe { val_ptr.add(offset) }, addr_cells);
                        offset += addr_cells as usize * 4;
                        let size = read_be_n(unsafe { val_ptr.add(offset) }, size_cells);
                        offset += size_cells as usize * 4;
                        callback(addr, size);
                    }
                }
                let padded = (len + 3) & !3;
                if !in_bounds(&hdr, dtb, pos, padded as usize) { return false; }
                pos = unsafe { pos.add(padded as usize) };
            }
            FDT_PROP => {
                let p = skip_prop(&hdr, dtb, pos);
                if p.is_null() { return false; }
                pos = p;
            }
            FDT_END => break,
            _ => {}
        }
    }
    true
}

fn walk_dtb_prop_raw<F>(dtb: *const u8, node_match: &[u8], prop_match: &[u8], mut callback: F) -> bool
where
    F: FnMut(*const u8, u32),
{
    let hdr = match fdt_parse_header(dtb) {
        Some(h) => h,
        None => return false,
    };

    let struct_base = if in_bounds(&hdr, dtb, dtb, hdr.off_dt_struct as usize) {
        unsafe { dtb.add(hdr.off_dt_struct as usize) }
    } else {
        return false;
    };
    let mut pos = if in_bounds(&hdr, dtb, struct_base, 4) { unsafe { struct_base.add(4) } } else { return false };
    pos = skip_name(&hdr, dtb, pos);
    if pos.is_null() { return false; }
    pos = align_ptr(pos);

    loop {
        if !in_bounds(&hdr, dtb, pos, 4) { return false; }
        let token = read_be_u32(pos);
        match token {
            FDT_PROP => {
                let p = skip_prop(&hdr, dtb, pos);
                if p.is_null() { return false; }
                pos = p;
            }
            FDT_BEGIN_NODE | FDT_END_NODE | FDT_END => break,
            _ => {
                if !in_bounds(&hdr, dtb, pos, 4) { return false; }
                pos = unsafe { pos.add(4) };
            }
        }
    }

    let mut depth: u32 = 1;
    let mut in_target = false;

    loop {
        if !in_bounds(&hdr, dtb, pos, 4) { return false; }
        let token = read_be_u32(pos);
        pos = unsafe { pos.add(4) };
        match token {
            FDT_BEGIN_NODE => {
                depth += 1;
                let node_name = pos;
                pos = skip_name(&hdr, dtb, pos);
                if pos.is_null() { return false; }
                pos = align_ptr(pos);
                if depth == 2 {
                    in_target = fdt_str_eq(node_name, node_match);
                }
            }
            FDT_END_NODE => {
                depth -= 1;
                if depth == 1 {
                    in_target = false;
                }
            }
            FDT_PROP if in_target => {
                if !in_bounds(&hdr, dtb, pos, 8) { return false; }
                let len = read_be_u32(pos);
                pos = unsafe { pos.add(4) };
                let nameoff = read_be_u32(pos);
                pos = unsafe { pos.add(4) };
                let name_ptr = fdt_string(&hdr, dtb, nameoff);
                if fdt_str_eq(name_ptr, prop_match) {
                    callback(pos, len);
                }
                let padded = (len + 3) & !3;
                if !in_bounds(&hdr, dtb, pos, padded as usize) { return false; }
                pos = unsafe { pos.add(padded as usize) };
            }
            FDT_PROP => {
                let p = skip_prop(&hdr, dtb, pos);
                if p.is_null() { return false; }
                pos = p;
            }
            FDT_END => break,
            _ => {}
        }
    }
    true
}

pub fn parse_memory(dtb: *const u8) -> &'static [MemoryRegion] {
    let hdr = match fdt_parse_header(dtb) {
        Some(h) => h,
        None => return fallback_memory(),
    };

    let struct_base = if in_bounds(&hdr, dtb, dtb, hdr.off_dt_struct as usize) {
        unsafe { dtb.add(hdr.off_dt_struct as usize) }
    } else {
        return fallback_memory();
    };
    let mut pos: *const u8 = if in_bounds(&hdr, dtb, struct_base, 4) { unsafe { struct_base.add(4) } } else { return fallback_memory() };
    pos = skip_name(&hdr, dtb, pos);
    if pos.is_null() { return fallback_memory(); }
    pos = align_ptr(pos);

    let mut addr_cells: u32 = 2;
    let mut size_cells: u32 = 2;

    loop {
        if !in_bounds(&hdr, dtb, pos, 4) { return fallback_memory(); }
        let token = read_be_u32(pos);
        match token {
            FDT_PROP => {
                if !in_bounds(&hdr, dtb, pos, 12) { return fallback_memory(); }
                pos = unsafe { pos.add(4) };
                let len = read_be_u32(pos);
                pos = unsafe { pos.add(4) };
                let nameoff = read_be_u32(pos);
                pos = unsafe { pos.add(4) };
                let name_ptr = fdt_string(&hdr, dtb, nameoff);
                let val_ptr = pos;
                if fdt_str_eq(name_ptr, b"#address-cells") && len >= 4 {
                    if in_bounds(&hdr, dtb, val_ptr, 4) {
                        addr_cells = read_be_u32(val_ptr);
                    }
                } else if fdt_str_eq(name_ptr, b"#size-cells") && len >= 4 {
                    if in_bounds(&hdr, dtb, val_ptr, 4) {
                        size_cells = read_be_u32(val_ptr);
                    }
                }
                let padded = (len + 3) & !3;
                if !in_bounds(&hdr, dtb, pos, padded as usize) { return fallback_memory(); }
                pos = unsafe { pos.add(padded as usize) };
            }
            FDT_BEGIN_NODE => break,
            FDT_END_NODE => break,
            FDT_END => break,
            _ => {
                if !in_bounds(&hdr, dtb, pos, 4) { return fallback_memory(); }
                pos = unsafe { pos.add(4) };
            }
        }
    }

    let mut region_count: usize = 0;
    let mut depth: u32 = 1;
    let mut in_memory = false;

    loop {
        if !in_bounds(&hdr, dtb, pos, 4) { break; }
        let token = read_be_u32(pos);
        pos = unsafe { pos.add(4) };
        match token {
            FDT_BEGIN_NODE => {
                depth += 1;
                let node_name = pos;
                pos = skip_name(&hdr, dtb, pos);
                if pos.is_null() { break; }
                pos = align_ptr(pos);
                if depth == 2 {
                    in_memory = fdt_str_eq(node_name, b"memory")
                        || fdt_str_eq(node_name, b"memory@80000000");
                }
            }
            FDT_END_NODE => {
                depth -= 1;
                if depth == 1 {
                    in_memory = false;
                }
            }
            FDT_PROP if in_memory => {
                if !in_bounds(&hdr, dtb, pos, 8) { break; }
                let len = read_be_u32(pos);
                pos = unsafe { pos.add(4) };
                let nameoff = read_be_u32(pos);
                pos = unsafe { pos.add(4) };
                let name_ptr = fdt_string(&hdr, dtb, nameoff);
                let val_ptr = pos;
                if fdt_str_eq(name_ptr, b"reg") && region_count < MAX_MEMORY_REGIONS {
                    let mut offset = 0usize;
                    while (offset as u32) < len {
                        if !in_bounds(&hdr, dtb, unsafe { val_ptr.add(offset) }, (addr_cells + size_cells) as usize * 4) {
                            break;
                        }
                        let addr = read_be_n(unsafe { val_ptr.add(offset) }, addr_cells);
                        offset += addr_cells as usize * 4;
                        let size = read_be_n(unsafe { val_ptr.add(offset) }, size_cells);
                        offset += size_cells as usize * 4;
                        if size > 0 {
                            let kind = if addr == 0x80000000 && size <= 0x100000 {
                                MemoryRegionKind::Reserved
                            } else {
                                MemoryRegionKind::Usable
                            };
                            unsafe {
                                (&mut *DTB_MEMORY_REGIONS.get())[region_count] = MemoryRegion { base: addr, size, kind };
                            }
                            region_count += 1;
                        }
                    }
                }
                let padded = (len + 3) & !3;
                if !in_bounds(&hdr, dtb, pos, padded as usize) { break; }
                pos = unsafe { pos.add(padded as usize) };
            }
            FDT_PROP => {
                let p = skip_prop(&hdr, dtb, pos);
                if p.is_null() { break; }
                pos = p;
            }
            FDT_END => break,
            _ => {}
        }
    }

    if region_count > 0 {
        let regions = unsafe { &mut *DTB_MEMORY_REGIONS.get() };
        &regions[..region_count]
    } else {
        fallback_memory()
    }
}

/// Read the CPU timebase frequency from the DTB.
///
/// `timebase-frequency` is a single big-endian u32 property of the `/cpus`
/// node (not per-CPU).  Returns 0 if the DTB is absent, malformed, or the
/// property is missing.
pub fn timebase_hz(dtb: *const u8) -> u64 {
    let mut hz: u64 = 0;
    walk_dtb_prop_raw(dtb, b"cpus", b"timebase-frequency", |ptr, len| {
        if len >= 4 {
            hz = read_be_u32(ptr) as u64;
        }
    });
    hz
}

pub fn find_rsdp(dtb: *const u8) -> u64 {
    let hdr = match fdt_parse_header(dtb) {
        Some(h) => h,
        None => {
            SerialPort::puts("[kernel] riscv64: RSDP not found in DTB (ACPI not available)\n");
            return 0;
        }
    };

    let struct_base = if in_bounds(&hdr, dtb, dtb, hdr.off_dt_struct as usize) {
        unsafe { dtb.add(hdr.off_dt_struct as usize) }
    } else {
        return 0;
    };
    let mut pos: *const u8 = if in_bounds(&hdr, dtb, struct_base, 4) { unsafe { struct_base.add(4) } } else { return 0 };
    pos = skip_name(&hdr, dtb, pos);
    if pos.is_null() { return 0; }
    pos = align_ptr(pos);

    let mut depth: u32 = 1;
    let mut in_chosen = false;

    loop {
        if !in_bounds(&hdr, dtb, pos, 4) { break; }
        let token = read_be_u32(pos);
        pos = unsafe { pos.add(4) };
        match token {
            FDT_BEGIN_NODE => {
                depth += 1;
                let node_name = pos;
                pos = skip_name(&hdr, dtb, pos);
                if pos.is_null() { break; }
                pos = align_ptr(pos);
                if depth == 2 {
                    in_chosen = fdt_str_eq(node_name, b"chosen");
                }
            }
            FDT_END_NODE => {
                depth -= 1;
                if depth == 1 { in_chosen = false; }
            }
            FDT_PROP if in_chosen => {
                if !in_bounds(&hdr, dtb, pos, 8) { break; }
                let len = read_be_u32(pos);
                pos = unsafe { pos.add(4) };
                let nameoff = read_be_u32(pos);
                pos = unsafe { pos.add(4) };
                let name_ptr = fdt_string(&hdr, dtb, nameoff);
                let val_ptr = pos;
                if fdt_str_eq(name_ptr, b"acpi-rsdp") && len >= 4 {
                    let rsdp = match len {
                        8 => {
                            let hi = read_be_u32(val_ptr) as u64;
                            let lo = read_be_u32(unsafe { val_ptr.add(4) }) as u64;
                            (hi << 32) | lo
                        }
                        _ => read_be_u32(val_ptr) as u64,
                    };
                    return rsdp;
                }
                let padded = (len + 3) & !3;
                if !in_bounds(&hdr, dtb, pos, padded as usize) { break; }
                pos = unsafe { pos.add(padded as usize) };
            }
            FDT_PROP => {
                let p = skip_prop(&hdr, dtb, pos);
                if p.is_null() { break; }
                pos = p;
            }
            FDT_END => break,
            _ => {}
        }
    }

    SerialPort::puts("[kernel] riscv64: RSDP not found in DTB (ACPI not available)\n");
    0
}

/// Parse CPU nodes from the DTB.
///
/// Returns a vector of `(hart_id, enabled)` for each cpu node found under
/// `/cpus`.  Hart IDs are taken from the `reg` property; enabled = true when
/// the `status` property is `"okay"` (or absent).
pub fn parse_cpus(dtb: *const u8) -> alloc::vec::Vec<(u32, bool)> {
    let mut cpus = alloc::vec::Vec::new();

    let hdr = match fdt_parse_header(dtb) {
        Some(h) => h,
        None => return cpus,
    };

    let struct_base = if in_bounds(&hdr, dtb, dtb, hdr.off_dt_struct as usize) {
        unsafe { dtb.add(hdr.off_dt_struct as usize) }
    } else {
        return cpus;
    };
    let mut pos = if in_bounds(&hdr, dtb, struct_base, 4) { unsafe { struct_base.add(4) } } else { return cpus };
    pos = skip_name(&hdr, dtb, pos);
    if pos.is_null() { return cpus; }
    pos = align_ptr(pos);

    let mut depth: u32 = 1;
    let mut in_cpus = false;

    loop {
        if !in_bounds(&hdr, dtb, pos, 4) { break; }
        let token = read_be_u32(pos);
        pos = unsafe { pos.add(4) };
        match token {
            FDT_BEGIN_NODE => {
                depth += 1;
                let node_name = pos;
                pos = skip_name(&hdr, dtb, pos);
                if pos.is_null() { break; }
                pos = align_ptr(pos);
                if depth == 2 && fdt_str_eq(node_name, b"cpus") {
                    in_cpus = true;
                } else if depth == 3 && in_cpus {
                    let mut hart_id: u32 = 0;
                    let mut enabled = true;
                    let mut is_cpu = false;
                    let saved_pos = pos;
                    let mut prop_pos = saved_pos;
                    loop {
                        if !in_bounds(&hdr, dtb, prop_pos, 4) { break; }
                        let t = read_be_u32(prop_pos);
                        if t == FDT_PROP {
                            if !in_bounds(&hdr, dtb, prop_pos, 12) { break; }
                            prop_pos = unsafe { prop_pos.add(4) };
                            let len = read_be_u32(prop_pos);
                            prop_pos = unsafe { prop_pos.add(4) };
                            let nameoff = read_be_u32(prop_pos);
                            prop_pos = unsafe { prop_pos.add(4) };
                            let name_ptr = fdt_string(&hdr, dtb, nameoff);
                            let val_ptr = prop_pos;
                            if fdt_str_eq(name_ptr, b"device_type") && len >= 3 {
                                if in_bounds(&hdr, dtb, val_ptr, 3) {
                                    is_cpu = unsafe {
                                        core::ptr::read_volatile(val_ptr) == b'c'
                                            && core::ptr::read_volatile(val_ptr.add(1)) == b'p'
                                            && core::ptr::read_volatile(val_ptr.add(2)) == b'u'
                                    };
                                }
                            }
                            if fdt_str_eq(name_ptr, b"reg") && len >= 4 {
                                if in_bounds(&hdr, dtb, val_ptr, 4) {
                                    hart_id = read_be_u32(val_ptr);
                                }
                            } else if fdt_str_eq(name_ptr, b"status") && len >= 1 {
                                if in_bounds(&hdr, dtb, val_ptr, 1) {
                                    enabled = unsafe { *val_ptr } == b'o';
                                }
                            }
                            let padded = (len + 3) & !3;
                            if !in_bounds(&hdr, dtb, prop_pos, padded as usize) { break; }
                            prop_pos = unsafe { prop_pos.add(padded as usize) };
                        } else if t == FDT_BEGIN_NODE || t == FDT_END_NODE || t == FDT_END {
                            break;
                        } else {
                            if !in_bounds(&hdr, dtb, prop_pos, 4) { break; }
                            prop_pos = unsafe { prop_pos.add(4) };
                        }
                    }
                    pos = prop_pos;
                    if is_cpu {
                        cpus.push((hart_id, enabled));
                    }
                }
            }
            FDT_END_NODE => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
                if depth == 1 {
                    in_cpus = false;
                }
            }
            FDT_PROP => {
                let p = skip_prop(&hdr, dtb, pos);
                if p.is_null() { break; }
                pos = p;
            }
            FDT_END => break,
            _ => {}
        }
    }

    cpus
}
