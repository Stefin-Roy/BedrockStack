//! SYSCALL/SYSRET MSR programming and user-mode GS management.
//!
//! Nothing here runs at boot: `setup_syscall_msrs` is only called once the
//! syscall entry stub and user address space exist, so enabling these MSRs
//! never changes pre-user boot behavior.

use alloc::string::String;
use alloc::vec::Vec;

use x86_64::VirtAddr;
use x86_64::registers::model_specific::{
    Efer, EferFlags, GsBase, KernelGsBase, LStar, SFMask, Star,
};
use x86_64::registers::rflags::RFlags;

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
/// safe at the instant of entry. It is *not* safe to carry through an IRETQ:
/// the return validates the popped SS against the current CPL, so the entry
/// stub reloads SS to the kernel data segment (0x10) before STI.
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
    pub r15: u64,         // 0x00
    pub r14: u64,         // 0x08
    pub r13: u64,         // 0x10
    pub r12: u64,         // 0x18
    pub r11: u64,         // 0x20  — SYSCALL destroyed the user value; holds user RFLAGS
    pub r10: u64,         // 0x28
    pub r9: u64,          // 0x30
    pub r8: u64,          // 0x38
    pub rbp: u64,         // 0x40
    pub rdi: u64,         // 0x48
    pub rsi: u64,         // 0x50
    pub rdx: u64,         // 0x58
    pub rcx: u64,         // 0x60  — SYSCALL destroyed the user value; holds user RIP
    pub rbx: u64,         // 0x68
    pub rax: u64,         // 0x70  — syscall number in, return value out
    pub user_rsp: u64,    // 0x78
    pub user_rflags: u64, // 0x80
    pub user_rip: u64,    // 0x88
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
// GS management: the swapgs invariant is preserved here without any GS selector
// load or WRMSR.  The entry `swapgs` already leaves GS.base = PerCpu, and the
// kernel never touches GS again, so GS.base stays PerCpu for the whole syscall
// (no interrupt can observe a stale user GS: every ISR checks `from_user` and
// only swaps for a ring-3 frame).  Only DS/ES/FS need explicit selector loads —
// SYSCALL leaves them as the user's data selectors, and long-mode addressing
// ignores their bases (FS.base/GS.base come from MSRs, not selector loads).
// The exit therefore only reloads DS/ES/FS to the user data selector before
// the final `swapgs`/`sysretq`, which restores GS.base = user GS.  This removes
// the two serializing WRMSRs (IA32_GS_BASE) and the `mov gs` that used to sit
// on every syscall.
//
// Interrupts are masked (SFMASK) on entry and stay masked until the kernel DS/
// ES/FS/SS selectors are loaded; an ISR taken in that window (CPL0 frame, so no
// user-mode swapgs) has the correct kernel GS state (GS.base = PerCpu from the
// entry swapgs). The stub enables IF for the dispatch body, then `cli`s before
// loading the user selectors, so no interrupt can fire while the user selectors
// are loaded but kernel code is still running (before sysretq re-enables IF).
// SS: SYSCALL loads SS = STAR.CS + 8 = 0x20, which is the *user* data
// descriptor in this GDT — safe to run on (base 0 in long mode), but an ISR
// frame carrying SS = 0x20 faults on IRETQ (the return validates SS against
// the current CPL). The stub therefore reloads SS to the kernel data segment
// (0x10) before STI, so no interrupt ever captures SS = 0x20.
core::arch::global_asm!(
    r#"
.macro SET_DATA_SEL sel
    mov  ax, \sel
    mov  ds, ax
    mov  es, ax
    mov  fs, ax
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
    SET_DATA_SEL 0x10               # kernel DS/ES/FS (GS.base already PerCpu)
    mov  ax, 0x10
    mov  ss, ax                     # SYSCALL loaded SS = 0x20 (user data); swap in kernel data
    sti                             # kernel GS/segments live: open the interrupt window
    mov  rdi, rsp                   # &mut SyscallFrame
    call syscall_dispatch
    cli                             # close the window before the user selectors load
    SET_DATA_SEL 0x23               # user DS/ES/FS (GS.base stays PerCpu for swapgs)
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

/// Syscall dispatcher — read/write over the unispace namespace.
///
/// ## Final syscall contract (x86_64)
///
///   0  read(path, buf, buf_len)
///   1  write(path, buf, buf_len)
///
/// Only two raw syscalls exist. Every other operation — memory management,
/// process control, sleeping — is a unispace object method (e.g.
/// `/proc/self:brk`, `/proc/self:mmap`, `/proc/self:munmap`,
/// `/proc/self:exit`).
///
/// - `path` (rdi) is a NUL-terminated C string (bounded scan; no separate
///   length).
/// - `buf` (rsi) is arbitrary bytes and may contain embedded NUL — its length
///   (`buf_len`, rdx) is required, since memory has no size metadata.
/// - `arg4` (r10) is an optional provider-defined flags word for `read`/
///   `write` — the register is unused by the two syscalls beyond that. `0` is
///   a plain value read/write; a nonzero value has semantics chosen by the
///   target object (the VFS file object uses it for read-at / append /
///   write-at, see `unispace/provider/vfs.rs`). An object without flag
///   semantics rejects a nonzero `arg4` with `-ENOSYS` rather than silently
///   ignoring it.
/// - `write`'s buffer is in-place request/response: the input is decoded first,
///   then the provider's return bytes (method output or error detail) are
///   rewritten into the same `buf` starting at byte 0, zero-filled past them.
///   A caller that still needs its input must keep its own copy.
/// - Return (`rax`): `>= 0` = result (bytes read, output bytes written;
///   0 is valid); `< 0` = errno. On error, error-detail bytes may still be in
///   `buf`.
///
/// The frame's `rax` slot carries the syscall number in and return value out.
/// r10 (the old fourth argument) is now unused.
#[unsafe(no_mangle)]
pub extern "sysv64" fn syscall_dispatch(frame: &mut SyscallFrame) {
    match frame.rax {
        0 => sys_read(frame),
        1 => sys_write(frame),
        _ => frame.rax = (-38i64) as u64, // ENOSYS

    }
}

// ── Syscall implementations ──────────────────────────────────────────

/// Zero-copy scan of a NUL-terminated path directly from user memory.
///
/// Validates every page as user-readable before forming a slice into it
/// (there is no #PF handler for user pointers). Returns the raw path str
/// and its `ParsedPath`, both borrowing the validated user VA for the
/// duration of the syscall (CR3 never changes mid-syscall under the
/// cooperative BSP-only scheduler). Heap cost: one `Vec<&str>` inside
/// `ParsedPath`; the raw bytes themselves stay in user memory (no
/// `String`/`Vec<u8>` copy).
fn scan_user_path(ptr: u64) -> Result<(&'static str, crate::unispace::path::ParsedPath<'static>), i64> {
    // SAFETY: syscall runs on task CR3 throughout; validated pages stay
    // mapped; scheduler is BSP-cooperative so no concurrent unmap.
    // Transmute to 'static is sound for the syscall duration; the borrow
    // ends before sysretq.
    if ptr >= USER_BOUNDARY {
        return Err(-14);
    }
    let root = current_task_root()?;
    let mut va = ptr;
    let mut nul_off: Option<u64> = None;
    while va < USER_BOUNDARY && va - ptr < MAX_COPY {
        let page = va & !0xFFF;
        match crate::mm::vmm::translate_user(root, page) {
            Some((_, user, _)) if user => {}
            _ => return Err(-14),
        }
        let page_end = page + 0x1000;
        let cap = core::cmp::min(page_end, core::cmp::min(USER_BOUNDARY, ptr + MAX_COPY));
        if cap <= va { break; }
        let src = unsafe { core::slice::from_raw_parts(va as *const u8, (cap - va) as usize) };
        if let Some(i) = src.iter().position(|&b| b == 0) {
            nul_off = Some(va + i as u64 - ptr);
            break;
        }
        va = cap;
    }
    let len = match nul_off {
        Some(off) => off as usize,
        None => {
            // No NUL within scan window: distinguish ENAMETOOLONG vs EINVAL
            let scanned = (va - ptr) as usize;
            if scanned as u64 >= MAX_COPY { return Err(-36); }
            return Err(-22);
        }
    };
    let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len) };
    let s = core::str::from_utf8(bytes).map_err(|_| -22i64)?;
    // SAFETY: `s` borrows validated user VA that lives through the syscall.
    let s_static: &'static str = unsafe { core::mem::transmute::<&str, &'static str>(s) };
    let parsed = crate::unispace::path::parse(s_static).map_err(|e| errno(e))?;
    Ok((s_static, parsed))
}

