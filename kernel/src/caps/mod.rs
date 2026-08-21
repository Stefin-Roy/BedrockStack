//! Capability system — per-process path capabilities mapped into a supervisor page.
//!
//! Model (see design plan):
//! - Each capability is an exact `(path, method)` entry with `R` or `RW`. Absence = R0W0
//!   which hides existence via `ENOENT`. `R0W1` is invalid and rejected at insertion.
//! - `R` on a path grants `read` / `:desc` / directory listing (filtered). `RW` grants
//!   `write` / `invoke`. Ancestor directories must all have `R` to traverse.
//! - For methods, `R` on `(path, method)` grants `read(path:method)` (schema), `RW`
//!   grants `write(path:method)` (invoke). `:desc` is readable with leaf `R` but its
//!   `methods` array is filtered by per-method `R`.
//! - Only process inheritance is subset-checked; object tree has no transitive grants.
//! - Supervisor page: one 4K frame per task mapped supervisor-only (READ, no USER) at a
//!   fixed low-half VA outside `usermem`'s ceiling. The page mirrors the `Vec<Cap>` for
//!   introspection; enforcement reads the `Vec` (fast, no physmap walk).

use alloc::string::String;
use alloc::vec::Vec;

use crate::unispace::{UnispaceError, path as upath};

/// Permission states. `0` = absent (R0W0), `1` = R, `2` = invalid R0W1, `3` = RW.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Perm {
    R = 1,
    RW = 3,
}

impl Perm {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Perm::R),
            3 => Some(Perm::RW),
            _ => None,
        }
    }

    /// Does self cover want? RW covers R and RW, R covers only R.
    pub fn covers(self, want: Perm) -> bool {
        match (self, want) {
            (Perm::RW, _) => true,
            (Perm::R, Perm::R) => true,
            _ => false,
        }
    }

    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// One capability entry. `path` is canonical "" for "/" else "a/b/c".
/// `method` is `None` for object value/dir, `Some(name)` for `:method`.
#[derive(Clone, Debug)]
pub struct Cap {
    pub path: String,
    pub method: Option<String>,
    pub perm: Perm,
}

impl Cap {
    pub fn new(path: String, method: Option<String>, perm: Perm) -> Self {
        Cap { path, method, perm }
    }
}

/// Supervisor page constants — filled by `mm::layout` after layout verification.
/// Fixed low-half VA just below the user stack's guard ceiling is chosen dynamically;
/// the constant here is the *default* slot used when a task has no caps page.
/// Real per-task mapping uses `CAP_SLOT_VA` from `mm::layout`.
pub const CAP_PAGE_SIZE: u64 = 4096;

/// Max path length for caps (prevents unbounded allocations on hostile spawn payloads).
pub const MAX_CAP_PATH_LEN: usize = 256;
pub const MAX_CAP_METHOD_LEN: usize = 64;
pub const MAX_CAPS_PER_TASK: usize = 8192;

/// Exact lookup: does caps grant `want` on `(path, method)`?
pub fn has_perm(caps: &[Cap], path: &str, method: Option<&str>, want: Perm) -> bool {
    for c in caps {
        if c.path == path && c.method.as_deref() == method && c.perm.covers(want) {
            return true;
        }
    }
    false
}

/// Check ancestor R chain for `components`. For "/a/b/c" with components ["a","b","c"],
/// checks "a" and "a/b" (all ancestors).
pub fn has_ancestor_r(caps: &[Cap], components: &[&str]) -> bool {
    if components.is_empty() {
        return true;
    }
    // Need R on every prefix except the final component.
    for i in 0..components.len().saturating_sub(1) {
        let prefix = components[..=i].join("/");
        if !has_perm(caps, &prefix, None, Perm::R) {
            return false;
        }
    }
    true
}

/// Convenience: canonical path string from components.
pub fn join_path(components: &[&str]) -> String {
    components.join("/")
}

