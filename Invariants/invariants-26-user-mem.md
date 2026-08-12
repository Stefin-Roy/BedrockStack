# Eager User Memory — Invariants

**Version:** 0.1.0
**Date:** 2026-08-12
**Source:** `kernel/src/mm/usermem.rs`, `kernel/src/task/load.rs`, `kernel/src/task/mod.rs`, `kernel/src/arch/x86_64/syscall.rs`
**Status:** Stable

---

## Design Contract

User-process memory is committed **eagerly and atomically**: the backing
frames for every mapping are allocated, zeroed and installed in the page
table at the moment the mapping is created — never on demand. There is no
demand paging, lazy commit, copy-on-write or swap, and the page-fault handler
is never used to allocate. This file codifies the invariants that keep that
contract enforceable.

## State Invariants

**UM-001 — Eager commit at creation:**
Every user mapping — ELF PT_LOAD segments and the user stack at spawn
(`task/load.rs` `create_process`), `brk` growth and `mmap` (`mm/usermem.rs`)
— allocates and zeroes its backing frames synchronously via `commit_pages`
before the mapping is made visible. No user PTE is ever installed without its
frame.
- Location: `kernel/src/mm/usermem.rs` (`commit_pages`), `kernel/src/task/load.rs` (`create_process`)

**UM-002 — #PF is never an allocator:**
A ring-3 page fault traps in `page_fault_handler` and unconditionally kills
the task (`crate::task::kill_user_fault`); the kernel never populates or
allocates a user page in the fault path. A user fault is always a program
bug, never a lazy-commit trigger.
- Location: `kernel/src/arch/x86_64/idt.rs:362-396`

**UM-003 — No overcommit, atomic commit:**
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
- Location: `kernel/src/mm/usermem.rs`, `kernel/src/arch/x86_64/syscall.rs`

**UM-009 — `munmap` unmaps whole anonymous regions only:**
`[addr, addr+len)` must tile exactly one or more contiguous `Anon` regions;
a partial-region unmap, or any attempt touching the image, stack or heap
region, returns `-EINVAL`. The heap shrinks exclusively through `brk`.
- Location: `kernel/src/mm/usermem.rs` (`munmap`)

---

## Usage / API Contract

- `create_process` returns `(root, entry, user_stack_top, vm_idx)`; the
  `vm_idx` is stored on `Task.vm` by `enter_userspace` and `:spawn` and read
  back by the syscalls through `task::current_vm`.
- Syscall numbers: `2 = brk(new_break)`, `3 = mmap(addr, len, prot)`,
  `4 = munmap(addr, len)` in `arch/x86_64/syscall.rs`. `brk(0)` is a query
  returning the current break.
