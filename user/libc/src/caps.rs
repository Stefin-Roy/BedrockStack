//! Capability introspection and helpers — mirrors `kernel::caps`.
//!
//! `read(/proc/self/caps)` exposes the caller's set as a `List` wire:
//! `u32 count` + `count * { u32 path_len, path, u32 method_len, method, u32 perm }`
//! where `perm` is `1=R` / `3=RW` and `method_len==0` encodes `None`.
//! All operations are `R`-gated on `proc/self/caps` + ancestors — a child
//! without that `R` gets `ENOENT` (hidden), not `EACCES`.
//!
//! This module is `no_std` and heap-backed (`alloc`). Single-threaded tasks
//! may call `has_cap` frequently; it reads the snapshot each time rather than
//! caching (cooperative scheduler, cheap).

extern crate alloc;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::ffi::{c_char, c_int};

use crate::errno;
use crate::syscall::read_path;

/// Permission values matching the kernel (`kernel/src/caps/mod.rs:22`).
pub const R: u8 = 1;
pub const RW: u8 = 3;

pub const PERM_R: u8 = R;
pub const PERM_RW: u8 = RW;

/// Borrowed cap descriptor for `spawn` — `perm` must be `R` or `RW`.
#[derive(Clone, Copy, Debug)]
pub struct Cap<'a> {
    pub path: &'a str,
    pub method: Option<&'a str>,
    pub perm: u8,
}

/// Owned cap returned by `current_caps`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedCap {
    pub path: String,
    pub method: Option<String>,
    pub perm: u8,
}

impl OwnedCap {
    /// View as borrowed `Cap`.
    pub fn as_cap(&self) -> Cap<'_> {
        Cap {
            path: &self.path,
            method: self.method.as_deref(),
            perm: self.perm,
        }
    }
    /// True if this cap covers `want` (`RW` covers `R` and `RW`).
    pub fn covers(&self, want: u8) -> bool {
        self.perm == RW || self.perm == want
    }
}

/// Builder for a spawn cap set. Heap-backed, single-threaded.
#[derive(Clone, Debug, Default)]
pub struct CapSet {
    caps: Vec<OwnedCap>,
}

impl CapSet {
    pub fn new() -> Self {
        CapSet { caps: Vec::new() }
    }
    pub fn with_capacity(n: usize) -> Self {
        CapSet { caps: Vec::with_capacity(n) }
    }
    pub fn push(&mut self, path: &str, method: Option<&str>, perm: u8) {
        // Mirror kernel `validate_cap` minimal checks; invalid perms are stored but will be rejected at spawn.
        self.caps.push(OwnedCap {
            path: String::from(path),
            method: method.map(|m| String::from(m)),
            perm,
        });
    }
    pub fn push_cap(&mut self, c: Cap<'_>) {
        self.push(c.path, c.method, c.perm);
    }
    /// Push many borrowed caps.
    pub fn extend_from_slice(&mut self, caps: &[Cap<'_>]) {
        for c in caps {
            self.push(c.path, c.method, c.perm);
        }
    }
    pub fn len(&self) -> usize {
        self.caps.len()
    }
    pub fn is_empty(&self) -> bool {
        self.caps.is_empty()
    }
    /// Borrowed view vector referencing the owned storage. Caller must keep `self` alive.
    pub fn as_borrowed_vec(&self) -> Vec<Cap<'_>> {
        self.caps.iter().map(|o| o.as_cap()).collect()
    }
    pub fn into_owned(self) -> Vec<OwnedCap> {
        self.caps
    }
    pub fn owned(&self) -> &[OwnedCap] {
        &self.caps
    }
    /// Common presets:
    pub fn push_stdio(&mut self) {
        for (p, m, perm) in [
            ("proc/self/std", None, RW),
            ("proc/self/std/in", None, RW),
            ("proc/self/std/out", None, RW),
            ("proc/self/std/err", None, RW),
            ("proc/self/std/out", Some("get"), R),
            ("proc/self/std/err", Some("get"), R),
            ("proc/self/std/in", Some("get"), R),
        ] {
            self.push(p, m, perm);
        }
    }
    pub fn push_proc_base(&mut self) {
        for (p, m, perm) in [
            ("proc", None, RW),
            ("proc/self", None, RW),
            ("proc/self", Some("exit"), RW),
            ("proc/self", Some("yield"), RW),
            ("proc/self", Some("kill"), RW),
            ("proc/self", Some("spawn_caps"), RW),
            ("proc/self", Some("brk"), RW),
            ("proc/self", Some("mmap"), RW),
            ("proc/self", Some("munmap"), RW),
            ("proc/self", Some("wait"), RW),
            ("proc/self/status", None, R),
            ("proc/self/args", None, R),
            ("proc/self/caps", None, R),
            ("proc/self/mem", None, R),
        ] {
            self.push(p, m, perm);
        }
    }
    pub fn push_timer(&mut self) {
        for (p, m, perm) in [
            ("kernel", None, R),
            ("kernel/timer", None, R),
            ("kernel/timer", Some("sleep"), RW),
            ("kernel/timer", Some("sleep_ms"), RW),
            ("kernel/timer", Some("until"), RW),
            ("kernel/timer", Some("epoch_secs"), RW),
        ] {
            self.push(p, m, perm);
        }
    }
}

