# RootGraph Objects / Capability Model — Invariants

**Version:** 0.1.0
**Date:** 2026-08-03
**Source:** `kernel/src/obj/{mod,rights,cap_handle,table,contract,registry,store,mint,bootstrap,driver,separation}.rs`
**Status:** Active (P2)

> **Note:** This subsystem implements the RootGraph object-graph / capability
> model of `Documentation/RootGraph.md`. The canonical property set is the
> numbered **I1–I10** of §9.3 of that document; this file mirrors that wording
> verbatim and cites the kernel code that enforces it. It is P2: mint creates
> placeholder `StubNode`s, service providers are reachable as capabilities,
> and the registry + separation proof run at boot — but real heap/phys hooks
> and revocable (deny-list) nodes are deferred to P3.

---

## State Invariants

**I1 (no ambient authority) — Every access to a protected resource is the
result of `R6` on a `Live` capability:**
There is no code path that reaches a node except through a domain table.
The single dispatch entry point is `invoke`, which PERMITs via `resolve`
before any hook body runs; `resolve` applies the whole slope (slot fetch →
Live → `INVOKE` → contract membership → per-hook contract right) and only a
`Live` handle passing every check yields the node's `Arc`. The raw, PERMIT-less
`get` exists solely for an object's *own* dispatch (which already passed
PERMIT). Providers are endowed only by inserting `CapHandle`s into a domain's
table (`bootstrap`, `driver`); nothing reaches a node out of band.
- Location: `obj/table.rs::resolve` (`PERMIT` slope, table.rs:87-114), `obj/mod.rs::invoke` (table.rs:150 then `dispatch`), `obj/table.rs::get` (PERMIT-less fetch, table.rs:58-66), `obj/bootstrap.rs:67-110` (endowment via table insert), `obj/driver.rs:43-54`

