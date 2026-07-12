#![cfg(target_arch = "riscv64")]

use crate::boot::{MemoryRegion, MemoryRegionKind};
use crate::drivers::serial::SerialPort;

const FDT_MAGIC: u32 = 0xD00DFEED;
const FDT_BEGIN_NODE: u32 = 0x00000001;
const FDT_END_NODE: u32 = 0x00000002;
const FDT_PROP: u32 = 0x00000003;
const FDT_END: u32 = 0x00000009;

const MAX_MEMORY_REGIONS: usize = 8;
static mut DTB_MEMORY_REGIONS: [MemoryRegion; MAX_MEMORY_REGIONS] = unsafe { core::mem::zeroed() };

struct FdtHeader {
    magic: u32,
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
    Some(FdtHeader {
        magic,
        off_dt_struct: read_be_u32(unsafe { dtb.add(8) }),
        off_dt_strings: read_be_u32(unsafe { dtb.add(12) }),
    })
}

fn fdt_string(hdr: &FdtHeader, dtb: *const u8, nameoff: u32) -> *const u8 {
    unsafe { dtb.add(hdr.off_dt_strings as usize + nameoff as usize) }
}

fn fdt_str_eq(ptr: *const u8, expected: &[u8]) -> bool {
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

fn skip_name(mut pos: *const u8) -> *const u8 {
    while unsafe { core::ptr::read_volatile(pos) } != 0 {
        pos = unsafe { pos.add(1) };
    }
    unsafe { pos.add(1) }
}

fn skip_prop(mut pos: *const u8) -> *const u8 {
    let len = read_be_u32(pos);
    pos = unsafe { pos.add(4) };
    pos = unsafe { pos.add(4) };
    let padded = (len + 3) & !3;
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

    let struct_base = unsafe { dtb.add(hdr.off_dt_struct as usize) };
    let mut pos = struct_base;

    pos = unsafe { pos.add(4) };
    pos = skip_name(pos);
    pos = align_ptr(pos);

    let mut addr_cells: u32 = 2;
    let mut size_cells: u32 = 2;

    loop {
        let token = read_be_u32(pos);
        match token {
            FDT_PROP => {
                pos = unsafe { pos.add(4) };
                let len = read_be_u32(pos);
                pos = unsafe { pos.add(4) };
                let nameoff = read_be_u32(pos);
                pos = unsafe { pos.add(4) };
                let name_ptr = fdt_string(&hdr, dtb, nameoff);
                let val_ptr = pos;
                if fdt_str_eq(name_ptr, b"#address-cells") && len >= 4 {
                    addr_cells = read_be_u32(val_ptr);
                } else if fdt_str_eq(name_ptr, b"#size-cells") && len >= 4 {
                    size_cells = read_be_u32(val_ptr);
                }
                let padded = (len + 3) & !3;
                pos = unsafe { pos.add(padded as usize) };
            }
            FDT_BEGIN_NODE => break,
            FDT_END_NODE => break,
            FDT_END => break,
            _ => { pos = unsafe { pos.add(4) }; }
        }
    }

    let mut depth: u32 = 1;
    let mut in_target = false;

    loop {
        let token = read_be_u32(pos);
        pos = unsafe { pos.add(4) };
        match token {
            FDT_BEGIN_NODE => {
                depth += 1;
                let node_name = pos;
                pos = skip_name(pos);
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
                let len = read_be_u32(pos);
                pos = unsafe { pos.add(4) };
                let nameoff = read_be_u32(pos);
                pos = unsafe { pos.add(4) };
                let name_ptr = fdt_string(&hdr, dtb, nameoff);
                let val_ptr = pos;
                if fdt_str_eq(name_ptr, prop_match) {
                    let mut offset = 0usize;
                    while (offset as u32) < len {
                        let addr = read_be_n(unsafe { val_ptr.add(offset) }, addr_cells);
                        offset += addr_cells as usize * 4;
                        let size = read_be_n(unsafe { val_ptr.add(offset) }, size_cells);
                        offset += size_cells as usize * 4;
                        callback(addr, size);
                    }
                }
                let padded = (len + 3) & !3;
                pos = unsafe { pos.add(padded as usize) };
            }
            FDT_PROP => {
                let len = read_be_u32(pos);
                pos = unsafe { pos.add(4) };
                let _ = read_be_u32(pos);
                pos = unsafe { pos.add(4) };
                let padded = (len + 3) & !3;
                pos = unsafe { pos.add(padded as usize) };
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

    let struct_base = unsafe { dtb.add(hdr.off_dt_struct as usize) };
    let mut pos = struct_base;

    pos = unsafe { pos.add(4) };
    pos = skip_name(pos);
    pos = align_ptr(pos);

    loop {
        let token = read_be_u32(pos);
        match token {
            FDT_PROP => { pos = skip_prop(pos); }
            FDT_BEGIN_NODE | FDT_END_NODE | FDT_END => break,
            _ => { pos = unsafe { pos.add(4) }; }
        }
    }

    let mut depth: u32 = 1;
    let mut in_target = false;

    loop {
        let token = read_be_u32(pos);
        pos = unsafe { pos.add(4) };
        match token {
            FDT_BEGIN_NODE => {
                depth += 1;
                let node_name = pos;
                pos = skip_name(pos);
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
                let len = read_be_u32(pos);
                pos = unsafe { pos.add(4) };
                let nameoff = read_be_u32(pos);
                pos = unsafe { pos.add(4) };
                let name_ptr = fdt_string(&hdr, dtb, nameoff);
                if fdt_str_eq(name_ptr, prop_match) {
                    callback(pos, len);
                }
                let padded = (len + 3) & !3;
                pos = unsafe { pos.add(padded as usize) };
            }
            FDT_PROP => { pos = skip_prop(pos); }
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

    let struct_base = unsafe { dtb.add(hdr.off_dt_struct as usize) };
    let mut pos: *const u8 = struct_base;

    pos = unsafe { pos.add(4) };
    pos = skip_name(pos);
    pos = align_ptr(pos);

    let mut addr_cells: u32 = 2;
    let mut size_cells: u32 = 2;

    loop {
        let token = read_be_u32(pos);
        match token {
            FDT_PROP => {
                pos = unsafe { pos.add(4) };
                let len = read_be_u32(pos);
                pos = unsafe { pos.add(4) };
                let nameoff = read_be_u32(pos);
                pos = unsafe { pos.add(4) };
                let name_ptr = fdt_string(&hdr, dtb, nameoff);
                let val_ptr = pos;
                if fdt_str_eq(name_ptr, b"#address-cells") && len >= 4 {
                    addr_cells = read_be_u32(val_ptr);
                } else if fdt_str_eq(name_ptr, b"#size-cells") && len >= 4 {
                    size_cells = read_be_u32(val_ptr);
                }
                let padded = (len + 3) & !3;
                pos = unsafe { pos.add(padded as usize) };
            }
            FDT_BEGIN_NODE => break,
            FDT_END_NODE => break,
            FDT_END => break,
            _ => { pos = unsafe { pos.add(4) }; }
        }
    }

    let mut region_count: usize = 0;
    let mut depth: u32 = 1;
    let mut in_memory = false;

    loop {
        let token = read_be_u32(pos);
        pos = unsafe { pos.add(4) };
        match token {
            FDT_BEGIN_NODE => {
                depth += 1;
                let node_name = pos;
                pos = skip_name(pos);
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
                let len = read_be_u32(pos);
                pos = unsafe { pos.add(4) };
                let nameoff = read_be_u32(pos);
                pos = unsafe { pos.add(4) };
                let name_ptr = fdt_string(&hdr, dtb, nameoff);
                let val_ptr = pos;
                if fdt_str_eq(name_ptr, b"reg") && region_count < MAX_MEMORY_REGIONS {
                    let mut offset = 0usize;
                    while (offset as u32) < len {
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
                                DTB_MEMORY_REGIONS[region_count] = MemoryRegion { base: addr, size, kind };
                            }
                            region_count += 1;
                        }
                    }
                }
                let padded = (len + 3) & !3;
                pos = unsafe { pos.add(padded as usize) };
            }
            FDT_PROP => {
                let len = read_be_u32(pos);
                pos = unsafe { pos.add(4) };
                let _ = read_be_u32(pos);
                pos = unsafe { pos.add(4) };
                let padded = (len + 3) & !3;
                pos = unsafe { pos.add(padded as usize) };
            }
            FDT_END => break,
            _ => {}
        }
    }

    if region_count > 0 {
        unsafe { &DTB_MEMORY_REGIONS[..region_count] }
    } else {
        fallback_memory()
    }
}

pub fn find_rsdp(dtb: *const u8) -> u64 {
    let hdr = match fdt_parse_header(dtb) {
        Some(h) => h,
        None => {
            SerialPort::puts("[kernel] riscv64: RSDP not found in DTB (ACPI not available)\n");
            return 0;
        }
    };

    let struct_base = unsafe { dtb.add(hdr.off_dt_struct as usize) };
    let mut pos: *const u8 = struct_base;

    pos = unsafe { pos.add(4) };
    pos = skip_name(pos);
    pos = align_ptr(pos);

    let mut depth: u32 = 1;
    let mut in_chosen = false;

    loop {
        let token = read_be_u32(pos);
        pos = unsafe { pos.add(4) };
        match token {
            FDT_BEGIN_NODE => {
                depth += 1;
                let node_name = pos;
                pos = skip_name(pos);
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
                pos = unsafe { pos.add(padded as usize) };
            }
            FDT_PROP => {
                let len = read_be_u32(pos);
                pos = unsafe { pos.add(4) };
                let _ = read_be_u32(pos);
                pos = unsafe { pos.add(4) };
                let padded = (len + 3) & !3;
                pos = unsafe { pos.add(padded as usize) };
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

    let struct_base = unsafe { dtb.add(hdr.off_dt_struct as usize) };
    let mut pos = struct_base;

    // Skip root node token + name.
    pos = unsafe { pos.add(4) };
    pos = skip_name(pos);
    pos = align_ptr(pos);

    let mut depth: u32 = 1;
    let mut in_cpus = false;

    loop {
        let token = read_be_u32(pos);
        pos = unsafe { pos.add(4) };
        match token {
            FDT_BEGIN_NODE => {
                depth += 1;
                let node_name = pos;
                pos = skip_name(pos);
                pos = align_ptr(pos);
                if depth == 2 && fdt_str_eq(node_name, b"cpus") {
                    in_cpus = true;
                } else if depth == 3 && in_cpus {
                    // This is a cpu@N node — collect reg, status, device_type.
                    let mut hart_id: u32 = 0;
                    let mut enabled = true;
                    let mut is_cpu = false;
                    let saved_pos = pos;
                    // Walk properties of this node.
                    let mut prop_pos = saved_pos;
                    loop {
                        let t = read_be_u32(prop_pos);
                        if t == FDT_PROP {
                            prop_pos = unsafe { prop_pos.add(4) };
                            let len = read_be_u32(prop_pos);
                            prop_pos = unsafe { prop_pos.add(4) };
                            let nameoff = read_be_u32(prop_pos);
                            prop_pos = unsafe { prop_pos.add(4) };
                            let name_ptr = fdt_string(&hdr, dtb, nameoff);
                            let val_ptr = prop_pos;
                            if fdt_str_eq(name_ptr, b"device_type") && len >= 3 {
                                is_cpu = unsafe {
                                    core::ptr::read_volatile(val_ptr) == b'c'
                                        && core::ptr::read_volatile(val_ptr.add(1)) == b'p'
                                        && core::ptr::read_volatile(val_ptr.add(2)) == b'u'
                                };
                            }
                            if fdt_str_eq(name_ptr, b"reg") && len >= 4 {
                                hart_id = read_be_u32(val_ptr);
                            } else if fdt_str_eq(name_ptr, b"status") && len >= 1 {
                                enabled = unsafe { *val_ptr } == b'o';
                            }
                            let padded = (len + 3) & !3;
                            prop_pos = unsafe { prop_pos.add(padded as usize) };
                        } else if t == FDT_BEGIN_NODE || t == FDT_END_NODE || t == FDT_END {
                            break;
                        } else {
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
                let len = read_be_u32(pos);
                pos = unsafe { pos.add(4) };
                let _ = read_be_u32(pos);
                pos = unsafe { pos.add(4) };
                let padded = (len + 3) & !3;
                pos = unsafe { pos.add(padded as usize) };
            }
            FDT_END => break,
            _ => {}
        }
    }

    cpus
}