/// Parse raw `CAP_LIST` wire into `OwnedCap`s. Wire is as emitted by
/// `kernel/src/unispace/provider/proc.rs: CapsObject`.
fn parse_wire(data: &[u8]) -> Option<Vec<OwnedCap>> {
    if data.len() < 4 {
        return None;
    }
    let count = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let mut off = 4usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        if off + 4 > data.len() {
            return None;
        }
        let plen = u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
        off += 4;
        if off + plen > data.len() {
            return None;
        }
        let path = core::str::from_utf8(&data[off..off + plen]).ok()?;
        let p = String::from(path);
        off += plen;

        if off + 4 > data.len() {
            return None;
        }
        let mlen = u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
        off += 4;
        if off + mlen > data.len() {
            return None;
        }
        let method = if mlen == 0 {
            None
        } else {
            let ms = core::str::from_utf8(&data[off..off + mlen]).ok()?;
            Some(String::from(ms))
        };
        off += mlen;

        if off + 4 > data.len() {
            return None;
        }
        let perm = u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as u8;
        off += 4;
        // Only 1 and 3 are valid; keep others but they will never cover.
        out.push(OwnedCap { path: p, method, perm });
    }
    // Trailing bytes are ignored (defensive).
    Some(out)
}

/// Read `/proc/self/caps` into a fresh `Vec<OwnedCap>`.
/// On `ENOENT`/`EACCES` (task lacks `R` on that leaf) returns empty vec vs error:
/// this fn returns `Ok(empty)` for deny-all and `Err(errno)` for true I/O errors.
pub fn current_caps() -> Result<Vec<OwnedCap>, c_int> {
    // 64 KiB covers ~160-entry INIT (~5 KiB) with headroom. Kernel MAX_COPY is 16 MiB but worst-case 8192*~320 >64KiB would be truncated; still OK for init.
    let mut raw = vec![0u8; 64 * 1024];
    let r = unsafe { read_path(b"/proc/self/caps\0", &mut raw, 0) };
    if r < 0 {
        // r is -errno already
        let e = (-r) as c_int;
        // Hidden (no R) -> empty set (deny-all) is not an error for has_cap callers; but we surface errno for callers that need to distinguish.
        // For current_caps, ENOENT/EACCES means the read was denied → treat as Err so has_cap can map to false.
        return Err(e);
    }
    let n = r as usize;
    raw.truncate(n);
    match parse_wire(&raw) {
        Some(v) => Ok(v),
        None => Err(errno::EINVAL),
    }
}

/// Raw helper: read caps into caller-provided `buf`, return parsed count or -errno.
/// Useful for C without heap.
pub fn current_caps_into(buf: &mut [OwnedCap]) -> isize {
    match current_caps() {
        Ok(v) => {
            let n = core::cmp::min(v.len(), buf.len());
            // Need mutable slice? caller passes &mut [OwnedCap]
            // Can't move without clone; we already have cloned OwnedCap, caller expects copy.
            for i in 0..n {
                // This requires OwnedCap Clone; we can clone each.
                buf[i] = v[i].clone();
            }
            // Leak? buf is caller-owned. Return count.
            n as isize
        }
        Err(e) => -(e as isize),
    }
}

/// Does the current task have `want` (`R` or `RW`) on `(path, method)`?
/// Returns `false` if the caps object is hidden (`ENOENT`) or entry absent.
pub fn has_cap(path: &str, method: Option<&str>, want: u8) -> bool {
    // Fast path: try to read; on hidden (ENOENT) -> false
    let caps = match current_caps() {
        Ok(v) => v,
        Err(_) => return false,
    };
    for c in &caps {
        if c.path == path && c.method.as_deref() == method {
            // RW covers R and RW, R covers only R
            if c.perm == RW {
                return true;
            }
            if c.perm == want {
                return true;
            }
            if c.perm == R && want == R {
                return true;
            }
        }
    }
    false
}

/// Convenience: `path` + `method` string pair with owned storage.
pub fn has_cap_str(path: &str, method: &str, want: u8) -> bool {
    let m = if method.is_empty() { None } else { Some(method) };
    has_cap(path, m, want)
}

