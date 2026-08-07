//! Process management and user-mode entry (x86_64).
//!
//! Single-task cooperative scheduler: load init, enter ring 3, handle
//! syscalls as they come in. When init exits, return to kernel idle.

extern crate alloc;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use spin::Once;

use crate::arch::x86_64::gdt;
use crate::drivers::serial::SerialPort;
use crate::mm::elf::{self, ElfError};
use crate::mm::phys_alloc::BitmapAllocator;
use crate::mm::vmm::{PageFlags, Vmm};
use crate::obj::cap_handle::CapId;
use crate::obj::domain::{self, Domain};

/// User stack: 8 KB, located near the top of the low canonical half.
const USER_STACK_SIZE: usize = 8 * 1024;
const USER_STACK_TOP: u64 = 0x7FFF_FFFF_F000;

/// Has the init process exited?
static INIT_EXITED: AtomicBool = AtomicBool::new(false);
/// Init exit code.
static INIT_EXIT_CODE: AtomicI64 = AtomicI64::new(0);

/// The capability endowment handed to the ring-3 init process at creation.
///
/// The process ABI is positional: the first eight slots of the user domain's
/// table are these capabilities, in insertion order (0=serial, 1=mount,
/// 2=registry, 3=physmem, 4=heap, 5=addrspace, 6=block, 7=table).
#[derive(Clone, Copy)]
pub struct ProcessEndowment {
    pub serial: CapId,
    pub mount: CapId,
    pub registry: CapId,
    pub physmem: CapId,
    pub heap: CapId,
    pub addrspace: CapId,
    pub block: CapId,
    pub table: CapId,
}

static PROCESS_ENDOWMENT: Once<ProcessEndowment> = Once::new();

/// The init process's capability endowment, once `run_init` has endowed it.
pub fn process_endowment() -> &'static ProcessEndowment {
    PROCESS_ENDOWMENT.get().expect("process endowment not set")
}

/// Load and run the init program from the ESP.
///
/// Called after the ESP is mounted as B:\. Reads `\EFI\BEDROCK\INIT`,
/// creates a user domain, loads the ELF, and enters ring 3.
pub fn run_init(alloc: &mut BitmapAllocator, kernel_root: u64) -> Result<(), &'static str> {
    SerialPort::puts("[proc] Loading init from ESP...\n");

    let init_data = read_init_from_esp()?;
    SerialPort::puts("[proc] Init binary read, ");
    SerialPort::put_u64(init_data.len() as u64);
    SerialPort::puts(" bytes\n");

    // Create user domain with its own address space (clone high half, empty low).
    let user_domain = Domain::with_addrspace(100, kernel_root);

    // Register the user domain so the projection tool and leak detector see it
    // (`find_domain(100)` resolves it), then endow it with the eight process
    // capabilities from the boot table. Delegation order is the documented
    // process ABI: the CapIds become 0..7 in insertion order (0=serial,
    // 1=mount, 2=registry, 3=physmem, 4=heap, 5=addrspace, 6=block, 7=table).
    domain::register_domain(user_domain);
    let serial = crate::obj::bootstrap::boot_domain()
        .table
        .delegate(&user_domain.table, crate::obj::bootstrap::boot_endowment().serial)
        .map_err(|_| "process endowment failed")?;
    let mount = crate::obj::bootstrap::boot_domain()
        .table
        .delegate(&user_domain.table, crate::obj::bootstrap::boot_endowment().mount)
        .map_err(|_| "process endowment failed")?;
    let registry = crate::obj::bootstrap::boot_domain()
        .table
        .delegate(&user_domain.table, crate::obj::bootstrap::boot_endowment().registry)
        .map_err(|_| "process endowment failed")?;
    let physmem = crate::obj::bootstrap::boot_domain()
        .table
        .delegate(&user_domain.table, crate::obj::bootstrap::boot_endowment().physmem)
        .map_err(|_| "process endowment failed")?;
    let heap = crate::obj::bootstrap::boot_domain()
        .table
        .delegate(&user_domain.table, crate::obj::bootstrap::boot_endowment().heap)
        .map_err(|_| "process endowment failed")?;
    let addrspace = crate::obj::bootstrap::boot_domain()
        .table
        .delegate(&user_domain.table, crate::obj::bootstrap::boot_endowment().addrspace)
        .map_err(|_| "process endowment failed")?;
    let block = crate::obj::bootstrap::boot_domain()
        .table
        .delegate(&user_domain.table, crate::obj::bootstrap::boot_endowment().block)
        .map_err(|_| "process endowment failed")?;
    let table = crate::obj::bootstrap::boot_domain()
        .table
        .delegate(&user_domain.table, crate::obj::bootstrap::boot_endowment().table)
        .map_err(|_| "process endowment failed")?;
    PROCESS_ENDOWMENT.call_once(|| ProcessEndowment {
        serial,
        mount,
        registry,
        physmem,
        heap,
        addrspace,
        block,
        table,
    });

    let root = user_domain.page_root().ok_or("no addrspace")?;
    // A handle to the same page-table root; the loader maps into it and
    // `set_current_domain` below activates it (via CR3 switch).
    let mut vmm = Vmm::from_root(root);

    // Load ELF into the user address space.
    let entry = match elf::load_elf(&init_data, &mut vmm, alloc) {
        Ok(e) => e,
        Err(ElfError::NotElf) => return Err("not an ELF"),
        Err(ElfError::Not64Bit) => return Err("not 64-bit"),
        Err(ElfError::NotLittleEndian) => return Err("not little-endian"),
        Err(ElfError::NotExecutable) => return Err("not executable"),
        Err(ElfError::WrongMachine) => return Err("wrong machine type"),
        Err(ElfError::InvalidPhdr) => return Err("invalid program header"),
        Err(ElfError::OutOfMemory) => return Err("out of memory"),
        Err(ElfError::SegmentTooLarge) => return Err("segment too large"),
    };

    SerialPort::puts("[proc] ELF loaded, entry at 0x");
    SerialPort::put_hex(entry);
    SerialPort::puts("\n");

    // Allocate the user stack (8 KB RW+USER near the top of the low half).
    let stack_bottom = USER_STACK_TOP - USER_STACK_SIZE as u64;
    for page in (stack_bottom..USER_STACK_TOP).step_by(4096) {
        let frame = alloc.alloc().ok_or("OOM for user stack")?;
        vmm.map_4k(alloc, page, frame, PageFlags::READ | PageFlags::WRITE | PageFlags::USER);
    }

    let user_rsp = USER_STACK_TOP - 16;

    // Switch to the user domain (CR3 switch). The kernel high half is shared,
    // so the syscall stacks, per-CPU data and current kernel stack stay
    // reachable under the user domain's page tables.
    domain::set_current_domain(user_domain);

    SerialPort::puts("[proc] Entering user mode at 0x");
    SerialPort::put_hex(entry);
    SerialPort::puts("\n");

    // Enter ring 3 — this does NOT return (the syscall handler runs in ring 0,
    // sysretq goes back to user mode on return).
    unsafe {
        enter_user_mode(entry, user_rsp);
    }
}

