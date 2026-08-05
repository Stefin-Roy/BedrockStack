# RootGraph Objects / Capability Model — Invariants

**Version:** 0.5.0
**Date:** 2026-08-05
**Source:** `kernel/src/obj/{mod,rights,cap_handle,table,contract,registry,store,mint,bootstrap,driver,separation,paged_isolation,nodes,memregion,devices,fs,adapters}.rs`
**Status:** Active (P6-B, capability-system upgrade complete)

> **Note:** This subsystem implements the RootGraph object-graph / capability
> model of `Documentation/RootGraph.md`. The canonical property set is the
> numbered **I1–I10** of §9.3 of that document; this file mirrors that wording
> verbatim and cites the kernel code that enforces it. The object/capability
> graph is fully implemented — no stubs, no placeholders. The five primitive
> family roots are real nodes wrapping the kernel's physical modules —
> `PhysMemNode` (`BitmapAllocator`, contract `physmem:allocation`),
> `HeapNode` (`heap:allocation`), `AddressSpaceNode` (`Vmm`,
> `mm:address_space`), `CpuRootNode` (`smp:cpu`), `IrqRootNode` (`irq:vector`)
> — minted by `mint_node` (§7.6) with real universal *and* contract-right
> masks (`PRIM_CONTRACT = READ|WRITE|CALL`, `obj/bootstrap.rs`), and every
> frame handed out is a pooled `MemRegion` capability (`mem:region`; no
> allocation inside the allocation hooks). The DMA allocator allocates and
> maps through the caller's endowed `physmem`/`addrspace` capabilities (§2.7
> graph composition). The bootstrap seed window (§5.7) still aborts on OOM;
> every post-bootstrap node hook returns `ObjError::OutOfMemory`. Deny-list
> (revocable) nodes are implemented (§3.7.3, R9; `obj/revocation.rs`).
>
> P3 refinements in 0.3.0:
> - `free()` is real, not a stub: the provider `free(region)` hooks take the
>   region's `CapId` from the caller's table and delegate to the `mem:region`
>   node's own `free` hook, which returns the backing to its allocator
>   (`BitmapAllocator::free` per frame for Phys; heap `dealloc` with the
>   stored `{size, align}` layout for Heap) and recycles the pooled wrapper.
>   The region zeroes its identity after releasing, so a double free of the
>   same capability is safe. (`obj/nodes.rs::release_region`,
>   `obj/memregion.rs::release_backing`, `MemRegionNode.align`.)
> - IRQ handlers are capability-gated, not raw addresses: a handler is bound
>   to a vector by passing a kernel-materialized `irq:handler` node
>   (`IrqHandlerNode`, implements `Obj::as_handler`), never a caller-supplied
>   scalar `fn()` address. The old `handler_from_addr` transmute is gone.
>   (`obj/nodes.rs::resolve_handler`, `obj/mod.rs::Obj::as_handler`.)
> - The framebuffer shadow buffer is allocated through the Boot domain's
>   Heap family-root capability instead of a raw kernel-heap call; the
>   `mem:region` cap's base is used as the shadow VA, with a plain-heap
>   fallback if the cap path ever fails. (`lib.rs::init_framebuffer_shadow`,
>   moved to run after `bootstrap()`.)
>
> P4 adds the capability-native VFS and device family nodes: `BlockNode`
> (block:storage), `BlockFamilyNode` (block:family, materializes child caps
> from BLOCK_DEVICES — hot-plug without minting), `MountNode` (fs:mount, mount
> via capability), `DirNode` (fs:dir, traverse/readdir gated by CapRights),
> `FileNode` (fs:file, read/write/label). Device census nodes:
> `PciForestNode` (pci:forest), `InputFamilyNode` (input:family),
> `AudioFamilyNode` (audio:family). The ambient string VFS layer
> (resolve_path, CWD, FD_TABLE, string fd API, path.rs) is deleted; all VFS
> access is now capability-native. The `kerneldump fs-walk` diagnostic
> exercises the capability path; the restricted-domain readdir projection
> proof asserts that a QUERY-only dir cap sees a strict subset and a domain
> with no dir cap sees nothing.
>
> P6-A (0.4.1) — paged domain isolation (§8.14 structural). A non-kernel
> domain no longer shares the boot CR3: it owns its own address space — a
> page-table root cloned from the kernel's higher half via
> `mm::vmm::clone_high_half`, with an empty, disjoint low half. The boot
> (kernel) domain keeps the kernel root (`Domain::set_kernel_addrspace`);
> every other domain is built with `Domain::with_addrspace`, which clones the
> kernel half and stores `Some(Vmm)`. `domain::set_current_domain` now writes
> the per-CPU slot *and* switches CR3/SATP to the domain's root when it owns
> one. The device-window PML4 entries (ACPI/ECAM/DMA) are pre-populated in the
> kernel root before the clone so lazy kernel mappings stay visible under a
> domain's CR3. The clone intentionally *shares* those PML4 entries (and their
> PDPT/PD/PT subtrees) with the kernel root rather than re-mapping the windows
> per-domain: the device sweep maps ECAM/DMA/MMIO lazily into the kernel root
> *after* the clone, and a per-domain rebuild would leave the driver pointing
> at empty private subtrees (ECAM config reads would fault). The parent root
> outlives every clone, so the shared-subtree lifetime is bounded by the
> kernel itself — there is no per-domain teardown hazard. `obj/paged_isolation.rs`
> runs before SMP and proves: disjoint root frames; an empty driver low half
> that a canary fills but the boot root cannot reach by position; both roots
> translate the kernel-half alias of a heap frame identically; and table
> mutation flows only through the capability API (endowed id Ok, unendowed id
> refused).
>
> P6-B (0.5.0) — the capability system is complete and real end to end.
> `StubNode` and the old `mint()` helper are deleted; `mint_node` takes
> `(caller, node, first_rights, first_contract)` and seeds each family root's
> contract-right mask, so the per-hook gate is live from boot (§7.6,
> `obj/mint.rs`). Per-hook contract rights are enforced in `resolve_with_rights`
> (`obj/table.rs`) against `Obj::hook_contract_right` (I12); an empty contract
> mask in a handle remains the transitional "unrestricted" rule (I13). The
> `SURFACE_READ` hook (`_read_surface`, `obj/hook.rs`) is handled centrally in
> `invoke` via `resolve_for_query`, which requires only the universal `QUERY`
> right — no INVOKE, no contract membership — and calls `Obj::surface_value`
> (I14). `CapabilityTable` gained `delegate` (rights-preserving clone into
> another domain's table, I15), `capacity`, and `revoke_cascade` gated on the
> universal `REVOKE` right; the boot domain holds a capability to its own
> table (`BootEndowment.table`), so delegation and table administration are
> capability-mediated (§7.8). The real `mem:region` contract joins the
> registry through the owned registry capability (`bootstrap.rs`), with
> `REGION_POOL_CAPACITY = 64` and `memregion::replenish`/`materialize_region_pools`.
> Real backing APIs: `Vmm::protect` (x86_64 `Mapper::update_flags`, riscv64
> hand-walk preserving `PTE_A|PTE_D` + `sfence.vma`), `heap::stats() ->
> (live, committed, chunk_count)`, `smp::started_count()`. The PCI device
> contract is real (`devices.rs` `pci:device` over `ecam_static()`/`PciConfigSpace`),
> and fs nodes carry `hook_contract_right` and `Rights::INVOKE.or(Rights::TRAVERSE)`
> attunement at every materialization site. `obj/separation.rs` (C6) now also
> proves QUERY surface reads, per-hook contract-right negatives, `addrspace:root`,
> `mem:region` registry membership, and capability-mediated delegation (positive
> boot→driver via the table cap, negative unknown-domain → `NoSuchCap`).

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
- Location: `obj/table.rs::resolve_with_rights` (`PERMIT` slope, table.rs:94-127), `obj/table.rs::resolve_for_query` (QUERY-only slope, table.rs:146-165), `obj/mod.rs::invoke` (dispatch after `resolve`, obj/mod.rs:183-218), `obj/table.rs::get` (PERMIT-less fetch, table.rs:60-68), `obj/bootstrap.rs:110-305` (mint + endowment via table insert), `obj/driver.rs:59-85`

**I2 (mint monopoly) — A new family root is created only by `R1`:**
At all times `{ c ∈ C : MINT ∈ rights(c) }` has cardinality ≤ 1, and that cap
is the Principal's — the Principal exercises it through the bootstrapper at
boot, and the bootstrapper self-revokes at boot end, after which the set of
nodes that could mint is empty. `mint_node` is only callable given a
`PrincipalContext`; once `MINT_FROZEN` is set by `finalize_mint()` every
subsequent `mint_node` fails with `MintAuthorityGone`, with an assert as a loud
canary against a concurrent/ISR-path mint racing the self-revoke. Only
`bootstrap()` holds a `PrincipalContext`, and only before self-revoke.
- Location: `obj/mint.rs::mint_node` (obj/mint.rs:32-56), `obj/mint.rs::PrincipalContext` (obj/mint.rs:12), `obj/mint.rs::finalize_mint` (obj/mint.rs:18-20), `obj/mint.rs::MINT_FROZEN` (obj/mint.rs:15), `obj/bootstrap.rs:110-129` (with `finalize_mint` called as `init()`'s last statement)

**I3 (monotone attunement) — for any attunement chain
`c₁ → c₂ → … → cₖ`, `rights(c₁) ⊇ rights(c₂) ⊇ … ⊇ rights(cₖ)`:**
No capability ever gains a right; every attuned capability is more precise
and tinier than its parent. `attune` ANDs both the universal mask and the
contract-rights mask (`NoAmplification` is a canary, unreachable via AND),
and `dup_limited` is the only handle-derivation path that shrinks rights —
`dup` copies rights identically and there is no operation that increases
them.
- Location: `obj/rights.rs::Rights::attune` (obj/rights.rs:37-44), `obj/rights.rs::CapRights::attune` (both dims, obj/rights.rs:125-138), `obj/table.rs::dup_limited` (obj/table.rs:208-227), `obj/table.rs::dup` (same-rights copy, obj/table.rs:193-204)

**I4 (lifetime = reachability) — `n` is allocated iff `reach(n) ≠ ∅` or `n`
is the Principal or a boot-era seed node:**
`reach(n) = ∅` implies `n`'s resources are reclaimed (R7). A `CapHandle`
holds the node through a strong `Arc<dyn Obj>`, so a node's lifetime is
exactly the life of the caps that reach it; the store holds no strong
reference (see I7), so nothing can resurrect a dead node. Default revocation
is drop-death (`RevocationPolicy::DropDeath`); deny-list `Revocable` nodes
(P5, §3.7.3, R9) add deactivation with caps retained.
- Location: `obj/cap_handle.rs::CapHandle{ node: Arc<dyn Obj> }` (obj/cap_handle.rs:27-32), `obj/cap_handle.rs::RevocationPolicy` (obj/cap_handle.rs:18-23), `obj/table.rs::resolve_with_rights` returns `Arc::clone(&h.node)` (obj/table.rs:126)

**I5 (one parent) — every node ≠ P has exactly one parent edge:**
`P` has none. The node-store record carries a single `parent: Option<ObjId>`
written once at registration; there is no structure for multiple parents.
- Location: `obj/store.rs::ObjRecord.parent` (obj/store.rs:29), `obj/store.rs::register` (parent written once, obj/store.rs:67-71)

**I6 (subsumption consistency) — if c names family root r, then any child
materialized under r has rights ⊆ rights(c), and r's parent edge is r's own
materializer's edge:**
A node is in exactly one family. Attunement restricts rights to a subset
(`attune`), and a node is bound to one parent (`ObjRecord.parent`), so a
child's rights shrink within its parent's family and no node straddles two
families; roots minted by `mint_node` register with `parent = None` and start new
families.
- Location: `obj/table.rs::dup_limited` (rights ⊆, obj/table.rs:208-227), `obj/rights.rs::attune` (obj/rights.rs:37-44,125-138), `obj/store.rs::parent` (one family, obj/store.rs:26-32), `obj/mint.rs::mint_node` (root registers with `None` parent, obj/mint.rs:50)

**I7 (store weakness) — the ObjectStore holds only weak references; it
never affects `reach`:**
`ObjRecord` holds a `Weak<dyn Obj>` per node (P5, never strong) alongside
`id`, `kind`, `parent`, `family_root`, per-root `cascade` state, and the
per-object deny-list set, so the store cannot keep a node alive nor
resurrect one. Projection is read-only and gated by the store-node
capability.
- Location: `obj/store.rs::ObjRecord` (`Weak<dyn Obj>`, no strong `Arc`/`Weak` node field, obj/store.rs:26-32), `obj/store.rs::register_weak`/`register_with_id_weak` (weak-only registration), `obj/store.rs::seal_cascade`/`is_cascade_severed`/`revoke_deny`/`is_denied` (weak-side bookkeeping), `obj/store.rs::ObjectStore` (obj/store.rs:40-45), `obj/store.rs::lock_records` (read-only, obj/store.rs:125-127)

**I8 (fast-path bound) — the `PERMIT` check of `R6` is O(1):**
A constant number of word-size operations: one `IrqMutex` acquire, one slot
index, Live, `INVOKE`, contract membership, per-hook contract-right test,
and the P5 deny-list probe (one hash-set load, §3.7.3/R9). All are
independent of table size `n` and the contract-membership probe is on a
small, frozen set. See §9.4 of `RootGraph.md` (I8).
- Location: `obj/table.rs::resolve_with_rights` (PERMIT slope incl. step-6 per-hook contract-right bit-test and step-7 deny probe, obj/table.rs:94-127), `obj/mod.rs::Obj::hook_contract_right` (obj/mod.rs:82-85), reference `RootGraph.md` §9.4 (fast-path bound, seven named steps)

**I9 (dispatch safety) — no table-slot lock is held across a hook body;
in-flight dispatch holds an `Arc` that prevents drop-death reclamation
until the reply returns:**
`resolve` releases the `IrqMutex` before returning the cloned `Arc`, so
`invoke`'s subsequent `dispatch` runs lock-free; the strong `Arc` keeps the
node alive even if the last table entry is revoked mid-hook.
- Location: `obj/mod.rs::invoke` (dispatch after `resolve_with_rights` returns, obj/mod.rs:205-218), `obj/table.rs::resolve_with_rights` (lock released on return; `Arc::clone` at obj/table.rs:126)

**I10 (contract identity) — `ContractId` is content-addressed; two distinct
`(name, surface, hooks)` tuples never share an id:**
The FNV-1a id is a hash of the identity byte stream `(name, surface.kind,
attrs, ordered hooks)`, which cannot collide because the `0xFF` separators
never appear in ASCII names/type discriminants — distinct tuples always
yield distinct streams. `ContractRegistry::register` validates the id: a
distinct tuple claiming an already-registered `ContractId` is refused loudly
with `ObjError::ContractCollision` and the genuine entry is left untouched;
re-registering the identical tuple is idempotent `Ok`. `ObjError::ContractCollision`
is defined at `obj/mod.rs:134`.
- Location: `obj/contract.rs::ContractId::of` (obj/contract.rs:80-125), `obj/contract.rs::ContractRegistry::register` (obj/contract.rs:163-178), `obj/contract.rs::ObjError::ContractCollision` (via `obj/mod.rs:134`), `obj/contract.rs::same_identity` (obj/contract.rs:198-242)

**I11 (paged domain isolation, P6-A) — every non-boot domain owns a disjoint
low-half address space; capability tables live only in the kernel half; CR3
follows the current domain:**
A non-kernel domain's root is produced by `mm::vmm::clone_high_half`: a fresh
frame with the kernel's higher-half entries (indices 256–511; kernel image,
heap arena, physmap, and the ACPI/ECAM/DMA device windows) copied and a low
half that is empty. No two domains share a root frame
(`domain::with_addrspace` always allocates a fresh one). Capability tables
are heap allocations, reachable only at `to_physmap` (kernel-half) addresses
in every domain's root, never in any low half. `domain::set_current_domain`
writes the per-CPU slot and, when the domain owns an address space, activates
its root (`mm::vmm::activate`), so the driver sweep executes under the
driver's CR3 and the idle loop returns to the kernel root. Because every
domain table carries the kernel higher half, IDT handler, per-CPU GS/GDT/TSS
data, and the current stack stay reachable across a CR3 switch — IRQs keep
running in the interrupted domain's context. The ACPI/ECAM/DMA window PML4
entries are pre-populated in the kernel root before the driver clone
(`bootstrap`), so lazy kernel mappings made after the clone remain visible
under a domain's CR3. This shared-subtree design is intentional (not a
rebuild-from-constants per domain): device mappings (ECAM config, DMA
buffers, MMIO) are created lazily into the kernel root *after* the clone, and
only the shared PDPT/PD/PT subtrees keep them reachable under the driver's
CR3; a per-domain rebuild would strand those windows empty in the driver root
and fault on the first device access. Proved by `obj/paged_isolation.rs::run()`
(disjoint root frames, empty-then-canary low half unreachable by position from
the boot root, shared kernel-half alias, cap-mediated mutation only).
- Location: `mm/vmm/{x86_64,riscv64}.rs::clone_high_half` and `::prepopulate_window` (mm/vmm/mod.rs re-export), `obj/domain.rs::{with_addrspace,set_kernel_addrspace,page_root,set_current_domain}`, `obj/bootstrap.rs::bootstrap` (pre-populate + `set_kernel_addrspace`), `obj/driver.rs::create` (with_addrspace), `obj/paged_isolation.rs::run` (proof)

