#![no_std]
#![no_main]

#[cfg(target_arch = "riscv64")]
use core::arch::global_asm;
use core::panic::PanicInfo;
use kernel::arch::{Arch, CurrentArch};
use kernel::boot::{FramebufferInfo, MemoryRegion};
use kernel::drivers::serial::SerialPort;

/// Kernel entry point.
///
/// # Safety
/// Called from boot after exit_boot_services.
#[unsafe(no_mangle)]
#[cfg(target_arch = "x86_64")]
pub extern "sysv64" fn _start(
    memory_map_ptr: *const MemoryRegion,
    memory_map_len: usize,
    framebuffer_ptr: *const FramebufferInfo,
    stack_guard: u64,
    rsdp_addr: u64,
) -> ! {
    // Reinit COM1 — boot already did this, but be safe
    SerialPort::init();
    SerialPort::puts("[kernel] _start entered\n");

    // Validate pointers from bootloader before dereferencing
    assert!(!memory_map_ptr.is_null(), "memory_map_ptr is null");
    assert!(!framebuffer_ptr.is_null(), "framebuffer_ptr is null");
    SerialPort::puts("[kernel] Pointers OK\n");

    let framebuffer = unsafe { &*framebuffer_ptr };
    let memory_map = unsafe {
        core::slice::from_raw_parts(memory_map_ptr, memory_map_len)
    };

    SerialPort::puts("[kernel] Creating Kernel struct...\n");
    let mut kernel = unsafe { kernel::Kernel::new(memory_map, framebuffer, stack_guard, rsdp_addr) };
    SerialPort::puts("[kernel] Init...\n");
    kernel.init();
    SerialPort::puts("[kernel] Init complete, running modules...\n");
    kernel.run();
}

#[cfg(target_arch = "riscv64")]
global_asm!(
    r#"
.section .text.boot, "ax"
.globl _start
_start:
    /* Write '>' directly to UART at 0x10000000 */
    li t0, 0x10000000
1:  lbu t1, 5(t0)
    andi t1, t1, 0x20
    beqz t1, 1b
    li t1, 62
    sb t1, 0(t0)

    /* Set stack pointer */
    la sp, __stack_end
    mv s0, zero

    /* Zero BSS */
    la t0, __bss_start
    la t1, __bss_end
2:  beq t0, t1, 3f
    sd zero, 0(t0)
    addi t0, t0, 8
    j 2b
3:
    /* Jump to the Rust entry point (a0=hart_id, a1=dtb). */
    tail rust_entry
"#,
);

#[cfg(target_arch = "riscv64")]
const MAX_MEMORY_REGIONS: usize = 8;
#[cfg(target_arch = "riscv64")]
static mut DTB_MEMORY_REGIONS: [MemoryRegion; MAX_MEMORY_REGIONS] = unsafe { core::mem::zeroed() };

#[cfg(target_arch = "riscv64")]
#[unsafe(no_mangle)]
pub extern "C" fn rust_entry(hart_id: u64, dtb_ptr: *const u8) -> ! {
    use kernel::boot::PixelFormat;
    SerialPort::init();
    SerialPort::puts("[kernel] riscv64 _start entered, hart_id=");
    SerialPort::put_u64(hart_id);
    SerialPort::puts("\n");

    // Store hart_id for PLIC (reads mhartid are illegal in S-mode).
    use core::sync::atomic::Ordering;
    kernel::platform::riscv_virt::plic::HART_ID.store(hart_id as usize, Ordering::Relaxed);

    // Debug: print DTB pointer and first 8 bytes.
    SerialPort::puts("[kernel] DTB ptr=0x");
    SerialPort::put_hex(dtb_ptr as u64);
    SerialPort::puts(", magic=0x");
    if !dtb_ptr.is_null() {
        SerialPort::put_hex(unsafe {
            u64::from(
                (core::ptr::read_volatile(dtb_ptr) as u32) << 24
                    | (core::ptr::read_volatile(dtb_ptr.add(1)) as u32) << 16
                    | (core::ptr::read_volatile(dtb_ptr.add(2)) as u32) << 8
                    | (core::ptr::read_volatile(dtb_ptr.add(3)) as u32)
            )
        });
    } else {
        SerialPort::puts("NULL");
    }
    SerialPort::puts("\n");

    // Parse memory map and RSDP from DTB.
    let memory_map = riscv_parse_dtb(dtb_ptr);
    let rsdp_addr = riscv_find_rsdp(dtb_ptr);

    // Compute stack guard: one unmapped page just below the stack area.
    let stack_guard = unsafe {
        let stack_start = &kernel::__stack_start as *const u8 as u64;
        stack_start - 4096
    };

    static FB_INFO: FramebufferInfo = FramebufferInfo {
        address: 0, width: 0, height: 0, stride: 0,
        pixel_format: PixelFormat::Bgr,
    };

    SerialPort::puts("[kernel] Creating Kernel struct...\n");
    let mut kernel = unsafe { kernel::Kernel::new(memory_map, &FB_INFO, stack_guard, rsdp_addr) };
    SerialPort::puts("[kernel] Init...\n");
    kernel.init();
    SerialPort::puts("[kernel] Init complete, running modules...\n");
    kernel.run();
}

