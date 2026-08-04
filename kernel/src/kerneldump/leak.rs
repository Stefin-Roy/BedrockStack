//! Reference-cycle / leak detector (§8.7), the enforcement of invariant I4
//! (lifetime = reachability).
//!
//! A post-process over the projection snapshot that runs **without perturbing
//! the live system**: it reads the store's weak records and every domain's
//! table, never invoking a hook or mutating state. Algorithm (§8.7
//! "Operationally the detector walks from the projection's roots …"):
//!
//! 1. Walk from the live domains' tables and mark every reachable node — the
//!    root set is every `ObjId` a domain holds, expanded along parent/family
//!    edges to its whole interior (a family root covers its tree, §3.5).
//! 2. Any live node **not** marked is unreachable from the surface and should
//!    have been reclaimed by drop-death (I4). Such nodes are the leaked
//!    cycles: a set reachable from one another only through held caps with no
//!    live root capability reachable from a visible domain (§8.7 "leaked
//!    cycle"). Legitimate mutual references are reachable from the surface, so
//!    they are already marked and suppressed (§8.7 "legitimate mutual
//!    reference").
//! 3. `infra:` seed nodes are the I4 boot-era exemption (the Principal owns
//!    them even with no live cap), so they are never flagged.
//!
//! Returns `true` if any leak was found — the building-block that lets the
//! CI-equivalent boot script fail a run on a leaked node (§8.7: "run it after
//! every test-suite execution").

extern crate alloc;

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::obj::domain::all_domains;
use crate::obj::store::object_store;
use crate::obj::ObjId;

/// Run the leak detector over the current projection. Returns `true` (a leak)
/// or `false` (clean).
pub fn leak_detect(w: &mut impl Write) -> bool {
    let _ = writeln!(w, "=== kerneldump leak detector (I4, §8.7) ===");

    // Reachable-from-surface root set: every node id a live domain table holds.
    let mut roots: BTreeSet<u64> = BTreeSet::new();
    for d in all_domains() {
        for (_, node_id, _, _) in d.table.snapshot() {
            roots.insert(node_id.0);
        }
    }

    // Adjacency over parent + family_root edges. A root node's own family_root
    // is None, so there is no self-edge; guard against any self-parent.
    let records = object_store().lock_records();
    let mut children: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    for (&id, rec) in records.iter() {
        if let Some(p) = rec.parent {
            if p.0 != id {
                children.entry(p.0).or_default().push(id);
            }
        }
        if let Some(fr) = rec.family_root {
            if fr.0 != id {
                children.entry(fr.0).or_default().push(id);
            }
        }
    }

    // Mark the closure of the root set along the interior edges.
    let mut reachable: BTreeSet<u64> = BTreeSet::new();
    let mut frontier: Vec<u64> = roots.iter().copied().collect();
    while let Some(cur) = frontier.pop() {
        if reachable.insert(cur) {
            if let Some(kids) = children.get(&cur) {
                for k in kids {
                    frontier.push(*k);
                }
            }
        }
    }

    // Report every non-seed node the projection could not reach from the
    // surface. Within the unreached residue each mutually-referencing cluster
    // is a reported cycle (§8.7).
    let mut leaked_any = false;
    let mut leak_count = 0usize;
    for (&id, rec) in records.iter() {
        // I4 seed exemption (§9.3) and §8.8 forensic retention: nodes of a
        // currently-severed family are kept for "what died with that root"
        // reports and so are never counted as leaks.
        let in_severed_family = match rec.family_root {
            Some(fr) => object_store().is_cascade_severed(fr),
            None if object_store().is_cascade_severed(ObjId(id)) => true,
            None => false,
        };
        if reachable.contains(&id) || rec.kind.starts_with("infra:") || in_severed_family {
            continue;
        }
        // A dead record — the store's `Weak` fired, so drop-death reclaimed
        // the node (R7) — is forensic history (§8.8), not a leak. Only a node
        // that is STILL alive yet unreachable from the surface violates I4
        // (§8.7: "the store Weak never fires while no live capability reaches
        // it").
        if rec.weak.upgrade().is_none() {
            continue;
        }
        leaked_any = true;
        leak_count += 1;
        let _ = writeln!(
            w,
            "LEAK node 0x{:04x} kind=\"{}\" parent={}",
            id,
            rec.kind,
            match rec.parent {
                Some(p) => format!("0x{:04x}", p.0),
                None => String::from("none"),
            }
        );
    }

    if leaked_any {
        let _ = writeln!(w, "leak_detect: FAIL — {} unreachable node(s) (I4)", leak_count);
        true
    } else {
        let _ = writeln!(w, "leak_detect: OK — no unreachable nodes (I4 holds)");
        false
    }
}