**I12 (per-hook contract-right enforcement) — a hook is invoked only if the
handle's contract-right mask contains the hook's required right:**
`resolve_with_rights` folds the per-hook contract right into the third bit-test
of `PERMIT`: the node's `Obj::hook_contract_right(contract, hook)` states the
right a hook needs (`PhysMemNode`: `free`/`reserve` → `WRITE`, `stats` → `READ`,
`alloc_frames`/`alloc_contiguous` → `CALL` default; `HeapNode`: `alloc` →
`CALL`, `stats` → `READ`; `AddressSpaceNode`: `map`/`unmap`/`protect` →
`WRITE`, `translate`/`root` → `READ`; `MemRegionNode`: `base`/`size` → `READ`,
`free`/`detach` → `WRITE`; `PciDeviceNode`: reads → `READ`, writes → `WRITE`;
`DirNode`/`FileNode`/`BlockNode` similar per-hook splits), and the handle's
held contract mask must contain that right or the invocation fails `Denied`
(§3.3, I8). The provider also receives the caller's exact rights through
`dispatch(…, rights: &CapRights)` and may gate its hook body further (S1).
- Location: `obj/table.rs::resolve_with_rights` step 5 (obj/table.rs:115-119), `obj/mod.rs::Obj::hook_contract_right` default `CALL` (obj/mod.rs:82-85), per-node overrides in `obj/nodes.rs` (PhysMem 133-140, Heap 319-326, Addrspace 435-442, Cpu 552-558/620-626, Irq 724-730/789-795), `obj/memregion.rs` (214-220), `obj/devices.rs` (297-305), `obj/fs.rs` (114-121, 217-223, 306-312, 526-533, 688-695)

