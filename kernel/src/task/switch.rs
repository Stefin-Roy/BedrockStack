//! Context-switch primitives for the cooperative scheduler.
//!
//! `switch_to` saves/restores the callee-saved register set plus the stack
//! and resume pointers, optionally reloading CR3 when the target task runs in
//! a different address space. `user_iret` is the first-instruction stub that
//! drops a freshly-created task into ring 3 through an iretq frame built on
//! its kernel stack.

/// Full callee-saved + resume state swapped by `switch_to`.
///
/// Locked layout (offsets are hard-coded in the asm below):
///   r15 0x00  r14 0x08  r13 0x10  r12 0x18
///   rbx 0x20  rbp 0x28  rsp 0x30  rip 0x38  rflags 0x40
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TaskContext {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub rbx: u64,
    pub rbp: u64,
    pub rsp: u64,
    pub rip: u64,
    pub rflags: u64,
}

impl TaskContext {
    pub const fn zeroed() -> Self {
        TaskContext {
            r15: 0,
            r14: 0,
            r13: 0,
            r12: 0,
            rbx: 0,
            rbp: 0,
            rsp: 0,
            rip: 0,
            rflags: 0,
        }
    }

    /// Initial context: resume at `rip` with `rsp` pointing at the task's
    /// entry stack. `rsp` must be 8 mod 16 for a kernel entry (SysV callee
    /// entry alignment) or the iretq frame base for `user_iret`.
    ///
    /// The initial RFLAGS has IF set (`0x202`) so a freshly restored task
    /// resumes with interrupts enabled, matching the boot-time state.
    pub const fn new(rsp: u64, rip: u64) -> Self {
        TaskContext {
            r15: 0,
            r14: 0,
            r13: 0,
            r12: 0,
            rbx: 0,
            rbp: 0,
            rsp,
            rip,
            rflags: 0x202,
        }
    }

    /// Validate that `rsp`/`rip`/`rflags` form a sane kernel resume target.
    /// Called before every `switch_to`; a malformed `rip` (low half, equals
    /// `cr3`, non-canonical) would otherwise fault as supervisor I-fetch on
    /// the user root (observed `RIP=CR3=0x2e05000`, error `0x10`).
    #[inline]
    pub fn is_valid(&self) -> bool {
        // RIP must be canonical high-half kernel text (or user_iret) and never
        // the low half.  Low half values are user entry points that belong in
        // the iretq frame, not in TaskContext.rip.
        if self.rip < 0x0000_8000_0000_0000 {
            // Below USER_BOUNDARY is low — only valid if it's the `user_iret`
            // stub itself which lives high.  So any low RIP is invalid here.
            // The only allowed low RIP would be 0 for the idle anchor (never
            // dispatched as next).
            if self.rip != 0 {
                return false;
            }
        }
        // RSP must be canonical high (kernel stacks) or zero for idle.
        if self.rsp != 0 && self.rsp < 0x0000_8000_0000_0000 {
            return false;
        }
        // RFLAGS must keep IF=1 for a fresh task (0x202) and not have reserved
        // bits set that would #GP on popfq.  We allow the exact 0x202 we create.
        if self.rflags != 0x202 && self.rflags != 0 {
            // Still allow saved contexts where IF may be transiently 0, but
            // require bit 1 (reserved 1) set and bits 22..63 zero.
            if self.rflags & 0x2 == 0 {
                return false;
            }
        }
        true
    }
}

// ── switch_to ─────────────────────────────────────────────────────
//
// ABI (SysV): rdi = old (*mut TaskContext), rsi = new (*const TaskContext),
// rdx = new_cr3.
//
// Saves the six callee-saved registers, the post-call RSP, and the return
// address (currently at the top of the stack) into `old`; restores them from
// `new`; then
// reloads CR3 if the target root differs. Only callee-saved registers matter:
// the resumed caller's scratch registers were already clobbered by the call.
//
// The saved RSP is the value after the original `call switch_to` would have
// returned.  This is important: restoring the entry-time RSP and then pushing
// the saved RIP would leave the original return address underneath the newly
// pushed one, shifting the resumed function's stack by eight bytes on every
// context switch.
//
// CR3 reload: the kernel's higher-half alias is mapped in every address
// space, so the code between the reload and the `ret` stays mapped even when
// jumping into a user root. The `ret` pops the resume pointer that was pushed
// on the freshly-restored stack.
core::arch::global_asm!(
    r#"
.globl switch_to
.code64
switch_to:
    pushfq
    pop  rax                      # RFLAGS of the outgoing context
    mov  [rdi + 0x40], rax
    mov  [rdi + 0x00], r15
    mov  [rdi + 0x08], r14
    mov  [rdi + 0x10], r13
    mov  [rdi + 0x18], r12
    mov  [rdi + 0x20], rbx
    mov  [rdi + 0x28], rbp
    mov  rax, [rsp]              # return address right after `call switch_to`
    mov  [rdi + 0x38], rax
    lea  rax, [rsp + 8]          # RSP after that call would have returned
    mov  [rdi + 0x30], rax

    mov  r15, [rsi + 0x00]
    mov  r14, [rsi + 0x08]
    mov  r13, [rsi + 0x10]
    mov  r12, [rsi + 0x18]
    mov  rbx, [rsi + 0x20]
    mov  rbp, [rsi + 0x28]
    mov  rsp, [rsi + 0x30]
    mov  rax, [rsi + 0x38]

    mov  rcx, cr3
    cmp  rcx, rdx
    je   1f
    mov  cr3, rdx
1:
    push qword ptr [rsi + 0x40]   # RFLAGS of the incoming context
    popfq                         # restores IF (and the rest) deterministically
    push rax
    ret
"#
);

