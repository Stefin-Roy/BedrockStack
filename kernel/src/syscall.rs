//! Syscall dispatch and handlers.
//!
//! Versioned syscall table: user code passes a table version in R10.
//! Version 1 is the P6 capability boundary: `invoke`, contract-id
//! resolution, and the cap manipulation helpers. Everything user-supplied
//! (pointers, sizes, wire-format descriptors) is validated up front and
//! never `.unwrap()`/`.expect()`ed — a malformed call returns `u64::MAX`.

use alloc::vec;
use alloc::vec::Vec;

use crate::drivers::serial::SerialPort;
use crate::mm::vmm::Vmm;
use crate::obj::adapters;
use crate::obj::contract::ContractId;
use crate::obj::hook::{HookId, SURFACE_READ};
use crate::obj::rights::{ContractRights, Rights};
use crate::obj::table::{CapabilityTable, TABLE_CONTRACT, TABLE_DELEGATE};
use crate::obj::{Args, CapId, ObjError, Reply, Value};

/// Syscall function type: (num, arg0, arg1, arg2) -> return value.
pub type SyscallFn = fn(u64, u64, u64, u64) -> u64;

/// Version 1 syscall table: write, exit, the P6 capability syscalls, and the
/// v2 user-boundary extensions (dup_limited, revoke, domain id, clock, sleep,
/// and the tagged surface read).
const TABLE_V1: [SyscallFn; 13] = [
    sys_write,              // 0
    sys_exit,               // 1
    sys_invoke,             // 2
    sys_contract_id,        // 3
    sys_cap_dup,            // 4
    sys_cap_query,          // 5
    sys_cap_delegate,       // 6
    sys_cap_dup_limited,    // 7
    sys_cap_revoke,         // 8
    sys_get_domain_id,      // 9
    sys_clock,              // 10
    sys_sleep,              // 11
    sys_cap_read,           // 12
];

/// Dispatch a syscall by (version, number, args).
pub fn dispatch(ver: u64, num: u64, arg0: u64, arg1: u64, arg2: u64) -> u64 {
    let table: &[SyscallFn] = match ver {
        1 => &TABLE_V1,
        _ => return u64::MAX, // -1: unknown version
    };
    if (num as usize) >= table.len() {
        return u64::MAX; // -1: unknown syscall
    }
    table[num as usize](num, arg0, arg1, arg2)
}

/// Continuation for an async syscall park: re-dispatches the syscall the task
/// was parked inside using the args still in its parked `UserFrame`. For
/// `invoke` this re-runs `sys_invoke`, whose re-entry now finds the task's
/// `IoState::Done` and marshals the real reply. Defined as a plain fn so it
/// can be stored in `Task::parked.continuation`.
fn retry_current_syscall(frame: &mut crate::arch::x86_64::syscall::UserFrame) -> u64 {
    crate::syscall::dispatch(frame.r10, frame.rax, frame.rdi, frame.rsi, frame.rdx)
}

/// The continuation handed to `park_async_retry` by the block layer's async
/// submit path: on resume, re-runs the parked syscall (see
/// [`retry_current_syscall`]), which now collects the completed I/O result.
pub fn syscall_retry_continuation() -> crate::proc::Continuation {
    retry_current_syscall
}

/// Top of the x86_64 low (user) canonical half — user pointers must stay below.
const USER_LIMIT: u64 = 0x0000_8000_0000_0000;

/// Validate that `[ptr, ptr+len)` lies in the user half and is fully mapped in
/// the current (user) page tables. The syscall handler runs with the user
/// domain's CR3 active, so a raw copy would fault the kernel on a bad pointer;
/// this checks the range up front. Returns `false` if the range crosses
/// `USER_LIMIT`, if the current domain has no address space (not running in a
/// user domain), or if any page the range touches is unmapped.
fn user_range_is_mapped(ptr: u64, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    if ptr >= USER_LIMIT {
        return false;
    }
    let end = match ptr.checked_add(len as u64) {
        Some(e) => e,
        None => return false,
    };
    if end > USER_LIMIT {
        return false;
    }

    // Every page the range touches must be mapped in the current address space.
    let root = match current_domain_page_root() {
        Some(r) => r,
        None => return false,
    };
    let vmm = Vmm::from_root(root);
    let mut page = ptr & !0xFFF;
    while page < end {
        if vmm.translate(page).is_none() {
            return false;
        }
        page += 4096;
    }
    true
}