**I13 (transitional empty contract mask) — an `empty()` contract-right mask
in a handle is read as "unrestricted":**
Until every endowment carries a narrowed mask, a handle whose contract mask is
`ContractRights::empty()` satisfies any per-hook requirement — the third
bit-test passes unconditionally (I12). An *empty* mask is therefore strictly
more permissive than any explicit mask, so a cap attuned to a non-empty mask
can never return to unrestricted via the same monotone attunement that left it
non-empty (`CapRights::attune` only ANDs). This is the documented transitional
rule of `ContractRights`, honoured by `resolve_with_rights` at obj/table.rs:117.
- Location: `obj/rights.rs::ContractRights` type docs (obj/rights.rs:54-69), `obj/rights.rs::ContractRights::empty` (obj/rights.rs:87-89), `obj/table.rs::resolve_with_rights` (`held != empty() && !held.contains(required)` gate, obj/table.rs:115-119)

**I14 (QUERY-gated surface reads) — reading a node's surface requires the
universal `QUERY` right and nothing else:**
The `SURFACE_READ` hook (`HookId::of("_read_surface")`) is not part of any
contract. `invoke` intercepts it before the contract-membership test and
resolves the handle through `CapabilityTable::resolve_for_query`, which
requires the handle to be `Live`, to hold the universal `QUERY` right, and to
pass the revocable deny-list probe — but *not* `INVOKE` and *not* contract
membership. The value comes from `Obj::surface_value(name)`, which defaults to
`None` (→ `NotSupported`) and is overridden by `PhysMemNode` ("total_frames"),
`HeapNode` ("arena"), `AddressSpaceNode` ("root"), `CpuRootNode`/`CpuNode`
("cpus"), `TableNode` ("slots"), `StoreNode` ("records"), `MemRegionNode`
("base"/"size"), and `PciDeviceNode`. The fs nodes (`fs:dir`, `fs:file`,
`fs:mount`, `fs:block`, `fs:block:family`) advertise surface *schemas* but do
not yet override `surface_value` — their surface reads answer `NotSupported`.
- Location: `obj/hook.rs::SURFACE_READ` (obj/hook.rs:23-27), `obj/mod.rs::invoke` surface branch (obj/mod.rs:194-204), `obj/table.rs::resolve_for_query` (obj/table.rs:146-165), `obj/mod.rs::Obj::surface_value` default (obj/mod.rs:61-63), overrides in `obj/nodes.rs` (126-131, 312-317, 428-433, 545-550, 613-618), `obj/table.rs` (399-404), `obj/store.rs` (273-278), `obj/memregion.rs` (206-212), `obj/devices.rs` (307-317)