// ── Minimal DTB parser (RISC-V only) ─────────────────────────────────

#[cfg(target_arch = "riscv64")]
const FDT_MAGIC: u32 = 0xD00DFEED;
#[cfg(target_arch = "riscv64")]
const FDT_BEGIN_NODE: u32 = 0x00000001;
#[cfg(target_arch = "riscv64")]
const FDT_END_NODE: u32 = 0x00000002;
#[cfg(target_arch = "riscv64")]
const FDT_PROP: u32 = 0x00000003;
#[cfg(target_arch = "riscv64")]
const FDT_END: u32 = 0x00000009;

#[cfg(target_arch = "riscv64")]
#[derive(Clone, Copy, Default)]
#[allow(dead_code)]
struct FdtHeader {
    magic: u32,
    totalsize: u32,
    off_dt_struct: u32,
    off_dt_strings: u32,
    off_mem_rsvmap: u32,
    version: u32,
    size_dt_strings: u32,
    size_dt_struct: u32,
}

#[cfg(target_arch = "riscv64")]
fn read_be_u32(ptr: *const u8) -> u32 {
    unsafe {
        let b0 = core::ptr::read_volatile(ptr) as u32;
        let b1 = core::ptr::read_volatile(ptr.add(1)) as u32;
        let b2 = core::ptr::read_volatile(ptr.add(2)) as u32;
        let b3 = core::ptr::read_volatile(ptr.add(3)) as u32;
        (b0 << 24) | (b1 << 16) | (b2 << 8) | b3
    }
}

#[cfg(target_arch = "riscv64")]
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
        totalsize: read_be_u32(unsafe { dtb.add(4) }),
        off_dt_struct: read_be_u32(unsafe { dtb.add(8) }),
        off_dt_strings: read_be_u32(unsafe { dtb.add(12) }),
        off_mem_rsvmap: read_be_u32(unsafe { dtb.add(16) }),
        version: read_be_u32(unsafe { dtb.add(20) }),
        size_dt_strings: read_be_u32(unsafe { dtb.add(32) }),
        size_dt_struct: read_be_u32(unsafe { dtb.add(36) }),
    })
}

#[cfg(target_arch = "riscv64")]
fn fdt_string(hdr: &FdtHeader, dtb: *const u8, nameoff: u32) -> *const u8 {
    unsafe { dtb.add(hdr.off_dt_strings as usize + nameoff as usize) }
}

#[cfg(target_arch = "riscv64")]
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

