//! Graph projection — `kerneldump graph` (seed gate: node census; later:
//! full walker, §7.13).
//!
//! Walks the `ObjectStore` and emits a read-only snapshot of every node's
//! record. The Revocation phase adds the full command-line form (roots / edges / caps /
//! contracts / revocations). Read-only: it never calls the mint, a hook, or
//! mutates the store (properties a–d, §7.13). The weak records carry only
//! `kind`/`parent`/`family_root`; `surface` and `contracts` are recovered by
//! upgrading a live node's `Weak` when it is still alive, and omitted for
//! dead records (forensics).

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write;

use crate::obj::cap_handle::HandleState;
use crate::obj::domain::all_domains;
use crate::obj::rights::{CapRights, Rights};
use crate::obj::store::{object_store, ObjRecord};
use crate::obj::ObjId;

/// Print the node census: one line per store record plus a summary (§7.13).
/// Retained from the Seed phase; the revocation gate also uses it as a cheap
/// pre/post snapshot.
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

/// Full projection: every flag on (§7.13).
pub fn graph(w: &mut impl Write) {
    graph_with_flags(w, &["--roots", "--edges", "--caps", "--contracts", "--revocations"]);
}

/// Full projection filtered by the §7.13 command-line flags. Unknown flags are
/// ignored; an empty flag list prints only the node/parent skeleton.
pub fn graph_with_flags(w: &mut impl Write, flags: &[&str]) {
    let show_roots = flags.contains(&"--roots");
    let show_edges = flags.contains(&"--edges");
    let show_caps = flags.contains(&"--caps");
    let show_contracts = flags.contains(&"--contracts");
    let show_rev = flags.contains(&"--revocations");

    let _ = writeln!(w, "=== kerneldump graph (full projection) ===");

    // held-by: node id -> [(domain id, CapId, rights, state)] across every
    // registered domain's table snapshot (§7.13).
    let mut held: BTreeMap<u64, Vec<(u32, u64, CapRights, HandleState)>> = BTreeMap::new();
    for d in all_domains() {
        for (cid, node_id, rights, state) in d.table.snapshot() {
            held.entry(node_id.0).or_default().push((d.id, cid.0, rights, state));
        }
    }

    // interior: parent edge index — node id -> child ids (§2.1, §7.13).
    // Snapshot records under the lock, then release before checking deny/cascade
    // to avoid holding OBJECT_RECORDS while acquiring OBJECT_DENY/OBJECT_CASCADE.
    let (records_snapshot, interior) = {
        let guard = object_store().lock_records();
        let mut interior: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        for (&id, rec) in guard.iter() {
            if let Some(p) = rec.parent {
                interior.entry(p.0).or_default().push(id);
            }
        }
        let snapshot: Vec<(u64, ObjRecord)> = guard
            .iter()
            .map(|(&id, rec)| (id, ObjRecord {
                id: rec.id,
                kind: rec.kind.clone(),
                parent: rec.parent,
                family_root: rec.family_root,
                weak: rec.weak.clone(),
            }))
            .collect();
        (snapshot, interior)
    };

    // §7.13 `--roots`: the family roots / parentless nodes of the projection.
    if show_roots {
        let roots: Vec<String> = records_snapshot
            .iter()
            .filter(|(_, r)| r.parent.is_none())
            .map(|(id, r)| format!("0x{:04x}({})", id, r.kind))
            .collect();
        let _ = writeln!(w, "roots: [{}]", roots.join(" "));
    }

    let mut count = 0usize;
    for (id, rec) in &records_snapshot {
        let _ = write!(
            w,
            "node 0x{:04x} kind=\"{}\" parent={}",
            id,
            rec.kind,
            match rec.parent {
                Some(p) => format!("0x{:04x}", p.0),
                None => String::from("none"),
            }
        );
        match rec.family_root {
            Some(fr) if fr.0 == *id => {
                let _ = write!(w, " family=root");
            }
            Some(_) => {
                let _ = write!(w, " family=child");
            }
            None => {
                let _ = write!(w, " family=none");
            }
        }
        let _ = writeln!(w);

        // live nodes expose surface + contracts via the store's weak (§4.1,
        // §4.3); dead records (forensics, §7.13 property d) omit them.
        let alive = rec.weak.upgrade();
        if show_contracts {
            if let Some(node) = alive.as_ref() {
                let contracts: Vec<String> =
                    node.contracts().iter().map(|c| format!("{:?}", c)).collect();
                let _ = writeln!(w, "  contracts: [{}]", contracts.join(" "));
                if let Some(s) = node.surface() {
                    let attrs: Vec<String> =
                        s.attrs.iter().map(|a| a.name.to_string()).collect();
                    let _ = writeln!(w, "  surface: {{kind: \"{}\" attrs: [{}]}}", s.kind, attrs.join(" "));
                }
            } else {
                let _ = writeln!(w, "  contracts: (dead record — forensics only)");
            }
        }

        if show_edges {
            if let Some(children) = interior.get(id) {
                let kids: Vec<String> = children.iter().map(|c| format!("0x{:04x}", c)).collect();
                let _ = writeln!(w, "  interior: [{}]", kids.join(" "));
            }
        }

        if show_caps {
            if let Some(entries) = held.get(id) {
                for (dom, cid, rights, state) in entries {
                    let _ = writeln!(
                        w,
                        "  held-by: domain {} cap=0x{:04x} rights={} state={:?}",
                        dom,
                        cid,
                        fmt_rights(rights),
                        state
                    );
                }
            }
        }

        if show_rev {
            let store = object_store();
            let revoked = store.is_denied(ObjId(*id));
            if rec.family_root.is_none() && store.is_cascade_severed(ObjId(*id)) {
                let _ = writeln!(w, "  revocations: cascade-sealed (deny-list: {})", revoked);
            } else if revoked {
                let _ = writeln!(w, "  revocations: denied (1)");
            } else {
                let _ = writeln!(w, "  revocations: 0");
            }
        }

        count += 1;
    }

    let _ = writeln!(w, "nodes: {}", count);
    let _ = writeln!(w, "domains: {}", all_domains().len());
}

/// Human-readable universal-rights mask (§3.3).
fn fmt_rights(r: &CapRights) -> String {
    let mut out: Vec<String> = Vec::new();
    for (name, bit) in [
        ("QUERY", Rights::QUERY),
        ("INVOKE", Rights::INVOKE),
        ("TRAVERSE", Rights::TRAVERSE),
        ("MINT", Rights::MINT),
        ("REVOKE", Rights::REVOKE),
    ] {
        if r.uni.contains(bit) {
            out.push(String::from(name));
        }
    }
    if out.is_empty() {
        out.push(String::from("(none)"));
    }
    let contract = r.contract.bits();
    if contract != 0 {
        out.push(format!("contract=0x{:x}", contract));
    }
    out.join(" ")
}