**I15 (delegation never amplifies; `revoke_cascade` requires universal
`REVOKE`) — capability flow across domains is a rights-preserving clone, and
cascade severing is gated:**
`CapabilityTable::delegate(&target, id)` inserts a *clone* of the source
handle into the target table under a fresh `CapId` with rights identical to
the source's — no right is ever added (I3). The clone carries the handle's
whole state verbatim, so a `Revoked` or deny-listed source cannot be smuggled
back to life by delegation (a nonexistent id → `NoSuchCap`). The
capability-mediated path runs through the table node's `delegate` hook
(`infra:table`), which first resolves the target domain via
`domain::find_domain` (an unknown id → `NoSuchCap`). `CapabilityTable::revoke_cascade(id)`
requires `REVOKE` in the handle's universal rights (else `Denied`), marks the
handle `Revoked`, frees the root slot, and seals the family in the store via
`seal_cascade`, which deny-lists the root and every descendant (R8) and
returns the severed subtree size; the `TableNode::revoke_cascade` hook re-checks
the universal `REVOKE` on the caller's handle.
- Location: `obj/table.rs::delegate` (obj/table.rs:232-243), `obj/table.rs::revoke_cascade` (obj/table.rs:272-293), `obj/table.rs::TableNode::dispatch` `delegate`/`revoke_cascade` hooks (obj/table.rs:425-447), `obj/domain.rs::find_domain` (obj/domain.rs:121-123), `obj/store.rs::seal_cascade` (obj/store.rs:139-177)

