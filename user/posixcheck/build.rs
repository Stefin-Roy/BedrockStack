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

    let ws_wsl = to_wsl(&ws);
    let gcc = format!(
        "cd {} && gcc {} {} -o {} 2>&1",
        ws_wsl,
        WSL_GCC_FLAGS,
        to_wsl(&src),
        to_wsl(&obj),
    );
    match wsl_ok(&gcc) {
        Ok(o) => {
            if !o.trim().is_empty() {
                eprintln!("[posixcheck] gcc: {o}");
            }
        }
        Err(e) => panic!("gcc failed for checks.c:\n{e}"),
    }

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

    println!("cargo:rustc-link-search=native={}", out.display());
}