/// Check ancestor `R` chain + leaf `want` exactly like `caps::check_path` but userspace.
/// Useful for debugging before a syscall.
pub fn check_path(path: &str, method: Option<&str>, want: u8) -> Result<(), c_int> {
    let want_perm = if want == RW { RW } else { R };
    let caps = current_caps().map_err(|e| e)?;
    if caps.is_empty() {
        // Deny-all: any non-root path fails as ENOENT
        let comps: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if comps.is_empty() {
            // "/" leaf -> "" path ; deny-all means leaf has no R
            return Err(errno::ENOENT);
        }
        return Err(errno::ENOENT);
    }
    let comps: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    // Ancestor R
    for i in 0..comps.len().saturating_sub(1) {
        let prefix = comps[..=i].join("/");
        if !has_cap_in(&caps, &prefix, None, R) {
            return Err(errno::ENOENT);
        }
    }
    let leaf = comps.join("/");
    if let Some(m) = method {
        if !has_cap_in(&caps, &leaf, None, R) {
            return Err(errno::ENOENT);
        }
        if has_cap_in(&caps, &leaf, Some(m), want_perm) {
            return Ok(());
        }
        if want_perm == RW && has_cap_in(&caps, &leaf, Some(m), R) {
            return Err(errno::EACCES);
        }
        return Err(errno::ENOSYS); // MethodNotFound -> ENOSYS in syscall mapping
    } else {
        if has_cap_in(&caps, &leaf, None, want_perm) {
            return Ok(());
        }
        if want_perm == RW && has_cap_in(&caps, &leaf, None, R) {
            return Err(errno::EACCES);
        }
        return Err(errno::ENOENT);
    }
}

fn has_cap_in(caps: &[OwnedCap], path: &str, method: Option<&str>, want: u8) -> bool {
    for c in caps {
        if c.path == path && c.method.as_deref() == method {
            if c.perm == RW {
                return true;
            }
            if c.perm == want {
                return true;
            }
        }
    }
    false
}

// ── C ABI helpers ──────────────────────────────────────────────────────

/// C struct mirror of `OwnedCap` for out params. Caller owns strings via `strdup`.
#[repr(C)]
pub struct CCap {
    pub path: *mut c_char,
    pub method: *mut c_char, // NULL if None
    pub perm: u32,
}

/// `bedrock_caps_count()` — number of caps or -errno. Cheap way to size a buffer before `bedrock_caps_list`.
#[unsafe(no_mangle)]
pub extern "C" fn bedrock_caps_count() -> c_int {
    match current_caps() {
        Ok(v) => v.len() as c_int,
        Err(e) => -e,
    }
}

/// `bedrock_has_cap(path, method, perm)` — `1` if present with covering perm, `0` if not, `-errno` if caps unreadable.
/// `method` may be NULL for object caps.
#[unsafe(no_mangle)]
pub extern "C" fn bedrock_has_cap(path: *const c_char, method: *const c_char, perm: u32) -> c_int {
    if path.is_null() {
        return -errno::EINVAL;
    }
    let p = unsafe { core::slice::from_raw_parts(path as *const u8, crate::string::strlen(path)) };
    let ps = core::str::from_utf8(p).unwrap_or("");
    let ms = if method.is_null() {
        None
    } else {
        let m = unsafe { core::slice::from_raw_parts(method as *const u8, crate::string::strlen(method)) };
        let s = core::str::from_utf8(m).unwrap_or("");
        if s.is_empty() { None } else { Some(s) }
    };
    let want = perm as u8;
    if want != R && want != RW {
        return -errno::EINVAL;
    }
    if has_cap(ps, ms, want) { 1 } else { 0 }
}

/// `bedrock_caps_list(out, cap)` — write up to `cap` caps into `out` (each entry `strdup`'d). Returns count or -errno.
/// Caller must `free` each `path`/`method`.
#[unsafe(no_mangle)]
pub extern "C" fn bedrock_caps_list(out: *mut CCap, cap: usize) -> c_int {
    if out.is_null() {
        return -errno::EINVAL;
    }
    let v = match current_caps() {
        Ok(v) => v,
        Err(e) => return -e,
    };
    let n = core::cmp::min(v.len(), cap);
    for i in 0..n {
        let c = &v[i];
        let path_dup = crate::string::strdup(c.path.as_ptr() as *const c_char);
        let meth_dup = if let Some(m) = &c.method {
            crate::string::strdup(m.as_ptr() as *const c_char)
        } else {
            core::ptr::null_mut()
        };
        unsafe {
            (*out.add(i)).path = path_dup;
            (*out.add(i)).method = meth_dup;
            (*out.add(i)).perm = c.perm as u32;
        }
    }
    n as c_int
}

/// `bedrock_caps_free(list, n)` — free strings allocated by `bedrock_caps_list`.
#[unsafe(no_mangle)]
pub extern "C" fn bedrock_caps_free(list: *mut CCap, n: usize) {
    if list.is_null() {
        return;
    }
    for i in 0..n {
        unsafe {
            let p = (*list.add(i)).path;
            if !p.is_null() {
                crate::mem::free(p as *mut core::ffi::c_void);
            }
            let m = (*list.add(i)).method;
            if !m.is_null() {
                crate::mem::free(m as *mut core::ffi::c_void);
            }
        }
    }
}