/// Validate a single cap at insertion (spawn) time.
pub fn validate_cap(cap: &Cap) -> Result<(), UnispaceError> {
    if cap.path.len() > MAX_CAP_PATH_LEN {
        return Err(UnispaceError::InvalidArgument);
    }
    if let Some(m) = &cap.method {
        if m.is_empty() || m.len() > MAX_CAP_METHOD_LEN || m.contains('/') || m.contains(':') {
            return Err(UnispaceError::InvalidArgument);
        }
    }
    // R0W1 impossible via Perm, but guard if raw value came from wire.
    // Reject empty method string if present.
    // Path must be canonical: no leading /, no empty components, no . / ..
    if !cap.path.is_empty() {
        if cap.path.starts_with('/') || cap.path.ends_with('/') || cap.path.contains("//") {
            return Err(UnispaceError::InvalidArgument);
        }
        for seg in cap.path.split('/') {
            if seg.is_empty() || seg == "." || seg == ".." || seg.contains(':') {
                return Err(UnispaceError::InvalidArgument);
            }
        }
    }
    // Also validate via path parser by round-tripping.
    let probe = if cap.path.is_empty() {
        String::from("/")
    } else {
        alloc::format!("/{}", cap.path)
    };
    let _ = upath::parse(&probe).map_err(|_| UnispaceError::InvalidArgument)?;
    if let Some(m) = &cap.method {
        let probe2 = alloc::format!("{}/x:{}", probe.trim_end_matches('/'), m);
        // Use a dummy resolve path to validate method name legality; parse will reject bad names
        let _ = upath::parse(&probe2).map_err(|_| UnispaceError::InvalidArgument)?;
    }
    Ok(())
}

/// Subset check: every child cap must be covered by some parent cap (exact + perm).
pub fn is_subset(parent: &[Cap], child: &[Cap]) -> bool {
    for c in child {
        if !has_perm(parent, &c.path, c.method.as_deref(), c.perm) {
            return false;
        }
    }
    true
}

/// Filter a provider's raw listing entries by per-child R.
pub fn filter_listing(caps: &[Cap], dir_path: &str, names: Vec<crate::unispace::ListingEntry>) -> Vec<crate::unispace::ListingEntry> {
    names
        .into_iter()
        .filter(|e| {
            let child_path = if dir_path.is_empty() {
                e.name.clone()
            } else {
                alloc::format!("{}/{}", dir_path, e.name)
            };
            has_perm(caps, &child_path, None, Perm::R)
        })
        .collect()
}

/// Filter method descriptors for :desc by per-method R.
pub fn filter_methods_by_perm<'a>(
    caps: &[Cap],
    leaf_path: &str,
    methods: &'a [crate::unispace::schema::MethodDesc],
) -> Vec<&'a crate::unispace::schema::MethodDesc> {
    methods
        .iter()
        .filter(|m| has_perm(caps, leaf_path, Some(m.name), Perm::R))
        .collect()
}

pub fn filter_owned_methods_by_perm<'a>(
    caps: &[Cap],
    leaf_path: &str,
    methods: &'a [crate::unispace::OwnedMethodDesc],
) -> Vec<&'a crate::unispace::OwnedMethodDesc> {
    methods
        .iter()
        .filter(|m| has_perm(caps, leaf_path, Some(m.name.as_str()), Perm::R))
        .collect()
}

/// Current task's caps snapshot. `None` = kernel/bypass (no current task or caps page absent).
pub fn current_caps() -> Option<Vec<Cap>> {
    #[cfg(target_arch = "x86_64")]
    {
        let pc = crate::smp::current_per_cpu();
        if pc.current_task.is_null() {
            return None;
        }
        let t = unsafe { &*(pc.current_task as *const crate::task::Task) };
        if t.caps_len == 0 && t.caps_phys == 0 {
            // No caps yet — treat as bypass for boot before INIT launch? Only INIT gets fullcaps.
            // After enforcement, tasks without caps will get ENOENT for everything except maybe root.
            // For now, allow kernel-roots (vm==0) to bypass; user tasks without caps will be denied.
            if t.vm == 0 {
                return None;
            }
            // User task with no caps: deny-all (empty set) — not bypass.
            return Some(Vec::new());
        }
        // Clone the Vec<Cap> stored in the task (authoritative; page is mirror)
        // Safety: we hold no lock, but scheduler is cooperative BSP-only, so the current
        // task's caps Vec is not mutated concurrently.
        let caps_ptr = t.caps_ptr as *const Vec<Cap>;
        if caps_ptr.is_null() {
            return Some(Vec::new());
        }
        let caps = unsafe { &*caps_ptr };
        Some(caps.clone())
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        None
    }
}

