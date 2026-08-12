//! SYSCALL/SYSRET MSR programming and user-mode GS management.
//!
//! Nothing here runs at boot: `setup_syscall_msrs` is only called once the
//! syscall entry stub and user address space exist, so enabling these MSRs
//! never changes pre-user boot behavior.

use alloc::string::String;
use alloc::vec::Vec;

use x86_64::registers::model_specific::{
    Efer, EferFlags, GsBase, KernelGsBase, LStar, SFMask, Star,
};
use x86_64::registers::rflags::RFlags;
use x86_64::VirtAddr;

use crate::unispace::{self, UnispaceError};

/// STAR value written to IA32_STAR.
///
/// Per Intel: SYSCALL loads CS = STAR[32:47] and SS = STAR[32:47] + 8;
/// SYSRETQ loads CS = STAR[63:48] + 16 and SS = STAR[63:48] + 8 (both with
/// RPL forced to 3). With this value that yields:
///   SYSCALL  CS = 0x18 (dedicated SYSCALL landing descriptor) / SS = 0x20
///   SYSRETQ  CS = 0x28 | RPL3 = 0x2B (user code) / SS = 0x20 | RPL3 = 0x23
/// Note SYSCALL's SS = 0x20 is the *user* data descriptor — Intel performs no
/// DPL check on the SS loaded by SYSCALL (base is 0 in long mode), so this is
/// safe while SS effectively carries no privilege.
const IA32_STAR: u64 = (0x001Bu64 << 48) | (0x0018u64 << 32);

/// Enable SYSCALL/SYSRET (EFER.SCE) and program the syscall MSRs:
/// IA32_STAR, IA32_LSTAR = `entry`, IA32_SFMASK = IF.
///
/// SFMASK masks the interrupt flag: after `syscall` the kernel runs with
/// IF = 0 until the entry stub re-enables it. This closes the window between
/// the `syscall` instruction and `SET_GS_SEL 0x10` — with IF clear, no
/// maskable interrupt can fire while GS.base or the data segments still hold
/// the *user* state. The stub `sti`s once kernel GS/segments are live, so
/// blocking device syscalls can wait on completion IRQs; it `cli`s again
/// before reloading the user selectors for sysretq.
///
/// EFER's existing NXE/WP bits are preserved (`Efer::update` keeps reserved
/// and other set bits).
///
/// # Safety
/// `entry` must be a canonical, mapped kernel address holding the syscall
/// entry stub. On APs it must be the same value as on the BSP (per-CPU state
/// is reached through GS, not LSTAR).
pub fn setup_syscall_msrs(entry: u64) {
    unsafe {
        Efer::update(|e| {
            e.insert(EferFlags::SYSTEM_CALL_EXTENSIONS);
        });
        Star::write_raw((IA32_STAR >> 48) as u16, (IA32_STAR >> 32) as u16);
    }
    LStar::write(VirtAddr::new(entry));
    SFMask::write(RFlags::INTERRUPT_FLAG);
}

/// Establish the kernel/user GS pair used across a user-mode transition.
///
/// Invariant: the kernel runs with GS.base = the current PerCpu and
/// IA32_KERNEL_GS_BASE holding the user GS; `swapgs` toggles between them.
/// This helper (re)sets both sides: GS.base to the current PerCpu, and
/// KERNEL_GS_BASE to `user_gs` (0 for a process that never set one).
///
/// Used by the enter-user stub before dropping to ring 3.
pub fn set_user_gs(user_gs: u64) {
    let percpu = crate::smp::current_per_cpu() as *const _ as u64;
    GsBase::write(VirtAddr::new(percpu));
    KernelGsBase::write(VirtAddr::new(user_gs));
}

// ── Syscall frame ──────────────────────────────────────────────────
//
// Locked layout: 18 × u64 = 144 B. The general registers occupy the LOW 120
// bytes (r15 first, rax last) and the three user-state fields sit at the TOP
// (offsets 0x78..0x88). The frame pointer handed to `syscall_dispatch` is rsp
// after the entry stub has finished pushing.