/// Transition from ring 0 to ring 3 using `sysretq`.
///
/// # Safety
/// Must be called with a valid user entry point, user RSP, and user segments
/// configured in the GDT. Syscall MSRs must be initialized.
///
/// GS is deliberately NOT reloaded: loading a data selector with RPL 3 would
/// zero `GS.base`, which would break the kernel's per-CPU access from user-mode
/// interrupts. GS.base stays on the kernel `PerCpu` struct in both ring 3 and
/// ring 0 (the syscall entry and every ISR depend on this), and user code never
/// touches GS.
unsafe fn enter_user_mode(entry: u64, user_rsp: u64) -> ! {
    unsafe {
        core::arch::asm!(
            "mov ax, {user_ds}",
            "mov ds, ax",
            "mov es, ax",
            "mov fs, ax",
            "mov rcx, {entry}",
            "mov r11, 0x202",
            "mov rsp, {user_rsp}",
            "sysretq",
            user_ds = const gdt::USER_DS_SELECTOR,
            entry = in(reg) entry,
            user_rsp = in(reg) user_rsp,
            options(noreturn),
        );
    }
}

/// Called by the exit syscall handler. Records exit and halts.
pub fn exit_process(code: i64) -> ! {
    INIT_EXITED.store(true, Ordering::SeqCst);
    INIT_EXIT_CODE.store(code, Ordering::SeqCst);

    SerialPort::puts("[proc] init exited with code ");
    SerialPort::put_u64(code as u64);
    SerialPort::puts("\n");

    // We're in ring 0 (syscall handler context). Halt.
    loop {
        crate::arch::CurrentArch::halt();
    }
}

/// Read the init binary from the mounted ESP (B:\EFI\BEDROCK\INIT).
fn read_init_from_esp() -> Result<Vec<u8>, &'static str> {
    use crate::filesystems::vfs::inode::InodeOps;

    // Get the mounted B: drive (ESP).
    let mount = crate::filesystems::vfs::get_mount('B').ok_or("B: not mounted")?;

    // Get root inode ops from the mount's root dentry.
    let root_inode = mount.root.inode.lock();
    let root_inode_arc = root_inode.as_ref().ok_or("no root inode")?;
    let root_ops: &dyn InodeOps = &*root_inode_arc.ops;

    // Walk B:\EFI\BEDROCK\INIT.
    let efi = root_ops.lookup("EFI").map_err(|_| "EFI not found")?;
    let bedrock = efi.lookup("BEDROCK").map_err(|_| "BEDROCK not found")?;
    let init = bedrock.lookup("INIT").map_err(|_| "INIT not found")?;

    let size = init.size() as usize;
    if size == 0 || size > 16 * 1024 * 1024 {
        return Err("init file invalid size");
    }

    let mut data = alloc::vec![0u8; size];
    init.read_at(0, &mut data).map_err(|_| "read failed")?;

    Ok(data)
}
