# User Memory — Invariants

**Version:** 0.2.0
**Date:** 2026-08-25
**Source:** `kernel/src/mm/usermem.rs`, `kernel/src/mm/fault.rs`, `kernel/src/mm/framecnt.rs`, `kernel/src/task/load.rs`, `kernel/src/task/mod.rs`, `kernel/src/unispace/provider/proc.rs`, `kernel/src/arch/x86_64/syscall.rs`
**Status:** Stable

---

## Design Contract

User-process memory is committed **eagerly and atomically by default**: the
backing frames for every mapping are allocated, zeroed and installed in the
page table at the moment the mapping is created. Two sanctioned exceptions
exist since v0.2.0:

1. **Lazy commit** — an `mmap` request that passes `prot bit 3 (0x8)`
   registers its region *without* committing; pages materialize on first
   touch through the demand-fill fault path, charged against the budget as
   they appear.
2. **Copy-on-write fork** — `/proc/self:fork` shares every leaf frame between
   parent and child with both sides' writable leaves downgraded to read-only;
   the first write faults into the COW resolver.

The page-fault handler allocates in exactly these cases (`mm::fault`);
every other ring-3 fault remains fatal for the task. There is still no swap.
This file codifies the invariants that keep that contract enforceable.

## State Invariants

**UM-001 — Eager commit at creation (default):**
Every user mapping — ELF PT_LOAD segments and the user stack at spawn
(`task/load.rs` `create_process`), `brk` growth and non-lazy `mmap`
(`mm/usermem.rs`) — allocates and zeroes its backing frames synchronously via
`commit_pages` before the mapping is made visible. Lazy regions are the only
mappings registered without frames (see UM-010).
- Location: `kernel/src/mm/usermem.rs` (`commit_pages`), `kernel/src/task/load.rs` (`create_process`)

**UM-002 — #PF allocates only through the resolver:**
A ring-3 page fault first runs `mm::fault::resolve_user_fault`. It returns
`true` (instruction retried) only for (a) a not-present fault inside a
writable `Heap`/`Stack`/`Anon` region → zeroed demand fill, or (b) a write
fault on a present non-writable leaf of a writable region → COW copy or
in-place upgrade. Every other outcome kills the task via
`crate::task::kill_user_fault`. `Image`-region holes never fill.
- Location: `kernel/src/arch/x86_64/idt.rs` (`page_fault_handler`), `kernel/src/mm/fault.rs`

**UM-003 — No partial eager commit, atomic bookkeeping:**
`commit_pages` allocates frames one 4 KiB page at a time; on the first
allocation failure it unmaps and frees everything it mapped so far (leaf
frames and empty intermediate tables) and returns `Err`. `brk`/`mmap` update
the region table and counters only *after* a successful commit, so a failed
call leaves the address space exactly as it was — no partial commit or stale
bookkeeping.
- Location: `kernel/src/mm/usermem.rs` (`commit_pages`, `brk`, `mmap`)

**UM-004 — W^X enforcement:**
Executable mappings are never writable. The loader maps `PF_X` segments as
USER|READ|EXECUTE (never WRITE) in `seg_page_flags`. The `mmap` prototype
rejects an RWX request outright with `-EINVAL` (`prot_to_flags`). Kernel
flags `PageFlags::EXECUTE` on a mapping and `page_flags_to_x86` adds NX
whenever it is absent, so a writable-only mapping is also non-executable.
- Location: `kernel/src/task/load.rs:95`, `kernel/src/mm/usermem.rs` (`prot_to_flags`), `kernel/src/mm/vmm/x86_64.rs:43-67`

**UM-005 — Guard pages:**
The 32 KiB user stack has an unmapped guard page immediately below it. Each
anonymous `mmap` region is separated by an unmapped guard page on its
underside (modelled by `Region.guard` and the rule that a mapping's collision
span `[vaddr - PAGE, end)` must not overlap any existing region). `brk`
growth is bounded by the lowest occupied-span start above the break, keeping
at least the anon/stack guard page separation. `munmap` and `brk` shrink can
never unmap a guard page: `munmap` operates only on whole `Anon` regions, and
the stack/image regions are never unmapable.
- Location: `kernel/src/task/load.rs` (`USER_STACK_TOP`, `USER_STACK_SIZE`), `kernel/src/mm/usermem.rs` (`coll_span`, `find_fit`, `collides`, `brk`, `munmap`)

**UM-006 — Teardown frees exactly the owned frames:**
On process exit `destroy_root` walks the low half of the clone root and frees
every present leaf frame plus empty intermediate tables, then the root PML4
(`mm/vmm/x86_64.rs`); `reap_dead` then calls `mm::usermem::unregister`, which
drops only the region table's kernel-heap `Vec`s. Frames released earlier at
run time (`brk` shrink, `munmap`) were already unmapped and freed by
`release_pages`, so `destroy_root` sees no PTE for them — a frame is never
double-freed (it is freed only once, either by `release_pages` or by the one
PDE/PTE that still referenced it).
- Location: `kernel/src/mm/vmm/x86_64.rs:434-500`, `kernel/src/task/mod.rs` (`reap_dead`), `kernel/src/mm/usermem.rs` (`release_pages`, `unregister`)

