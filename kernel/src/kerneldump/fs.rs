//! `kerneldump fs-walk` — capability-native VFS diagnostic (§7.12.3).
//!
//! Exercises the mounted filesystem through capabilities: finds a DirNode cap
//! in the boot table, calls readdir, traverses into a child, and prints the
//! results. Labels are surface data; the kernel resolves no string.

extern crate alloc;

use alloc::string::{String, ToString};
use core::fmt::Write;

use crate::obj::bootstrap::boot_domain;
use crate::obj::{invoke, Args, Reply, Value};
use crate::obj::fs;

pub fn fs_walk(w: &mut impl Write) {
    let _ = writeln!(w, "=== kerneldump fs-walk (capability-native VFS, §7.12.3) ===");

    let table = &boot_domain().table;

    // 1. Find the first DirNode cap in the boot table (has readdir).
    let dir_cap = match table.resolve_first(fs::DIR_CONTRACT, fs::DIR_READDIR) {
        Some(id) => id,
        None => {
            let _ = writeln!(w, "  (no dir cap found in boot table)");
            let _ = writeln!(w, "=== fs-walk complete ===");
            return;
        }
    };

    // 2. readdir the root directory.
    match invoke(table, dir_cap, fs::DIR_CONTRACT, fs::DIR_READDIR, &Args::none()) {
        Ok(Reply::Caps(caps)) => {
            let _ = writeln!(w, "  readdir root: {} children", caps.len());
            for h in &caps {
                let label = match invoke(
                    table,
                    h.id,
                    fs::DIR_CONTRACT,
                    fs::DIR_LABEL,
                    &Args::none(),
                ) {
                    Ok(Reply::Data(vals)) if !vals.is_empty() => match &vals[0] {
                        Value::Str(s) => s.to_string(),
                        _ => String::from("?"),
                    },
                    _ => String::from("(no label)"),
                };
                let _ = writeln!(
                    w,
                    "    label=\"{}\" kind={}",
                    label,
                    h.node.kind()
                );
            }
        }
        _ => {
            let _ = writeln!(w, "  readdir root: unexpected reply variant");
        }
    }

    // 3. Traverse into the "tmp" directory (created by lib.rs as A>tmp).
    let traverse_args = Args {
        vals: alloc::vec![Value::Str("tmp")],
    };
    match invoke(
        table,
        dir_cap,
        fs::DIR_CONTRACT,
        fs::DIR_TRAVERSE,
        &traverse_args,
    ) {
        Ok(Reply::Caps(caps)) if !caps.is_empty() => {
            let h = &caps[0];
            let _ = writeln!(
                w,
                "  traverse(\"tmp\"): kind={} id=0x{:04x}",
                h.node.kind(),
                h.id.0
            );
            // readdir the tmp dir — expect 0 children (empty tmpfs dir).
            match invoke(table, h.id, fs::DIR_CONTRACT, fs::DIR_READDIR, &Args::none()) {
                Ok(Reply::Caps(inner_caps)) => {
                    let _ = writeln!(
                        w,
                        "  readdir tmp: {} children (expect 0)",
                        inner_caps.len()
                    );
                }
                _ => {
                    let _ = writeln!(w, "  readdir tmp: empty or failed");
                }
            }
        }
        _ => {
            let _ = writeln!(w, "  traverse(\"tmp\"): failed");
        }
    }

    let _ = writeln!(w, "=== fs-walk complete ===");
}
