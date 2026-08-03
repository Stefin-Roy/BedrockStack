//! Graph projection stub — `kerneldump graph` (P1 gate: node census only).
//!
//! Walks the `ObjectStore` and emits a read-only snapshot of every node's
//! record. P1 returns only the census; the full command-line form (roots /
//! edges / caps / contracts / revocations) is P5 (§7.13). Read-only: it never
//! calls the mint, a hook, or mutates the store (properties a–d).

extern crate alloc;

use alloc::collections::BTreeMap;
use core::fmt::Write;

use crate::obj::store::object_store;

/// Print the node census: one line per store record plus a summary (§7.13).
pub fn graph_census(w: &mut impl Write) {
    let _ = writeln!(w, "=== kerneldump graph (node census) ===");

    let guard = object_store().lock_records();
    let mut count = 0usize;
    let mut tally: BTreeMap<&str, usize> = BTreeMap::new();

    for (&id, rec) in guard.iter() {
        let _ = write!(w, "node 0x{:04x} kind=\"{}\" parent=", id, rec.kind);
        match rec.parent {
            Some(p) => {
                let _ = writeln!(w, "0x{:04x}", p.0);
            }
            None => {
                let _ = writeln!(w, "none");
            }
        }
        *tally.entry(rec.kind.as_str()).or_insert(0) += 1;
        count += 1;
    }

    let _ = writeln!(w, "nodes: {}", count);
    for (kind, n) in &tally {
        let _ = writeln!(w, "  kind \"{}\": {}", kind, n);
    }
}