---

## Cross-Check

The invariants are exercised at boot, not only asserted statically.
`obj/separation.rs` proves them against the live graph right after bootstrap
and before SMP:

- **I1 / I8 — no ambient authority, reachability only via the table:** a
  genuine DMA alloc through the endowed cap succeeds (`separation.rs:38-55`);
  an unendowed id is refused with `NoSuchCap` (`separation.rs:58-65`); a
  foreign contract is refused by PERMIT with `Denied` (`separation.rs:67-70`);
  and a per-hook contract right (READ held, CALL required) is refused
  (`separation.rs:72-83`).
- **C8 separation — the driver domain is disjoint from boot:** endowed
  dma/pci_cfg resolve from the driver table; unendowed ids and foreign
  (serial / registry) contracts resolve `NoSuchCap`/`Denied`
  (`separation.rs:92-127`); its table holds exactly `count() == 4`
  (dma + pci_cfg + physmem + addrspace, `separation.rs:111-119`); the heap
  cap is provably absent (`separation.rs:245-251`).
- **I12 — per-hook contract rights are live on the real nodes:** a CALL-only
  physmem mask reaches neither `free` (WRITE required) nor `stats` (READ
  required), while the full mask reaches `stats` (`separation.rs:344-374`).
- **I14 — QUERY-gated surface reads:** physmem / heap / addrspace / table
  surfaces answer live values through the endowed caps
  (`separation.rs:275-333`); a cap with QUERY dropped is refused `Denied`
  (`separation.rs:335-342`).