/// The current domain's page-table root, if running in a user domain.
fn current_domain_page_root() -> Option<u64> {
    let d = crate::obj::domain::current_domain()?;
    d.page_root()
}

/// The current domain's capability table, if running in a user domain.
fn current_table() -> Option<&'static CapabilityTable> {
    let d = crate::obj::domain::current_domain()?;
    Some(&d.table)
}

/// Copy `len` bytes from a user-space pointer, validating that the whole range
/// lies in the user half and is mapped in the current (user) page tables.
///
/// The syscall handler runs with the user domain's CR3 active, so a raw copy
/// would fault the kernel on a bad pointer; this checks the range up front and
/// returns `None` instead. Returns `None` if the current domain has no address
/// space (not running in a user domain).
fn copy_from_user(ptr: u64, len: usize) -> Option<Vec<u8>> {
    if !user_range_is_mapped(ptr, len) {
        return None;
    }
    if len == 0 {
        return Some(Vec::new());
    }

    let mut out = vec![0u8; len];
    unsafe {
        core::ptr::copy_nonoverlapping(ptr as *const u8, out.as_mut_ptr(), len);
    }
    Some(out)
}

/// Copy `bytes` to a user-space pointer, validating the range exactly as
/// `copy_from_user` does. Returns `false` if the range is not a valid mapped
/// user range, or if the current domain has no address space.
fn copy_to_user(dst: u64, bytes: &[u8]) -> bool {
    if !user_range_is_mapped(dst, bytes.len()) {
        return false;
    }
    if bytes.is_empty() {
        return true;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst as *mut u8, bytes.len());
    }
    true
}

/// Read a NUL-terminated string from user memory, bounded by `max` bytes,
/// returning the bytes WITHOUT the terminating NUL.
///
/// Walks page by page, validating each page before reading bytes within it, so
/// a short string sitting just before an unmapped page is read successfully.
/// Returns `None` if `ptr` is outside the user half, if no NUL is found within
/// `max` bytes, or if any page is unmapped.
fn read_user_str(ptr: u64, max: usize) -> Option<Vec<u8>> {
    if ptr >= USER_LIMIT {
        return None;
    }
    let mut out = Vec::new();
    let mut page = ptr & !0xFFF;
    if !user_range_is_mapped(page, 1) {
        return None;
    }
    let mut off = 0usize;
    while off < max {
        let addr = ptr.checked_add(off as u64)?;
        if addr >= USER_LIMIT {
            return None;
        }
        // Entering a new page: validate it before reading from it.
        if addr & !0xFFF != page {
            page = addr & !0xFFF;
            if !user_range_is_mapped(page, 1) {
                return None;
            }
        }
        let b = unsafe { core::ptr::read(addr as *const u8) };
        if b == 0 {
            return Some(out);
        }
        out.push(b);
        off += 1;
    }
    None
}

/// Syscall 0: write(fd, buf_ptr, len) -> bytes_written or -1.
///
/// fds 0/1/2 route through the current task's standard streams (stdin/stdout/
/// stderr); a Serial-kind stdout/stderr also echoes to the console. Any other
/// fd fails with -1.
fn sys_write(_num: u64, fd: u64, buf_ptr: u64, len: u64) -> u64 {
    let len = len as usize;
    if len == 0 {
        return 0;
    }
    if len > 4096 {
        return u64::MAX; // -1: too large
    }

    let buf = match copy_from_user(buf_ptr, len) {
        Some(b) => b,
        None => return u64::MAX, // -1: invalid user buffer
    };

    // fds 0/1/2 route through the current task's streams; any other fd fails.
    if let Some(task) = crate::proc::current_task() {
        let stream = match fd {
            0 => &task.stdin,
            1 => &task.stdout,
            2 => &task.stderr,
            _ => return u64::MAX,
        };
        stream.write(&buf);
        return len as u64;
    }

    // Pre-scheduler fallback: raw serial console.
    for &byte in &buf {
        SerialPort::putc(byte);
    }
    len as u64
}

/// Syscall 1: exit(code) → never returns.
///
/// With the scheduler live this tears the current task down and hands the CPU
/// to the scheduler (which idles when the queue is empty). Defensive fallback
/// for the pre-scheduler window halts forever.
fn sys_exit(_num: u64, code: u64, _arg1: u64, _arg2: u64) -> u64 {
    if crate::proc::scheduler_active() {
        crate::proc::exit_process(code as i64);
    }
    loop {
        crate::arch::CurrentArch::halt();
    }
}

