//! Build script for the `doom` userspace crate.
//!
//! Compiles the GPL vendored `doomgeneric` C engine (plus our platform glue
//! and shims) with WSL gcc into a static archive, then hands it to the link
//! via `cargo:rustc-link-search` + the `#[link]` attribute in `src/lib.rs`.
//! The engine is freestanding: no libc, no runtime, no host headers — only
//! what `user/doom/include/` provides plus gcc's freestanding builtins.
//!
//! WSL is mandatory (same as `create_image.py`).  Sources are listed in
//! `build_sources.txt`, one relative-to-workspace-root path per line.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const WSL_GCC_FLAGS: &[&str] = &[
    "-c",
    "-O2",
    "-ffreestanding",
    "-fno-stack-protector",
    "-fno-pic",
    "-fno-pie",
    "-mcmodel=large",
    "-mno-red-zone",
    "-fno-builtin",
    "-w",
    "-I",
    "third_party/doomgeneric",
    "-I",
    "user/libc/include",
];

/// FNV-1a hash of a path, used for stable object filenames.
fn hash(s: &str) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h
}

/// Translate a Windows path (`D:\a\b`) to the WSL view (`/mnt/d/a/b`).
/// Handles the `\\?\` extended-length prefix that `canonicalize()` emits.
fn to_wsl(p: &Path) -> String {
    let mut s = p.to_string_lossy().into_owned();
    if let Some(rest) = s.strip_prefix("\\\\?\\") {
        s = rest.to_string();
    }
    let b = s.as_bytes();
    if b.len() >= 3 && b[1] == b':' && b[2] == b'\\' {
        let drive = (b[0] as char).to_ascii_lowercase();
        format!("/mnt/{}/{}", drive, &s[3..].replace('\\', "/"))
    } else {
        s.replace('\\', "/")
    }
}

fn wsl_ok(cmd: &str) -> Result<String, String> {
    let out = Command::new("wsl")
        .arg("bash")
        .arg("-c")
        .arg(cmd)
        .output()
        .map_err(|e| format!("failed to spawn wsl: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let ws = manifest.join("..").join("..");
    let ws = ws.canonicalize().unwrap_or(ws);
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());

    let src_list = manifest.join("build_sources.txt");
    println!("cargo:rerun-if-changed={}", src_list.display());
    let text = fs::read_to_string(&src_list).expect("read user/doom/build_sources.txt");
    let files: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect();

    let objdir = out.join("obj");
    fs::create_dir_all(&objdir).expect("create obj dir");

    // Headers are real compilation inputs too: an edit to doomfeatures.h or
    // config.h must rebuild every object that includes it, otherwise the
    // archive becomes a silent mix of feature flags (e.g. i_sound.c built
    // with FEATURE_SOUND but m_config.c without).  Track every header in the
    // include paths, and treat an object as stale if any header is newer
    // than it.
    let mut headers: Vec<PathBuf> = Vec::new();
    for dir in ["third_party/doomgeneric", "user/libc/include"] {
        let base = ws.join(dir);
        if !base.exists() {
            continue;
        }
        let mut stack: Vec<PathBuf> = vec![base];
        while let Some(d) = stack.pop() {
            if let Ok(rd) = fs::read_dir(&d) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        stack.push(p);
                    } else if p.extension().map_or(false, |x| x == "h") {
                        headers.push(p);
                    }
                }
            }
        }
    }
    for h in &headers {
        println!("cargo:rerun-if-changed={}", h.display());
    }
    let header_cutoff = headers
        .iter()
        .filter_map(|h| fs::metadata(h).ok().and_then(|m| m.modified().ok()))
        .max();

    let ws_wsl = to_wsl(&ws);
    let flag_key = WSL_GCC_FLAGS.join("\x1f");
    let mut objs: Vec<String> = Vec::new();
    for f in &files {
        let src = ws.join(f);
        println!("cargo:rerun-if-changed={}", src.display());
        // Include the compile flags in the object name.  This prevents an
        // object built with (for example) GCC's small code model from being
        // silently reused after the high-half image switches to large-model
        // relocations.
        let obj_key = format!("{f}\0{flag_key}");
        let obj = objdir.join(format!("{}.o", hash(&obj_key)));
        let src_m = fs::metadata(&src).and_then(|m| m.modified()).ok();
        let obj_m = fs::metadata(&obj).and_then(|m| m.modified()).ok();
        let stale = match (src_m, obj_m) {
            (Some(s), Some(o)) => o < s,
            (Some(_), None) => true,
            _ => true,
        } || match (header_cutoff, obj_m) {
            (Some(h), Some(o)) => o < h,
            (Some(_), None) => true,
            _ => false,
        };
        if stale {
            let cmd = format!(
                "cd {} && gcc {} {} -o {} 2>&1",
                ws_wsl,
                WSL_GCC_FLAGS.join(" "),
                to_wsl(&src),
                to_wsl(&obj),
            );
            match wsl_ok(&cmd) {
                Ok(out) => {
                    if !out.trim().is_empty() {
                        eprintln!("[doom] gcc {}: {out}", f);
                    }
                }
                Err(e) => panic!("gcc failed for {}:\n{}", f, e),
            }
        }
        objs.push(to_wsl(&obj));
    }

    let archive = out.join("libdoomgeneric.a");
    // `ar rcs` updates named members but does not remove members from an
    // older archive.  The object-name key includes the compiler flags, so
    // replace the generated archive before collecting the current objects;
    // otherwise old small-model members would be linked alongside the new
    // large-model objects.
    let _ = fs::remove_file(&archive);
    let ar_cmd = format!(
        "cd {} && ar rcs {} {}",
        ws_wsl,
        to_wsl(&archive),
        objs.join(" "),
    );
    if let Err(e) = wsl_ok(&ar_cmd) {
        panic!("ar failed: {e}");
    }

    println!("cargo:rustc-link-search=native={}", out.display());
}
