//! Build script for the `posixcheck` crate.
//!
//! Compiles the small `src/checks.c` (C-ABI conformance checks against the
//! permissive `user/libc` headers) with WSL gcc into `libchecks.a`, then
//! hands it to the link via `cargo:rustc-link-search` + the `#[link]`
//! attribute in `src/main.rs`.  Same freestanding flags as the doom port.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const WSL_GCC_FLAGS: &str = "-c -O2 -ffreestanding -fno-stack-protector -fno-pic -fno-pie -mcmodel=large -mno-red-zone -fno-builtin -w -I user/libc/include";

/// Opt-in sccache prefix for WSL gcc. Respects `SCCACHE=1` or `RUSTC_WRAPPER=sccache`.
fn gcc_prefix() -> &'static str {
    let want = std::env::var("SCCACHE").is_ok()
        || std::env::var("RUSTC_WRAPPER")
            .map(|v| v.contains("sccache"))
            .unwrap_or(false)
        || std::env::var("CC_WRAPPER")
            .map(|v| v.contains("sccache"))
            .unwrap_or(false);
    if want { "sccache gcc" } else { "gcc" }
}

/// Translate a Windows path (`D:\a\b`) to the WSL view (`/mnt/d/a/b`).
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
    let ws = manifest.join("..").join("..").canonicalize().unwrap();
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());

    let src = ws.join("user/posixcheck/src/checks.c");
    let obj = out.join("checks.o");
    let archive = out.join("libchecks.a");
    println!("cargo:rerun-if-changed={}", src.display());
    // Track headers so cargo fingerprint is correct — but we also do an mtime guard
    // to avoid spawning WSL gcc on no-op builds (cargo still re-runs build.rs, but we skip work).
    let include = ws.join("user/libc/include");
    let mut headers: Vec<PathBuf> = Vec::new();
    if include.exists() {
        let mut stack: Vec<PathBuf> = vec![include];
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

    // mtime guard — match user/doom/build.rs: skip WSL gcc/ar if outputs are newer than inputs
    let src_m = fs::metadata(&src).and_then(|m| m.modified()).ok();
    let obj_m = fs::metadata(&obj).and_then(|m| m.modified()).ok();
    let arch_m = fs::metadata(&archive).and_then(|m| m.modified()).ok();
    let need_gcc = match (src_m, obj_m) {
        (Some(s), Some(o)) => o < s,
        (Some(_), None) => true,
        _ => true,
    } || match (header_cutoff, obj_m) {
        (Some(h), Some(o)) => o < h,
        (Some(_), None) => true,
        _ => false,
    };
    // Re-archive only if object newer than archive or gcc is needed
    let need_ar = need_gcc
        || match (obj_m, arch_m) {
            (Some(o), Some(a)) => a < o,
            (_, None) => true,
            _ => false,
        };

    let ws_wsl = to_wsl(&ws);
    let gcc_bin = gcc_prefix();
    if need_gcc {
        let gcc_inner = if gcc_bin == "sccache gcc" {
            format!(
                "if command -v sccache >/dev/null 2>&1; then sccache gcc {flags} {src} -o {obj}; else gcc {flags} {src} -o {obj}; fi",
                flags = WSL_GCC_FLAGS,
                src = to_wsl(&src),
                obj = to_wsl(&obj),
            )
        } else {
            format!("gcc {} {} -o {}", WSL_GCC_FLAGS, to_wsl(&src), to_wsl(&obj))
        };
        let gcc = format!("cd {} && {} 2>&1", ws_wsl, gcc_inner);
        match wsl_ok(&gcc) {
            Ok(o) => {
                if !o.trim().is_empty() {
                    eprintln!("[posixcheck] gcc: {o}");
                }
            }
            Err(e) => panic!("gcc failed for checks.c:\n{e}"),
        }
    }

    if need_ar {
        let _ = fs::remove_file(&archive);
        let ar = format!(
            "cd {} && ar rcs {} {} 2>&1",
            ws_wsl,
            to_wsl(&archive),
            to_wsl(&obj)
        );
        if let Err(e) = wsl_ok(&ar) {
            panic!("ar failed: {e}");
        }
    }

    println!("cargo:rustc-link-search=native={}", out.display());
}
