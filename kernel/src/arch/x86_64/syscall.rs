//! Syscall/sysret infrastructure for x86_64.
//!
//! Uses `IA32_STAR` MSR to define kernel/user segment selectors,
//! `IA32_LSTAR` for the syscall entry point, and `IA32_FMASK` to
//! clear RFLAGS.IF and RFLAGS.DF on entry. `EFER.SCE` enables the
//! `syscall`/`sysret` instructions.
//!
//! # GS design
//!
//! This kernel keeps its per-CPU data in `GS.base` (see `smp::PerCpu`), and the
//! interrupt path never swaps GS. User mode therefore does **not** load a user
//! GS selector — `enter_user_mode` leaves GS untouched, so `GS.base` stays
//! pointing at the kernel `PerCpu` struct in both ring 3 and ring 0. The syscall
//! entry uses that base to stash the user RSP and pick up the dedicated syscall
//! stack; no `swapgs` is needed anywhere.

use core::cell::UnsafeCell;
use core::mem::offset_of;

use x86_64::registers::model_specific::{Efer, EferFlags, Msr};

use crate::smp::{MAX_CPUS, PerCpu, current_per_cpu};

const IA32_LSTAR: u32 = 0xC000_0082;
const IA32_FMASK: u32 = 0xC000_0084;

/// Size of the per-CPU syscall stack.
const SYSCALL_STACK_SIZE: usize = 16 * 1024;

/// Saved user registers on syscall entry.
///
/// Field order matches the push sequence in `syscall_entry`: r15 is pushed
/// last, so it lands at the lowest address (`UserFrame` offset 0).
#[repr(C)]
pub struct UserFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub rbx: u64,
    pub rbp: u64,
    pub rcx: u64,       // saved user RIP
    pub r11: u64,       // saved user RFLAGS
    pub rax: u64,       // syscall number / return value
    pub rdi: u64,       // arg 0
    pub rsi: u64,       // arg 1
    pub rdx: u64,       // arg 2
    pub r10: u64,       // table version
}

/// Per-CPU syscall stacks (16 KiB each). Live for kernel lifetime, in the
/// higher half, so they stay reachable under every cloned domain's page
/// tables (the syscall handler runs with the user domain's CR3 active).
struct Shared<T>(UnsafeCell<T>);

unsafe impl Sync for Shared<[[u8; SYSCALL_STACK_SIZE]; MAX_CPUS]> {}
unsafe impl Send for Shared<[[u8; SYSCALL_STACK_SIZE]; MAX_CPUS]> {}

static SYSCALL_STACKS: Shared<[[u8; SYSCALL_STACK_SIZE]; MAX_CPUS]> =
    Shared(UnsafeCell::new([[0; SYSCALL_STACK_SIZE]; MAX_CPUS]));

fn syscall_stacks() -> &'static mut [[u8; SYSCALL_STACK_SIZE]; MAX_CPUS] {
    // SAFETY: each CPU only ever touches its own slot, and only the BSP's
    // slot is written (in `init`) / read (by the syscall entry).
    unsafe { &mut *SYSCALL_STACKS.0.get() }
}

/// Initialize the syscall MSRs and the BSP's syscall stack for the current CPU.
pub fn init() {
    unsafe {
        // IA32_STAR MSR layout:
        //   bits 47:32 = Syscall CS (loaded directly, SS = CS + 8)
        //   bits 63:48 = Sysret base (CS = base + 16, SS = base + 8, both RPL=3)
        //
        // We want:
        //   syscall: CS = 0x08 (kernel code), SS = 0x10 (kernel data)
        //   sysret:  CS = 0x23 (user code), SS = 0x1B (user data)
        //
        // IA32_STAR[47:32] = 0x08 → syscall CS=0x08, SS=0x10 ✓
        // IA32_STAR[63:48] = 0x13 → sysret CS=0x23, SS=0x1B ✓
        let star_val: u64 = (0x13_u64 << 48) | (0x08_u64 << 32);
        Msr::new(0xC000_0081).write(star_val);

        // IA32_LSTAR: syscall entry point.
        Msr::new(IA32_LSTAR).write(syscall_entry as u64);

        // IA32_FMASK: clear IF (bit 9) and DF (bit 10) on syscall entry.
        Msr::new(IA32_FMASK).write(0x0600);

        // Enable syscall/sysret in EFER.
        Efer::update(|flags| flags.insert(EferFlags::SYSTEM_CALL_EXTENSIONS));
    }

    // Point the BSP's per-CPU syscall stack at the top of its dedicated slot.
    // (Only the BSP runs user code in this stage; APs keep `syscall_stack` 0.)
    let pc = current_per_cpu();
    pc.syscall_stack =
        (&syscall_stacks()[0] as *const u8 as u64) + SYSCALL_STACK_SIZE as u64;
    pc.syscall_user_rsp = 0;

    crate::drivers::serial::SerialPort::puts("[syscall] MSRs initialized\n");
}

/// Syscall entry point — called by `syscall` instruction from ring 3.
///
/// On entry:
///   RAX = syscall number
///   RDI = arg 0
///   RSI = arg 1
///   RDX = arg 2
///   R10 = syscall table version
///   RCX = clobbered (saved RIP by `syscall`)
///   R11 = clobbered (saved RFLAGS by `syscall`)
///
/// `GS.base` is the kernel `PerCpu` in both ring 3 and ring 0 (user mode never
/// loads GS), so no `swapgs` is performed.
#[unsafe(naked)]
pub unsafe extern "C" fn syscall_entry() {
    core::arch::naked_asm!(
        // Save user RSP into the per-CPU scratch and switch to the syscall stack.
        "mov gs:[{usr}], rsp",
        "mov rsp, gs:[{kst}]",

        // Save user registers to the stack, building a `UserFrame`.
        // Push order matches the struct layout: r10 first, r15 last (lowest
        // address, i.e. what `mov rdi, rsp` hands the handler).
        "push r10",                  // table version
        "push rdx",                  // arg 2
        "push rsi",                  // arg 1
        "push rdi",                  // arg 0
        "push rax",                  // syscall number
        "push r11",                  // saved RFLAGS
        "push rcx",                  // saved RIP
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",

        // Call the Rust handler with a pointer to the saved frame.
        "mov rdi, rsp",
        "call {handler}",

        // RAX = return value. Store it into the frame's rax field (offset
        // `rax` within `UserFrame`).
        "mov [rsp + {rax_off}], rax",

        // Restore user registers (reverse of the push order).
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "pop rcx",
        "pop r11",
        "pop rax",                   // return value
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop r10",

        "mov rsp, gs:[{usr}]",       // restore user RSP
        "sysretq",
        usr = const offset_of!(PerCpu, syscall_user_rsp),
        kst = const offset_of!(PerCpu, syscall_stack),
        rax_off = const offset_of!(UserFrame, rax),
        handler = sym syscall_handler,
    );
}

/// Rust syscall dispatcher.
///
/// Receives a pointer to the saved user register frame. Reads syscall
/// number and args from the frame, dispatches, and writes the return
/// value back to the frame (in rax).
#[unsafe(no_mangle)]
pub extern "C" fn syscall_handler(frame: *const UserFrame) -> u64 {
    let frame = unsafe { &*frame };
    let num = frame.rax;
    let arg0 = frame.rdi;
    let arg1 = frame.rsi;
    let arg2 = frame.rdx;
    let table_ver = frame.r10;

    // Route by the table version the caller passed in R10; dispatch selects
    // the right table and rejects unknown versions.
    crate::syscall::dispatch(table_ver, num, arg0, arg1, arg2)
}