/// Parse the DTB to populate memory regions and return a slice.
///
/// Falls back to the hardcoded 256 MB QEMU virt layout on failure.
#[cfg(target_arch = "riscv64")]
fn riscv_parse_dtb(dtb: *const u8) -> &'static [MemoryRegion] {
    use kernel::boot::MemoryRegionKind;
    let hdr = match fdt_parse_header(dtb) {
        Some(h) => h,
        None => return riscv_fallback_memory(),
    };

    // Read #address-cells and #size-cells from the root node.
    let mut addr_cells: u32 = 2;
    let mut size_cells: u32 = 2;

    let struct_base = unsafe { dtb.add(hdr.off_dt_struct as usize) };
    let mut pos: *const u8 = struct_base;

    // Skip past the root node's name (BEGIN_NODE token + name string).
    pos = unsafe { pos.add(4) };
    while unsafe { core::ptr::read_volatile(pos) } != 0 {
        pos = unsafe { pos.add(1) };
    }
    pos = unsafe { pos.add(1) };
    pos = align_ptr(pos);

    // Scan root properties for #address-cells and #size-cells.
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

    // Now scan for /memory node.
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
                while unsafe { core::ptr::read_volatile(pos) } != 0 {
                    pos = unsafe { pos.add(1) };
                }
                pos = unsafe { pos.add(1) };
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
        riscv_fallback_memory()
    }
}

#[cfg(target_arch = "riscv64")]
fn read_be_n(ptr: *const u8, cells: u32) -> u64 {
    let mut val: u64 = 0;
    for i in 0..cells {
        let word = read_be_u32(unsafe { ptr.add(i as usize * 4) });
        val = (val << 32) | word as u64;
    }
    val
}

#[cfg(target_arch = "riscv64")]
fn align_ptr(p: *const u8) -> *const u8 {
    let addr = p as usize;
    ((addr + 3) & !3) as *const u8
}

#[cfg(target_arch = "riscv64")]
fn riscv_fallback_memory() -> &'static [MemoryRegion] {
    use kernel::boot::MemoryRegionKind;
    static FALLBACK: [MemoryRegion; 3] = [
        MemoryRegion { base: 0x80050000, size: 0x0FFB0000, kind: MemoryRegionKind::Usable },
        MemoryRegion { base: 0x00100000, size: 0x00001000, kind: MemoryRegionKind::Reserved },
        MemoryRegion { base: 0x80000000, size: 0x00050000, kind: MemoryRegionKind::Reserved },
    ];
    &FALLBACK
}

/// Locate the ACPI RSDP on RISC-V.
///
/// First attempts to read the `acpi-rsdp` property from the `chosen` node
/// of the device-tree blob.  Falls back to the QEMU virt default address
/// (`0x7FE0`) if the DTB is absent or lacks the property.
#[cfg(target_arch = "riscv64")]
fn riscv_find_rsdp(dtb: *const u8) -> u64 {
    let hdr = match fdt_parse_header(dtb) {
        Some(h) => h,
        None => {
            SerialPort::puts("[kernel] riscv64: RSDP not found in DTB, trying QEMU virt fallback 0x7FE0\n");
            return 0x7FE0;
        }
    };

    let struct_base = unsafe { dtb.add(hdr.off_dt_struct as usize) };
    let mut pos: *const u8 = struct_base;

    // Skip root node name (BEGIN_NODE token + name string).
    pos = unsafe { pos.add(4) };
    while unsafe { core::ptr::read_volatile(pos) } != 0 {
        pos = unsafe { pos.add(1) };
    }
    pos = unsafe { pos.add(1) };
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
                while unsafe { core::ptr::read_volatile(pos) } != 0 {
                    pos = unsafe { pos.add(1) };
                }
                pos = unsafe { pos.add(1) };
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

    SerialPort::puts("[kernel] riscv64: RSDP not found in DTB, trying QEMU virt fallback 0x7FE0\n");
    0x7FE0
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    SerialPort::puts("\n*** KERNEL PANIC: ");
    if let Some(loc) = info.location() {
        SerialPort::puts(loc.file());
        SerialPort::puts(":");
        SerialPort::put_u64(loc.line() as u64);
        SerialPort::puts(" ");
    }
    use core::fmt::Write;
    let _ = write!(SerialPort::new(), "{}", info.message());
    SerialPort::puts("\n");
    // Disable interrupts and spin forever with wfi to prevent reboot.
    CurrentArch::disable_interrupts();
    loop {
        // wfi may be a NOP when interrupts are disabled on some implementations;
        // busy-wait with a fence to prevent the compiler from optimising the
        // loop away entirely.
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
        CurrentArch::halt();
    }
}