/// Full register state saved across a syscall.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SyscallFrame {
    pub r15: u64,          // 0x00
    pub r14: u64,          // 0x08
    pub r13: u64,          // 0x10
    pub r12: u64,          // 0x18
    pub r11: u64,          // 0x20  — SYSCALL destroyed the user value; holds user RFLAGS
    pub r10: u64,          // 0x28
    pub r9: u64,           // 0x30
    pub r8: u64,           // 0x38
    pub rbp: u64,          // 0x40
    pub rdi: u64,          // 0x48
    pub rsi: u64,          // 0x50
    pub rdx: u64,          // 0x58
    pub rcx: u64,          // 0x60  — SYSCALL destroyed the user value; holds user RIP
    pub rbx: u64,          // 0x68
    pub rax: u64,          // 0x70  — syscall number in, return value out
    pub user_rsp: u64,     // 0x78
    pub user_rflags: u64,  // 0x80
    pub user_rip: u64,     // 0x88
}

const RAX_OFF: u64 = core::mem::offset_of!(SyscallFrame, rax) as u64;
const USER_RSP_OFF: u64 = core::mem::offset_of!(SyscallFrame, user_rsp) as u64;
const USER_RFLAGS_OFF: u64 = core::mem::offset_of!(SyscallFrame, user_rflags) as u64;
const USER_RIP_OFF: u64 = core::mem::offset_of!(SyscallFrame, user_rip) as u64;
const FRAME_SIZE: u64 = core::mem::size_of::<SyscallFrame>() as u64;

// ── syscall_entry asm ──────────────────────────────────────────────
//
// On entry (from SYSCALL): rcx = user RIP, r11 = user RFLAGS, rsp = user RSP,
// rax = syscall number, rdi/rsi/rdx/r10 = args. All other GP registers are
// untouched by the hardware.
//
// GS management: the swapgs invariant is preserved here. Loading a flat data
// selector into GS forces GS.base = 0, so every `mov gs, sel` is immediately
// followed by re-writing IA32_GS_BASE = PerCpu (captured from gs:[0] first).
// This keeps KERNEL_GS_BASE = user GS and GS.base = PerCpu at every swapgs.
// Interrupts are masked (SFMASK) on entry and stay masked until `SET_GS_SEL
// 0x10` has re-established the kernel data segments and GS.base; an ISR taken
// in that window (CPL0 frame, so no user-mode swapgs) has the correct kernel
// GS state. The stub enables IF for the dispatch body, then `cli`s before
// `SET_GS_SEL 0x23`, so no interrupt can fire while the user selectors are
// loaded but kernel code is still running (before sysretq re-enables IF).
core::arch::global_asm!(
    r#"
.macro SET_GS_SEL sel
    mov  r11, gs:[0]                # r11 = PerCpu (self_ptr) — GS.base still PerCpu here
    mov  ax, \sel
    mov  ds, ax
    mov  es, ax
    mov  fs, ax
    mov  gs, ax                     # GS.base := 0 (flat descriptor base)
    mov  ecx, 0xC0000101            # IA32_GS_BASE
    mov  eax, r11d                  # low 32 of PerCpu
    shr  r11, 32
    mov  edx, r11d                  # high 32 of PerCpu
    wrmsr                           # GS.base = PerCpu (kernel state restored)
.endm

.globl syscall_entry
.code64
syscall_entry:
    swapgs                          # GS.base = PerCpu, KERNEL_GS_BASE = user GS
    xchg rsp, gs:[{p_off}]          # rsp = kernel stack top; gs:[p_off] stages user RSP
    sub  rsp, {frame}               # frame_base = kernel_top - 144
    mov  [rsp + {rax_off}], rax     # syscall number -> frame.rax (rax now scratch)
    mov  rax, gs:[{p_off}]          # reload user RSP staged by the xchg above
    mov  [rsp + {u_rsp}], rax       # frame.user_rsp
    mov  [rsp + {u_rfl}], r11       # frame.user_rflags
    mov  [rsp + {u_rip}], rcx       # frame.user_rip
    lea  rsp, [rsp + 112]           # frame_base + 112: pushes fill 0x68..0x00
    push rbx                        # 0x68
    push rcx                        # 0x60 (user RIP, dead)
    push rdx                        # 0x58
    push rsi                        # 0x50
    push rdi                        # 0x48
    push rbp                        # 0x40
    push r8                         # 0x38
    push r9                         # 0x30
    push r10                        # 0x28
    push r11                        # 0x20 (user RFLAGS, dead)
    push r12                        # 0x18
    push r13                        # 0x10
    push r14                        # 0x08
    push r15                        # 0x00; rsp = frame_base
    lea  rcx, [rsp + {frame}]       # kernel_top
    mov  gs:[{p_off}], rcx          # restore the syscall_rsp0 mirror for the next syscall
    SET_GS_SEL 0x10                 # kernel data selectors + GS.base = PerCpu
    sti                             # kernel GS/segments live: open the interrupt window
    mov  rdi, rsp                   # &mut SyscallFrame
    call syscall_dispatch
    cli                             # close the window before the user selectors load
    SET_GS_SEL 0x23                 # user data selectors + GS.base = PerCpu (pre-swapgs)
    pop  r15
    pop  r14
    pop  r13
    pop  r12
    pop  r11
    pop  r10
    pop  r9
    pop  r8
    pop  rbp
    pop  rdi
    pop  rsi
    pop  rdx
    pop  rcx
    pop  rbx
    pop  rax                         # rsp now at frame.user_rsp (0x78)
    mov  rcx, [rsp + 16]             # user RIP
    mov  r11, [rsp + 8]              # user RFLAGS
    mov  rsp, [rsp]                  # user RSP
    swapgs                           # GS.base = user GS, KERNEL_GS_BASE = PerCpu
    sysretq
"#,
    p_off = const crate::smp::PERCPU_SYSCALL_RSP0_OFF,
    frame = const FRAME_SIZE,
    rax_off = const RAX_OFF,
    u_rsp = const USER_RSP_OFF,
    u_rfl = const USER_RFLAGS_OFF,
    u_rip = const USER_RIP_OFF,
);