**I2 (mint monopoly) — A new family root is created only by `R1`:**
At all times `{ c ∈ C : MINT ∈ rights(c) }` has cardinality ≤ 1, and that cap
is the Principal's — the Principal exercises it through the bootstrapper at
boot, and the bootstrapper self-revokes at boot end, after which the set of
nodes that could mint is empty. `mint` is only callable given a
`PrincipalContext`; once `MINT_FROZEN` is set by `finalize_mint()` every
subsequent `mint` fails with `MintAuthorityGone`, with an assert as a loud
canary against a concurrent/ISR-path mint racing the self-revoke. Only
`bootstrap()` holds a `PrincipalContext`, and only before self-revoke.
- Location: `obj/mint.rs::mint` (obj/mint.rs:35-57), `obj/mint.rs::PrincipalContext` (obj/mint.rs:16), `obj/mint.rs::finalize_mint` (obj/mint.rs:22-24), `obj/mint.rs::MINT_FROZEN` (obj/mint.rs:19), `obj/bootstrap.rs:50-63` (with `finalize_mint` called as `init()`'s last statement)

**I3 (monotone attunement) — for any attunement chain
`c₁ → c₂ → … → cₖ`, `rights(c₁) ⊇ rights(c₂) ⊇ … ⊇ rights(cₖ)`:**
No capability ever gains a right; every attuned capability is more precise
and tinier than its parent. `attune` ANDs both the universal mask and the
contract-rights mask (`NoAmplification` is a canary, unreachable via AND),
and `dup_limited` is the only handle-derivation path that shrinks rights —
`dup` copies rights identically and there is no operation that increases
them.
- Location: `obj/rights.rs::Rights::attune` (obj/rights.rs:37-44), `obj/rights.rs::CapRights::attune` (both dims, obj/rights.rs:125-138), `obj/table.rs::dup_limited` (obj/table.rs:147-148), `obj/table.rs::dup` (same-rights copy, obj/table.rs:117-128)

**I4 (lifetime = reachability) — `n` is allocated iff `reach(n) ≠ ∅` or `n`
is the Principal or a boot-era seed node:**
`reach(n) = ∅` implies `n`'s resources are reclaimed (R7). A `CapHandle`
holds the node through a strong `Arc<dyn Obj>`, so a node's lifetime is
exactly the life of the caps that reach it; the store holds no strong
reference (see I7), so nothing can resurrect a dead node. Default revocation
is drop-death (`RevocationPolicy::DropDeath`); deny-list `Revocable` nodes
are deferred to P3.
- Location: `obj/cap_handle.rs::CapHandle{ node: Arc<dyn Obj> }` (obj/cap_handle.rs:27-32), `obj/cap_handle.rs::RevocationPolicy` (obj/cap_handle.rs:18-23), `obj/table.rs::resolve` returns `Arc::clone(&h.node)` (obj/table.rs:109,113)

**I5 (one parent) — every node ≠ P has exactly one parent edge:**
`P` has none. The node-store record carries a single `parent: Option<ObjId>`
written once at registration; there is no structure for multiple parents.
- Location: `obj/store.rs::ObjRecord.parent` (obj/store.rs:14-18), `obj/store.rs::register` (parent written once, obj/store.rs:38-44)

**I6 (subsumption consistency) — if c names family root r, then any child
materialized under r has rights ⊆ rights(c), and r's parent edge is r's own
materializer's edge:**
A node is in exactly one family. Attunement restricts rights to a subset
(`attune`), and a node is bound to one parent (`ObjRecord.parent`), so a
child's rights shrink within its parent's family and no node straddles two
families; roots minted by `mint` register with `parent = None` and start new
families.
- Location: `obj/table.rs::dup_limited` (rights ⊆, obj/table.rs:132-151), `obj/rights.rs::attune` (obj/rights.rs:37-44,125-138), `obj/store.rs::parent` (one family, obj/store.rs:17), `obj/mint.rs::mint` (root registers with `None` parent, obj/mint.rs:49)

**I7 (store weakness) — the ObjectStore holds only weak references; it
never affects `reach`:**
`ObjRecord` holds no node reference at all (just `id`, `kind`, `parent`),
so the store cannot keep a node alive nor resurrect one. Projection is
read-only and gated by the store-node capability.
- Location: `obj/store.rs::ObjRecord` (no `Arc`/`Weak` node field, obj/store.rs:14-18), `obj/store.rs::ObjectStore` (obj/store.rs:21-24), `obj/store.rs::lock_records` (read-only, obj/store.rs:49-50)

**I8 (fast-path bound) — the `PERMIT` check of `R6` is O(1):**
A constant number of word-size operations: one `IrqMutex` acquire, one slot
index, Live, `INVOKE`, contract membership, per-hook contract-right test
(and, in P3, a deny-bit load). All are independent of table size `n` and the
contract-membership probe is on a small, frozen set. See §9.4 of
`RootGraph.md` (I8).
- Location: `obj/table.rs::resolve` (obj/table.rs:87-114), `obj/mod.rs::Obj::hook_contract_right` (obj/mod.rs:67-70), reference `RootGraph.md` §9.4 (fast-path bound, five named steps)

**I9 (dispatch safety) — no table-slot lock is held across a hook body;
in-flight dispatch holds an `Arc` that prevents drop-death reclamation
until the reply returns:**
`resolve` releases the `IrqMutex` before returning the cloned `Arc`, so
`invoke`'s subsequent `dispatch` runs lock-free; the strong `Arc` keeps the
node alive even if the last table entry is revoked mid-hook.
- Location: `obj/mod.rs::invoke` (dispatch after `resolve` returns, obj/mod.rs:150-151), `obj/table.rs::resolve` (lock released on return; `Arc::clone` at obj/table.rs:113)

**I10 (contract identity) — `ContractId` is content-addressed; two distinct
`(name, surface, hooks)` tuples never share an id:**
The FNV-1a id is a hash of the identity byte stream `(name, surface.kind,
attrs, ordered hooks)`, which cannot collide because the `0xFF` separators
never appear in ASCII names/type discriminants — distinct tuples always
yield distinct streams. `ContractRegistry::register` validates the id: a
distinct tuple claiming an already-registered `ContractId` is refused loudly
with `ObjError::ContractCollision` and the genuine entry is left untouched;
re-registering the identical tuple is idempotent `Ok`. `ObjError::ContractCollision`
is defined at `obj/mod.rs:109`.
- Location: `obj/contract.rs::ContractId::of` (obj/contract.rs:70-125), `obj/contract.rs::ContractRegistry::register` (obj/contract.rs:163-178), `obj/contract.rs::ObjError::ContractCollision` (via `obj/mod.rs:110`), `obj/contract.rs::same_identity` (obj/contract.rs:198-242)

---

## Cross-Check

The invariants are exercised at boot, not only asserted statically.
`obj/separation.rs` proves them against the live graph right after bootstrap
and before SMP:

- **I1 / I8 — no ambient authority, reachability only via the table:** a
  genuine DMA alloc through the endowed cap succeeds (`separation.rs:27`);
  an unendowed id is refused with `NoSuchCap` (`separation.rs:47-54`); a
  foreign contract is refused by PERMIT with `Denied` (`separation.rs:57-59`);
  and a per-hook contract right (READ held, CALL required) is refused
  (`separation.rs:65-72`). The driver domain resolves exactly its two
  endowed caps and nothing else (`separation.rs:81-103`).
- **C8 separation — the driver domain is disjoint from boot:** endowed
  DMA/PCI-config resolve; unendowed ids and foreign (serial / registry)
  contracts resolve `NoSuchCap`/`Denied` (`separation.rs:81-126`); its table
  holds exactly `count() == 2` (`separation.rs:103`).
- **I10 — contract identity validated loudly, not in the field:** a
  duplicate-name contract with an empty hook list claiming an
  already-registered `ContractId` is refused with `ContractCollision`, and
  the genuine entry survives (`separation.rs:162-187`); registration is
  idempotent for the identical tuple (`separation.rs:156-160`).
- **Registry is discovery-by-owned-capability, not ambient:** a domain
  without the registry cap cannot consult it (`Denied`); a domain holding it
  looks up `dma:alloc` and gets name + doc back; a bogus id returns
  `Reply::None` (`separation.rs:119-152`).

- Location: `obj/separation.rs::run` (obj/separation.rs:22-192)