- **I15 / I10 — capability-mediated delegation and registry:** the boot
  domain delegates its physmem cap to the driver domain through the table
  node's `delegate` hook; the clone resolves in the driver table while the
  source stays untouched (`separation.rs:433-463`), and an unknown target
  domain is refused `NoSuchCap` (`separation.rs:465-480`). A duplicate-name
  contract claiming an already-registered `ContractId` is refused with
  `ContractCollision` and the genuine entry survives (`separation.rs:176-201`);
  re-registering the same tuple is idempotent (`separation.rs:168-174`).
- **Registry is discovery-by-owned-capability, not ambient:** the driver
  domain (no registry cap) is refused `Denied`; the boot domain looks up
  `dma:alloc` and `mem:region` and gets name + doc back; a bogus id returns
  `Reply::None` (`separation.rs:133-166`, `separation.rs:393-414`).

- Location: `obj/separation.rs::run` (obj/separation.rs:33-485), `obj/separation.rs::run_post_mount` (obj/separation.rs:491-511)

## P4 Gate

The P4 gate (section 7.12) requires:

1. Capability-native navigation (7.12.3): DirNode::traverse resolves a child
   by name and returns a DirNode or FileNode capability attuned to the
   caller's rights. DirNode::readdir returns child capabilities. Labels are
   surface data retrieved via label(), not path strings. The kerneldump
   fs-walk diagnostic exercises this.