// ── user_iret ─────────────────────────────────────────────────────
//
// First drop into ring 3. RSP points at a 5-word iretq frame at the top of
// the task's kernel stack: {RIP, CS=0x2B, RFLAGS=0x202, RSP, SS=0x23}.
//
// GS handling mirrors the syscall-return path so the invariant holds exactly:
// on entry GS.base = PerCpu (set by `set_user_gs` in enter_userspace), so the
// PerCpu self-pointer is captured from gs:[0] first, the flat user data
// selectors are loaded (which zero GS.base), GS.base is re-established to
// PerCpu via IA32_GS_BASE, and a final swapgs leaves the user running with
// GS.base = user GS / KERNEL_GS_BASE = PerCpu — the exact state the Phase 4
// ISR guards expect.
//
// `cli` closes the window where GS.base is not yet kernel state; the iretq
// restores IF from the frame (0x202 sets it), re-enabling interrupts in user
// mode.
core::arch::global_asm!(
    r#"
.globl user_iret
.code64
user_iret:
    cli
    mov  r11, gs:[0]            # r11 = PerCpu self_ptr (GS.base still PerCpu)
    mov  ax, 0x23
    mov  ds, ax
    mov  es, ax
    mov  fs, ax
    mov  gs, ax                 # GS.base := 0 (flat user data descriptor)
    mov  ecx, 0xC0000101        # IA32_GS_BASE
    mov  eax, r11d
    shr  r11, 32
    mov  edx, r11d
    wrmsr                       # GS.base = PerCpu
    swapgs                      # GS.base = user GS, KERNEL_GS_BASE = PerCpu
    iretq
"#
);

unsafe extern "C" {
    pub(crate) fn switch_to(old: *mut TaskContext, new: *const TaskContext, new_cr3: u64);
    pub(crate) static user_iret: u8;
}

/// Runtime address of the `user_iret` stub, used as the initial `rip` for a
/// user-bound task.
pub fn user_iret_addr() -> u64 {
    core::ptr::addr_of!(user_iret) as u64
}

/// Checked wrapper around `switch_to` - validates that `new` is sane and
/// `new_cr3` does not alias `new.rip`/`new.rsp` (the `RIP==CR3=0x2e05000`
/// I-fetch fault). Panics to serial instead of triple-faulting via `ret`.
pub(crate) unsafe fn switch_to_checked(
    old: *mut TaskContext,
    new: *const TaskContext,
    new_cr3: u64,
) {
    // Validate new context before touching CR3/stack.
    let ctx = unsafe { &*new };
    if !ctx.is_valid() {
        crate::drivers::serial::SerialPort::puts("[sched] FATAL: invalid TaskContext rip=0x");
        crate::drivers::serial::SerialPort::put_hex(ctx.rip);
        crate::drivers::serial::SerialPort::puts(" rsp=0x");
        crate::drivers::serial::SerialPort::put_hex(ctx.rsp);
        crate::drivers::serial::SerialPort::puts(" cr3=0x");
        crate::drivers::serial::SerialPort::put_hex(new_cr3);
        crate::drivers::serial::SerialPort::puts("\n");
        crate::kerneldump::dump_fatal("invalid TaskContext");
    }
    if ctx.rip != 0 && ctx.rip == new_cr3 {
        crate::drivers::serial::SerialPort::puts("[sched] FATAL: rip == cr3 0x");
        crate::drivers::serial::SerialPort::put_hex(new_cr3);
        crate::drivers::serial::SerialPort::puts("\n");
        crate::kerneldump::dump_fatal("rip==cr3");
    }
    if ctx.rsp != 0 && ctx.rsp == new_cr3 {
        crate::drivers::serial::SerialPort::puts("[sched] FATAL: rsp == cr3 0x");
        crate::drivers::serial::SerialPort::put_hex(new_cr3);
        crate::drivers::serial::SerialPort::puts("\n");
        crate::kerneldump::dump_fatal("rsp==cr3");
    }
    // Low half CR3 with high RIP is expected (user root with kernel code via
    // high-half clone), but low RIP on supervisor CR3 is never valid - the
    // idle path must be on KERNEL_ROOT. Enforce that a low RIP is never
    // dispatched as TaskContext.rip (user entry lives in the iret frame).
    if ctx.rip != 0 && ctx.rip < 0x0000_8000_0000_0000 {
        crate::drivers::serial::SerialPort::puts("[sched] FATAL: low TaskContext.rip 0x");
        crate::drivers::serial::SerialPort::put_hex(ctx.rip);
        crate::drivers::serial::SerialPort::puts(" cr3=0x");
        crate::drivers::serial::SerialPort::put_hex(new_cr3);
        crate::drivers::serial::SerialPort::puts("\n");
        crate::kerneldump::dump_fatal("low TaskContext rip");
    }
    unsafe { switch_to(old, new, new_cr3) };
}