/// Phase 7 syscall dispatcher — read/write/exit over the unispace namespace.
///
/// Args arrive in rdi/rsi/rdx/r10 (see the entry asm): rax = number, rdi =
/// arg0, rsi = arg1, rdx = arg2, r10 = arg3. The frame's `rax` slot carries
/// the return value out.
#[unsafe(no_mangle)]
pub extern "sysv64" fn syscall_dispatch(frame: &mut SyscallFrame) {
    match frame.rax {
        0 => sys_read(frame),
        1 => sys_write(frame),
        2 => sys_exit(frame),
        _ => frame.rax = (-1i64) as u64,
    }
}

// ── Syscall implementations ──────────────────────────────────────────

/// 0 read(path, path_len, buf, buf_len): read the object's value into `buf`.
///
/// The kernel never buffers more than `buf_len` bytes of the object, so a
/// hostile/reckless read cannot exhaust the heap regardless of how large the
/// source object is; `copy_user_out` further validates the buffer pages.
fn sys_read(frame: &mut SyscallFrame) {
    let path = match copy_user_string(frame.rdi, frame.rsi) {
        Ok(p) => p,
        Err(e) => {
            frame.rax = e as u64;
            return;
        }
    };
    let mut data = Vec::new();
    match unispace::read(&path, &mut data, core::cmp::min(frame.r10, MAX_COPY) as usize) {
        Ok(()) => match copy_user_out(frame.rdx, frame.r10, &data) {
            Ok(n) => frame.rax = n as u64,
            Err(e) => frame.rax = e as u64,
        },
        Err(e) => frame.rax = errno(e) as u64,
    }
}

/// 1 write(path, path_len, buf, buf_len): decode+validate `buf` as the
/// object's value schema and apply it.
fn sys_write(frame: &mut SyscallFrame) {
    let path = match copy_user_string(frame.rdi, frame.rsi) {
        Ok(p) => p,
        Err(e) => {
            frame.rax = e as u64;
            return;
        }
    };
    let data = match copy_user_in(frame.rdx, frame.r10) {
        Ok(d) => d,
        Err(e) => {
            frame.rax = e as u64;
            return;
        }
    };
    let mut resp = Vec::new();
    match unispace::write(&path, &data, &mut resp) {
        Ok(()) => frame.rax = data.len() as u64,
        Err(e) => frame.rax = errno(e) as u64,
    }
}

/// 2 exit(code): log and terminate the current task. Never returns.
fn sys_exit(frame: &mut SyscallFrame) -> ! {
    let code = frame.rdi;
    log::info!("[sched] init exit({})", code);
    crate::task::exit_current(code);
}

// ── User-pointer helpers ─────────────────────────────────────────────
//
// The kernel runs on the process CR3 throughout a syscall (the entry stub
// does not reload it), so user memory is directly addressable. The helpers
// still validate every page first — there is no page-fault handler installed
// for user pointers, so a raw copy at a bogus address would double-fault and
// abort the kernel. Malformed pointers only produce `Err`.