// ── Version 2: the P6 capability boundary ────────────────────────────────

/// Hard bound on the `invoke` descriptor size, in bytes.
const DESC_LIMIT: usize = 8192;

/// Maximum number of hook arguments an `invoke` descriptor may carry.
const MAX_ARGS: usize = 16;

/// Maximum byte length of a `Buf` argument.
const MAX_BUF_ARG: usize = 4096;

/// Maximum byte length of a `Str` argument (including UTF-8 validation).
const MAX_STR_ARG: usize = 255;

/// Map an `ObjError` to its wire ordinal in enum-declaration order (§7.2).
fn obj_error_ordinal(e: &ObjError) -> u64 {
    match e {
        ObjError::NoSuchCap => 1,
        ObjError::Denied => 2,
        ObjError::Revoked => 3,
        ObjError::Disowned => 4,
        ObjError::NoAmplification => 5,
        ObjError::OutOfMemory => 6,
        ObjError::MintAuthorityGone => 7,
        ObjError::Exhausted => 8,
        ObjError::NotSupported => 9,
        ObjError::ContractCollision => 10,
    }
}

/// Append a little-endian u64 to a byte buffer.
fn push_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Append one `Value` to a reply buffer: tag (0=U64, 1=Buf, 2=Str) + payload.
fn marshal_value(out: &mut Vec<u8>, v: &Value<'_>) {
    match v {
        Value::U64(x) => {
            push_u64(out, 0);
            push_u64(out, *x);
        }
        Value::Buf(b) => {
            push_u64(out, 1);
            push_u64(out, b.len() as u64);
            out.extend_from_slice(b);
        }
        Value::Str(s) => {
            push_u64(out, 2);
            push_u64(out, s.len() as u64);
            out.extend_from_slice(s.as_bytes());
        }
    }
}

/// An incremental byte-range reader over a user descriptor. Each `take`
/// validates the requested range (cumulative limit, user half, page mapping)
/// before copying it out; on any violation it returns `None`. Integers are
/// parsed from the copied bytes — never by casting user memory to `*const u64`.
struct UserReader {
    base: u64,
    offset: usize,
    limit: usize,
}

impl UserReader {
    fn new(base: u64, limit: usize) -> Self {
        UserReader {
            base,
            offset: 0,
            limit,
        }
    }

    /// Read the next `n` bytes, advancing past them.
    fn take(&mut self, n: usize) -> Option<Vec<u8>> {
        let next = self.offset.checked_add(n)?;
        if next > self.limit {
            return None;
        }
        let ptr = self.base.checked_add(self.offset as u64)?;
        if !user_range_is_mapped(ptr, n) {
            return None;
        }
        self.offset = next;
        if n == 0 {
            return Some(Vec::new());
        }
        let mut out = vec![0u8; n];
        unsafe {
            core::ptr::copy_nonoverlapping(ptr as *const u8, out.as_mut_ptr(), n);
        }
        Some(out)
    }

