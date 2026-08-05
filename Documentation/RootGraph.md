# RootGraph — The Object Graph / Capability Model for BedrockOS

**Document:** Documentation/RootGraph.md
**Canonical companion:** `Invariants/invariants-26-objects.md` (numbered invariants distilled from this document)
**Status:** Design Specification — normative for implementation phases P0–P6; phases **P0–P5 and P6-A/B are implemented** in the tree (see §10 for per-phase status). Remaining future work: P6 user boundary + sessions.
**Version:** 1.1.0
**Date:** 2026-08-05
**Target repository state:** post capability-system upgrade (P6-B)

---

## Table of Contents

1.  [Preface — Everything Is a Graph](#1-preface--everything-is-a-graph)
2.  [Foundational Concepts](#2-foundational-concepts)
3.  [The Capability System](#3-the-capability-system)
4.  [The Trinity: Surface, Hook, Contract](#4-the-trinity-surface-hook-contract)
5.  [Bootstrap Sequence](#5-bootstrap-sequence)
6.  [Domain Model](#6-domain-model)
7.  [Implementation Plan](#7-implementation-plan)
8.  [Edge Cases and Their Handling](#8-edge-cases-and-their-handling)
9.  [Formal Semantics](#9-formal-semantics)
10. [Migration Roadmap](#10-migration-roadmap)
11. [Glossary](#11-glossary)
12. [Design References and Prior Art](#12-design-references-and-prior-art)

---

# 1. Preface — Everything Is a Graph

## 1.1 The Thesis

BedrockOS abandons the Unix model of *"everything is a file."* In its place we
adopt a single, recursive, self-similar primitive:

> **Everything is an object. An object is a graph node. A node contains a
> graph. That graph's nodes are objects. The abstraction recurses without
> bound.**

This one sentence replaces, at the conceptual level, the entire stack of
operating-system abstractions that BedrockOS currently ships: the VFS
(dentry/inode/file), the block-device layer, the PCI/USB/AHCI device trees,
the input layer, the interrupt manager, the memory managers, the audio
subsystem, and — eventually — the user-space process and syscall boundary.

The graph is not decorative. It is load-bearing in three separate ways:

1.  **Structure** — *where things are.* Every resource in the system is a
    node in a graph, and every node has a well-defined place relative to
    other nodes (its family, its interior, its parent edge).
2.  **Authority** — *who may touch what.* Every interaction between two nodes
    is mediated by a capability. A capability is, at heart, *permission to
    traverse a portion of the graph.* This is the security model, the
    permission model, and the naming model, unified into one mechanism.
3.  **Lifetime** — *when things exist.* An object exists if and only if at
    least one capability held by some domain reaches it. When the last such
    capability is gone, the object ceases to exist. Lifetime is not a
    separate concern from access control; they are the same concern.

## 1.2 The Core Claims

The following claims are normative. Every design decision in this document
and every line of code produced from it must be consistent with them.

**Claim 1 (No ambient authority).** No code path may access a protected
resource purely because it runs in supervisor mode. Every access to every
protected resource crosses a capability edge. Position (privilege ring, CPU
mode, address space membership) grants *nothing* on its own. The trap
handler, the page-table walker, the heap allocator, and the spinlock are all
*implementation* — code that runs only behind a hook that the caller was
authorized to invoke. They are never ambient privileges.

**Claim 2 (The graph is a projection).** The full graph never exists as an
addressable object. The kernel maintains an *object store* for bookkeeping
(refcounts, revocation, forensics, garbage collection), but that store is
**not** a namespace. No domain may enumerate it. Each domain's world — the
only graph it can see, traverse, or affect — is the *transitive closure of
the capabilities it holds.* A new object becomes visible to a domain only by
being handed to it along a capability, exactly like a token passed among a
group of friends. There is no "ls /" over the whole world.

**Claim 3 (Mint monopoly).** Only the Principal *mints*. To *mint* is to
create a new family root — a new capability with new authority — and it is
the Principal's act and the Principal's alone. Every other node, process, and
domain only *attunes*: it takes a capability it already holds and makes it
more precise and tinier — shrinking rights (§3.3), materializing a
pre-authorized child within a family it already holds (§3.5), delegating a
copy into another's table, or composing a derived node by calling hooks
across family boundaries via paths it already holds (§3.6). Attunement never
creates authority out of nothing; it only refines and narrows the authority
the attuner was entrusted with. The moment any line of prose says a
non-Principal node "mints," it is wrong — that node is *attuning*.

**Claim 4 (Recursive subsumption).** A capability over a node subsumes that
node's entire interior graph, transitively. To hold the xHCI controller's
capability is to hold the USB subtree — present devices *and future ones* —
because the controller *owns the bring-up tree.* Subsumption is the reason
the projection model is coherent: a small set of granted family roots covers
an arbitrarily large and growing set of descendants.

**Claim 5 (Lifetime is reachability).** An object is alive if and only if at
least one live capability held by a live domain reaches it (its interior
included). Drop-death is the default: when the last reachable capability is
revoked, dropped, or consumed, the object's resources are reclaimed. Family
roots additionally support *cascade revocation*: revoking the root
capability severs the entire subtree at once. The Principal's power-off is
the total instance of cascade revocation — the entire projection collapses
under one external act.

**Claim 6 (Lifetime and authority are the same mechanism).** There is no
separate "access control layer" and "memory management layer." Both are the
capability graph. Freeing memory is the same operation as revoking a
capability. A use-after-free is impossible by construction because a node
cannot be freed while a capability to it exists, and a capability to a freed
node cannot exist because dropping the last capability is the *only* thing
that frees the node.

## 1.3 What Dies With This Design

For clarity, the following Unix-isms are explicitly *not* part of the model
and will be removed from the system over the migration:

- Ambient global namespaces: drive letters (`A>`, `B>`), the drive map
  (`DRIVE_MAP`), the ambient current-working-directory (`CWD`).
- String path resolution inside the kernel (`resolve_path`, `walk_from`,
  `split_drive_path`, `getcwd`, `chdir` by string).
- The ambient global service accessor `kernel_services()`.
- The single global file-descriptor table `FD_TABLE`.
- The notion that a file, device, socket, or driver are different *kinds* of
  things with different access rules. In BedrockOS they are all nodes with a
  surface, hooks, and contracts, reached only through capabilities.

What survives, reincarnated:

- `InodeOps` survives as a *contract* (a set of hooks) implemented by
  filesystem nodes.
- `BlockDevice` survives as a contract implemented by storage nodes.
- `FdTable` survives, generalized, as a *capability table*.
- `KernelServices` survives as the boot domain's *endowment*.
- `kerneldump` survives and grows the *projection* tool.

## 1.4 Reading Guide

- Sections 2–4 define the model abstractly. Read these first; everything
  else refers back to them.
- Section 5 specifies the bootstrap, including the answer to *"how does the
  first graph appear."*
- Section 6 specifies domains — the answer to "who is a principal-in-the-
  small, and what may it do."
- Section 7 is the concrete, file-level implementation plan mapped onto the
  BedrockOS codebase as it exists at commit `4624b92`.
- Section 8 is the edge-case catalogue. Each edge case names the risk, the
  principle that governs it, and the concrete handling.
- Section 9 gives the formal semantics: the operational rules, the
  invariants, and the theorems that the implementation is expected to
  maintain.
- Section 10 is the phase-by-phase migration roadmap with build gates.
- Section 11 is the glossary; Section 12 the references.

---

# 2. Foundational Concepts

## 2.1 Object (Node)

**Definition.** An *object* is the atomic unit of the system's structure,
authority, and lifetime. It is a graph node. It has an identity, a surface,
a set of hooks, a set of contracts it implements, and an interior graph.

**Identity.** Every object has a globally unique `ObjId`, a monotonically
increasing integer issued by the kernel at construction. `ObjId`s are
*not* capabilities and *not* addresses. They exist solely so that (a) the
object store can key its records, and (b) forensics (the projection tool) can
refer to nodes unambiguously. No domain may use an `ObjId` to access an
object; access requires a capability. An `ObjId` confers nothing.

**Structural shape.** Every object is recursively a graph:

```
        node
         |
         +-- surface     : passive data face (typed attributes, events)
         +-- hooks       : active face (invocable operations)
         +-- contracts   : promises the node makes about its hooks
         +-- interior    : a graph whose nodes are also objects (may be empty)
         +-- parent edge : one upward reference to the family root (optional)
```

**Interior graph.** The *interior* of a node is itself a graph. For a
controller node (PCI root complex, AHCI controller, xHCI controller) the
interior is the tree of devices it brought up. For a filesystem directory
node the interior is the graph of its children. For an IRQ vector node the
interior is the graph of registered handlers. For a leaf (atom) the interior
is empty. There is no special-casing: a node and its interior obey the *same*
rules. This is the recursion claim, and it is the design's central economy —
one rule set, applied at every scale.

**Parent edge.** Every node except the Principal has exactly one upward
reference to the family root that subsumes it. The parent edge is used for:
accountability (a node can name its root), cascade revocation (severing the
root severs the subtree), and projection (the debugger can walk upward). The
parent edge is *not* itself a capability and cannot be traversed downward to
gain access — the interior of a family root is reachable only through
capabilities the root (or its authorized nodes) hand out.

## 2.2 The Principal

**Definition.** The *Principal* is the first object. It is in the graph, it
is permanent for the lifetime of the machine, and it is the sole source of
minting authority.

**Existence condition.** Booting implies the Principal exists. There is no
external act, no firmware ceremony, no "login"; the machine being powered on
*is* the Principal's existence condition. Its parent edge points **out of
the graph** to the act of booting — it has no parent within the graph, and
this is the single permitted exception to the parent-edge rule. Conceptually
the Principal is the machine owner's in-graph presence; the up-edge is a
marker carrying only the fact "born of the machine powering up," never a
reference to another node.

**Posture.** The Principal is *quiescent-but-capable*:

- **Quiescent:** during normal operation the Principal performs no action.
  It does not respond to messages, does not hold a per-CPU domain, and does
  not appear in any domain's capability table (it is *above* tables, not
  inside them).
- **Capable:** the Principal retains the power to act, and the only power it
  has is to *mint*. A future session/login mechanism is expressed as the
  Principal minting new family roots on demand. Power-off is the Principal
  exercising its power in the negative: it revokes the world.

**The mint monopoly.** No node other than the Principal may mint a capability.
Minting is the act of creating a *new family root* — a new node whose
interior is empty at birth and whose first capability is created by the
minter. The Principal delegates mint authority once, to the bootstrapper, at
boot; the bootstrapper uses it to build the world and then self-revokes
(drop-death), returning the graph to a state where *only* the Principal can
mint. After boot, the Principal is the only node that could mint, and it is
quiescent until it chooses otherwise.

**Why "only the Principal."** If any node could mint, the graph would be
inconsistent: a node could create authority it was never granted, and the
projection claim (Claim 2) would collapse — the world would contain objects
reachable by caps that no grant produced. Centralizing minting in the
Principal gives the entire graph a single, auditable root of authority. Every
capability in the system, walking its derivation history, terminates at a
mint performed by the Principal (or by its sole delegate, the bootstrapper,
whose mint rights the Principal granted).

## 2.3 The Bootstrapper

**Definition.** The *root bootstrapper* is the node the Principal roots at
boot. It is the Principal's sole delegate and its one and only child for the
duration of boot.

**Powers.** The bootstrapper holds the Principal's *delegated mint authority*
(see 3.4), exercised at boot — and only at boot — on the Principal's behalf.
Every capability it brings into being is attributable to the Principal; the
bootstrapper is the hand through which the Principal mints, never an
independent minter. With it:

1.  Constructs the first graph units (the primitive service nodes: physical
    memory, heap, address space, CPUs, interrupt vectors, the object store,
    the contract registry).
2.  The Principal mints capabilities for those units through it.
3.  The Principal mints capabilities to hardware through it (family roots
    for every controller that will own a device tree: PCI, AHCI, xHCI,
    audio, input, serial).
4.  Attunes an endowment subset for each secondary domain — smaller, more
    precise versions of the family roots it holds, never new mints.

**Termination.** When all kernel modules have been started and the world is
fully built, the bootstrapper **revokes its own capability** and **dies**
(drop-death, C2). Its nodes remain as forensic records in the object store
(the store holds weak references and history), but its mint authority is
gone. It can never act again. It is not re-rootable; if a future boot is
needed, that is a *new* boot and a *new* bootstrapper minted by the
Principal from scratch.

**Relationship to `Kernel::init()`.** In the BedrockOS codebase, the
bootstrapper is not a new invention: it is the existing `Kernel::init()`
sequence, re-read through this lens. The sequence heap → serial → SMP →
page tables → arch init → ACPI → IOAPIC → service container → SMP wake →
interrupt enable is precisely "load the first graph units and, on the
Principal's authority, mint their capabilities." The final act before
`Kernel::run()`'s device sweep — the "all kernel modules started" point — is
where the bootstrapper self-revokes.

## 2.4 Families and Subsumption

**Definition.** A *family* is the set of nodes subsumed by a single granted
capability. The family root is the node the capability directly names. Every
node other than the Principal belongs to exactly one family (its parent edge
names the family root). A family is a *subtree* of the object graph.

**Subsumption.** A capability over a node *subsumes* that node's interior
graph transitively. This is the property that makes the model scale: you do
not need a capability per device, per file, per descendant. You need the
family root, and the whole subtree is yours to materialize from. The xHCI
capability does not name "the four devices currently plugged in"; it names
*the USB bring-up tree*, which is an open-ended set.

**Materialization vs. authority.** Because a family root capability subsumes
its interior, bringing a new child into existence inside the family is *not*
minting. It is *materialization* — realizing a pre-authorized node. A USB
device unplugging and replugging, a port coming alive, a disk appearing, a
file being created in a directory you hold: all materializations. None
require the Principal, none create authority. This single insight dissolves
the "hot-plug needs minting" problem (see 8.3).

**Families are the projection units.** When a domain holds the xHCI
capability, the projection it can see is exactly the xHCI subtree. When it
holds a directory capability, it sees that directory's interior (filtered by
its rights). The granularity of the world a domain perceives is the
granularity of the family roots it holds. This is the operational meaning of
Claim 2.

## 2.5 Atoms

**Definition.** An *atom* is a node whose interior graph is empty. It has a
surface (pure data) and possibly hooks, but no children.

**Examples.** A physical frame of memory. A byte of serial output. An IRQ
vector. A register. A single key event. An endpoint of a USB device.

**Why they matter.** Atoms bottom out the downward recursion. Without them,
"nodes contain graphs" would be an infinite regress. Atoms are the point
where the graph meets raw reality — memory cells, device registers, bus
signals. Everything above an atom is structure; the atom is data. Atoms obey
the same rules as any node (capabilities reach them, drop-death frees them),
but their interior is trivially empty, so materialization below them is
impossible by construction.

## 2.6 Domains

**Definition.** A *domain* is an independent principal-in-the-small: an
execution context with its own capability table, its own rights, and its own
view of the graph. Domains are the entities that *hold* capabilities and
*invoke* hooks. The kernel, each CPU, each thread, each future process, each
future user session is a domain.

**Separation.** Capabilities mean nothing without multiple domains. A single
principal holding every capability is indistinguishable from ambient
authority. Therefore the very first implementation step (P1) must establish
at least two domains — the *boot domain* and at least one *driver domain* —
with disjoint capability tables, so that the separation property is real
from the first commit (see 6.1).

**The current-domain problem.** Every hook invocation must be able to answer
the question "whose capability table is authoritative right now?" The answer
is threaded through the dispatch path: a pointer to the current domain's
`CapabilityTable` accompanies every call, every trap, every interrupt
delivery. Section 6 defines this threading precisely.

## 2.7 Paths

**Definition (paths as bounded descent).** A *path* is a finite sequence of
capability-gated descents into a node's interior graph, terminating in a
hook invocation. Each step enters a node, selects a child, and descends; the
selection is itself capability-gated (you can only select a child for which
a child-capability is available to you).

```
path := cap + (enter + select-child)* + invoke(hook, args)
```

**Paths are not strings.** The classic Unix path `A>music/song.mp3` is a
string that the kernel resolves *ambiently*. In RootGraph, "reaching the
song" means: you hold a directory capability whose interior contains a child
node named `music`; you traverse (a capability-gated `traverse` hook) to the
child, descend, traverse to `song.mp3`, and invoke a hook. Every step is a
permission you hold. The string form, if it appears at all (in a shell, in a
debugger), is sugar rendered *from* your owned capabilities — a label read
off a surface, never a lookup key.

**Composition via paths.** The original thesis — "the application, using its
own contracts, uses another object's hooks" — is exactly paths in action. A
node in one family can hold paths (capabilities) into another family; using
those paths it can invoke hooks across the family boundary. If the
interaction is productive, a *derived node* may materialize (a session, a
mount, a connection). The derived node's authority is fully the composition
of rights the interacting parties already held; nothing was minted. Section
3.6 covers this in detail.

## 2.8 The Projection

**Definition.** The *projection* of a domain is the portion of the object
graph that domain can see, traverse, and affect: the transitive closure of
its held capabilities. It is the only world that exists from that domain's
point of view.

**The full projection.** The kernel *does* keep a record of the entire graph
(the object store), and the `kerneldump` tool can emit a *full projection* —
a snapshot of every node, every edge, every capability, every right, every
contract, every revocation state — for forensics and debugging. This is
consistent with the projection claim because the full projection is:
(a) read-only, (b) only available to a domain that holds the
`kerneldump:project` contract capability, and (c) *not addressable* — it is
a report, not a namespace. You may look at a map of the city; you may not
teleport anywhere the map shows.

**Collapse.** Because lifetime is reachability, the projection is *volatile*:
drop a capability and the projection of what is reachable through it
collapses. The full projection at time t is always a historical fact, never
a living handle.

---

# 3. The Capability System

## 3.1 Capability — Informal

A *capability* is an unforgeable token that names a node and a set of rights
over that node, held by exactly one domain (or, during boot, by the
bootstrapper on behalf of the Principal). To have a capability is to have the
right; there is no separate check "does this domain have permission" beyond
"does this domain hold the capability." Capabilities are the *only* way to
reach a node — there is no syscall "open object by id," no global lookup, no
kernel_services shortcut.

The informal reading the reader should carry forever:

> A capability is the ability to traverse a portion of the graph. Holding a
> capability is holding a path. The full graph is a projection; it never
> truly exists. Only parts exist, at a time, through the capabilities that
> subsume them.

## 3.2 Capability — Formal Structure

Formally a capability is a triple, rendered as `CapHandle` in code:

```
CapHandle {
    id     : CapId,          // unforgeable, kernel-issued, monotonically increasing
    node   : Arc<dyn Obj>,   // the node the capability names (strong reference)
    rights : Rights,         // the permitted operations (monotone-decreasing)
    state  : HandleState,    // Live | Revoked | Zombie
}
```

**Unforgeability.** `CapId` values are monotonically increasing integers
issued by the kernel's capability machinery (per-table allocators for
attuned handles; the mint for the Principal's family roots). They are *not*
pointers and *not* guessable from the node: no amount of arithmetic or
spelunking can turn an `ObjId` or a memory address into a `CapId`, because
the mapping from `CapId` to node exists only inside a domain's capability
table, and the tables are kernel-resident. Forging a capability would require
manufacturing an entry in a table that only the kernel writes. In a
single-address-space kernel this protection is structural (no userspace can
write kernel memory); in a future paged user model it is enforced by the
syscall boundary and the kernel's exclusive write access to tables (see
8.16).

**Strong reference.** The `node` field holds an `Arc`. This is what makes
*lifetime = reachability* true: a node's strong-reference count is exactly
the number of capabilities that name it (across all tables, plus transient
dispatch references). When the last capability is revoked/dropped, the count
hits zero, and the node is reclaimed. There is no reference to a dead node,
and a live node always has at least one reachable capability — or it would
have died.

**Rights.** The `rights` field is a bitset. Rights are monotone-decreasing
along any attunement chain: a capability attuned from another has a subset
of the parent's rights, never a superset. This is the *no-amplification*
rule, and it is enforced at the mint (the Principal's, which fixes the
root's rights) and at every attunement (which can only clear bits).

## 3.3 Rights

The rights defined for BedrockOS (this set is the normative minimum; new
rights may be added per contract but the listed five are universal):

| Right | Meaning | Notes |
|---|---|---|
| `QUERY` | Read the node's surface (attributes, labels, version, state) | Always side-effect-free; the minimum a holder may have |
| `INVOKE` | Call a hook on the node | The right to make the node *do* something |
| `TRAVERSE` | Enter the node's interior graph and request child capabilities | The right to *materialize* children and descend |
| `MINT` | Create a new family root and its first capability | The Principal's exclusive right; delegated only to the bootstrapper at boot |
| `REVOKE` | Revoke capabilities naming this node | Only meaningful on family roots and revocable objects (see 3.7) |

**Composite rights.** Contracts may define additional rights as an
orthogonal dimension (e.g. `READ` / `WRITE` on a data object's hooks). The
universal five are orthogonal to contract rights: `INVOKE` gates whether any
hook may be called at all; the contract right gates *which* hook.

**Purity of `QUERY` (refinement note).** The `QUERY` right is described as
"always side-effect-free." The strict reading is *side-effect-free by
contract*: a node that advertises a surface schema promises that reading it
changes no observable state. Most surfaces are pure reads of kernel-resident
state; a hardware-status register, however, is read *through* a node whose
surface getter executes the read — which is pure only in the weak sense
(no state change, but the value is a live observation). For now all surface
reads are treated as pure; a future refinement may split `QUERY` into
`QUERY_PURE` (kernel-resident state only) and `QUERY_OBSERVE` (live
hardware), so that a caller holding only the pure form cannot observe
volatile device state. This is deferred; it does not affect P1–P5.

**Invocation permission check.** For a caller holding `CapHandle h` to invoke
`hook` on `node` under `contract`:

```
PERMIT(h, contract, hook)  ==
    h.state == Live
    AND h.rights.has(INVOKE)
    AND h.rights.has(contract-right of hook)     // contract-defined
    AND node implements(contract)
    AND node.revocation_state != Revoked            // if revocable
```

The check is a few loads and bit-tests; it is designed to be the fast path,
not an abstraction barrier that costs a function call per operation (see 7.5
and 9.4).

**Implementation (P6-B).** This is `CapabilityTable::resolve_with_rights`
(`obj/table.rs:94-127`), with two refinements. First, the contract-right test
is a *per-hook* bit-test: the node's `Obj::hook_contract_right(contract, hook)`
states what a hook requires (e.g. `PhysMemNode`: `free`/`reserve` → `WRITE`,
`stats` → `READ`, `alloc_frames` → `CALL`; `HeapNode`: `alloc` → `CALL`,
`stats` → `READ`; `AddressSpaceNode`: `map`/`unmap`/`protect` → `WRITE`,
`translate`/`root` → `READ`; `MemRegionNode`: `base`/`size` → `READ`,
`free`/`detach` → `WRITE`; `PciDeviceNode` and the fs nodes similarly
split), so a narrowed contract mask gates the exact hooks a cap may reach.
Second, the transitional rule: an `empty()` contract mask is read as
*"not yet narrowed"* and satisfies any requirement (`if held != empty() &&
!held.contains(required)`), so endowments predating the dimension keep
working; monotonicity guarantees a cap narrowed to a non-empty mask can never
return to unrestricted. The deny-list probe is step 6 (§3.7.3, §9.4).
Surface reads skip `INVOKE` and contract membership entirely and resolve via
`resolve_for_query` with only the universal `QUERY` right (§4.1).

## 3.4 Obtaining Capabilities — Mint and Attunement

The system permits exactly two *kinds* of capability production. The first
has exactly one operator; the second is what every node does. These are
exhaustive; any code that seems to "get" a capability another way is, by
definition, ambient authority and is a bug.

1.  **Mint — only the Principal.** The Principal creates a new family root
    and its first capability. Mint is the *only* operation that creates new
    authority. During boot the Principal performs its mints through the
    rooted bootstrapper (its sole agent, §2.3); no other node ever mints.
    Every operation in the list below is an *attunement* — it takes an
    existing capability and makes it more precise and tinier.

2.  **Endowment (attune).** A domain is born (or a session begins) with a
    concrete list of capabilities handed to it. The boot domain is endowed
    by the bootstrapper with *attuned subsets* of the family roots;
    each driver domain is endowed by the boot domain; a future user session
    is endowed by the Principal. Endowment is giving a smaller, more precise
    capability along — never a mint.

3.  **Attunement by duplication (derive).** A domain duplicates a capability
    it holds into another slot in a table (its own or another's) with a
    *subset* of rights — the canonical "make it more precise, tinier." The
    universal five and all contract rights are monotone-decreasing under
    attunement. Attunement never creates new authority; it copies and
    shrinks existing authority. `dup`/`dup2` in today's `FdTable` are
    attunements (the code name for the base operation is *derive*).

4.  **Invocation return (attune).** A hook may return capabilities as part of
    its reply. This is how materialization and composition deliver their
    results: the directory's `traverse` returns a child capability; the
    xHCI `bring_up` hook returns a device capability; a connection hook
    returns a session capability. Returning a capability is *not* minting —
    the returned capability is an attunement of authority the callee already
    held, or a materialization of a pre-authorized child of a family the
    callee holds.

**The mint guard.** The kernel's capability-mint entry point checks, at the
single choke point where new family roots are issued:

- Is the caller the Principal (that is, the Principal itself, acting through
  its rooted bootstrapper, only during boot and only before its
  self-revoke)?
- If not, deny. There is no "elevated" second mint path. In the P6-B build
  the guard is `mint_node` (§7.6): it is callable only with a
  `PrincipalContext` (a seed value only the bootstrap path can enter) and
  only while `MINT_FROZEN` is clear. No capability anywhere carries a `MINT`
  *right* — `PRIM_RIGHTS` is `INVOKE|QUERY|TRAVERSE`, never `MINT` — so the
  check is authority itself, not a bit. The bootstrapper self-revokes as the
  last step of `init()` (`finalize_mint`), and every later `mint_node` fails
  with `MintAuthorityGone`. After boot, the set of nodes that could possibly
  mint is empty.

## 3.5 Materialization

**Definition.** *Materialization* is the act of realizing a child node inside
a family whose root capability the actor already holds. Materialization does
not mint; the child's authority was pre-authorized by the family root's
subsumption.

**Mechanics.** A node with `TRAVERSE` in its rights may enter the interior of
a held node and request children. The held node (the family root, or an
interior node acting within the family) decides, per its own logic and the
caller's rights, whether a child capability is issued. If yes, the child
capability is an *attunement of the family root's subsumption* — a tinier,
more precise capability — never an independent mint.

**Examples.**

- A USB device appears on the bus: the xHCI controller materializes a device
  node in its interior and returns the device capability to the domain that
  holds the controller capability.
- A file is created: the directory node materializes a child inode in its
  interior.
- A block device is found on an AHCI port: the AHCI controller materializes
  the block node.
- An IRQ fires: the interrupt vector materializes (by dispatch) a delivery to
  each registered handler — each handler's registration being a pre-existing
  edge in the vector's interior.

**Rights inheritance.** A materialized child's capability carries rights no
greater than the materializing actor's rights over the family root. The
child cannot exceed its parent's allowance — the monotone rule extends down
the family tree by construction (Claim: rights are monotone down a family).

## 3.6 Composition

**Definition.** *Composition* is the creation of a *derived node* by the
interaction of nodes across family boundaries, using paths. When two (or
more) parties already hold capabilities that, combined, authorize an
interaction, and they invoke hooks on each other, a new node may be brought
into existence whose authority is entirely the composition of the
participants' rights.

**Why composition is not minting.** Minting creates authority from nothing;
composition derives it from what exists. The derived node's capability set is
a function of the participants' capabilities, and no participant ever gains a
right it did not hold. Composition therefore respects Claim 3.

**Examples.**

- A keyboard (input family) and a console (display family) interact: the
  interaction materializes a *session* node — a derived node that routes key
  events to the console. The session node's authority (to read key events,
  to write to the console) is exactly the union of the caps its parents held
  and delegated to it.
- A block device (storage family) and a filesystem driver (fs family)
  interact: a *mount* node materializes, binding the driver to the device
  and gaining a path into the block node's interior (the partition/directory
  graph).
- Two processes interact over a capability: a *connection* node
  materializes, carrying the message queue and the pair of endpoint caps.

**The path condition.** Composition requires paths: the interacting nodes
must hold capabilities into each other's families. This is precisely the
thesis sentence — "the application, using its own contracts, uses another
object's hooks" — formalized. If a node cannot reach another, they cannot
compose; the graph prevents unpermissioned alliances by construction.

## 3.7 Revocation

Revocation is the mechanism by which capabilities stop working. BedrockOS
supports three revocation modes, and the choice of mode is a *property of the
object*, decided by the object's type, not by the caller:

### 3.7.1 Drop-Death (Default)

**Mechanics.** No explicit revoke operation exists. A capability dies when it
is removed from the domain's table (a `drop` hook, table teardown at domain
death, or explicit `revoke` on the handle by its holder). The node's strong
reference is released; when the last one is released, the node's resources
are reclaimed. The node *is* its capabilities.

**Properties.** Pure, Rust-native, zero kernel-side bookkeeping beyond the
`Arc` count. There is no way to forcibly kill a node while someone holds a
capability to it — that is a feature, not a limitation: an object you have
handed out cannot be yanked from under its holders.

**Applicability.** Default for leaves and ordinary interior nodes (files,
events, buffers, sessions).

### 3.7.2 Cascade Revocation (Family Roots)

**Mechanics.** A family root capability may carry `REVOKE`. Revoking a family
root's capability severs the entire subtree in one operation: every
capability that reaches any descendant becomes dead, and (if no other
capabilities reach the descendants) the descendants die by drop-death. The
tree does not get individually unwound; it is cut at the trunk.

**Properties.** This is how a controller can be taken offline atomically
(PCI root detached → all devices under it die), and how the Principal's
power-off works at the total scale (revoke the whole projection).

**Implementation shape.** Revocation of a family root sets a liveness flag on
the root; the object store's cascade registry records which descendants are
covered; dispatch consults the flag on every `PERMIT` along the affected
edges (see 8.5 for the race handling).

### 3.7.3 Deny-List Revocation (Opt-In Revocable Objects)

**Mechanics.** An object may declare itself `Revocable`. The kernel then
maintains a per-object *deny-list*: a `revoked` flag that any holder with the
`REVOKE` right over the object can set. Once set, all future `PERMIT` checks
against that object fail, even though the capabilities still exist and still
hold strong references. The capabilities become *zombie* handles: present,
counted, but inert.

**Properties.** This is the mechanism for "I lent you this and I want it
back," or "this key is compromised, kill it." The object does not die (its
holders may still hold the strong reference); it is merely *deactivated*.
Whether it is later reclaimed depends on whether the revoker also releases
its capabilities — revoke is not free, and the deny-list exists precisely
because drop-death cannot express "make it stop even though I still hold it."

### 3.7.4 The Total Case

Power-off is the Principal revoking the entire projection — cascade
revocation at maximum scope. Under a crash, the same effect is achieved by
the machine stopping; the graph simply ceases to exist because its existence
condition (the machine booted) is gone. This closes the loop on the
Principal's existence condition: the Principal exists because the machine
booted; the world exists because the Principal rooted it; the world ends
because the machine stops.

---

---

# 4. The Trinity: Surface, Hook, Contract

Every node in the graph participates in the graph through exactly three
faces. They are the whole of a node's relationship to the world. There are no
other kinds of relationships. A node is *known* through its **Surface**, is
*made to act* through its **Hooks**, and is *promised to* through its
**Contracts**. A **Contract** is also called a **Path**; the two words are
synonyms in this document and throughout the codebase. Where one is used, the
other is implied.

## 4.1 Surface

**Definition.** The *surface* of a node is its passive data face: everything
about the node that can be read without causing a side effect. It is what the
node *is*, as opposed to what it *does*.

**Properties.**

1.  **Read-only.** Reading a surface never changes the node, the graph, or
    any holder. Surface reads are the only operations that require no more
    than the `QUERY` right (and, in general, the only operations callable on
    a node one merely *knows about*). They are side-effect-free by
    construction and may be freely invoked.
2.  **Typed.** Every piece of a surface has a type. A surface is not a flat
    blob; it is a structured description with a schema. The schema is part of
    the node's type identity and is captured in its contracts.
3.  **Labels are surface data — never addressing.** A name, a label, an
    identifier, a human-readable string attached to a node is *surface data*:
    something you read, display, and reason about. It is **never** a lookup
    key, never something the kernel resolves, never addressing. This is the
    concrete death of the Unix string-path model. `song.mp3`, `COM1`,
    `cpu0` — these are labels on surfaces, meaningful to a human reading a
    projection, meaningless to the kernel as addresses.
4. **Aggregability.** A node's surface can describe its interior in aggregate
   (count, total size, names of children, health) without exposing the
   interior graph itself. You can know "this directory has 12 children and
   they occupy 3.4 MB" without being able to traverse to any of them —
   traversal is a *hook*, not a surface read.

**Examples of surfaces in the current system (renamed):**

| Current construct | It is | Its surface would be |
|---|---|---|
| `Stat` (vfs) | surface of an inode node | kind, size, mtime, ino, permissions |
| `model_string()` (BlockDevice) | a surface attribute | human label, present for display only |
| `StatFs` (vfs) | surface of a mount node | block counts, free space, fs type |
| `InputDevice` (UInputL) | surface of an input device | name, `capabilities` bitmask, id |
| `KernelServices` fields | surfaces of service nodes | timer, interrupts, serial, pci, dma… |
| `IoCompletions` | surface of a completion record | completed/error counts |
| `KernelLayout` section bounds | surface of the kernel node | text/rodata/rela/idt ranges |

**The surface contract.** The *kind* of a surface is part of the node's
contract identity. Two nodes with identical surface kinds expect to be read
with compatible semantics. The exact schema is defined per contract (see 4.3).

**Implementation (P6-B).** Surface reads are the reserved `_read_surface`
hook (`SURFACE_READ` in `obj/hook.rs`). `invoke` intercepts it *before* the
contract-membership test and resolves the handle through
`CapabilityTable::resolve_for_query`, which requires only that the handle be
`Live` and hold the universal `QUERY` right (no `INVOKE`, no contract
membership), and still probes the revocable deny-list so a revoked node's
surface is inert too. The value comes from `Obj::surface_value(name)`
(default `None` → `NotSupported`); live overrides include
`PhysMemNode` (`total_frames`), `HeapNode` (`arena`), `AddressSpaceNode`
(`root`), `CpuRootNode`/`CpuNode` (`cpus`), `TableNode` (`slots`),
`StoreNode` (`records`), `MemRegionNode` (`base`/`size`), `PciDeviceNode`
(`bus`/`device`/`function`/`vendor_id`/`device_id`/`class`); the fs nodes
advertise surface schemas but do not yet override `surface_value`
(`NotSupported`). Separation proves the gate: an `INVOKE`-only cap (QUERY
dropped) is refused `Denied` on any surface read (`separation.rs:335-342`).

## 4.2 Hook

**Definition.** A *hook* is the active face of a node: an operation that
makes the node *do* something. A hook has a name, a signature (input and
output types), and, under a contract, a promised behavior.

**Property 1 — Hooks require the `INVOKE` right (and a contract right).**
Calling a hook is the only way to make a node act, and it is capability-
gated. A caller must hold a capability whose `rights` contain `INVOKE` *and*
the contract-specific right for the particular hook. Merely *knowing about* a
node (surface) is never enough to make it act.

**Property 2 — Hooks divide by contract.** A node may implement several
contracts; under each contract a different, possibly overlapping, set of
hooks is addressable. The same life object can expose `read` under a storage
contract and `seek` under another contract that bundles `read`+`seek`+`size`.
The contract is the leaf set of allowed hooks; you cannot invoke a hook the
contract does not name.

**Property 3 — Hooks may attune, materialize, or compose.** A hook's reply may
carry capabilities (invocation-return powers, §3.4.4). Thus hooks are the
vehicle of attunement and materialization (`traverse`, `bring_up`) and
composition (connection, mount, session). The hook itself never mints —
minting is exclusively the Principal's (§3.4) — it only hands out *attuned*
capabilities derived from the authority the callee already held.

**Property 4 — Hooks are the implementation boundary.** A hook's body *is* the
implementation of the node. The page-table walker, the heap allocator body,
the IRQ dispatch table, the lock acquisition — all are hook bodies. Running
inside a hook body is not ambient authority (Claim 1): you are only there
because a caller held the capability and invoked the hook. The trap handler
is the dispatch of an interrupt node's `deliver` hook; nobody runs it by
position, they run it because an interrupt edge reached a registered handler.

**Property 5 — Hooks suspend nothing (kernel) / may suspend (user).** In the
kernel (single-address-space, multicore), hook invocation is synchronous and
non-preemptible by default. Across the user boundary (§ state), a hook
invocation may become an asynchronous message under the contract (see Section 6
and P6).

**Signature form.** Every hook has a `HookId` and a parameter/result schema:

```
HookId = hash("kind" : "opname", signature)
signature := (params: type*), -> (reply: type | cap | error)
```

The signature is part of the hook's identity; two hooks with the same name
but different signatures are different hooks. That is what is hashed into a
`ContractId`.

**Examples of hooks (current system, renamed):**

| Current | Under the graph it is a |
|---|---|
| `InodeOps::read_at/ write_at /lookup/create/unlink/mkdir/rmdir/readdir/getattr/rename/truncate` | the hooks of the inode/filesystem contract |
| `BlockDevice::submit / sector_count` | hooks of the block-device contract |
| `FdTable::alloc / get / free / dup / dup2` | handle-semantics hooks of a capacity table |
| `UniversalTimer::one_shot ` next_deadline | hooks of the timer contract |
| `DmaAllocator::alloc_page / _contiguous / map_mmio / virt_to_phys | hooks of the DMA contract |
| `KernelServices` fields accessed via `kernel_services()` | hooks of various service contracts (soon to be invoked *through capabilities, not ambient) |

## 4.3 Contract (a.k.a. Path)

**Definition.** A *contract* is the promise that ties a node's surface to
its hooks: *"if you do X (invoke this hook on me under this contract), I give
or do Y (the reply/effect specified)."* A contract is also called a *path*.
The two words are interchangeable.

**The two readings of "contract-as-path":**

1. **Contract = a promise ("if...then...").** The behavioral reading. When a
   node advertises that it implements contract `c`, it is *promising* that,
   for the hooks named by `c`, given the inputs `c` declares, it produces the
   outputs and side effects `c` declares. This is where "if you do X, I do
   Y" lives.

2. **Contract = a path (the road you travel).** The relational reading. A
   contract is the schema of a *journey* through the graph: with this
   capability, I may call these hooks, which will by their signatures lead me
   through certain descendants, to discover other capabilities (returns) and
   to make the node *do* things. A path is nothing more than the
   shape of a contract — a bounded way to get somewhere. The original
   thesis — "the application, using its own contracts, uses another object's
   hooks" — is exactly: the app's capability (its contract/path) lets it reach
   the object and invoke a hook.

Because the same concept carries the promise (semantics: if X then Y) and
the journey (the permitted invocation to reach it), it is named both
*contract* and *path* and treated as one.

**Formal shape of a contract.**

```
ContractId     := hash( kind_name, surface_schema, ordered_signatures(hooks) )
Contract {
    id         : ContractId
    name       : &'static str   // e.g. "FAT32", "block:storage", "input:keyboard"
    surface    : SurfaceSchema  // what can be read, and its types
    hooks      : [HookSignature] // ordered; order matters for identity
    doc        : &'static str   // normative: "if you do X, you get Y"
}
```

Contract identity is *content-addressed* by `(name + surface schema + ordered
hook signatures)`. Two nodes that agree on all three implement the *same*
contract and are interchangeable across family boundaries. A node can
implement several contracts; it advertises the set via its surface.

### Contracts are how user-space later participates

For in-kernel nodes, the implementation boundary is a Rust `dyn` trait (the
fast, typed path). For nodes that live in *user space*, `dyn` downcasting
cannot cross the address boundary. Contracts are the *portable* interface:
the schema survives serialization. A user-space object can say "I implement
contract `FAT`" and the kernel can dispatch to it by encoding the contract's
hooks into messages, exactly as §7.12 specifies. Contracts are therefore both
the in-kernel typed-interface and the cross-boundary wire-protocol — one
structure, two transports.

## 4.4 The Dispatch Flow (a Node's Life on a Hook Call)

The complete lifecycle of an invocation, from "a domain decides to call a
hook" to "the node responds":

```
 caller (domain) holds CapHandle h      // 1. permission to reach a node
     |  invoke(contract_id, hook_id, args)
     ↓
 PERMIT(h, contract, hook)              // 2. capability check (§3.3)
     o  h is Live
     o  h.rights has INVOKE
     o  h.rights has the contract right
     o  node implements contract_id
     o  node not revoked (if revocable)
     |  deny → Disowned/Revoked error
     ↓
 encode/select the body                    // 3a. kernel: this is the fast dyn call,
                                          // 3b. user   : serialize a message
     ↓
 invoke the node's dispatch(contract_id, hook_id, args)
     ↓
 node runs its hook body                   // 4. the implementation (§4.2 Prop 4)
     ↓
     reply (data, and/or attuned capabilities) // 5. invocation-return (§3.4.4)
     ↓
 reply handed to the caller domain          // 6. the path journey's step completes
```

Step 2 is the only *permission* logic in the entire system; everything else
is ordinary dispatch. The check is designed to be a small constant number of
word comparisons (see 9.4, "The fast path"). Nothing in steps 3–6 ever mints
on its own: every capability a reply carries is an *attunement* of authority
the callee already held. The one exception is a hook that returns a
brand-new family root, which is the sole province of the mint guard (§3.4)
— consulted at Step 3 whenever a hook wants one, and satisfied only when the
caller is the Principal acting through the bootstrapper, and the hook is
*authorized* to mint.

## 4.5 The Trinity in One Table

| Face | Direction | It is | Gated by | Used by |
|---|---|---|---|---|
| **Surface** | read | what the node *is* (passive, typed, labels) | `QUERY` (trivially) | reasoning, forensics, discovery-by-ownership, labels |
| **Hook** | invoke | what the node *does* (active, effect) | `INVOKE` + contract right | make the node act, materialize returns |
| **Contract / Path** | promise + journey | if you do X you get Y; the permitted way to reach & use | identity + rights | invocation semantics and reachability; the "path" the app travels |

A node is *known* via its surface, *acted on* via its hooks, and *traversed /
pledged to* via its contracts — the contract being the schema (which hooks,
with what signature, promising what) and its alias the path being the journey
(the way a caller with the right capability actually gets from "here" to
"doing X").

---

# 5. Bootstrap Sequence

## 5.1 The Question

"How does the first graph appear?"

The answer is that the graph is **born endowed**, not self-created, and the
first edge of the graph is an *act*, not a node. There is no fiat in the
system, and there is precisely one event that is not a capability operation:
the *act of booting*. Everything after it is capability-mediated.

## 5.2 Stage Zero — The Seed

When the machine powers on, the first thing to exist is the **Principal**.
The Principal's existence condition *is* the machine's being on. It has:

- an `ObjId` (0),
- an empty surface (a marker: "born of the machine powering up"),
- no hooks (it is quiescent),
- a parent edge pointing **out of the graph** — a null edge tagged
  "boot",
- and the **only** `MINT` right in existence.

The Principal has no capability table and is not a domain. It is the root of
all authority, not a participant.

## 5.3 Stage 1 — The Principal Roots the Bootstrapper

The Principal performs its one and only boot-time act: it **roots the root
bootstrapper**, delegating mint authority to it. Concretely:

```
boot(machine)            // the seed act
Principal = mint_root()  // stage 0; ObjId 0, has MINT
Principal.root_bootstrapper(Principal, { MINT })  // delegate the mint right
```

The bootstrapper is born with a single capability: a `CapHandle` naming a
`Principal`-rooted bootstrap domain, carrying the Principal's `MINT`
authority. This is the *only* capability with `MINT` that has ever been
issued, and (by §3.4) the only one that *can* be issued (mint is
Principal-only). It is the trunk of the whole tree.

*(P6-B implementation note: the authority is the `PrincipalContext` seed, not
a capability — no `CapHandle` in the system ever carries a `MINT` bit. The
bootstrapper performs its mints by passing the seed to `mint_node`, and the
guard freezes at `init()`'s end (`MINT_FROZEN`, §7.6). The narrative above is
the authority model; the concrete mechanism is in `obj/{mint,bootstrap}.rs`.)*

## 5.4 Stage 2 — Mint the First Graph

Through its rooted agent, the bootstrapper, the Principal mints the world.
The bootstrapper constructs the primitive nodes and the Principal attaches
their initial capabilities through it; the bootstrapper then attunes smaller,
more precise endowment subsets for each secondary domain:

1. **Primitive service nodes** — their interiors start empty; they are the
   tools the world is built with (each is a family root the Principal mints
   through the bootstrapper):
   - an **ObjectStore** node (weak registry + gc result; holds the projection
     material),
   - a **ContractRegistry** node,
   - a **mint gateway** (the mechanism through which the bootstrapper mints;
     it is inert after self-revoke),
   - a **PhysMem** node (frame bitmap; an atom-pool),
   - a **Heap** node (the dynamic-allocation surface),
   - an **AddressSpace** node (per-address-space page tables),
   - a **Cpu** node per discovered vCPU (per-CPU domains, §6.2),
   - an **Irq** node set (pulled from interrupts/MSI registry).
2. **Controller family roots** — the nodes that will own device trees. For
   each controller (PCI root complex, AHCI, xHCI, audio, input, serial), the
   Principal mints a family root capability through the bootstrapper.
   *These are the open-ended subsumptions* (§2.4). Hardware nodes (devices,
   ports, slots, endpoints) later *materialize* as children of these roots.
 3. **Endowment.** With the family roots in place, the bootstrapper hands each
    secondary domain its *attuned* starting capability set (§6) — precise,
    narrow subsets of the family roots' rights, never new mints. No domain
    ever looks something up; it is *born* with the caps it may touch.
4. **Module bring-up.** This is the literal `Kernel::init()` / ↑ sequence:
   heap → serial → SMP → page tables → arch → ACPI → IOAPIC → service
   container → SMP wake → interrupts enabled. Re-read, every subsystem that
   initializes is *loading a graph unit* and *collecting its capability* into
   the domain table.

## 5.5 Stage Three — Self-Revoke (drop-death)

When every kernel module has been started and the world is fully built, the
bootstrapper performs its final act: it **revokes its own capability** and
**dies** (§3.7.1 drop-death, decision C2). Two immediate consequences:

1. **Mint returns to the Principal.** The only capability that could ever
   mint has been destroyed. The graph is now authority-*closed*: the entire
   mystery of where caps come from reduces to *the Principal*, which is
   quiescent after boot in the lifetime of the system.
2. **The bootstrap edge dies.** Principal → bootstrapper are no longer live
   capability edges (the weak store keeps a record for history, but they do
   not matter now). The principal's bootstrapping work is done and cannot run
   again.

The projection has *settled*: what exists after boot is exactly what the
boot domain was endowed with, plus whatever has been materialized from the
family roots' subsumption.

## 5.6 The Mapping onto `Kernel::init()`

```
Kernel::new()      → principal lives (the constructor returns; the Principal
                     is the "I was invoked" of the kernel start)
init() [: switch_to_higher_half → heap → arch → ACPI → IOAPIC →
         init_services (the container) → set_global (the ambient leak = the
         premature endowment) → SMP wake → enable_interrupts]
run()              → device sweep (PCI enum, AHCI, VFS/tmpfs, mount ESP)
idle               → the principal sleeps; boot complete.
```

The single most significant change to be made to `init()` is the removal of
the final *ambient* hand-shake: `set_global(svc_static)` (§7.2, P1) that
currently lets any kernel code call `kernel_services()` without holding a
capability. In the graph, that hand-shake is replaced by placing
`KernelServices` into the boot domain's *endowment*, so that the same
providers are reachable onable, but only through the table.

## 5.7 Failure During Bootstrap

Bootstrap is the one place where nothing protects you (there are no parents,
no tables, no revocation, yet). Concretely:

- **Mint fails** (OOM): the boot must fail loudly. There is no recovery
  before the world exists. The current `BitmapAllocator` and `heap::init`
  already abort on exhaustion (`alloc`/`heap` panic or abort) — that
  behavior is preserved.
- **Bootstrapper dies before self-revoke**: an abrupt kernel panic at any
  stage leaks the boot table (the boot domain dies with it). This is
  unreachable in practice if the self-revoke is the last statement of
  `init()`/`run()` bring-up.

The bootstrap is a single linear trust zone. It is the one place the model
is "the machine is on, and this domain holds the key" — because nothing else
exists yet. The rules of the model apply *from the first minute* from after
the mint of the primitive nodes, but *during* the seeding steps you are using
the only authority a cold machine has.

## 5.8 What Power-off Does

Power-off is the Principal revoking the entire projection (the total case of
§3.7.4). Every family root, every cascade, every leaf — gone in one external
act. Under an orderly shutdown the `PowerControl` hook (§of the platform
node) is invoked; under a crash the blessings same thing by ceasing to
run, the graph has no existence requirement other than the machine's fact of
being on (§2.2), so its full collapse is instantaneous and consistent.

---

# 6. Domain Model

## 6.1 What Is a Domain, Again

A **domain** is an independent principal-in-the-small: an execution context
holding its own capability table. Domains are the *holders* of capabilities
and the *invokers* of hooks. A domain is a prison of allowed actions—the
closure of its table; it can do nothing not represented in its table, and
nothing its caps don't allow.

## 6.2 The First Domains

Because capabilities are meaningless with a single principal, the
very first step of the implementation (P1) establishes at least two disjoint
domains:

- **The boot domain**, inheriting the endowment of `KernelServices` and the
  family roots it holds, and
- **at least one driver domain**, e.g. the USB/device bring-up path, which
  *only* holds the caps it was endowed with (a controller family root or
  two), proving that the kernel cannot silently reach the driver's addresses
  by position.

Every CPU is ***also*** a domain: during SMP bring-up, each AP gets its own
table. The BSP belongs to the boot domain; the APs to their own. The result:
the kernel's "single principal" fiction is broken into real, disjoint,
wall-behaved -principal units.

## 6.3 Threading the Caller

The core mechanism: at any instant, exactly one domain's capability table is
"current." The pointer to it is threaded through all dispatch paths:

```
type CurrentDomain = &'static Domain;

signature dispatch(caller: &Current, handle: CapHandle, ...)
```

Because the kernel is single-address-space and, in the hot path,
non-preempting, the "current domain" is stored in a small per-CPU slot that
is set when a domain is entered and validates on each dispatches (see §8.13
on permission re-entrancy). On SMP, the slot is per-CPU; each CPU's
interrupt runner runs the interrupt in the *interrupted domain's* context
(§6.4), so the ISR that tries to allocate memory does so *through that
domain's heap cap*, not the kernel's.

## 6.4 IRQ Dispatch in the Interrupted Domain's Context

This is the design's answer to "who allocates in an ISR?" The rule:

> An interrupt is delivered in the context of the domain that was executing
> when it fired. The interrupt node's interior holds registered handlers;
> PERMIT/PERMIT is evaluated in the interrupted domain's context, and every
> hook a handler calls (including the allocator) runs in that same context's
> caps.

Consequences:

- An ISR cannot do more than the domain it interrupted could already do.
- "Allocate memory" in an ISR is only possible if the interrupted domain held
  a heap cap. It is not the kernel's; it is the domain's.
- The interrupt node's `register_handler` hook is its own contract (requires
  `INVOKE` over the vector), so arbitrary code can't self-attach to
  interrupt vectors by position.

**ISR-safe hook annotation (documented now, enforced later).** The design
relies on the *interrupted domain's* context being a sound authority for
anything an ISR does — but an ISR runs *inside* whatever the interrupted
thread was doing, so a hook body that blocks on a lock that thread holds
deadlocks on the first interrupt (a priority inversion that now manifests as
a hard hang). The kernel is currently non-preemptible in the hot path, which
makes this manageable, but the invariant must be stated before it is
enforced. Therefore every contract hook carries an **`isr_safe`** annotation
in its contract definition:

```rust
pub enum IsrSafety { IsrSafe, ThreadOnly }
// contract:
pub const DELIVER: HookSpec { right: INVOKE, isr_safe: IsrSafe }
```

- `IsrSafe` — the hook body must not acquire a lock that a preempted thread
  in the same domain could hold while blocked; must not sleep; must be
  re-entrant against itself.
- `ThreadOnly` — the hook may block; it must never be dispatched from an ISR
  context.

The annotation is *contract metadata now, enforcement later*: during P1–P5 it
documents the discipline; when P6 introduces user-space scheduling and
preemption, the dispatcher can reject a `deliver`-path call of a
`ThreadOnly` hook (and, in development, trip the same kind of loud assertion
the mint guard uses, §8.2). Registering a hook whose `isr_safe` is `false`
onto an `Irq` node is a contract error from day one.

## 6.5 Domain Death

Domain death is revoking the entire table + drop-death. A domain that has
spawned secondary domains (or established sessions in other families)
releases their caps; and when the last domain holding a capability dies, the
drop-death cascades apply. This is how sessions terminate, how a driver being
removed ends its devices, and how the boot domain, were it to "exit," would
cede everything back to the Principal (unreachable in practice).

## 6.6 Sessions (future) — the Principal mints again

C3 says the Principal *can* act post-boot. A **session** is the mechanism by
which the Principal grants new authority: a user logs in → the Principal
mints a new family root (a *user session* domain) → that domain is endowed
(login, itself an attunement of what the user was granted) → as long as the
session lives, the session's projection exists; logout = the Principal
revokes the session's family root → the session's subtree collapses
(cascade). This is how multi-user/log-in is structurally expressed in a
model defined in terms of mint ✕ dominion ✕ revocation. It is future (F6) —
but it is the *reason* the mint monopoly exists as it does.

---

---

# 7. Implementation Plan

This section is the *code-level* specification. It names the new module,
every new type, every reworked existing module, and every deletion. It is
written against the repository as it exists at commit `4624b92`.

## 7.1 Module Layout

A new top-level kernel module, `kernel/src/obj/`, hosts the graph. It is the
only place where the raw capability mechanics live; the rest of the kernel
is its client.

```
kernel/src/obj/
├── mod.rs          // Obj trait, ObjId, dispatch entry, module surface
├── rights.rs       // Rights bitmask (QUERY, INVOKE, TRAVERSE, MINT, REVOKE)
├── cap_handle.rs   // CapHandle, CapId, HandleState, RevocationPolicy
├── table.rs        // CapabilityTable (generalized from vfs::fdtable)
├── domain.rs       // Domain, current-domain slot (per-CPU)
├── store.rs        // ObjectStore (weak registry, history, projection material)
├── contract.rs     // ContractId, Contract, ContractRegistry
├── surface.rs      // SurfaceDesc, AttributeId, typed attribute reads
├── hook.rs         // HookId, HookSignature
└── mint.rs         // the mint guard + mint entry point (Principal-gated)
```

Existing modules reworked:

```
kernel/src/services/capability.rs   // → seed of obj/ types (Rights, CapHandle)
kernel/src/filesystems/vfs/fdtable.rs // → generalized into obj::table
kernel/src/filesystems/vfs/mod.rs     // → DRIVE_MAP/FD_TABLE/CWD die; readdir gated
kernel/src/services/mod.rs            // → KernelServices becomes endowment; global dies
kernel/src/kerneldump/                // → + graph projection walker (P5)
kernel/src/mm/{heap,phys_alloc,vmm}/* // → wrapped as nodes (P3)
kernel/src/lib.rs                     // → init re-read as bootstrap (§5.5)
```

## 7.2 The Core Types (Rust Sketches)

These are *normative shapes*: the actual implementation must match these
semantics, even where the precise Rust spelling evolves.

### 7.2.1 ObjId and Obj

```rust
/// Globally unique node identity. Confers nothing; used by the store and
/// by forensics only. Never an access key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ObjId(pub u64);

/// The node. Every object in the system implements this.
pub trait Obj: Send + Sync {
    /// Stable node identity (kernel-issued, monotone).
    fn obj_id(&self) -> ObjId;

    /// The node's kind, e.g. "controller:xhci", "fs:fat32", "atom:irq".
    /// The kind is part of contract identity.
    fn kind(&self) -> &'static str;

    /// Passive face (§4.1). Read-only, side-effect-free.
    fn surface(&self) -> &'static SurfaceDesc;

    /// Contracts this node implements (§4.3).
    fn contracts(&self) -> &'static [ContractId];

    /// Whether this node is revocable (deny-list) or pure drop-death (§3.7).
    fn revocation(&self) -> RevocationPolicy {
        RevocationPolicy::DropDeath
    }

    /// Active face (§4.2). The node implements the hooks its contracts name.
    /// `caller` is the invoking domain's table, threaded per §6.3.
    fn dispatch(
        &self,
        caller: &CapabilityTable,
        hook: HookId,
        args: &Args,
    ) -> Result<Reply, ObjError>;
}
```

### 7.2.2 Rights

```rust
pub struct Rights(u32);
impl Rights {
    pub const QUERY:   Rights = Rights(1 << 0); // read surface
    pub const INVOKE:  Rights = Rights(1 << 1); // call hooks
    pub const TRAVERSE:Rights = Rights(1 << 2); // enter interior, materialize children
    pub const MINT:    Rights = Rights(1 << 3); // create family roots (Principal-only)
    pub const REVOKE:  Rights = Rights(1 << 4); // set deny-list / cascade on a root
}
```

**Monotone rule.** Attunement (`attune(new_rights)`, code name `derive`) may
only *clear* bits, never set them — this is what makes an attuned capability
*more precise and tinier*:

```rust
pub fn attune(&self, keep: Rights) -> Result<Rights, ObjError> {
    let r = self.0 & keep.0;
    if r == self.0 { Ok(Rights(r)) } else { Err(ObjError::NoAmplification) }
}
```

`attune` keeps every bit the caller already has that it asked to keep; if the
attuned capability would be *larger* (has bits the parent lacks) — impossible
by construction, since we AND — the error is unreachable. It is a canary for
future bugs. This single operation is the whole of what "making a capability
more precise" means: a process takes a capability it holds and attunes it
into a tinier one.

**Contract-rights dimension (monotonicity must cover both).** The `attune`
mask above only constrains the bits of the universal five. Contract-specific
rights (`READ`/`WRITE`, §3.3) live in a *separate* bitfield alongside, not in
the universal mask. Monotonicity therefore has two dimensions, and both must
shrink together:

```rust
pub fn attune(&self, keep: Rights, keep_contract: ContractRights)
    -> Result<CapRights, ObjError> {
    let u = self.uni & keep;
    let c = self.contract & keep_contract;
    if u == self.uni && c == self.contract {  // every kept dimension shrank-or-held
        Ok(CapRights { uni: u, contract: c })
    } else { Err(ObjError::NoAmplification) }
}
```

If the two masks were not settled together, a `derive` that ANDs the universal
mask but forgets the contract mask would let a caller *gain* a contract right
its parent never held — a silent amplification. The contract-rights mask
must shrink monotonically alongside the universal mask; the implementation
must never construct a `Cap` whose two dimensions move in opposite
directions.

### 7.2.3 CapHandle

```rust
pub struct CapHandle {
    pub id:     CapId,            // unforgeable; kernel-issued; monotone
    pub node:   Arc<dyn Obj>,     // strong reference → lifetime = reachability
    pub rights: Rights,
    pub state:  HandleState,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HandleState {
    Live,
    Revoked,   // deny-list fired, or family-root cascade severed this
    Zombie,    // handle exists (holds ref) but object is deactivated
}

pub enum RevocationPolicy {
    DropDeath,   // default; object dies with its last cap
    Revocable,   // deny-list; REVOKE right can set HandleState::Revoked
}
```

### 7.2.4 Contracts and Hooks

```rust
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContractId(pub u64);   // hash of the contract's identity tuple

pub struct Contract {
    pub id:      ContractId,
    pub name:    &'static str,       // "fs:fat32", "block:storage", ...
    pub surface: &'static SurfaceSchema,
    pub hooks:   &'static [HookSignature], // ordered; order is part of identity
    pub doc:     &'static str,       // normative: "if you do X, you get Y"
}

pub struct HookSignature {
    pub name: &'static str,          // "read_at"
    pub params: &'static [TypeTag],  // [U64, BufMut]
    pub reply:  ReplyTag,            // {Data([TypeTag]), Caps, Err}
}
```

**Identity.** `ContractId = FNV/sha-over (name, surface schema, ordered hook
signatures)`. Rename any part and the contract changes identity — a breaking
change, detected at registration (see 7.8, and §8.18 for versioning).

### 7.2.5 Surface

```rust
pub struct SurfaceDesc {
    pub kind:   &'static str,       // surface kind; part of contract identity
    pub attrs:  &'static [SurfaceAttr],  // typed, readable fields
    pub events: &'static [EventDesc],    // optional event-stream descriptors
}

pub struct SurfaceAttr {
    pub name: &'static str,   // label, e.g. "size", "mtime", "model"
    pub ty:   TypeTag,        // U64, Str, Blob, Mask, ...
    pub read: fn(&dyn Obj) -> AttrValue,  // side-effect-free getter
}
```

Labels live here. A name on a surface is data for humans and tools; it is
never resolved (§4.1 Property 3).

## 7.3 The ObjectStore

`obj::store` holds the weak record of the entire graph. Its purpose is
threefold: (a) **projection material** for `kerneldump graph` (§7.12), (b)
**cascade bookkeeping** for family-root revocation (§8.5), (c) **history**
for forensics after drop-death.

```rust
struct ObjRecord {
    id:       ObjId,
    kind:     &'static str,
    parent:   Option<ObjId>,        // the family root's id (Principal = None)
    created:  u64,                  // boot-time monotone counter
    weak:     Weak<dyn Obj>,        // does NOT keep the node alive
    revocations: usize,             // deny-list toggle count (forensic)
}

pub struct ObjectStore {
    records: Mutex<BTreeMap<ObjId, ObjRecord>>,
    family_roots: Mutex<Vec<ObjId>>,        // roots with REVOKE outstanding
    cascade: Mutex<HashMap<ObjId, CascadeState>>, // root → severed?
    next_id: AtomicU64,
}
```

**Critical rule — the store is weak.** `ObjRecord.weak` is a `Weak`; the
store never keeps a node alive. This is what makes *lifetime = reachability*
true and the projection claim honest: the store can *report* a node that
died, but cannot resurrect it. The store is not a namespace (§2.8): nothing
addresses a node by looking it up in the store; the store is only consulted
by the projection tool, the cascade machinery, and the debugger.

## 7.4 CapabilityTable (from FdTable)

`vfs/fdtable.rs` already implements a sparse, growable, free-list slot array
with `dup`/`dup2`. It becomes `obj::table::CapabilityTable`:

```rust
pub struct CapabilityTable {
    slots: IrqMutex<Vec<Option<CapHandle>>>,   // IrqMutex: interrupt-safe (§vfs/irq)
    free_list: Vec<u32>,
    next: AtomicU64,                            // CapId source (per-table)
}
```

Differences from `FdTable`:

1.  Slots hold `CapHandle` (node + rights + state) instead of
    `FileDescription`.
2.  `dup(old)` attunes a *copy* with the *same* rights — `FdTable::dup`
    semantics. `dup_limited(old, rights)` attunes with reduced rights
    (§3.4.3). There is no operation that increases rights.
 3.  `get(id)` is the `PERMIT`-less raw fetch (used by the object's own
     dispatch, which already passed `PERMIT`); the *public* path is
     `resolve_with_rights(id, contract, hook) -> Result<(Arc<dyn Obj>,
     CapRights)>` which runs the full `PERMIT` (3.3), returns a
     contract-checked reference *and* the invoking handle's rights (copied
     under the same lock, so a provider may re-check them, S1); `resolve`
     drops the rights; `resolve_for_query(id)` is the QUERY-only surface-read
     fetch (§4.1).
 4.  Revocation-aware: `revoke(id)` marks `HandleState::Revoked`; family-root
    cascade severs *every* reachable descendant (via store cascade
    bookkeeping).

**The mapping is one-to-one.** `FD_TABLE` is a single static; the graph
replaces it with one table *per domain*. The 10 call sites that fetch
`kernel_services()`-backed providers today (§7.7) become `domain.resolve(...)`
calls.

## 7.5 The Fast Path (PERMIT)

`PERMIT` (§3.3) is the one permission check in the system, and it must be
cheap. Its shape on the hot path:

```
resolve_with_rights(id, contract, hook):
    slot = slots.lock()?                        // IrqMutex (spin; cheap)
    h    = slot[id]?                            // array fetch
    if h.state != Live           -> Err(Revoked)
    if !(h.rights & INVOKE)      -> Err(Denied)
    if !node.implements(contract) -> Err(Denied)
    if h.contract != empty() and
       !(h.contract & node.hook_contract_right(contract, hook)) -> Err(Denied)
    if node.revocation()==Revocable and store.deny(node) -> Err(Revoked)
    ok (node, h.rights copied under the lock)
```

That is: one mutex acquire, one array index, three bit-tests (INVOKE, per-hook
contract right, deny), one contract-membership hash probe, one deny-bit hash
probe. The `dyn` downcast + call follows. No allocation, no serialization, no
message encoding — on the in-kernel path. An `empty()` contract mask is the
transitional "not yet narrowed" state and passes the per-hook test
unconditionally (§3.3). This is the whole "cost of the graph": a few loads per
privileged operation. Section 9.4 proves the bound.

## 7.6 The Mint Guard

`obj::mint` is the single place a *brand-new family root* — and with it a new
capability — is brought into being. It is the most audited code in the
kernel, and it is the Principal's act and the Principal's alone. In the P6-B
build the mint entry is `mint_node`: it mints over an **already-constructed
real node** (the `StubNode` placeholders and the old `mint(kind, …)` helper
are deleted), and it seeds the handle's *contract-right* mask (`first_contract`)
so the per-hook gate is live from the first commit (§3.3, §7.5).

```rust
/// Returns the root capability of a newly created family, over an already
/// constructed node. Callable ONLY by the Principal — which, at boot, acts
/// through its rooted agent, the bootstrapper, and only before the
/// bootstrapper's self-revoke.
pub fn mint_node(
    caller: &PrincipalContext,       // who is asking (must be the Principal's)
    node: Arc<dyn Obj>,              // the node to root (it owns its identity)
    first_rights: Rights,            // the root's universal rights
    first_contract: ContractRights,  // the root's contract-right mask (READ|WRITE|CALL)
) -> Result<CapHandle, ObjError>;
```

Guard logic:

1.  `MINT_FROZEN` is a single-shot: the bootstrapper self-revokes as the last
    step of `init()` (`finalize_mint`), after which every `mint_node` fails
    with `MintAuthorityGone` (§8.2). A `MINT` *right* is never granted in any
    endowment — the guard is authority itself, not a capability (`PRIM_RIGHTS`
    = `INVOKE|QUERY|TRAVERSE`, never `MINT`).
2.  The `PrincipalContext` is a special value of the current-domain slot that
    only the bootstrap seed (§5.2) can enter; after `run()` begins it is no
    longer enterable.
3.  Mint takes an already-constructed node that *owns* its identity: the
    physical-world roots carry stable family-root `ObjId`s (e.g. `0x11_0000`)
    registered in the store under their own id and kind with `parent = None`.
    `mint_node` registers that record (no fresh id is allocated) and returns
    the single `CapHandle` that *is* the family root, carrying
    `CapRights::new(first_rights, first_contract)`.

There is no path to "issue a new family root" other than through `mint_node`.
All other capability creation (§3.4.2–4) is *attunement* — it derives narrower
handles from existing ones and never touches the guard.

## 7.7 Killing the Ambient Globals

At commit `4624b92`, the ambient-access surface of the kernel is small and
exactly enumerable. The P1/P2 sweep deletes it. The inventory:

### 7.7.1 `kernel_services()` — 10 call sites

`rg "kernel_services\\(\\)"` at commit lists:

```
kernel/src/services/mod.rs                 // definition + set_global
kernel/src/pci/mod.rs                      // enum / init entry
kernel/src/pci/enumerate.rs                // bus scan
kernel/src/pci/caps.rs                     // capability probing
kernel/src/pci/msi.rs                      // MSI alloc
kernel/src/pci/msix.rs                     // MSI-X programming
kernel/src/usb/xhci/mod.rs                 // controller init/DMA
kernel/src/filesystems/blockdriver/ahci.rs // AHCI init/DMA
kernel/src/filesystems/blockdriver/driver.rs
kernel/src/audio/hda.rs                    // audio DMA
```

**Change.** `set_global(svc_static)` is deleted from `Kernel::init()`.
Instead the boot domain's endowment is constructed from the *same*
`KernelServices` values, and each driver that today calls
`kernel_services().dma.alloc_page(...)` instead holds a `CapHandle` naming
the DMA node and calls `caller.resolve(dma_handle, DMA_CONTRACT, ALLOC)`. The
container itself (`KernelServices`) does not disappear — it becomes the boot
domain's private endowment table, exactly as §5.5 promises: *the providers
are reachable, but only through the table.*

### 7.7.2 `FD_TABLE`, `DRIVE_MAP`, `CWD`

```
kernel/src/filesystems/vfs/mod.rs:32-33   // static FD_TABLE, DRIVE_MAP
kernel/src/filesystems/vfs/mod.rs:40      // static CWD
```

**Change.** `FD_TABLE` dies (per-domain tables replace it, §7.4).
`DRIVE_MAP` and `CWD` die with the string-path model (§7.11). The `FdTable`
struct survives as `CapabilityTable`; the ambient statics do not.

### 7.7.3 `vfs::path` — the string resolver

```
kernel/src/filesystems/vfs/path.rs        // split_drive_path, split_components,
                                          // walk_from, attempt_mount_cross, next_mount_id
kernel/src/filesystems/vfs/mod.rs         // resolve_path, resolve_parent, getcwd, chdir
```

**Change.** Deleted under P4 (pure capabilities). The directory *node* takes
over: `traverse(name)` becomes a hook that *attunes* a child capability from
within the family; `readdir` becomes capability-gated (returns only children
the caller could be granted). No kernel code resolves a string.

## 7.8 The Contract Registry

`obj::contract::ContractRegistry` is a node (§2.4) whose interior is the set
of registered contracts. It is where the content-addressed identity is
*validated* — two nodes claiming the same `ContractId` must hash to the same
tuple, or registration fails loudly (a duplicate-name-with-different-signature
bug surfaces here, not in the field).

```rust
pub struct ContractRegistry {
    by_id: Mutex<HashMap<ContractId, &'static Contract>>,
}
impl ContractRegistry {
    pub fn register(&self, c: &'static Contract) -> Result<(), ObjError>;
    pub fn lookup(&self, id: ContractId) -> Option<&'static Contract>;
}
```

**The registry is a node, and so are the tables and the store** (§2.4, the
"infrastructure is also nodes" principle). Holding a registry capability is
how a driver queries "what does `block:storage` promise?" — it is a
discovery-by-owned-capability, not ambient. The registry's own hooks
(`register`, `lookup`) require `INVOKE`; only domains endowed with the
registry cap can consult it.

**Implementation (P6-B).** The boot domain is endowed with the registry cap
(`BootEndowment.registry`, holding `INVOKE|QUERY`), and every contract —
providers (`dma:alloc`, `pci:cfg`, serial), the five physical-world families,
the fs families, the device families (`pci:forest`, input, audio), and
`mem:region` — is seeded **through that owned capability** (the
`invoke(… REGISTRY_REGISTER)` calls in `obj/bootstrap.rs`), so registration
is never ambient. The table is itself a node (`infra:table`, `obj/table.rs`):
the boot domain holds a table cap (`INVOKE|QUERY|REVOKE`) exposing `count`,
`snapshot_size`, `delegate`, and `revoke_cascade` hooks, making delegation and
cascade revocation capability-mediated (§8.24). The store is a node too
(`infra:store`): read-only `count`/`lookup`/`denied` forensics hooks over the
weak records.

## 7.9 Dispatch Entry (wiring the table to the object)

The single entry point the rest of the kernel calls to reach a node:

```rust
pub fn invoke(
    table: &CapabilityTable,
    id: CapId,
    contract: ContractId,
    hook: HookId,
    args: &Args,
) -> Result<Reply, ObjError> {
    // §4.1 surface reads: node-level, QUERY-gated, exempt from contract
    // membership; resolved via resolve_for_query before PERMIT.
    if hook == SURFACE_READ { … node.surface_value(name) … }
    // §7.5 fast path; the caller's exact CapRights are copied under the same
    // lock and threaded into dispatch for the provider to re-check (S1).
    let (node, rights) = table.resolve_with_rights(id, contract, hook)?;
    // capabilities in the reply are inserted into the caller's table
    node.dispatch(table, &rights, hook, args)        // §4.4 step 4
}
```

`Args` / `Reply` are a small tagged-union over scalars, buffers, and
capabilities. In-kernel they stay in-memory; across the user boundary they
serialize (§7.12). Capabilities in a `Reply` are inserted into the caller's
table by the kernel *before* the reply is returned — never handed over as raw
`CapId`s (unforgeability §3.2).

---

## 7.10 The Physical World as Nodes (P3) *(implemented)*

Under the graph, the "core memory" layer is not a kernel-private backroom —
it is a set of nodes with surfaces, hooks, and contracts, reached only by
capability. Nothing is ambient, including the allocators and the page
tables (Claim 1). The following nodes wrap the existing modules.

### 7.10.1 `PhysMem` — the frame pool

Wraps `kernel/src/mm/phys_alloc.rs` (`BitmapAllocator`).

| Face | Shape |
|---|---|
| Surface | memory-map regions, free/used counts, reserved ranges, allocator state snapshot |
| Hooks | `alloc_frames(n) -> MemRegion` (returns a capability to a new frame-region node or atom), `free(region)`, `reserve(start,len)`, `alloc_contiguous(n)` |
| Contract | `physmem:allocation` |

Every frame handed out is wrapped in a **MemRegion** node (an atom-pool
family) so that "a page of memory" is a *thing with a capability*, and
freeing is *revoking the last capability to it*. The `BitmapAllocator`
becomes the *implementation* of the PhysMem node's hooks.

**The bootstrap carve-out.** Before `heap::init` runs there is no heap, so
the PhysMem node itself is carved from a static seed — the same trick the
heap already uses (`HEAP: Mutex<HeapInner> = Mutex::new(HeapInner::empty())`,
`kernel/src/mm/heap.rs:430`). The first nodes (PhysMem, the store) are
static; everything after is heap-born.

### 7.10.2 `Heap` — dynamic allocation

Wraps `kernel/src/mm/heap.rs`.

| Face | Shape |
|---|---|
| Surface | arena bounds, allocated bytes, peak, fragmentation stats |
| Hooks | `alloc(size, align) -> MemRegion`, `free(region)`, `stats()` |
| Contract | `heap:allocation` |

**The ISR rule.** Because IRQs dispatch in the interrupted domain's context
(§6.4), a handler that must allocate does so *through the interrupted
domain's heap cap* — not the kernel's. The heap node is not "the kernel's
heap"; it is a node whose hook any domain holding the cap may call. The boot
domain holds it; a driver domain may or may not, by endowment.

### 7.10.3 `AddressSpace` — page tables

Wraps `kernel/src/mm/vmm/*` (`Vmm`, `CurrentArch::setup_virt_mem`,
`paging::setup`).

| Face | Shape |
|---|---|
| Surface | mapping census, region list, fault counters |
| Hooks | `map(va, phys, flags)`, `unmap(va)`, `protect(va, flags)`, `shootdown(cpu)` |
| Contract | `mm:address_space` |

The page-table walk is the *implementation* of the address-space node's
hooks; nobody walks the tables by position. TLB shootdown is not a side
channel: `shootdown` is a hook that the address-space node invokes against
the **Cpu** nodes (§7.10.4) via the capabilities it holds — a cross-edge
call, visible in the projection like any other.

### 7.10.4 `Cpu` — one per vCPU

Wraps `kernel/src/smp/*` (`MAX_CPUS`, per-CPU state, AP wake).

| Face | Shape |
|---|---|
| Surface | apic id, online flag, per-cpu stats |
| Hooks | `wake(ctx)`, `ipi(target, vector)`, `shootdown()`, `stats()` |
| Contract | `smp:cpu` |

Every CPU is a *domain* (§6.2) with its own table. The BSP's table is the
boot domain's; each AP's is endowed by the bootstrapper during SMP bring-up.
`Cpu` nodes are the targets of `shootdown` calls, which is how the
"cross-edge TLB flush" of §7.10.3 is realized as visible graph traffic.

### 7.10.5 `Irq` — one node per vector/device-interrupt

Wraps `kernel/src/services/interrupts.rs`, `msi.rs`, `null_msi.rs`, and the
arch IDT/PLIC machinery.

| Face | Shape |
|---|---|
| Surface | vector, device binding, handler census, pending/EOI state |
| Hooks | `register_handler(cap)`, `unregister(cap)`, `ack()`, `set_enabled(bool)` |
| Contract | `irq:vector` |

**Dispatch in caller context** (§6.4): the asm trap entry is the `Irq`
node's `deliver` hook body; it runs with the interrupted domain's table as
`caller`. `register_handler` is a hook requiring `INVOKE` over the vector —
no code attaches to an interrupt by position. MSI/MSI-X programming becomes
an `Irq`-family operation gated by the cap the device domain holds.

## 7.11 The Device / Service World as Nodes (P4) *(implemented)*

Every trait-object that the kernel already dispatches through `dyn` becomes
a node by *implementing* `Obj` (a thin adapter), and every global registry
becomes a family root. The existing typed interfaces are the *contracts*;
the existing `dyn` calls are the *hook dispatch* — we are not inventing new
mechanisms, we are capping existing ones.

### 7.11.1 `BlockDevice` → node

`kernel/src/filesystems/blockdriver/traits.rs`:

| Current | Graph role |
|---|---|
| `BlockDevice::submit(&[IoRequest]) -> IoCompletions` | hook `submit` of contract `block:storage` |
| `sector_count()` | surface attribute |
| `model_string()` | surface label (display only) |

The AHCI node is a **controller family root**: its interior is the set of
attached ports; each port materializes a block node. `IoRequest` /
`IoCompletions` become the surface/hook argument types, unchanged in
semantics.

### 7.11.2 `InodeOps` → filesystem node contract

`kernel/src/filesystems/vfs/inode.rs` — the whole `InodeOps` trait becomes
the *hooks* of the filesystem contract (which every FS implements): `read_at`,
`write_at`, `lookup`, `create`, `unlink`, `mkdir`, `rmdir`, `readdir`,
`getattr`, `rename`, `truncate`, `on_unlink`. `Stat` is the surface.

**The famous change: `lookup` and `readdir` become capability-gated.**
Today `resolve_path` walks the dentry tree *ambiently* and `readdir` returns
every child. Under the graph:

- `lookup(name)` is renamed `traverse(name)`. It requires `TRAVERSE` in the
  caller's rights and returns a *child capability* attuned within the
  directory's family (rights no greater than the caller's over the family —
  this is how "make it more precise, tinier" works in practice).
- `readdir()` returns only the children for which the caller *could* be
  granted a capability — capability-gated readdir, decision from the design
  discussion ("true ocap"). The projection rule (§2.8) becomes literally
  observable: the listing *is* the subset of the interior the caller's
  rights subsume.

### 7.11.3 UInputL → input family

`kernel/src/input/mod.rs` (the `InputDevice` registry) becomes an **input
family root**. Each `InputDevice` is a child node; its `capabilities` mask is
surface; event delivery is the family's hook. A consumer holding the family
cap materializes a session (§3.6) that subscribes to events. `grab`/`focus`
become rights on the session capability — a game can grab a device because it
holds a cap that says so, not because a kernel API let it.

### 7.11.4 PCI → controller forest

`kernel/src/pci/*` becomes a **PCI forest**: the root complex is a family
root; each bus/device/function is a materialized child; each device's BARs
and capabilities are surface; MSI/MSI-X and config-space programming are
hooks gated by the device capability. `pci::enumerate` (today ambient,
called from `Kernel::run`) becomes the controller materializing its children
during bring-up — a family fill, not a bus scan.

### 7.11.5 Audio → controller family

`kernel/src/audio/*` (HD Audio) becomes a family root with codec/pin nodes;
DMA buffers used by the codec are MemRegion capabilities the audio domain
derives from its heap cap (§7.10.2), not ambient `kernel_services().dma`.

### 7.11.6 Hot-plug = materialization (§8.3)

xHCI already owns the bring-up tree (`kernel/src/usb/xhci/*`: slots →
devices → interfaces). Under the graph its cap *subsumes* the whole USB
subtree (§2.4). A device appearing is the controller materializing a child —
the original "new device is just another child of the original family" —
and the child's capability derives from the family root the controller was
endowed with at boot. **No minting is required for hot-plug**; the Principal
was not involved. This is the operational payoff of subtree-scoped
capabilities.

## 7.12 VFS Rebuild: Pure Capabilities (P4)

### 7.12.1 What dies

```
vfs/path.rs        split_drive_path, split_components, walk_from,
                   attempt_mount_cross, next_mount_id
vfs/mod.rs         resolve_path, resolve_parent, getcwd, chdir, CWD,
                   DRIVE_MAP, FD_TABLE (ambient), 'A>'/'>B>' letter syntax
vfs/drive.rs       DriveMap
```

### 7.12.2 What replaces it

- `mount(fstype, device_cap) -> DirectoryCap` — a hook on the VFS root node
  that materializes a *mount* node (§3.6) composing a device capability and
  a filesystem driver capability into a directory-family root. There is no
  drive letter; the mount *is* the capability returned.
- `traverse(name)` on a directory node returns a child cap (§7.11.2).
- Names appear only as *surface labels* for human/tool display; the kernel
  resolves nothing by string.
- The boot domain is *endowed* with the initial directory caps (the old
  `A>` tmpfs root and, later, the ESP mount) directly — `VFS::init` places
  them into the endowment (attuned from the family roots) instead of
  registering them in a drive map.

### 7.12.3 What a shell becomes

A shell holds directory caps; it *renders* the labels of the surfaces it
owns (readdir output is labels + child caps). "cd music" is the shell
calling `traverse` and *stashing the returned cap in its own table* — never
the kernel resolving a string. "Open song.mp3" is the shell invoking `read`
on the child cap it obtained by traversal. The user-visible *path* is the
shell's own concatenation of labels it followed; the *kernel* sees only
capability operations. This preserves shell usability while keeping the
kernel pure.

## 7.13 The Projection Tool: `kerneldump graph` (P5)

*Status: implemented (P6-B). `kernel/src/kerneldump/{graph,leak}.rs` provide
`graph_census`, `graph`, `graph_with_flags` and `leak_detect`; the walker is
live, not a stub (the P1-era "returns the node census only" stub is gone).*

`kernel/src/kerneldump/*` (`dump.rs` `dump_full_fault`, `disasm.rs`) gains a
graph projector. It walks `ObjectStore.records` (§7.3) and emits a
**read-only snapshot**:

```
kerneldump graph [--roots] [--edges] [--caps] [--contracts] [--revocations]
```

Output shape (per node):

```
node 0x0000 kind="principal"         parent=none
node 0x0010 kind="controller:xhci"   parent=0x0004 family=root
  surface: {name, slots_used, ...}
  contracts: [usb:controller]
  held-by: domain 0x1 (boot), rights={INVOKE TRAVERSE} state=Live
  revocations: 0
  interior: [0x0011 .. 0x001F]  (8 materialized children)
```

**Properties.** (a) read-only — the projection cannot forge, invoke, or
change anything; (b) gated — only a domain holding the `kerneldump:project`
contract cap may request it; (c) unaddressable — it is a report, not a
namespace (§2.8); (d) historical — it can name nodes that have since died
(the weak store's records remain), making it a forensics tool, not a handle.

**The full projection is the answer to "can we make a full projection of all
graphs?": yes — as a snapshot. The graph never exists as a living whole; the
projection is a photograph of what the graph *was*, taken by someone holding
the camera's capability.**

## 7.14 The User Boundary (P6)

When the first user process exists, the boundary is expressed purely as
contracts:

- **Syscalls become "invoke hook on handle."** There is no `open(path)`,
  `read(fd)` syscall vocabulary. The ABI is `invoke(table_id, cap_id,
  contract_id, hook_id, args)`; `PERMIT` is evaluated in-kernel, the hook
  runs (or is forwarded across the boundary serialized), and the reply (data
  + inserted capabilities) is returned.
- **The address boundary is a transport, not a model change.** Contracts
  (§4.3) are the portable interface; a user-space node implements a contract
  by a message loop that decodes hooks from the serialized form. The same
  `ContractId` that governs an in-kernel `dyn` call governs the wire
  message. One structure, two transports.
- **Unforgeability at the boundary** (§8.16): user code receives `CapId`s but
  never raw pointers to tables; the kernel alone inserts capabilities into
  tables, so a user process cannot fabricate a `CapId` (it is not a number
  it can invent — the table slot it would need to write is kernel-owned).
- **Sessions (F6, §6.6):** a login mints a session family root via the
  Principal; logout revokes it (cascade). Multi-user is this document's
  model in miniature: mint, endow, project, revoke.

## 7.15 Build Gates

Every phase must leave both targets compiling and the x86_64 image bootable:

```
cargo build --target x86_64-unknown-none -p kernel --features cpu_slow
cargo build --target riscv64gc-unknown-none-elf -p kernel
fullrun.bat x86_64        # image + boot in QEMU
```

The riscv64 build is a compatibility gate: the graph model is arch-neutral
(§8.15), and the riscv64 target keeps compiling from the first phase.

---

---

# 8. Edge Cases and Their Handling

This catalogue is normative. Each entry names the risk, the governing
principle, and the concrete handling. If an implementation choice contradicts
an entry here, the entry wins.

## 8.1 Bootstrap chicken-and-egg (no heap before the heap)

**Risk.** The object store, capability tables, and the first nodes need
allocation, but allocation (the Heap node) doesn't exist until the Heap node
is minted.

**Principle.** The seed is static (§5.2); the world is minted from a static
arena before the heap exists.

**Handling.** The first graph units (PhysMem node, ObjectStore, the boot
domain's table, the bootstrapper's capability) are carved from the static
seeds the kernel already has: `HEAP: Mutex<HeapInner> = Mutex::new(
HeapInner::empty())` and the `BitmapAllocator` built in `Kernel::new`.
Exactly like today, nothing between `new()` and `heap::init()` allocates from
the heap; the new rule is that "nothing between the seed and `heap::init()`
uses a capability" — those few steps run on the static bootstrapper context
(§7.6's `PrincipalContext`), which is not enterable again (§8.2).

## 8.2 Bootstrapper self-revoke reentrancy

**Risk.** The bootstrapper, while minting, could be asked (by reentrant code)
to mint again *after* it has begun self-revoke, or an interrupt could fire
mid-self-revoke and try to mint.

**Principle.** The mint guard is entered once and never re-entered (§7.6).

**Handling.** The `PrincipalContext` (the only caller allowed to mint) is a
single-use seed: after the bootstrapper begins its final self-revoke, the
current-domain slot is flipped to the boot domain and the `PrincipalContext`
is marked *consumed*. Any subsequent `mint` call receives
`ObjError::MintAuthorityGone`. The self-revoke is the *last* statement of the
bootstrapper's body; nothing runs after it (§5.5). Interrupts are disabled
during the self-revoke step (they are not enabled until after
`enable_interrupts()`, which already runs after init completes).

**Runtime assertion (development canary).** The transition from static seed
to heap-born node is a fragile boundary, and a reentrant `mint` after
endowment finalization is exactly the kind of race that survives inspection
and dies only under an interrupt. So the mint guard carries a runtime
assertion, in addition to the soft `ObjError` path: once the boot domain's
endowment is finalized (the last `endow` into the boot table has completed),
the guard is frozen and the `PrincipalContext` is destroyed as a value.
Any subsequent access — including a reentrant or ISR-path access that never
checks the returned `Result` — trips an explicit
`panic!("mint after endowment finalization")` in development. The `Result`
path is the *authorized* failure (any well-behaved caller sees
`ObjError::MintAuthorityGone`); the assertion is the *unauthorized* one: it
turns a subtle race into a loud, immediate failure during development, at the
cost of a single flag-check on the guarded path. It is cheap enough to keep
in release.

## 8.3 Hot-plug (a new device appears after boot)

**Risk.** A device that did not exist at boot appears (USB unplug/replug,
PCI surprise-add). Minting is Principal-only and post-boot closed (§5.5).
Does the new device starve for capabilities?

**Principle.** Subtree-scoped capabilities subsume the future (§2.4).
Hot-plug is materialization, not minting (§3.5, §7.11.6).

**Handling.** The controller family root's capability already covers every
present *and future* child. `bring_up` is a hook on the controller that
materializes the child node and returns its capability, derived from the
family root's subsumption. The xHCI controller already owns the slot/device
tree (`usb/xhci`); under the graph, "slot becomes alive" is the controller
materializing a child. No Principal, no mint. The child's rights inherit
monotonically from the family (§3.5, "rights inheritance").

## 8.4 Device removal / unplug

**Risk.** A device disappears (USB yanked, disk detached, PCI hot-remove).
Its capabilities are still in tables; its hooks would touch dead hardware.

**Principle.** Cascade revocation for family roots (§3.7.2); handlers hold
sessions that are severed when the device's subtree collapses.

**Handling.** The controller revokes the *device's* capability (it holds the
device's family root with `REVOKE`). The device's subtree cascades: every
capability reaching any descendant is marked `Revoked`, the descendants'
last-strong-ref is released, and they drop-death. Sessions (§3.6) that
subscribed to the device hold *derived* caps; those become `Revoked`, and
the session's own hooks fail with `ObjError::Disowned` (the standard "the
thing you're talking to is gone" error). Handlers that poll a removed device
learn via the revoked handle, not via touching dead MMIO.

## 8.5 Revocation races

**Risk.** A capability is checked (PERMIT passes) and then revoked a
microsecond later, mid-dispatch; or a family root is cascade-revoked while a
child is mid-call.

**Principle.** Permission is checked at the moment of dispatch; the check and
the call are atomic within the (non-preempting) dispatch step; revocation is
linearized against dispatch by the table lock.

**Handling.** In the single-address-space kernel, `resolve` (§7.5) acquires
the table's `IrqMutex` and performs `PERMIT` *and* the node reference fetch
under the same lock acquisition; the `Arc` keeps the node alive for the
duration of the call even if a concurrent revocation marks the handle
`Revoked`. So an in-flight call completes on a valid, alive node, and the
next call fails. The deny-list check reads the store's cascade/deny flag
under the same lock (§7.3). The observed race window is exactly "the call
that already started," which is the desired semantics — you do not yank
memory out from under a running hook.

## 8.6 Cascade revocation and in-flight calls

**Risk.** Revoking the PCI root severs a subtree while a driver is mid-write
to a disk under it.

**Principle.** Same as 8.5, at family scale.

**Handling.** The cascade marks every reachable descendant's handles
`Revoked` and releases strong refs. In-flight dispatch (holding an `Arc`) is
unaffected and completes; the next dispatch fails `Disowned`. The actual
hardware-teardown ordering is the *driver's* job: the controller's revoke is
cooperative, not a surprise free. A driver that must guarantee "no in-flight
I/O when I unplug" uses a `sync`/`quiesce` hook on its device family *before*
the root revoke, exactly as `unmount`/`sync_drive` does today
(`vfs/mod.rs:sync_drive`).

**Cascade latency (cost of atomicity).** Revocation is *atomic in semantics*
but *linear in scope*: severing a large subtree (a PCI root complex with
dozens of devices and their sessions) walks the store's cascade registry and
marks every descendant. If that walk held a single global lock, it would be a
latency spike that stalls all other dispatch. The design mitigates in three
layers, in order of preference:

1. **Per-root cascade state (§7.3)** — `cascade` records are keyed by family
   root, so a walk is scoped to one subtree, not the whole store. This is the
   already-committed decision and the first thing to measure.
2. **Lazy marking.** Most descendants need no eager bit-flip: a handle reads
   its *nearest live ancestor's deny flag* at dispatch (§8.5). Cascading a
   root is then close to O(1) — set the root's flag once — and individual
   children observe `Revoked` on their *next* dispatch. The eager walk is
   then only for *strong-ref release* (freeing memory), which can proceed
   after the flag is set, and that release is allowed to stay in-flight on a
   preempted path or a low-priority sweep.
3. **Sharded structure.** If measurement shows the eager release still
   contends, the store's `Mutex<HashMap>` cascade map becomes a set of
   shards — one per CPU or per family — so parallel cascade walks don't
   serialize on one lock. This is a P5 change behind the same store API.

The in-flight-call safety (§8.5) is unaffected by any of these: an in-flight
`Arc` completes; the *subsequent* call after a large cascade is the one that
may observe a momentarily higher latency while the eager release drains.
Any kernel in Shi's world does a latency budget for "worst-case cascade" as
part of P5 measurement, not a correctness matter.

## 8.7 Reference cycles (drop-death deadlock)

**Risk.** A node's interior contains a node that (via a returned capability)
points back; when the last external cap drops, the cycle keeps strong refs
alive and neither dies.

**Principle.** Capabilities are the only strong refs; cycles are broken by
construction because **edges are capabilities, and capabilities live in
tables, and a node never holds a strong ref to its own family root's cap in
a way that forms an unreachable cycle.** Where a cycle would form (e.g. a
session holding a cap to its creator, which holds a cap to the session), the
design forbids the self-referential *strong* edge: the session's up-edge to
its creator is the weak store record (§7.3), not a held capability.

**Handling.** Two structural rules prevent cycles from being fatal:
(a) interior child nodes do not hold strong caps to their family root — the
parent edge is a `Weak` store record, not a capability; (b) any node that
returns a capability to a node *that can reach it* (a session back-pointer)
returns a *weak or revoked-on-drop* handle. The net effect: the graph's
strong edges are acyclic (downward family edges + cross-family path edges
that do not point at an ancestor's table). Drop-death therefore terminates.
If a genuine user-level cycle is ever constructed, it leaks exactly one
node's worth of memory and is detectable by the store's record (the node
whose `weak` never fires) — a leak detector is part of `kerneldump graph`
(§7.13).

**Leaked cycle vs legitimate mutual reference.** The weak-parent rule kills
*family-tree* cycles, but *cross-family composition* (§3.6) can still build a
cycle: two derived nodes, one materialized by `A` holding a cap to `B`, and
`B`'s session holding a cap back to `A`'s derived node. That is a real
mutual reference, and it is *legal* — the session legitimately knows its
conversation partner. The leak detector must therefore distinguish two
classes, or it will cry wolf on every long-lived conversation:

- **Leaked cycle.** A set of nodes reachable from one another *only* through
  held capabilities, where no live root capability of any of them is reachable
  from a domain the projection can still see (i.e. the cycle is unreachable
  from the surface). Drop-death *should* have reclaimed it (I4). Verdict:
  leak, fix the edge construction.
- **Legitimate mutual reference.** A cycle reachable from the projection's
  surface — some live capability reaches one member from a live domain, so
  the pair is intentionally held. Verdict: healthy, suppress the report.

Operationally the detector walks from the projection's roots (the domains'
tables) and marks reachable nodes; any live node *not* marked is suspect, and
within that set the strongly-connected components whose members reference
only each other are the reported cycles. §7.13's snapshot already contains
everything needed (table handles + store records); the detector is a
post-processing pass over that snapshot, so it runs without perturbing the
live system.

## 8.8 The ObjectStore and weak refs

**Risk.** The store's records grow forever (leaked projection material), or
the store is used as a back-door namespace.

**Principle.** The store is weak and consultable only for projection and
cascade (§7.3, §2.8).

**Handling.** Records are retained for the boot (a bounded, boot-time period)
then pruned on a low-priority sweep, keeping only: (a) live nodes, (b) nodes
that are children of a currently-severed family (for forensic "what died with
that root" reports), (c) the Principal and any Principal-minted session
roots. The store is *never* consulted by any access path; the only readers
are `kerneldump graph`, the cascade machinery, and the leak detector. A
`store:write` hook exists but is gated by the store cap — which only the
boot domain holds by default.

## 8.9 Mint during shutdown

**Risk.** During power-off, a driver, seeing its tree collapse, tries to mint
a replacement family root.

**Principle.** Mint is Principal-only; the Principal is quiescent; there is no
replacement path (the total case is §3.7.4).

**Handling.** `mint` fails with `MintAuthorityGone` throughout shutdown.
Drivers are expected to *tidy*, not *re-create*: quiesce (§8.6), revoke their
subtrees, release caps. The shutdown sequence is the reverse of the boot
sequence: tear down controllers (cascade), drop the boot domain's endowment,
and let the projection collapse. `PlatformControl.halt()` (§KernelServices)
is the final hook, reached via a capability the boot domain still holds.

## 8.10 Multi-principal / sessions

**Risk.** Two human users, or the "system" and a "user," need independent
authority, and one must not be able to reach the other's session.

**Principle.** Sessions are family roots minted by the Principal (§6.6); each
session is a separate subtree; projections are disjoint by construction.

**Handling.** Login: the Principal mints a session family root and endows it
(§3.4.2). Logout: the Principal (or the session's owner with `REVOKE`)
revokes the session root — cascade collapses the session's subtree and all
its caps. Sessions never share tables; a capability must be explicitly
delegated to cross sessions, and delegation is a capability operation
(§3.4.3). There is no ambient "switch to user X" primitive.

## 8.11 Path equivalence and labeling

**Risk.** Two different capabilities reach the same node; does the system
deduplicate? Is a node's identity its label?

**Principle.** Capabilities are per-domain; the *node* is shared; labels are
surface data (§4.1 Property 3), never identity.

**Handling.** Two domains may hold different caps naming the same node —
that is *fine* and *normal* (the boot domain and a driver both hold the DMA
node, with different rights). The node's `ObjId` (§2.1) is its identity; the
label ("dma0", "song.mp3") is per-surface and may differ per holder. A
"path" string is never a key (§7.12.3): the shell renders labels; the kernel
sees caps. Equality of paths is a shell-side rendering concern, not a kernel
semantic.

## 8.12 Memory exhaustion

**Risk.** Minting, materialization, or a session allocates and the heap is
full.

**Principle.** Allocation is a heap-node hook (§7.10.2); failure is an
ordinary hook error, not a panic, except in the bootstrap seed (§5.7).

**Handling.** All `ObjError::OutOfMemory` results propagate to the caller as
errors. The boot domain, on an irrecoverable OOM during bring-up, falls back
to the existing abort behavior (heap and BitmapAllocator already abort on
exhaustion). The graph adds one discipline: *no operation may allocate while
holding a table lock*, so a failed allocation never wedges the lock (§8.5).
The `FdTable` free-list pattern (`vfs/fdtable.rs`) is preserved: tables grow
in bounded chunks and reuse freed slots, so cap exhaustion is amortized.

## 8.13 Interrupt reentrancy and the trap path

**Risk.** An interrupt fires while a hook is mid-dispatch; the handler calls
hooks on the *same* table; the table lock re-enters and deadlocks, or the
handler does something the interrupted domain couldn't.

**Principle.** IRQ dispatch runs in the interrupted domain's context (§6.4)
and the `IrqMutex` is interrupt-safe by design (`vfs/irq.rs` is the model).

**Handling.** All table accesses use `IrqMutex` (the codebase's IRQ-aware
spin mutex), so a handler can safely re-enter the table. The handler's caps
are *exactly* the interrupted domain's caps — it cannot do more than that
domain could (§6.4). If the interrupted domain did not hold a heap cap, the
handler cannot allocate; this is a feature (bounded, honest ISR power), and
the driver contract documents it. The one sanctioned exception: the `Irq`
node's own `deliver` runs on the boot domain's *temporary* context during
vector bring-up, before domains are settled (§6.2), and this is explicitly
not ambient (it is the bootstrapper's endowed dispatch, §5.4).

The `IrqMutex` discipline is *necessary but not sufficient*: the deadlock
that an ISR can still hit is not the table lock but *any* lock held by the
preempted thread while it is blocked. So the `isr_safe` contract annotation
(§6.4) is the real guard: a `ThreadOnly` hook may never appear on an `Irq`
node's handler list, and an `IsrSafe` hook body is constrained to spin-locks
that cannot be held by a blocked, preempted thread in the same domain. This
is a contract-level rule even though enforcement is deferred to P6; the
projection tool (§7.13) can already flag a `ThreadOnly` hook registered on an
`Irq` node as a contract violation from the snapshot.

## 8.14 Single-address-space kernel vs domains

**Risk.** Without paged user isolation, a domain "in" the kernel could, in
principle, write another domain's table by raw pointer arithmetic.

**Principle.** Separation is *structural* where the hardware gives it (user
vs kernel later) and *conventional* now (kernel code accesses tables only
through the API). Unforgeability (§3.2, §8.16) rests on the invariant that
only the kernel writes tables — which holds in a single address space
because the *only* writer is the capability API.

**Handling.** P1–P5 accept the convention: kernel code is trusted *as a
whole* to use the table API (the same trust the current kernel places in
`IrqMutex`). Real adversarial isolation arrives with P6's paged user
boundary (§7.14). The document does not pretend otherwise; the graph model is
*the same* at both trust levels, which is the point — the mechanism doesn't
change when the hardware boundary appears.

## 8.15 riscv64 vs x86_64

**Risk.** The two arch targets drift; the graph work lands only on x86_64.

**Principle.** The model is arch-neutral; build gates enforce both (§7.15).

**Handling.** `obj/` contains no `#[cfg(target_arch)]` — it is pure
abstraction. The arch-specific nodes (`Cpu`, `Irq`, `AddressSpace`,
`PhysMem`) are thin adapters over `kernel/src/services/x86_64/*` and
`riscv64/*`. The riscv64 build (which today has unwired timer and stub PCI
paths — see `invariants-23-services.md` SVC-D001/riscv caveats) compiles
against the *same* `obj/` core; its stubs simply materialize fewer nodes at
boot. The riscv64 target stays green from the first phase (§7.15).

## 8.16 Capability forgery / ID guessing

**Risk.** A caller fabricates a `CapId` and gains access to a node it never
held.

**Principle.** `CapId`s are unforgeable because only the kernel writes
tables (§3.2, §8.14); a number is not a capability — a *table slot* is.

**Handling.** In the single-address-space kernel, the table is kernel-owned
and only the `CapabilityTable` API mutates it; `resolve` on a slot that was
never written (or was freed) returns `BadFileDescriptor`-equivalent
(`ObjError::NoSuchCap`). At the user boundary (P6), `CapId`s cross only in
*kernel-inserted* reply frames (§7.14), so a user process can never invent a
slot. `ObjId`s are never exposed to user code as access keys (§2.1). The
free-list reuse pattern (`vfs/fdtable.rs`) means stale ids from freed slots
either hit an empty slot (`NoSuchCap`) or a *live, still-valid* handle whose
rights were granted — there is no confusion between a reused id and a
forgery.

## 8.17 Denial-of-service via capability exhaustion

**Risk.** A domain accumulates/holds so many capabilities that tables or the store
grow without bound, or it spams `traverse` to force unbounded materialization.

**Principle.** Capability growth is a *resource* like memory (§8.12);
materialization is gated by the family root's rights and the store's sweep.

**Handling.** Per-domain table size is bounded (configurable cap; the
free-list reuses slots §8.12). The store's sweep (§8.8) bounds its records.
`traverse`/`readdir` are ordinary hooks: they cost what the underlying
filesystem costs today and fail with `ObjError::OutOfMemory` on pressure.
The boot domain is endowed with a finite table; a driver that exhausts its
endowment simply cannot grow — a *feature* (its blast radius is bounded by
construction). Session roots (§8.10) are capped per Principal.

## 8.18 Contract versioning

**Risk.** A filesystem changes its hook signature; older holders of the
capability now invoke a contract that no longer matches; the registry rejects
the new contract and the system breaks.

**Principle.** `ContractId` is content-addressed (§4.3, §7.2.4); a changed
signature is a *different* contract.

**Handling.** A node that changes semantics adopts a *new* `ContractId` (e.g.
`fs:fat32:v2`) and implements both v1 and v2 while old holders exist; the
registry rejects a collision only if two distinct tuples hash to the same id
(which is the loud failure mode, §7.8). Migration is by *capability*: old
holders keep calling the v1 contract (served by an adapter node) until their
caps are replaced. This is the contract analog of API versioning, and it
falls out of content-addressing rather than requiring a separate mechanism.

**Adapter materialization (who builds and who holds).** The adapter node is
not magic: it is a normal node, and its capabilities come from the same
attunement rules as everything else.

- **Who builds it.** The family root that *owns the implementation*. When
  `fs:fat32` is rewritten to v2, the `fs:fat32` family root materializes an
  adapter child that *implements the v1 ContractId* by translating to v2
  behind the scenes. The adapter is part of the fat32 family; it is not a
  standalone node dropped into the registry.
- **Who holds it.** The root endows the adapter's capability into the domains
  that still hold stale v1 caps — a migration endowment, no different in
  kind from any other endowment (§3.4.2). Old holders see their v1 call
  "still work" because the v1 id is served by the adapter; the caller never
  learns a rewrite happened.
- **Reaping.** When the last v1 holder's cap is replaced (or revoked), the
  adapter has no live handles, drop-death reclaims it, and the family stops
  implementing v1. The registry's `ContractId → implementations` map then
  drops the v1 entry. Migration is complete with no global "update all
  tables" pass — it is distributed as normal capability flow.

During P4, when `InodeOps` becomes the `fs:*` contracts, *every* existing
filesystem driver needs an adapter or a rewrite. The `ContractRegistry` must
therefore support **multiple versions of a contract registered under
distinct `ContractId`s simultaneously** (v1 and v2 coexist; both are
resolveable; a node may implement both). This is a registry-level requirement
from day one, not a retrofit.

**Stale-reference reporting.** The projection tool (§7.13) is the debugging
lens for migration: its snapshot already lists, per node, which contracts it
implements and per domain, which `ContractId`s its caps name. The `graph
stale-contracts` subcommand filters that snapshot to *domains whose caps
name a ContractId no longer implemented by any node* — the "dead contract"
report. A driver team migrating a filesystem can run it to find every holder
that still needs an adapter endowment. Because it reads the snapshot, it is
safe to run against a live system.

## 8.19 Debug and projection leakage

**Risk.** `kerneldump graph` leaks another domain's capabilities, surfaces,
or revoked handles to a caller who should not see them.

**Principle.** Projection is gated and unaddressable (§2.8, §7.13).

**Handling.** The projection hook is a contract (`kerneldump:project`) that
only the boot domain holds by default. The projection *report* includes
every node's record — but it is a snapshot, not access; knowing a node's
`ObjId` and its labels confers nothing (§2.1). If a domain without the
project cap must introspect its *own* world, it uses its own table's
surfaces, never the store. The debugger in the field is expected to hold the
boot domain's cap, exactly as a root debugger holds the kernel's privileges
today.

## 8.20 Power-off and crash

**Risk.** Nothing cleans up; or the "graceful" path is ambient.

**Principle.** Power-off is the total revocation (§3.7.4, §5.8); the graph's
existence condition is the machine being on (§2.2).

**Handling.** Graceful shutdown: a `power_off` hook on the platform node,
reached via a capability the boot domain holds; the sequence reverses boot
(§8.9) and the projection collapses. Crash: no cleanup is required or
attempted — the graph's lifetime is bounded by the machine, and the machine
stopped; the store's records, if the dump is taken, are forensically
meaningful (§7.13) because they describe a consistent historical state. A
fault dump still captures the full projection (§7.13) alongside the existing
`dump_full_fault` registers/disassembly.

## 8.21 Deadlock between nested domains

**Risk.** Domain A calls a hook on Domain B's node; B's hook (in B's context)
calls back into A's node; both tables lock and wait.

**Principle.** Table locks are short (§7.5: one mutex, an index, a bit-test,
a reference); hook bodies never hold the table lock across a nested call
(§8.12 discipline).

**Handling.** `resolve` releases the table lock *before* invoking the hook
(§7.9: `resolve` returns the `Arc`; `dispatch` runs unlocked). Therefore a
hook body can safely call back into any table, including the caller's,
without self-deadlock. The only lock held across dispatch is the store's
cascade flag, which is read-only during dispatch (§8.5). Cross-domain
reentrancy is therefore safe by the same discipline that makes the current
kernel's `IrqMutex` usage safe — short critical sections, no lock nesting.

## 8.22 A node whose interior references itself

**Risk.** A filesystem node contains a hardlink to its own ancestor; the
interior graph cycles.

**Principle.** Strong edges are acyclic by construction (§8.7): child caps do
not point up to their family root; the parent edge is a weak store record.

**Handling.** A self-loop in *labels* (a directory containing a link that
renders as its own path) is fine — it is surface data, no strong cap. A
self-loop in *capabilities* is impossible because traversal returns child
caps *within* the interior (derivations of the family root's subsumption),
and a child cap's parent edge points down, never up. The store's leak
detector (§8.7) double-checks: any node whose `weak` never fires while no
live cap reaches it is reported.

## 8.23 Two family roots subsuming the same node

**Risk.** A node is claimed by two families; cascade from one revokes the
other's reach; identity becomes ambiguous.

**Principle.** Every node has exactly one parent edge (§2.1); a node belongs
to exactly one family (§2.4).

**Handling.** A node is *created* inside one family (its materializing
controller) and its `parent` is fixed at materialization (§7.3). If two
controllers genuinely share a device (a PCI device behind two bridges — an
illusion; it is one device), the *device node* is one node with one parent;
the second controller holds a capability to it *by delegation* (§3.4.3), not
by subsumption. Cascade from the first controller's root severs the shared
node; the second controller's delegated handle becomes `Revoked`. Ownership
is unambiguous even when sharing is allowed.

## 8.24 Sending a capability across domains (delegation)

**Risk.** A capability leaves one domain's table and arrives in another's;
the sender must not be able to keep using it, and the receiver must not be
able to amplify it.

**Principle.** Delegation is a capability operation (§3.4.3); rights are
monotone (§3.2, §7.2.2).

**Handling.** In the P6-B build, cross-domain delegation is
`CapabilityTable::delegate(&target, id)`: it clones the source handle into
the receiver's table under a fresh `CapId` with **identical rights** (the
sender keeps theirs — a copy, not a transfer; a transfer primitive is a
future extension). The clone carries the handle's whole state verbatim, so a
`Revoked`/deny-listed handle cannot be smuggled back to life, and a
nonexistent source id fails `NoSuchCap`. The capability-mediated path is the
`infra:table` node's `delegate` hook (boot's table cap carries `INVOKE` +
`QUERY` + `REVOKE`), which resolves the target domain by id through
`domain::find_domain` (unknown id → `NoSuchCap`) and then clones the named
source handle. There is no operation that increases rights, so a receiver
can never amplify. This is `dup`/`dup2` generalized across domains — the same
mechanism the `FdTable` already provides within one table. (Note: this is the
delegation *of capabilities between domains*; it is distinct from the
Principal's one-time delegation of *mint authority* to the bootstrapper at
boot, §3.4.1.)

## 8.25 CapId / ObjId exhaustion

**Risk.** Billions of attunements and traversals exhaust a 64-bit counter; wraparound
aliases old handles.

**Principle.** Ids are monotone; wraparound is prevented, not handled.

**Handling.** `next_id` counters saturate: on reaching `u64::MAX` the mint and
the store's allocator return `ObjError::Exhausted` (which is effectively the
OOM of §8.12). No wraparound is possible by construction. This is a
theoretical limit (a million caps per second for ~580,000 years), not an
operational one.

## 8.26 Does the kernel have any private nodes?

**Risk.** Some kernel-internal state (the heap arena headers, the IDT, the
page tables' root frame) is reachable by *no* capability, implying the kernel
has ambient backstage.

**Principle.** Every protected resource is a node (§7.10); but the *kernel as
the boot domain* is *endowed* with every one of its own primitive nodes (§5.4,
§7.7.1). "Kernel-private" and "boot-domain-owned" are the same set.

**Handling.** The boot domain's endowment literally contains `PhysMem`,
`Heap`, `AddressSpace`, `Cpu`, `Irq`, the store, and the registry. A driver
domain is endowed with only its controllers' family roots — it has no path
to the heap unless the boot domain delegated one (it hasn't, by default).
The kernel's "private" state is not ambient; it is *owned* by the boot
domain through capabilities it happens to hold, and the projection (§7.13)
can show those edges. Nothing exists that no capability reaches; if it
existed it would be a leak, and the store would show it (or it is a static
atom like the IDT, whose node is a root-level `Cpu`/arch atom owned at
bootstrap).

## 8.27 Surface reads during revocation

**Risk.** A `QUERY` races a revoke: the caller reads a surface a nanosecond
after the node was revoked.

**Principle.** Surfaces are side-effect-free reads (§4.1); a surface read on
a `Revoked`/`Zombie` handle returns a *stale-safe* error, not garbage.

**Handling.** `QUERY` follows the same PERMIT shape (§7.5): a `Revoked`
handle fails `ObjError::Disowned` *before* the getter runs. A surface read
on a *Live* handle always succeeds against a node the caller's `Arc` keeps
alive for the duration of the read (§8.5). There is no window where a caller
reads freed memory: the `Arc` is acquired under the table lock, and the node
cannot drop while the read runs.

## 8.28 A hook drops the last capability to itself

**Risk.** Inside `dispatch`, the node's implementation calls `revoke`/`drop`
on the very handle the caller used to reach it; the `Arc` count hits zero
mid-call.

**Principle.** In-flight dispatch holds its own `Arc` (§8.5, §8.21): the call
already started and cannot be yanked.

**Handling.** `resolve` returns an `Arc` that the dispatch frame keeps alive
until the reply is returned (§7.9). Self-revoke during dispatch marks the
handle `Revoked` and drops the table slot, but the dispatch's `Arc` holds the
node until the hook returns — then the node drops, if that was truly the last
ref. The reply is still delivered (it was already computed); the next call
fails. No use-after-free, no torn call.

---

# 9. Formal Semantics

## 9.1 The Universe

Let:

- `N` = set of nodes (objects).
- `D` = set of domains.
- `C` = set of capabilities. `C(d) ⊆ C` is domain `d`'s table.
- `K = {mint, endow, attune, materialize, compose, revoke, invoke}` — the
  operation vocabulary, of which `mint` has exactly one permitted caller.
- `R` = the rights set `{QUERY, INVOKE, TRAVERSE, MINT, REVOKE}` plus per-
  contract rights.
- `P` = the Principal. `P ∈ N`; `P` has no parent edge.

Every capability `c ∈ C` is `c = (id, node(c), rights(c), state(c))` where
`node : C → N`, `rights : C → 2^R`, `state : C → {Live, Revoked, Zombie}`.

The **reachability** of a node `n` at time `t` is the set of domains whose
tables contain a `Live` capability naming `n`:

```
reach(n) = { d ∈ D : ∃ c ∈ C(d). node(c) = n ∧ state(c) = Live }
```

The **projection** of a domain `d` is:

```
proj(d) = { n ∈ N : n is reachable from a cap in C(d) by descending
            node(c)'s interior graph, and the descent used only Live caps }
```

## 9.2 Operational Rules

**R1 (mint).** `mint(P, kind, R0)`: creates `n = new_node(kind)`, issues
`c = (id, n, R0, Live)` — the Principal's act, performed at boot through the
rooted bootstrapper. Guard: `R0 ⊆ MINT-allowed` and the caller is the
Principal (or, during boot, the bootstrapper exercising the Principal's
authority before its self-revoke) (§7.6). No other caller ever passes.

**R2 (endow, an attunement).** `endow(d, [c_1..c_k])`: inserts the listed
capabilities into `C(d)`. Used at domain creation and session start. The
listed capabilities are always subsets of the endower's own — endowment is
attunement, never a mint.

**R3 (attune, code name derive).** `attune(c, keep)`: creates `c' = (id',
node(c), rights(c) ∩ keep, Live)`. Invariant: `rights(c') ⊆ rights(c)`
(monotone, §7.2.2). This is the entire content of "make it more precise,
tinier."

**R4 (materialize).** `materialize(c_root, name)`: where `c_root` names a
family root `r` and `TRAVERSE ∈ rights(c_root)`, creates `n'` as a child of
`r` and returns `c' = (id', n', rights' ⊆ rights(c_root)`, Live)`. Rights
inherit down the family (§3.5).

**R5 (compose).** Two domains `d1`, `d2` with caps `c1, c2` invoke hooks on
each other's nodes; the interaction materializes `n''` whose caps are derived
from `c1, c2` only. `n''` belongs to a family among `{d1,d2}`; never a new
root (mint is the only root creator).

**R6 (invoke).** `invoke(d, c, contract, hook, args)` succeeds iff:
`state(c)=Live ∧ INVOKE ∈ rights(c) ∧ hook ∈ contract ∧ node(c) implements
contract ∧ (rights(c).contract = ∅ ∨ node(c).hook_contract_right(contract,
hook) ∈ rights(c).contract) ∧ not deny-list-revoked`. On success the hook runs
and returns a reply (which may carry caps inserted into `C(d)`).

**R7 (revoke, drop-death).** Removing `c` from `C(d)` decrements
`node(c)`'s refcount. If the count reaches zero, `node(c)` dies: its
resources are freed and its interior drops recursively (their refcounts
decrement in turn).

**R8 (revoke, cascade).** `revoke(c_root)` where `REVOKE ∈ rights(c_root)`:
every `c'` with `node(c')` reachable from `node(c_root)` by descendant edges
has `state(c') := Revoked` and its refcount released; the subtree dies by
R7 where no other caps reach it.

**R9 (revoke, deny-list).** For `Revocable` nodes only: `revoke_deny(n)` sets
`deny(n) := true`; future `R6` against `n` fails even if caps remain
(`Zombie`).

## 9.3 Invariants

These are the numbered invariants the implementation must maintain at every
instant. They are the executable contract of this document.

- **I1 (no ambient authority).** Every access to a protected resource is the
  result of `R6` on a `Live` capability. There is no code path that reaches
  a node except through a domain table.
- **I2 (mint monopoly).** A new family root is created only by `R1`. At all
  times `{ c ∈ C : MINT ∈ rights(c) }` has cardinality ≤ 1, and that cap is
  the Principal's — the Principal exercises it through the bootstrapper at
  boot, and the bootstrapper self-revokes at boot end, after which the set
  of nodes that could mint is empty. *(P6-B implementation: no capability
  carries a `MINT` bit at all — `PRIM_RIGHTS = INVOKE|QUERY|TRAVERSE`. The
  guard is `mint_node`'s `PrincipalContext` plus the `MINT_FROZEN` single-shot
  (`finalize_mint` as `init()`'s last statement), after which every
  `mint_node` returns `MintAuthorityGone`; §3.4, §7.6.)*
- **I3 (monotone attunement).** For any attunement chain
  `c1 → c2 → … → ck`, `rights(c1) ⊇ rights(c2) ⊇ … ⊇ rights(ck)`. No
  capability ever gains a right; every attuned capability is more precise
  and tinier than its parent.
- **I4 (lifetime = reachability).** `n` is allocated iff `reach(n) ≠ ∅` or
  `n` is the Principal or a boot-era seed node owned by the boot domain.
  `reach(n) = ∅` implies `n`'s resources are reclaimed (R7).
- **I5 (one parent).** Every `n ≠ P` has exactly one parent edge; `P` has
  none.
- **I6 (subsumption consistency).** If `c` names family root `r`, then any
  child materialized under `r` (R4) has rights ⊆ `rights(c)`, and `r`'s
  parent edge is `r`'s own materializer's edge. A node is in exactly one
  family (§8.23).
- **I7 (store weakness).** The ObjectStore holds only weak references; it
  never affects `reach`. Projection is read-only and gated (§7.13).
- **I8 (fast-path bound).** The `PERMIT` check of `R6` is O(1): a constant
  number of word comparisons (§7.5, §9.4).
- **I9 (dispatch safety).** No table lock is held across a hook body
  (§8.21); in-flight dispatch holds an `Arc` that prevents drop-death
  reclamation until the reply returns (§8.28).
- **I10 (contract identity).** `ContractId` is content-addressed; two
  distinct `(name, surface, hooks)` tuples never share an id (§7.2.4,
  §8.18).
- **I12 (per-hook contract-right enforcement).** A hook is invoked only if
  the handle's contract-right mask contains the hook's required right
  (`node.hook_contract_right(contract, hook)`); an `empty()` mask is the
  transitional "unrestricted" state (I13) — §3.3, §7.5.
- **I13 (transitional empty contract mask).** An `empty()` contract-right
  mask is read as "not yet narrowed" and satisfies any per-hook requirement;
  monotonicity prevents a non-empty-attuned cap from returning to
  unrestricted (§3.3).
- **I14 (QUERY-gated surface reads).** Reading a node's surface requires the
  universal `QUERY` right and nothing else — no `INVOKE`, no contract
  membership; the `SURFACE_READ` hook is handled centrally in `invoke` via
  `resolve_for_query` (§4.1).
- **I15 (delegation never amplifies).** Cross-domain delegation is a
  rights-preserving clone; `revoke_cascade` requires the universal `REVOKE`
  right (§8.24).

## 9.4 The Fast-Path Bound (I8)

Let `P(n)` be the cost of `PERMIT` on a table of size `n`. The operation is:

1. acquire `IrqMutex` (spin, uncontended: a few ns),
2. index `slots[id]` (O(1) array),
3. test `state == Live` (1 compare),
4. test `INVOKE ∈ rights` (1 bit-test),
5. test contract membership (1 hash-table probe on a small, frozen set),
6. test per-hook contract right (1 bit-test of the handle's contract mask
   against `node.hook_contract_right(contract, hook)`; an `empty()` mask is
   the transitional "unrestricted" state and passes — §3.3),
7. test deny-flag (1 hash-set probe, O(1)).

All steps are independent of `n` (the contract-membership table is constant
size for a given node's advertised contracts; the slot index is direct).
Hence `P(n) = O(1)`. The subsequent dispatch is a `dyn` virtual call — the
same cost the kernel already pays for `InodeOps`/`BlockDevice`. The graph
therefore adds a *constant* per-operation overhead on the hot path, no
allocation, no serialization.

**The O(1) claim rests on one structural fact.** Step 5 — the
contract-membership check — must be a hash probe on a *frozen set*, not a
linear scan. `node.implements(contract)` must be, concretely, a probe into a
set that is built once when the node registers and never mutates while the
node is live (a contract is content-addressed and immutable by definition,
§4.3, so the set is naturally frozen). A node's contract set is small
(typically 1–3: its family contract, an optional interface contract, an
optional adapter contract), so even a linear scan would be a handful of
comparisons — but the invariant is stated so the hot path never turns into a
registry-wide search. If a node should ever implement many contracts
(an aggregator), the same frozen-set rule holds; the implementation may
switch to a perfect hash or a tiny Bloom filter over the frozen set, but the
*semantics* — probe the node's own immutable set, never a global registry —
is the load-bearing part. The ContractRegistry (§7.8) is consulted only at
*registration* and *projection* time, never on the dispatch path.

## 9.5 Theorems (with proof sketches)

**T1 — No node can mint except the Principal.** *Proof.* New roots require
`R1`; `R1`'s guard permits only the Principal (acting, at boot, through its
rooted bootstrapper) (I2). Materialize (R4), compose (R5), endow (R2), and
attune (R3) create children of existing families or derived nodes from
existing caps — they never issue a new root. After the bootstrapper's
self-revoke there is, by I2, no capability anywhere that could mint, so no
node other than the Principal *can* mint even in principle. ∎

**T2 — Capabilities are unforgeable.** *Proof.* A capability is a table
slot; only the capability API writes table slots (§7.4, §8.16); a node's
`ObjId` is not an access key (§2.1); `CapId`s are issued by the kernel's
capability machinery and never derivable from node state. In a single
address space, only kernel code can call the table API (structural trust,
§8.14); at the user boundary the API is the syscall entry (P6), which is
kernel-exclusive. A number is not a capability; a table slot is. ∎

**T3 — Lifetime equals reachability.** *Proof.* Nodes are `Arc`-held only by
capabilities (I4; the store is weak, I7). Drop-death (R7) fires exactly when
the last ref drops, which is exactly when `reach = ∅`; cascade (R8) and
deny-list (R9) either release refs (drop-death) or deactivate without
freeing (Zombie). No path frees a reachable node, and no unreachable node
survives. ∎

**T4 — The projection claim.** *Proof.* By I1, access requires a `Live` cap.
By I2–I3, all caps descend from Principal-minted roots and never gain
rights. By I4, a node exists only while reachable. Hence a domain's
perceptible world is exactly `proj(d)` (§9.1); the store (I7) is a gated,
read-only, weak report. No domain can enumerate beyond `proj(d)`, and the
full graph is never addressable. ∎

**T5 — The hot-plug non-problem.** *Proof.* For a family root `r` granted at
boot, every child ever materialized under `r` (R4) is subsumed by `r`'s
cap (I6). A device appearing after boot is such a child; it requires no
mint (T1). ∎

**T6 — Revocation terminates.** *Proof.* R7 decreases total refcount; R8
marks and releases in bounded per-descendant steps; R9 is a single flag set.
Strong-edge acyclicity (§8.7) ensures R7/R8 propagate finitely (no cycles
re-increment). The total case (power-off) is a single external act. ∎

---

# 10. Migration Roadmap

**Status legend.** Phases below reflect the current tree (P6-B). The
capability-system upgrade landed in the `obj/` crate as planned; the "stub"
gates listed under P1–P3 were design-era placeholders and are superseded by
the implemented code (see the invariants document §7.6, §7.8 and the P6-B
note).

| Phase | Status |
|---|---|
| P0 — The Spec | **Done** (this document + `invariants-26-objects.md`) |
| P1 — Seed + Domains | **Done** (`obj/{mod,rights,cap_handle,table,domain,store,contract,surface,hook,mint}.rs`) |
| P2 — Trinity Core | **Done** (PERMIT fast path, ContractRegistry, store/registry/tables as nodes) |
| P3 — Physical World as Nodes | **Done** (physmem/heap/addrspace/cpu/irq nodes; `REGION_POOL_CAPACITY = 64`) |
| P4 — Device/Service Nodes + Capability VFS | **Done** (`obj/{devices,fs}.rs`; tmpfs + ESP mounts) |
| P5 — Revocation Modes + Projection | **Done** (`obj/revocation.rs`, `kerneldump/{graph,leak}.rs`) |
| P6-A — Paged Domain Isolation | **Done** (`obj/paged_isolation.rs`) |
| P6-B — Capability-system upgrade | **Done** (per-hook contract rights, QUERY surface reads, delegation, real mem:region/PCI/fs nodes) |
| P6 — User Boundary + Sessions | **Open** (see below) |

## Phase P0 — The Spec *(implemented)*

**Deliverable.** This document + `Invariants/invariants-26-objects.md` (the
numbered I1–I10 distilled into the repo's invariants format).

**Gate.** Document review. No code changes. The repo remains at `4624b92`.

## Phase P1 — Seed + Domains (the foundation) *(implemented)*

**Scope.** The single most invasive phase (§6, §7.2–7.8).

- Create `kernel/src/obj/` with `mod.rs`, `rights.rs`, `cap_handle.rs`,
  `table.rs`, `domain.rs`, `store.rs`, `contract.rs`, `surface.rs`,
  `hook.rs`, `mint.rs`.
- Generalize `vfs/fdtable.rs` into `obj::table::CapabilityTable` (slot +
  rights + state; `dup_limited`).
- Add the `Domain` type + per-CPU current-domain slot (§6.3).
- Principal node (ObjId 0) + bootstrapper with delegated `MINT`; the
  `PrincipalContext` seed (§7.6).
- Replace the ambient `kernel_services()` global: delete `set_global`,
  turn `KernelServices` into the boot domain's endowment; convert the 10
  call sites (§7.7.1) to `domain.resolve(...)`.
- Establish the first driver domain (the USB/device bring-up path) with its
  own disjoint table (§6.2) — the separation proof.
- The Principal (acting through the bootstrapper) mints the primitive nodes
  (PhysMem, Heap, AddressSpace, Cpu, Irq, store, registry) as family roots;
  the bootstrapper self-revokes (drop-death) at end of init.

**Incremental conversion of the 10 call sites (do not do it in one commit).**
P1 is the highest-risk phase, and the call-site conversion is its sharpest
edge. Convert the ambient `kernel_services()` sites one *service* at a time,
in dependency order, with a separation test after each:

1. **DMA allocator** (used by both USB and AHCI) — the first conversion.
   Every driver domain gets a `dma` cap through endowment; nothing else may
   reach the DMA node.
2. **Serial** — boot log output moves to the boot domain's serial cap.
3. **PCI enumeration** — the PCI forest becomes a node family; the
   enumeration walks it via `TRAVERSE`.

Each conversion is committed with a **separation test** that asserts the
negation: a driver domain without the endowed capability *cannot* reach the
service through any residual global — `kernel_services()` has no second
entry point, the service node has no un-guarded static, and a `resolve` with
no matching cap returns `ObjError::NoSuchCapability`. The separation proof
(§6.2) is not optional; it is the validation that the model works. If the
USB driver domain can still reach the heap via some residual global, the
foundation is compromised and the phase has not finished.

**Gate.** Both targets build (§7.15); x86_64 boots; serial log shows the
bootstrap self-revoke message. `kerneldump graph` is stubbed (returns the
node census only). *(Implemented — the full walker landed in P5 and the
P1-era stub is gone; see §7.13.)*

## Phase P2 — Trinity Core *(implemented)*

**Scope.** §4.4, §7.2, §7.8.

- Complete `Obj` trait + `dispatch`; hook `PERMIT` fast path (I8).
- Content-addressed `ContractRegistry` (I10); `register`/`lookup`.
- Store, registry, and tables exposed as nodes (§7.8); `invoke` entry
  (§7.9).
- Rework `services/capability.rs` into the `Rights`/`CapHandle` seed.

**Gate.** Builds; the boot domain can `invoke` a hook on the PhysMem node
through its endowment and gets a frame back; ambient `kernel_services()`
fully gone.

## Phase P3 — Physical World as Nodes *(implemented)*

**Scope.** §7.10.

- Wrap `BitmapAllocator` → `PhysMem` node; `heap.rs` → `Heap` node; `vmm/*`
  → `AddressSpace` node; `smp/*` → `Cpu` nodes; `interrupts`/`msi` → `Irq`
  nodes.
- Route every alloc/map/IRQ-register through the appropriate node's hooks;
  TLB shootdown crosses `Cpu` edges (§7.10.3).

**Allocation-failure contract (bootstrap vs runtime).** The existing
abort-on-OOM behavior is *preserved for the bootstrap seed only*: between the
static seed and `heap::init()`, an allocation failure is a programming error
and aborts. Post-bootstrap, an allocation failure through a node's hook
returns `ObjError::OutOfMemory` and the caller handles it — the graph model
treats OOM as a *recoverable* error on every non-critical path, and OOM
propagates as a `Result`, never a panic. The two modes are structurally
distinct: the abort path is the seed's, the error path is every node hook's.

**No allocation inside the allocation hooks.** The `PhysMem` node's
`alloc_frames` hook must not itself require a heap allocation to *construct*
the returned `MemRegion` capability — that would be a circular dependency
(frames require frames require frames). The construction of a returned
capability is a fixed-size operation (a `CapHandle` is a word-sized triple);
it must be performed from a **pre-allocated pool of `MemRegion` wrapper
objects built during bootstrap** (or, where possible, direct
value-construction that never touches the heap). The rule, stated as an
invariant: *no hook that hands out memory may allocate to do so.* The
`kerneldump graph` census in the P3 gate confirms it by booting with a
deliberately tiny heap and observing that the failure surfaces as
`OutOfMemory` errors, not a panic, on the runtime path.

**Gate.** Builds; boots; the OS runs the existing init using only
capability-mediated allocation; `kerneldump graph` shows the physical layer
as a node family.

## Phase P4 — Device/Service Nodes + Pure-Capability VFS *(implemented)*

**Scope.** §7.11–7.12.

- Wrap `BlockDevice` (contract `block:storage`), `InodeOps` (contract
  `fs:*`), UInputL (input family), PCI forest, audio family.
- Controller family trees with cascade revocation; `traverse`/`readdir`
  capability-gated; `bring_up` materializes hot-plug children.
- Delete the string-path machinery (§7.12.1); `mount` returns a directory
  cap; shells render labels (§7.12.3).

**Ordering: prove navigation before deleting the compatibility layer.**
Deleting `vfs/path.rs` is the point of no return for the Unix-string layer —
nothing in the tree will accept `"/A/B"` after it. So the deletion is the
*last* step of P4, not the first, and it happens only after two things are
true:

1. **Shell-side label rendering works (§7.12.3).** The shell displays the
   *projection* of a capability (a rendered label), and the user navigates
   by `cd`/`ls`/`cat` operating *entirely through capability traversal* —
   `traverse` on the directory cap, never a string `resolve_path`. Basic
   navigation being capability-native is the observable proof.
2. **The mount hook is the key primitive.** `mount` returning a directory
   capability is the contract; it must be exercised against *both* real
   mounts — `tmpfs` (the A: volume) and the ESP (B:) — before the string
   layer dies, so that every mount the OS actually uses is capability-native
   first.

**readdir gating is the projection proof.** `traverse`/`readdir` must filter
children by the caller's rights (§7.11.2): a restricted domain sees *only*
the subset of files it could traverse to, not the full listing. This is the
observable demonstration of the projection claim (§2.8) — the domain's view
of a directory *is* its capability set, rendered as a listing — and it is
checked by the P4 gate's restricted-domain readdir test.

**Gate.** Builds; boots; tmpfs and ESP mount as *capability results*, not
drive letters; `readdir` on a restricted domain returns the subset its
rights allow; hot-plug materializes without minting.

## Phase P5 — Revocation Modes + Projection *(implemented)*

**Scope.** §3.7, §7.13.

- Drop-death, cascade, and deny-list fully wired (R7–R9); `kerneldump graph`
  full walker (nodes, edges, caps, rights, contracts, revocations).
- Leak detector (§8.7) as part of the projection tool.

**The projection tool is the verification oracle.** `kerneldump graph` is
not merely a debugger — it is the formal-verification oracle for the whole
model: separation, monotonicity, lifetime, and cascade are all *observable*
as snapshot invariants, and P5's gate is the first phase where they can be
checked mechanically.

- **Implement the leak detector early in P5, not at the end.** It is the
  enforcement of I4 (lifetime = reachability): any node whose store `Weak`
  never fires while no live capability reaches it is a violation. Build it as
  the snapshot post-process described in §8.7 (reachability from the
  projection's roots, then SCC analysis on the unreached residue), and **run
  it after every test-suite execution** as part of the CI-equivalent boot
  script — a leaked node fails the run.
- **Cascade correctness is a snapshot assertion.** The P5 gate already
  cascade-revokes a test driver domain; the assertion is that its *entire
  subtree* vanishes from the *next* projection snapshot. If the snapshot
  shows orphaned descendants — nodes still recorded in the store, still
  reachable from nothing, or handles still `Live` — the cascade machinery is
  broken. The gate's comparison (pre-cascade snapshot vs post-cascade
  snapshot) is the test; a diff showing only the intended subtree is the
  pass.
- **Cascade latency measurement.** The §8.6 latency budget is profiled here:
  a worst-case cascade (the PCI root complex with its dozen-plus devices) is
  timed end-to-end under a `kerneldump graph` trace, and the result decides
  whether the lazy-marking or sharded-store work (§8.6, layers 2–3) is
  pulled in. P5 is the phase where that measurement is *made*, not deferred.

**Gate.** Builds; a test driver domain can be cascade-revoked and its whole
subtree disappears from the next projection; deny-list revocation makes a
`Revocable` node `Zombie` while caps remain.

## Phase P6 — User Boundary + Sessions *(open — the only unimplemented phase)*

**Scope.** §7.14, §6.6.

- Syscall ABI = `invoke(table, cap, contract, hook, args)`; contract
  serialization for user-space nodes; kernel-exclusive table writes
  (§8.16).
- Sessions: Principal mints session family roots at login; logout cascades.

**Gate.** Builds; a user process calls a hook on a kernel node and gets a
capability back; two sessions cannot see each other's projections; logout
collapses the session subtree.

---

# 11. Glossary

| Term | Meaning |
|---|---|
| **Atom** | A node with an empty interior; the bottom of the recursion (§2.5) |
| **Boot domain** | The domain endowed by the bootstrapper with `KernelServices` + primitive nodes (§6.2) |
| **Bootstrapper** | The node the Principal roots at boot; sole mint delegate; self-revokes to drop-death (§2.3, §5) |
| **Capability** | An unforgeable `(id, node, rights, state)` triple held in a domain's table (§3) |
| **CapabilityTable** | Per-domain slot array holding capabilities; generalized from `FdTable` (§7.4) |
| **Cascade revocation** | Revoking a family root severs the whole subtree (§3.7.2) |
| **Composition** | Cross-family interaction creating a derived node from existing rights (§3.6) |
| **Contract / Path** | The trinity's third face; "if you do X I give/do Y"; the permitted journey (§4.3) |
| **Controller** | A family root that owns a bring-up tree (PCI, AHCI, xHCI, audio, input) (§2.4, §7.11) |
| **Delegation** | Sending a capability into another domain's table (§3.4.3, §8.24) |
| **Deny-list revocation** | `Revocable` objects only; a flag that deads all future use while caps remain (§3.7.3) |
| **Attunement** | The universal non-Principal capability operation: taking a capability one holds and making it more precise and tinier (shrinking rights, materializing a child, delegating a copy, invocation returns). Code name *derive* (§3.4) |
| **Derivation** | Code name for Attunement's base operation: duplicating a capability with a subset of rights (§3.4.3, §7.2.2) |
| **Domain** | An independent principal-in-the-small with its own capability table (§2.6, §6) |
| **Drop-death** | The default lifetime: an object dies when its last capability drops (§3.7.1) |
| **Endowment** | The concrete cap list a domain is born with; always an attunement of the endower's caps (§3.4.2) |
| **Family** | The subtree subsumed by a granted capability (§2.4) |
| **Family root** | The node a granted capability directly names; the tree's trunk (§2.4) |
| **Hook** | A node's active face; an invocable operation requiring rights (§4.2) |
| **Interior graph** | The graph inside a node; its children (§2.1) |
| **Materialization** | Realizing a pre-authorized child within a held family (§3.5) |
| **Mint** | Creating a new family root; Principal-only. After boot, no node can mint (§3.4.1) |
| **Mint guard** | The single audited entry point that enforces the monopoly (§7.6) |
| **Node / Object** | The atomic graph unit (§2.1) |
| **ObjId** | Kernel-issued node identity; confers nothing (§2.1) |
| **Path** | Synonym for Contract; also, the bounded descent used to reach a node (§2.7, §4.3) |
| **Principal** | The first node; sole mint authority; exists because the machine booted (§2.2) |
| **Projection** | The graph a domain can see; the closure of its caps (§2.8) |
| **Rights** | The permission bitset; monotone-decreasing along attunements (§3.3) |
| **Self-revoke** | The bootstrapper destroying the mint authority the Principal lent it, and dying (drop-death) (§5.5) |
| **Subsumption** | A cap over a node covering its interior graph transitively (§2.4) |
| **Surface** | A node's passive, typed, read-only data face (§4.1) |
| **Traverse** | Entering a node's interior and requesting an attuned child capability (§3.5, §7.11.2) |
| **Zombie** | A handle that exists and holds a ref but is inert (deny-list revoke) (§3.7.3) |

---

# 12. Design References and Prior Art

The model is not invented in a vacuum; it is a deliberate synthesis. Where a
concept below is referenced, this document intends its standard meaning.

- **Object-capability discipline.** The "no ambient authority" principle —
  mint by a single root, and everything else as attunement of what one holds —
  is the E/ocap discipline. *Robust Composition*, Mark S. Miller — the source
  for "no ambient authority," capability derivation, and the argument that
  capabilities compose safely.
- **Capability systems.** KeyKOS / EROS / CapROS — drop-death ("death by
  construction"), the root-object bootstrap, memory-as-objects, and the
  "you cannot force-kill a held object" property. EROS's capability
  revocation semantics inform R7–R9.
- **seL4 / CAmkES.** Capability tables per CNode, the boot thread giving
  away its caps ("shedding"), and interface-based component composition —
  the closest real-world echo of "hooks as contracts."
- **Fuchsia / Zircon.** Handles as capabilities; the syscall boundary as
  handle-mediated operations; the deny-list/revoke dialectic. Fuchsia is a
  handle-based system, not a full ocap system; BedrockOS takes the
  *discipline* from ocap, not Fuchsia's ambient-adjacent naming.
- **Redox OS.** "Everything is a URL" (scheme-based) — the nearest cousin to
  the filesystem-as-object view, but still namespace-resolving rather than
  capability-passing.
- **CHERI.** Hardware capabilities as unforgeable fat pointers — the
  reference point for future hardware-accelerated capability checking
  (§8.16 hardening).
- **The BedrockOS codebase.** `services/` (capability container seed),
  `vfs/` (`FdTable`, `InodeOps` as the hook surface), `usb/xhci` (a real
  controller family tree), `kerneldump/` (projection tool seed). This
  document is deliberately written to *evolve* these, not replace them.

## Concluding Note

The full graph never truly exists. It is a projection, realized only in
parts, at a time, through the capabilities that subsume them. The kernel is
not a container of processes, files, and drivers — it is the curator of a
single act: the Principal's granting of the world. Everything after that is
the graph doing what graphs do: nodes containing graphs, subsumption
reaching forward in time, rights shrinking as they descend, and the last
capability's fall being the object's death. That is the whole system. It
fits in one sentence, and it recurses forever.

*End of RootGraph.md.*