/// 0 read(path, buf, buf_len): read the object's value into `buf`.
///
/// The kernel never buffers more than `buf_len` bytes of the object, so a
/// hostile/reckless read cannot exhaust the heap regardless of how large the
/// source object is; `copy_user_out` further validates the buffer pages.
#[allow(unused_variables)]
fn sys_read(frame: &mut SyscallFrame) {
    let (raw, parsed) = match scan_user_path(frame.rdi) {
        Ok(v) => v,
        Err(e) => { frame.rax = e as u64; return; }
    };
    #[cfg(feature = "heap_trace")]
    let trace_t0_ms = crate::services::universal_timer::now_ns() / 1_000_000;
    #[cfg(feature = "heap_trace")]
    {
        use crate::drivers::serial::SerialPort;
        let cap = core::cmp::min(frame.rdx, MAX_COPY);
        SerialPort::puts("[sys-read] path=");
        SerialPort::puts(&raw);
        SerialPort::puts(" want=");
        SerialPort::put_u64(cap);
        SerialPort::puts(" t0=");
        SerialPort::put_u64(trace_t0_ms);
        SerialPort::puts("\n");
    }
    let mut data = Vec::new();
    let r = unispace::read_parsed(
        &parsed,
        &mut data,
        core::cmp::min(frame.rdx, MAX_COPY) as usize,
        frame.r10,
    );
    #[cfg(feature = "heap_trace")]
    {
        use crate::drivers::serial::SerialPort;
        SerialPort::puts("[sys-read] path=");
        SerialPort::puts(&raw);
        SerialPort::puts(" -> ");
        SerialPort::put_u64(data.len() as u64);
        SerialPort::puts(" bytes dt=");
        let trace_t1_ms = crate::services::universal_timer::now_ns() / 1_000_000;
        SerialPort::put_u64(trace_t1_ms.saturating_sub(trace_t0_ms));
        SerialPort::puts("ms\n");
    }
    match r {
        Ok(()) => match copy_user_out(frame.rsi, frame.rdx, &data) {
            Ok(n) => frame.rax = n as u64,
            Err(e) => frame.rax = e as u64,
        },
        Err(e) => frame.rax = errno(e) as u64,
    }
}