/// Grant a new cap to the current task (in-place, re-serializes supervisor page).
/// Used to auto-grant RW on newly created VFS objects so creator can immediately use them.
pub fn grant_to_current(path: String, method: Option<String>, perm: Perm) -> Result<(), UnispaceError> {
    #[cfg(target_arch = "x86_64")]
    {
        let pc = crate::smp::current_per_cpu();
        if pc.current_task.is_null() {
            return Ok(());
        }
        let t = unsafe { &mut *(pc.current_task as *mut crate::task::Task) };
        // If task has no caps allocation yet (empty child), allocate now
        if t.caps_ptr == 0 {
            let alloc = crate::mm::heap::get_phys_allocator_mut();
            let mut new_caps = Vec::new();
            let cap = Cap { path: path.clone(), method: method.clone(), perm };
            crate::caps::validate_cap(&cap)?;
            new_caps.push(cap);
            if let Some((ptr, len, phys)) = crate::task::install_caps(t.root, new_caps, alloc) {
                t.caps_ptr = ptr;
                t.caps_len = len;
                t.caps_phys = phys;
                return Ok(());
            } else {
                return Err(UnispaceError::OutOfMemory);
            }
        }
        let caps = unsafe { &mut *(t.caps_ptr as *mut Vec<Cap>) };
        if caps.len() >= MAX_CAPS_PER_TASK {
            return Err(UnispaceError::OutOfMemory);
        }
        // Deduplicate: if already has with covering perm, no-op (upgrade if needed)
        for c in caps.iter_mut() {
            if c.path == path && c.method == method {
                if c.perm.covers(perm) {
                    return Ok(());
                } else {
                    c.perm = perm;
                    if t.caps_phys != 0 {
                        serialize_to_page(caps, t.caps_phys);
                    }
                    return Ok(());
                }
            }
        }
        let cap = Cap { path: path.clone(), method: method.clone(), perm };
        validate_cap(&cap)?;
        caps.push(cap);
        t.caps_len = caps.len();
        if t.caps_phys != 0 {
            serialize_to_page(caps, t.caps_phys);
        }
        Ok(())
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = (path, method, perm);
        Ok(())
    }
}

/// Snapshot of parent caps for spawn subset check — reads parent task's caps Vec directly.
pub fn parent_caps_snapshot() -> Option<Vec<Cap>> {
    current_caps()
}

/// Helper: check traversal ancestors + leaf perm in one call.
/// `leaf_want` is R for reads, RW for writes.
pub fn check_path(caps: Option<&[Cap]>, components: &[&str], method: Option<&str>, leaf_want: Perm) -> Result<(), UnispaceError> {
    let Some(caps) = caps else {
        return Ok(());
    };
    if caps.is_empty() && !components.is_empty() {
        // Empty set = R0 on everything -> hide
        return Err(UnispaceError::NotFound);
    }
    // Ancestor R
    if !has_ancestor_r(caps, components) {
        return Err(UnispaceError::NotFound);
    }
    let leaf_path = join_path(components);
    // For method ops, leaf itself must be at least R-traversable? Spec says ancestor R required
    // for traversal, but method lives on leaf object. Require leaf R as well for method ops
    // (otherwise you could invoke method without being able to see object). Enforce leaf R.
    if method.is_some() {
        if !has_perm(caps, &leaf_path, None, Perm::R) {
            return Err(UnispaceError::NotFound);
        }
        if !has_perm(caps, &leaf_path, method, leaf_want) {
            // Distinguish R0 (hide) vs R but not RW (EACCES)
            let has_r = has_perm(caps, &leaf_path, method, Perm::R);
            if has_r && leaf_want == Perm::RW {
                return Err(UnispaceError::AccessDenied);
            }
            // Check if method cap absent vs leaf exists; map to MethodNotFound vs NotFound
            // Caller will map MethodNotFound -> -38, NotFound -> -2. For method R0 we hide as MethodNotFound.
            return Err(UnispaceError::MethodNotFound);
        }
    } else {
        if !has_perm(caps, &leaf_path, None, leaf_want) {
            let has_r = has_perm(caps, &leaf_path, None, Perm::R);
            if has_r && leaf_want == Perm::RW {
                return Err(UnispaceError::AccessDenied);
            }
            return Err(UnispaceError::NotFound);
        }
    }
    Ok(())
}