2. Both real mounts are capability-native (ordering point 2): A> (tmpfs) via
   MountNode::mount; B> (ESP fat32) via BlockFamilyNode::first +
   MountNode::mount.

3. Restricted-domain readdir projection: A QUERY-only dir cap receives
   Denied on readdir; the driver domain (no dir cap) resolves None on
   resolve_first(DIR_READDIR).

4. Device families: PCI forest, input, audio, block (with hot-plug via
   BLOCK_DEVICES extension in the idle loop) are all registered and visible
   in the kerneldump graph census.

5. No ambient string VFS: resolve_path, CWD, FD_TABLE, getcwd, chdir, and
   the string fd API are deleted. vfs/path.rs is deleted.

## P5 Gate

The P5 gate (section 7.13 and the Phase P5 section) requires:

1. All three revocation modes wired (R7–R9): drop-death (R7) is the
   default; cascade (R8) via `CapabilityTable::revoke_cascade` (REVOKE gate
   → mark the root handle Revoked → `seal_cascade` the subtree → release the
   root slot), keyed per family root by the store's `cascade` state (§8.6
   layer 1); deny-list (R9) via `revoke_deny` on the per-object deny set.

2. The store stays weak (I7): `register_weak`/`register_with_id_weak` record
   a real `Weak<dyn Obj>` per node plus `family_root` and per-root `cascade`
   state — never a strong reference — with `seal_cascade -> usize` (subtree
   size) and `is_cascade_severed`/`is_denied` read-backs.

3. PERMIT keeps the fast path (I8): `resolve_with_rights` adds the step-6
   deny-list probe as a single hash-set probe, so the check stays O(1).

4. The projection tool (§7.13): `kerneldump graph`/`graph_with_flags`
   (`--roots --edges --caps --contracts --revocations`) plus the P1
   `graph_census`; `kerneldump/leak.rs::leak_detect` is the §8.7
   post-process — reachability from all registered domains' tables, with
   `infra:` seed nodes and cascade-severed-family records exempt (I4/§8.8)
   — and returns `true` on a leak so CI can fail the run.

5. The PCI forest is a real family root: `materialize_pci_tree`/
   `materialize_pci_child` register one `PciDeviceNode` per discovered
   device with weak parent edges (ObjId base 0x11_3000), and the boot
   domain's PCI forest cap carries REVOKE. `obj/domain.rs` exposes a domain
   registry (`register_domain`/`all_domains`) for the projection tool.

6. Cascade gate assertion: `run_revocation_gate` cascade-revokes a 4-node test
   subtree (root + three weak-parented children) and asserts the whole
   subtree is deny-marked and absent from the next projection, with no
   handle left Live.

7. Deny-list gate assertion: deny-list-revoking a `Revocable` node makes
   PERMIT fail `Revoked` while the cap slot is retained (Zombie); the leak
   detector runs clean after the test run.