/// Top of the user canonical range; everything at or above this is kernel
/// (higher-half) space.
const USER_BOUNDARY: u64 = 0x0000_8000_0000_0000;

/// Largest single copy accepted, so a hostile length cannot force an
/// enormous page walk or allocation.
const MAX_COPY: u64 = 16 * 1024 * 1024;

/// Page-table root of the task that made this syscall.
fn current_task_root() -> Result<u64, i64> {
    let pc = crate::smp::current_per_cpu();
    if pc.current_task.is_null() {
        return Err(-1);
    }
    Ok(unsafe { (*(pc.current_task as *const crate::task::Task)).root })
}

/// True if `[ptr, ptr+len)` lies fully inside the user canonical range.
fn user_range_ok(ptr: u64, len: u64) -> bool {
    if ptr >= USER_BOUNDARY || len > MAX_COPY {
        return false;
    }
    ptr.checked_add(len).is_some_and(|end| end <= USER_BOUNDARY)
}

/// Validate that every page touched by `[ptr, ptr+len)` is present and
/// user-accessible in `root`. When `writable`, also requires the leaf to be
/// writable.
fn validate_user_range(root: u64, ptr: u64, len: u64, writable: bool) -> Result<(), i64> {
    if !user_range_ok(ptr, len) {
        return Err(-14); // EFAULT
    }
    if len == 0 {
        return Ok(());
    }
    let start = ptr & !0xFFF;
    let end = (ptr + len - 1) & !0xFFF;
    let mut va = start;
    loop {
        match crate::mm::vmm::translate_user(root, va) {
            Some((_, user, w)) if user && (!writable || w) => {}
            _ => return Err(-14),
        }
        if va == end {
            break;
        }
        va += 0x1000;
    }
    Ok(())
}

/// Copy a NUL-less byte string from user memory, validating UTF-8.
pub fn copy_user_string(ptr: u64, len: u64) -> Result<String, i64> {
    let bytes = copy_user_in(ptr, len)?;
    String::from_utf8(bytes).map_err(|_| -22) // EINVAL
}

/// Copy `len` raw bytes from user memory into a kernel buffer.
pub fn copy_user_in(ptr: u64, len: u64) -> Result<Vec<u8>, i64> {
    let root = current_task_root()?;
    if !user_range_ok(ptr, len) {
        return Err(-14);
    }
    validate_user_range(root, ptr, len, false)?;
    if len == 0 {
        return Ok(Vec::new());
    }
    let slice = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };
    Ok(slice.to_vec())
}

/// Copy `data` into a user buffer of size `len`. The full `len` range is
/// validated and written: `min(len, data.len())` bytes are copied and any
/// remainder is zero-filled, so the caller's buffer is always defined.
/// Returns the number of bytes copied.
pub fn copy_user_out(dst: u64, len: u64, data: &[u8]) -> Result<usize, i64> {
    let root = current_task_root()?;
    if !user_range_ok(dst, len) {
        return Err(-14);
    }
    validate_user_range(root, dst, len, true)?;
    let n = core::cmp::min(len, data.len() as u64) as usize;
    if n > 0 {
        unsafe {
            core::ptr::copy_nonoverlapping(data.as_ptr(), dst as *mut u8, n);
        }
    }
    if len as usize > n {
        unsafe {
            core::ptr::write_bytes((dst + n as u64) as *mut u8, 0, len as usize - n);
        }
    }
    Ok(n)
}

/// Map a unispace error to a negative errno for the caller's `rax`.
fn errno(e: UnispaceError) -> i64 {
    use UnispaceError::*;
    match e {
        NotFound => -2,        // ENOENT
        IsADirectory => -21,   // EISDIR
        NotADirectory => -20,  // ENOTDIR
        PermissionDenied => -13, // EACCES
        InvalidPath | DecodeError | SchemaMismatch => -22, // EINVAL
        MethodNotFound => -38, // ENOSYS
        Vfs(_) => -5,          // EIO
    }
}

/// Runtime address of the `syscall_entry` stub, for IA32_LSTAR.
pub fn syscall_entry_addr() -> u64 {
    unsafe extern "C" { static syscall_entry: u8; }
    core::ptr::addr_of!(syscall_entry) as u64
}