/// Manual fullcaps for INIT — every static object and method enumerated, no wildcard.
/// This satisfies spec "INIT MUST NOT have a wildcard ALL AUTH cap, it MUST be manually
/// granted capability to everything".
pub fn full_caps_for_init() -> Vec<Cap> {
    let mut caps = Vec::new();
    let mut add = |path: &str, method: Option<&str>, perm: Perm| {
        caps.push(Cap { path: String::from(path), method: method.map(|s| String::from(s)), perm });
    };
    // Root
    add("", None, Perm::RW);
    // Top-level dirs
    for d in ["A", "B", "sys", "dev", "driver", "input", "kernel", "proc"] {
        add(d, None, Perm::RW);
    }
    // sys leaves
    for f in ["sys/version", "sys/phys_mem", "sys/cpus", "sys/features"] {
        add(f, None, Perm::RW);
    }
    // dev/fb + methods
    add("dev/fb", None, Perm::RW);
    for m in ["mode", "clear"] { add("dev/fb", Some(m), Perm::RW); }
    // driver
    add("driver/debugserial", None, Perm::RW);
    add("driver/audio", None, Perm::RW);
    for m in ["play_tone", "play_pcm"] { add("driver/audio", Some(m), Perm::RW); }
    // input
    for f in ["input/devices", "input/events", "input/overflows", "input/kbd"] { add(f, None, Perm::RW); }
    for m in ["get", "flush"] { add("input/kbd", Some(m), Perm::RW); }
    // kernel/timer
    add("kernel/timer", None, Perm::RW);
    for m in ["sleep", "sleep_ms", "until", "epoch_secs"] { add("kernel/timer", Some(m), Perm::RW); }
    // proc - static self subtree
    for p in ["proc/self", "proc/self/status", "proc/self/mem", "proc/self/args", "proc/self/std", "proc/self/std/in", "proc/self/std/out", "proc/self/std/err"] {
        add(p, None, Perm::RW);
    }
    for m in ["exit", "yield", "kill", "spawn", "spawn_caps", "brk", "mmap", "munmap", "wait"] {
        add("proc/self", Some(m), Perm::RW);
    }
    // also allow proc/self/std/out:get etc (StdStream :get)
    add("proc/self/std/out", Some("get"), Perm::RW);
    add("proc/self/std/err", Some("get"), Perm::RW);
    add("proc/self/std/in", Some("get"), Perm::RW);
    // proc root methods? proc methods are on ProcDir, but we treat proc/self as alias.
    // VFS top-level dir methods for A and B
    for dir in ["A", "B", "A/init", "B/EFI", "B/EFI/BEDROCK"] {
        add(dir, None, Perm::RW);
        for m in ["create", "mkdir", "rmdir", "unlink", "rename", "symlink", "link", "mkfifo", "mknod", "stat"] { add(dir, Some(m), Perm::RW); }
    }
    // Files under A for demo
    for f in ["A/init/test", "A/init", "B/EFI/BEDROCK/INIT", "B/EFI/BEDROCK/STARTUP.WAV"] {
        add(f, None, Perm::RW);
        for m in ["stat", "truncate", "chmod", "chown", "utimens", "readlink"] { add(f, Some(m), Perm::RW); }
    }
    // Generic file method grants for any file under A/B - enumerated via prefix auto-grant at runtime,
    // but pre-grant a few more VFS dir methods for subdirs that may be created.
    // Ensure caps fit within page
    caps.truncate(MAX_CAPS_PER_TASK);
    caps
}

/// Serialize caps into a 4K supervisor page (header u32 count + entries).
/// Each entry: u32 path_len, path bytes, u32 method_len (0xFFFFFFFF = None), method bytes, u8 perm, 3 pad to 4.
/// Page is zeroed first; callers must ensure `caps.len() <= MAX_CAPS_PER_TASK`.
pub fn serialize_to_page(caps: &[Cap], phys: u64) {
    let va = crate::mm::layout::to_physmap(phys) as *mut u8;
    unsafe {
        core::ptr::write_bytes(va, 0, 4096);
        let mut off = 0usize;
        // u32 count
        *(va.add(off) as *mut u32) = caps.len() as u32;
        off += 4;
        for c in caps {
            if off + 8 + c.path.len() + 8 + c.method.as_ref().map(|m| m.len()).unwrap_or(0) + 4 > 4096 {
                break;
            }
            let path_bytes = c.path.as_bytes();
            *(va.add(off) as *mut u32) = path_bytes.len() as u32;
            off += 4;
            core::ptr::copy_nonoverlapping(path_bytes.as_ptr(), va.add(off), path_bytes.len());
            off += path_bytes.len();
            // align to 4
            while off % 4 != 0 {
                *va.add(off) = 0;
                off += 1;
            }
            if let Some(m) = &c.method {
                let mb = m.as_bytes();
                *(va.add(off) as *mut u32) = mb.len() as u32;
                off += 4;
                core::ptr::copy_nonoverlapping(mb.as_ptr(), va.add(off), mb.len());
                off += mb.len();
                while off % 4 != 0 {
                    *va.add(off) = 0;
                    off += 1;
                }
            } else {
                *(va.add(off) as *mut u32) = 0xFFFFFFFF;
                off += 4;
            }
            *va.add(off) = c.perm as u8;
            off += 1;
            *va.add(off) = 0;
            *va.add(off + 1) = 0;
            *va.add(off + 2) = 0;
            off += 3;
        }
    }
}