    /// Read the next little-endian u64.
    fn take_u64(&mut self) -> Option<u64> {
        let b = self.take(8)?;
        Some(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
}

/// Syscall 2 (v2): invoke(desc_ptr, reply_ptr, reply_cap, 0) -> 0 or -1.
///
/// Parses a capability invocation descriptor from user memory, runs the
/// `invoke` fast path, and marshals the reply (status, values, caps) back to
/// the caller. Returns 0 when the descriptor was valid and a reply was
/// written; the invoke outcome lives in the reply's status word. Returns
/// `u64::MAX` on any descriptor, parse, or marshalling failure.
///
/// Descriptor (sequential bytes, all integers little-endian u64):
///   u64 cap_id; u64 contract_id; u64 hook_id; u64 nargs (<= 16)
///   then per arg: u64 tag (0=U64, 1=Buf, 2=Str)
///     tag 0: u64 value
///     tag 1: u64 len (<= 4096), then len raw bytes
///     tag 2: u64 len (<= 255),  then len raw bytes (must be UTF-8)
/// The whole descriptor is bounded by `DESC_LIMIT` bytes.
fn sys_invoke(_num: u64, desc_ptr: u64, reply_ptr: u64, reply_cap: u64) -> u64 {
    if desc_ptr >= USER_LIMIT || reply_ptr >= USER_LIMIT {
        return u64::MAX;
    }
    if reply_cap < 8 {
        // The status word alone must fit.
        return u64::MAX;
    }

    let mut r = UserReader::new(desc_ptr, DESC_LIMIT);

    let cap_id = match r.take_u64() {
        Some(v) => v,
        None => return u64::MAX,
    };
    let contract_id = match r.take_u64() {
        Some(v) => v,
        None => return u64::MAX,
    };
    let hook_id = match r.take_u64() {
        Some(v) => v,
        None => return u64::MAX,
    };
    let nargs = match r.take_u64() {
        Some(v) => v,
        None => return u64::MAX,
    };
    if nargs > MAX_ARGS as u64 {
        return u64::MAX;
    }

    let mut arena: Vec<Vec<u8>> = Vec::new();
    enum Tok {
        U64(u64),
        Buf(Vec<u8>),
        Str(usize),
    }
    let mut toks: Vec<Tok> = Vec::with_capacity(nargs as usize);
    for _ in 0..nargs {
        let tag = match r.take_u64() {
            Some(v) => v,
            None => return u64::MAX,
        };
        match tag {
            0 => {
                let v = match r.take_u64() {
                    Some(v) => v,
                    None => return u64::MAX,
                };
                toks.push(Tok::U64(v));
            }
            1 => {
                let len = match r.take_u64() {
                    Some(v) => v,
                    None => return u64::MAX,
                };
                if len > MAX_BUF_ARG as u64 {
                    return u64::MAX;
                }
                let bytes = match r.take(len as usize) {
                    Some(b) => b,
                    None => return u64::MAX,
                };
                toks.push(Tok::Buf(bytes));
            }
            2 => {
                let len = match r.take_u64() {
                    Some(v) => v,
                    None => return u64::MAX,
                };
                if len > MAX_STR_ARG as u64 {
                    return u64::MAX;
                }
                let bytes = match r.take(len as usize) {
                    Some(b) => b,
                    None => return u64::MAX,
                };
                if core::str::from_utf8(&bytes).is_err() {
                    return u64::MAX;
                }
                arena.push(bytes);
                toks.push(Tok::Str(arena.len() - 1));
            }
            _ => return u64::MAX,
        }
    }

    let mut vals: Vec<Value> = Vec::with_capacity(toks.len());
    for t in toks {
        match t {
            Tok::U64(v) => vals.push(Value::U64(v)),
            Tok::Buf(b) => vals.push(Value::Buf(b)),
            Tok::Str(i) => vals.push(Value::Str(
                core::str::from_utf8(&arena[i]).expect("validated"),
            )),
        }
    }

    let args = Args { vals };

    let table = match current_table() {
        Some(t) => t,
        None => return u64::MAX,
    };

    let result =
        crate::obj::invoke(table, CapId(cap_id), ContractId(contract_id), HookId(hook_id), &args);

    // Marshal the reply: status, nvalues, ncaps, values, then cap ids.
    let status = match &result {
        Ok(_) => 0,
        Err(e) => obj_error_ordinal(e),
    };
    let mut out: Vec<u8> = Vec::new();
    push_u64(&mut out, status);
    match result {
        Ok(Reply::None) => {
            push_u64(&mut out, 0); // nvalues
            push_u64(&mut out, 0); // ncaps
        }
        Ok(Reply::Data(vals)) => {
            push_u64(&mut out, vals.len() as u64);
            push_u64(&mut out, 0); // ncaps
            for v in vals {
                marshal_value(&mut out, &v);
            }
        }
        Ok(Reply::Caps(caps)) => {
            push_u64(&mut out, 0); // nvalues
            push_u64(&mut out, caps.len() as u64);
            for h in &caps {
                push_u64(&mut out, h.id.0);
            }
        }
        Err(_) => {
            push_u64(&mut out, 0); // nvalues
            push_u64(&mut out, 0); // ncaps
        }
    }

    if out.len() > reply_cap as usize {
        // The reply would not fit: write only the 0xFFFF overflow marker.
        if !copy_to_user(reply_ptr, &0xFFFFu64.to_le_bytes()) {
            return u64::MAX;
        }
        return 0;
    }

    if !copy_to_user(reply_ptr, &out) {
        return u64::MAX;
    }
    0
}

/// Syscall 3 (v2): contract_id(name_ptr, 0, 0) -> ContractId.0 or -1.
///
/// Resolves a contract name to its content-addressed id. An id confers nothing
/// without a capability to a node implementing the contract, so this is a pure
/// information syscall.
fn sys_contract_id(_num: u64, name_ptr: u64, _a1: u64, _a2: u64) -> u64 {
    let bytes = match read_user_str(name_ptr, 256) {
        Some(b) => b,
        None => return u64::MAX,
    };
    let name = match core::str::from_utf8(&bytes) {
        Ok(n) => n,
        Err(_) => return u64::MAX,
    };
    match adapters::contract_def(name) {
        Some(def) => def.id.0,
        None => u64::MAX,
    }
}

/// Syscall 4 (v2): cap_dup(cap_id, 0, 0) -> new CapId or -1.
fn sys_cap_dup(_num: u64, cap_id: u64, _a1: u64, _a2: u64) -> u64 {
    let table = match current_table() {
        Some(t) => t,
        None => return u64::MAX,
    };
    match table.dup(CapId(cap_id)) {
        Ok(id) => id.0,
        Err(_) => u64::MAX,
    }
}

/// Syscall 5 (v2): cap_query(cap_id, attr_name_ptr, 0) -> attribute or -1.
///
/// Reads one typed attribute off the cap's node surface (`SURFACE_READ`). All
/// current surface attrs are U64, so the value is returned directly in RAX.
fn sys_cap_query(_num: u64, cap_id: u64, name_ptr: u64, _a2: u64) -> u64 {
    let bytes = match read_user_str(name_ptr, 256) {
        Some(b) => b,
        None => return u64::MAX,
    };
    let name = match core::str::from_utf8(&bytes) {
        Ok(n) => n,
        Err(_) => return u64::MAX,
    };
    let table = match current_table() {
        Some(t) => t,
        None => return u64::MAX,
    };
    let args = Args {
        vals: vec![Value::Str(name)],
    };
    match crate::obj::invoke(table, CapId(cap_id), ContractId(0), SURFACE_READ, &args) {
        Ok(Reply::Data(v)) => match v.first() {
            Some(Value::U64(u)) => *u,
            _ => u64::MAX,
        },
        _ => u64::MAX,
    }
}

/// Syscall 6 (v2): cap_delegate(cap_id, target_id, 0) -> delegated CapId or -1.
///
/// Delegates `cap_id` into the target domain's table by invoking the caller's
/// own table node's `delegate` hook — no ambient authority: the caller must
/// hold the table cap that resolves `TABLE_DELEGATE`.
fn sys_cap_delegate(_num: u64, cap_id: u64, target_id: u64, _a2: u64) -> u64 {
    let table = match current_table() {
        Some(t) => t,
        None => return u64::MAX,
    };
    let table_cap = match table.resolve_first(TABLE_CONTRACT, TABLE_DELEGATE) {
        Some(id) => id,
        None => return u64::MAX,
    };
    let args = Args {
        vals: vec![Value::U64(target_id), Value::U64(cap_id)],
    };
    match crate::obj::invoke(table, table_cap, TABLE_CONTRACT, TABLE_DELEGATE, &args) {
        Ok(Reply::Data(v)) => match v.first() {
            Some(Value::U64(id)) => *id,
            _ => u64::MAX,
        },
        _ => u64::MAX,
    }
}

// ── v2 user-boundary extensions ─────────────────────────────────────────

/// Syscall 7: cap_dup_limited(cap_id, keep_uni_bits, keep_contract_bits)
/// -> new CapId or -1.
///
/// Duplicates `cap_id` into a fresh slot attuned to a subset of the original's
/// rights (§7.4 item 2). Rights are monotone: the copy never gains a bit the
/// source lacked. The two masks are the raw `Rights`/`ContractRights` bit
/// fields (bit0=QUERY, bit1=INVOKE, bit2=TRAVERSE, bit3=MINT, bit4=REVOKE;
/// and bit0=READ, bit1=WRITE, bit2=CALL). A zero contract mask is the
/// transitional "not yet narrowed" state and keeps the full held mask; to
/// actually narrow, pass the exact mask to keep (e.g. READ alone).
fn sys_cap_dup_limited(_num: u64, cap_id: u64, keep_uni: u64, keep_contract: u64) -> u64 {
    let table = match current_table() {
        Some(t) => t,
        None => return u64::MAX,
    };
    match table.dup_limited(
        CapId(cap_id),
        Rights::from_bits(keep_uni as u32),
        ContractRights::from_bits(keep_contract as u32),
    ) {
        Ok(id) => id.0,
        Err(_) => u64::MAX,
    }
}

/// Syscall 8: cap_revoke(cap_id) -> 0 or -1.
///
/// Marks the caller's own handle `Revoked` (§3.7): the slot and its strong
/// reference are retained, but any later resolve through this cap fails with
/// `ObjError::Revoked`. Cascade/family revocation stays kernel-only.
fn sys_cap_revoke(_num: u64, cap_id: u64, _a1: u64, _a2: u64) -> u64 {
    let table = match current_table() {
        Some(t) => t,
        None => return u64::MAX,
    };
    match table.revoke(CapId(cap_id)) {
        Ok(()) => 0,
        Err(_) => u64::MAX,
    }
}

/// Syscall 9: get_domain_id() -> the caller's domain id or -1.
///
/// A pure information syscall; the id is what `cap_delegate`'s `target_id`
/// argument names (§6). Returns -1 if not running in a user domain.
fn sys_get_domain_id(_num: u64, _a0: u64, _a1: u64, _a2: u64) -> u64 {
    match crate::obj::domain::current_domain() {
        Some(d) => d.id as u64,
        None => u64::MAX,
    }
}

/// Syscall 10: clock(which) -> time.
///
///   which = 0: wall-clock seconds since the Unix epoch (falls back to
///              monotonic seconds on riscv64, which has no RTC).
///   which = 1: monotonic nanoseconds since boot.
fn sys_clock(_num: u64, which: u64, _a1: u64, _a2: u64) -> u64 {
    match which {
        0 => crate::services::wallclock::now_secs(),
        _ => crate::services::universal_timer::now_ns(),
    }
}

/// Syscall 11: sleep(ms) -> 0.
///
/// With the scheduler live this parks the calling task on the universal timer
/// (the ISR wake re-queues it); the old HLT-based wait is the defensive
/// fallback for the pre-scheduler window.
fn sys_sleep(_num: u64, ms: u64, _a1: u64, _a2: u64) -> u64 {
    if crate::proc::scheduler_active() {
        crate::proc::sleep_current(ms);
    }
    crate::arch::CurrentArch::enable_interrupts();
    crate::services::universal_timer::sleep_ms(ms);
    crate::arch::CurrentArch::disable_interrupts();
    0
}

/// Fixed reply-buffer capacity for `cap_read` (all surface attrs are
/// kernel-bounded well below this).
const CAP_READ_CAP: usize = 4096;

/// Syscall 12: cap_read(cap_id, attr_name_ptr, reply_ptr) -> 0 or -1.
///
/// Reads one typed attribute off the cap's node surface (`SURFACE_READ`) and
/// marshals the value into `reply_ptr` as a tagged reply:
///   u64 tag (0=U64, 1=Buf, 2=Str); U64: 8-byte value; Buf/Str: u64 len, bytes.
/// The reply capacity is fixed at [`CAP_READ_CAP`] bytes. On overflow only the
/// 0xFFFF marker is written (mirroring `sys_invoke`). This is the
/// full-featured sibling of the U64-only `cap_query` fast path.
fn sys_cap_read(_num: u64, cap_id: u64, name_ptr: u64, reply_ptr: u64) -> u64 {
    if reply_ptr >= USER_LIMIT {
        return u64::MAX;
    }
    let bytes = match read_user_str(name_ptr, 256) {
        Some(b) => b,
        None => return u64::MAX,
    };
    let name = match core::str::from_utf8(&bytes) {
        Ok(n) => n,
        Err(_) => return u64::MAX,
    };
    let table = match current_table() {
        Some(t) => t,
        None => return u64::MAX,
    };
    let args = Args {
        vals: vec![Value::Str(name)],
    };
    let value = match crate::obj::invoke(table, CapId(cap_id), ContractId(0), SURFACE_READ, &args) {
        Ok(Reply::Data(v)) => match v.into_iter().next() {
            Some(val) => val,
            None => return u64::MAX,
        },
        _ => return u64::MAX,
    };

    let mut out: Vec<u8> = Vec::new();
    marshal_value(&mut out, &value);
    if out.len() > CAP_READ_CAP {
        // Would not fit: write only the 0xFFFF overflow marker.
        if !copy_to_user(reply_ptr, &0xFFFFu64.to_le_bytes()) {
            return u64::MAX;
        }
        return 0;
    }
    if !copy_to_user(reply_ptr, &out) {
        return u64::MAX;
    }
    0
}