**UM-007 — Committed budget accounting:**
`AddressSpace.committed` counts every committed leaf page (image + stack from
spawn; heap and anon as added). The default per-process budget is 256 MiB
(`USER_BUDGET_BYTES`); `brk` and `mmap` reject a grow that would exceed it
with `-ENOMEM` and leave the space unchanged. `/proc/<pid>/mem` reports
`{root, brk, stack_top, committed_pages, budget_pages}` through
`mm::usermem::summarize`.
- Location: `kernel/src/mm/usermem.rs`, `kernel/src/unispace/provider/proc.rs` (`MemObject`)

**UM-008 — No panics on user/vm input:**
`brk`/`mmap`/`munmap` return `Result<_, i64>` with errno (`-EFAULT`,
`-ENOMEM`, `-EINVAL`) for every malformed request — misaligned or overflowing
addresses, non-page-aligned length, protected-region collisions, partial
`munmap`, RWX `prot`. Address arithmetic is `checked`. With `panic = "abort"`
a panic is fatal, so user-supplied values must only ever produce `Err`, never
a panic.
- Location: `kernel/src/mm/usermem.rs`, `kernel/src/unispace/provider/proc.rs` (`mem_method`)

**UM-009 — `munmap` unmaps whole anonymous regions only:**
`[addr, addr+len)` must tile exactly one or more contiguous `Anon` regions;
a partial-region unmap, or any attempt touching the image, stack or heap
region, returns `-EINVAL`. The heap shrinks exclusively through `brk`.
- Location: `kernel/src/mm/usermem.rs` (`munmap`)

**UM-010 — Lazy commit is opt-in and charged on fault:**
A lazy region (`Region.lazy`, requested via `mmap` prot bit 0x8) registers
with no frames and no committed charge. Each first-touch fault materializes
one zeroed page via `demand_fill`, which re-checks the dynamic budget under
the `ADDR_SPACES` lock before allocating — so overcommit at registration is
bounded to one page per fault, and exhaustion kills rather than panics.
Unfaulted lazy pages unmap/fork/teardown as absent leaves (no-ops).
- Location: `kernel/src/mm/usermem.rs` (`mmap`, `demand_fill`), `kernel/src/mm/fault.rs`

**UM-011 — COW frames are refcounted; shares precede schedulability:**
Every leaf frame shared by a fork passed through `framecnt::share_frame`
*before* the child root became schedulable (INV-FC-01). Counting convention:
entry 0 = untracked single owner, ≥2 = shared; teardown routes every user
leaf free through `framecnt::decref_or_free` (`release_pages`,
`destroy_root`, commit rollback), which frees only the last reference. The
COW resolver upgrades in place only when `is_sole_owner`; otherwise it makes
a private copy and drops one reference. A failed fork unwinds its tables and
shares completely (`clone_user_space_cow`).
- Location: `kernel/src/mm/framecnt.rs`, `kernel/src/mm/vmm/x86_64.rs`
  (`clone_user_space_cow`, `user_leaf_*`), `kernel/src/mm/usermem.rs` (`fork_as`),
  `kernel/src/mm/fault.rs` (`cow_resolve`)

**UM-012 — COW write-downgrade visibility:**
`fork_as` downgrades writable leaves in the parent root while that root can
only be resident on the calling CPU (BSP-only scheduler); the post-lock
`shootdown_tlb()` covers any residual cross-CPU residency. The COW resolver's
leaf edits invalidate locally only — valid because the faulting CPU is by
definition where the root is active (INV-FC-02).
- Location: `kernel/src/mm/usermem.rs` (`fork_as`), `kernel/src/mm/vmm/x86_64.rs` (`edit_user_leaf`)

---

## Usage / API Contract

- `create_process` returns `(root, entry, user_stack_top, vm_idx)`; the
  `vm_idx` is stored on `Task.vm` by `enter_userspace` and `:spawn` and read
  back by the memory methods through `task::current_vm`.
- Memory operations are unispace methods on `/proc/self`, not syscalls:
  `write(/proc/self:brk, {new_break})` (in `{new_break: u64}`, out `{brk: u64}`;
  `{new_break: 0}` is a query returning the current break),
  `write(/proc/self:mmap, {addr, len, prot})` (out `{base: u64}`), and
  `write(/proc/self:munmap, {addr, len})` (out unit). They target the *running*
  task's address space (they mutate the caller's CR3) and map their errnos
  through `UnispaceError::{OutOfMemory, BadAddress, InvalidArgument}` to
  `-ENOMEM`/`-EFAULT`/`-EINVAL` in `arch/x86_64/syscall.rs::errno`.