/// 1 write(path, buf, buf_len): decode+validate `buf` as the object's value
/// schema, apply it, and rewrite the provider's output (or error detail) into
/// `buf` in place. Returns the number of output bytes — not the input length.
fn sys_write(frame: &mut SyscallFrame) {
    let (raw, parsed) = match scan_user_path(frame.rdi) {
        Ok(v) => v,
        Err(e) => { frame.rax = e as u64; return; }
    };
    // Fast-path: whole-buffer write-through to `/dev/fb`.  A full-screen blit
    // is a few MB; routing it through the general path (schema decode + the
    // response zero-fill) would waste a full page-walk and copy.  Here we
    // validate the user pages and copy straight from user VA into the scanout
    // — one copy, no allocations.  `r10` keeps its meaning as the byte offset
    // (`flags`). Raw comparison keeps both "/dev/fb" and "dev/fb" spellings
    // matching the original `String == "/dev/fb"` check (leading slash optional
    // per path::parse, but raw is verbatim).
    if raw == "/dev/fb" {
        if let Some(caps) = crate::caps::current_caps() {
            if let Err(e) = crate::caps::check_path_on(Some(caps.as_slice()), &parsed.components, None, crate::caps::Perm::RW) {
                frame.rax = errno(e) as u64;
                return;
            }
        }
        let src = frame.rsi;
        let len = frame.rdx;
        let root = match current_task_root() {
            Ok(r) => r,
            Err(e) => { frame.rax = e as u64; return; }
        };
        if !user_range_ok(src, len) || validate_user_range(root, src, len, false).is_err() {
            frame.rax = (-14i64) as u64;
            return;
        }
        let bytes = unsafe { core::slice::from_raw_parts(src as *const u8, len as usize) };
        if crate::display::write_at(frame.r10, bytes) {
            frame.rax = len as u64;
        } else {
            frame.rax = errno(UnispaceError::InvalidArgument) as u64;
        }
        return;
    }
    let mut resp = Vec::new();
    // The buffer is read in and rewritten in place, so validate the whole
    // range once (present, user-accessible, writable — writable subsumes
    // readable) and copy straight from user VA into the provider, no `Vec`
    // input copy.  `write_parsed` decodes `data` synchronously and never
    // retains it, so handing it the validated user slice is safe; the
    // response is copied back raw after.
    let root = match current_task_root() {
        Ok(r) => r,
        Err(e) => { frame.rax = e as u64; return; }
    };
    if !user_range_ok(frame.rsi, frame.rdx)
        || validate_user_range(root, frame.rsi, frame.rdx, true).is_err()
    {
        frame.rax = (-14i64) as u64;
        return;
    }
    let data = unsafe { core::slice::from_raw_parts(frame.rsi as *const u8, frame.rdx as usize) };
    match unispace::write_parsed(&parsed, data, &mut resp, frame.r10) {
        Ok(()) => {
            let n = copy_validated_out(frame.rsi, frame.rdx, &resp);
            frame.rax = n as u64;
        }
        Err(e) => {
            // Best-effort copy of any error-detail bytes; the errno wins.
            let _ = copy_validated_out(frame.rsi, frame.rdx, &resp);
            frame.rax = errno(e) as u64;
        }
    }
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
    let ptr = pc.current_task.load(core::sync::atomic::Ordering::Relaxed);
    if ptr.is_null() {
        return Err(-1);
    }
    Ok(unsafe { (*(ptr as *const crate::task::Task)).root })
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
/// writable. Uses the batched walker that caches upper-level entries across
/// consecutive pages (~1 physmap load per page instead of 4).
fn validate_user_range(root: u64, ptr: u64, len: u64, writable: bool) -> Result<(), i64> {
    if !user_range_ok(ptr, len) {
        return Err(-14); // EFAULT
    }
    if len == 0 {
        return Ok(());
    }
    // Fast batched path — caches upper table entries across pages.
    let ok = crate::mm::vmm::translate_user_range_ok(root, ptr, len, writable);
    if ok { Ok(()) } else { Err(-14) }
}

/// Copy a NUL-terminated string from user memory.
///
/// Walks the string page by page, validating each page readable before the
/// raw deref — there is no page-fault handler for user pointers, so a raw
/// copy at a bogus address would double-fault. Only pages actually touched up
/// to the first 0x00 are validated (never an up-front full `MAX_COPY` window:
/// a user address space maps far less than that contiguously). Bounded by
/// `MAX_COPY` bytes and the `USER_BOUNDARY`. Decodes UTF-8, returning
/// `-EINVAL` on bad bytes. Returns `ENAMETOOLONG` if no NUL within cap.
pub fn copy_user_cstring(ptr: u64) -> Result<String, i64> {
    if ptr >= USER_BOUNDARY {
        return Err(-14); // EFAULT
    }
    let root = current_task_root()?;
    let mut out: Vec<u8> = Vec::new();
    let mut va = ptr;
    let mut found_nul = false;
    while va < USER_BOUNDARY && va - ptr < MAX_COPY {
        let page = va & !0xFFF;
        // Validate this page is present and user-accessible before reading it.
        match crate::mm::vmm::translate_user(root, page) {
            Some((_, user, _)) if user => {}
            _ => return Err(-14), // EFAULT
        }
        // Read up to the end of this page, capped by the user boundary and the
        // MAX_COPY budget.
        let page_end = page + 0x1000;
        let cap = core::cmp::min(page_end, core::cmp::min(USER_BOUNDARY, ptr + MAX_COPY));
        if cap <= va {
            break;
        }
        let src = unsafe { core::slice::from_raw_parts(va as *const u8, (cap - va) as usize) };
        match src.iter().position(|&b| b == 0) {
            Some(i) => {
                out.extend_from_slice(&src[..i]);
                found_nul = true;
                break;
            }
            None => {
                out.extend_from_slice(src);
                va = cap;
            }
        }
    }
    if !found_nul && out.len() as u64 >= MAX_COPY {
        return Err(-36); // ENAMETOOLONG
    }
    if !found_nul && out.len() > 0 {
        // Truncated without NUL and hit USER_BOUNDARY before MAX_COPY
        return Err(-22); // EINVAL
    }
    String::from_utf8(out).map_err(|_| -22) // EINVAL
}

/// Copy `data` into a user buffer of size `len` whose pages were already
/// validated writable (see `validate_user_range`). The full `len` range is
/// written: `min(len, data.len())` bytes are copied and any remainder is
/// zero-filled, so the caller's buffer is always defined. Returns the number
/// of bytes copied.
fn copy_validated_out(dst: u64, len: u64, data: &[u8]) -> usize {
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
    n
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
    Ok(copy_validated_out(dst, len, data))
}

/// Map a unispace error to a negative errno for the caller's `rax`.
fn errno(e: UnispaceError) -> i64 {
    use UnispaceError::*;
    match e {
        NotFound => -2,                                    // ENOENT
        IsADirectory => -21,                               // EISDIR
        NotADirectory => -20,                              // ENOTDIR
        Unsupported => -38,                                // ENOSYS
        InvalidPath | DecodeError | SchemaMismatch => -22, // EINVAL
        OutOfMemory => -12,                                // ENOMEM
        BadAddress => -14,                                 // EFAULT
        InvalidArgument => -22,                            // EINVAL
        AccessDenied => -13,                               // EACCES
        MethodNotFound => -38,                             // ENOSYS
        Vfs(_) => -5,                                      // EIO
    }
}

/// Runtime address of the `syscall_entry` stub, for IA32_LSTAR.
pub fn syscall_entry_addr() -> u64 {
    unsafe extern "C" {
        static syscall_entry: u8;
    }
    core::ptr::addr_of!(syscall_entry) as u64
}
