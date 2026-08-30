# Scheduler — Invariants

**Version:** 0.2.0
**Source:** `kernel/src/task/mod.rs`, `kernel/src/task/switch.rs`, `kernel/src/task/load.rs`, `kernel/src/smp/mod.rs` (PerCpu), `kernel/src/arch/x86_64/syscall.rs`, `kernel/src/arch/x86_64/idt.rs`, `kernel/src/services/universal_timer.rs`
**Status:** Stable (preemptive tick-sliced BSP-only, 2-class weighted round-robin)

---

## State Invariants

**SCHED-001 — BSP-only, single global queue, WRR:** Only the BSP runs the scheduler (`ap_entry64` `arch/x86_64/trampoline.rs:311` halts; AP ticks gate on `sched_active`). `QUEUE: IrqMutex<VecDeque<&'static mut Task>>` is the sole run queue; selection is weighted round-robin over classes, not per-CPU queues until SMP scheduling lands.
- `kernel/src/task/mod.rs` (`QUEUE`, `schedule` pick loop)

**SCHED-002 — Five lists + CURRENT + IDLE:** Live tasks are exactly one of `CURRENT: Mutex<Option<&'static mut Task>>` (`Running`), `QUEUE` (`Ready`), `SLEEPING: Mutex<Vec<(u64,&'static mut Task)>>` sorted asc deadline (`ZzZ`), `WAITERS: Mutex<Vec<(&'static mut Task,u64)>>` (`ZzZ` waiting for child pid), or `ZOMBIES: Mutex<Vec<&'static mut Task>>` (`Dead`). `RECLAIM` holds consumed zombies awaiting `reap_dead`. `IDLE` is the boot/idle anchor whose `ctx` is swapped via `idle_ctx()` (`UnsafeCell<Task>` to avoid `static mut` UB).
- `mod.rs:440-498` (S5 UnsafeCell fix)

**SCHED-003 — Task layout:** `Task { id, state Ready|Running|ZzZ|Dead, kernel_stack_top, kstack_slot, vm, root(CR3), user_gs, ctx:TaskContext{r15,r14,r13,r12,rbx,rbp,rsp,rip,rflags}, exit_code, parent_pid, args:String, caps_phys, caps_arc:Option<Arc<Vec<Cap>>>, caps_pages }`. `TaskContext` offsets `0x00..0x40` are hard-coded in `switch_to` asm.
- `mod.rs:37-83`, `switch.rs:9-26`

**SCHED-004 — IDLE anchor:** `IDLE: IdleCell(UnsafeCell<Task>)` (`static`, `Sync` impl) — only `idle_ctx()` exposes `*mut TaskContext` via `addr_of_mut!((*IDLE.0.get()).ctx)`; no `&mut` aliasing across `switch_to` (S5).
- `mod.rs:467-494`

**SCHED-005 — PerCpu.current_task is AtomicPtr:** `PerCpu.current_task: AtomicPtr<c_void>` (not raw `*mut`) stores opaque `&'static mut Task` for syscall/caps fast-path. Readers use `load(Relaxed)`, writers `store(Relaxed)` under `CURRENT`/`QUEUE` ordering. Offset unchanged (`repr(C)`, `AtomicPtr` same layout as `*mut`).
- `smp/mod.rs:148-152`, `task/mod.rs:160-167`

**SCHED-006 — Lineage unbounded, cycle-guarded:** `LINEAGE: Once<Mutex<HashMap<u64,u64>>>` pid→parent durable past reap. `lineage_chain` / `is_ancestor` / `lineage_gc` walk until `0`/missing, guarded by `HashSet<u64>` cycle detector. No 128-depth truncation (S8 removed, 2026-08-23). GC keeps entries that are live or ancestors of live.
- `mod.rs:106-210`

**SCHED-007 — Kernel-stack window sharing, not snapshotting:** `KSTACK_VADDR_BASE` in PML4 slot 511 already populated before first `clone_high_half`. `clone_high_half` copies PML4[256..511] entries sharing PDPTs, so a stack `map_4k` into `kernel_root` is visible under every clone via shared subtree. `alloc_kernel_stack` goes through `Vmm::map_4k` which calls `sync_clone_half` when `new_higher_half_slot` true; leaf inserts under existing slot 511 need no sync. Rollback on OOM mid-way unmaps already mapped pages.
- `mod.rs:214-236`, `mm/vmm/x86_64.rs:104,249-266`

**SCHED-008 — Caps window:** Per-task `CAP_SLOT_VA` (8K, 2×4K, supervisor READ, no USER) backing `caps_phys` + `caps_arc: Arc<Vec<Cap>>`. `alloc_caps_page` / `install_caps` / `free_caps_page` map/unmap exactly 2 pages; `free_caps_page` now solely relies on `Vmm::unmap_range_collect` (no translate fallback) and asserts `frames.len()<=2` (S4).
- `mod.rs:238-310`

**SCHED-009 — SLEEPING sorted, batch wake:** `SLEEPING` kept `deadline` asc via `binary_search` insert O(n), O(1) `earliest_sleep_deadline` peek, `wake_sleepers` drains contiguous expired prefix `drain(..n)` O(k) single shift. Never held with `QUEUE`; `SLEEPING` dropped before `QUEUE` in both `wake_sleepers` and `sleep_until`→`schedule` (deadlock avoidance).
- `mod.rs:453-465,616-636`

**SCHED-010 — `schedule()` wakes before picking:** Since 2026-08-23 `schedule()` calls `wake_sleepers()` before popping FIFO, so sleepers never starve (S1). Idle loops (`Kernel::run` / `enter_userspace`) also wake, but wake-in-schedule is primary.
- `mod.rs:1261-1265`

**SCHED-011 — Empty-queue fast-path keeps Running (with sleeper arm):** When `next==None` and `prev==Running`, `schedule()` restores `CURRENT=Some(prev)`, `PerCpu.current_task=prev`, `TSS.rsp0=syscall_rsp0=prev.kernel_stack_top`, keeps `Running`, **cancels slice timer but keeps/arms sleep timer** to the earliest `SLEEPING` deadline (`sync_sleep_timer`), so a sleeping task is still woken even while a lone Batch burns CPU (MONIKA liveness fix). Returns without switch/requeue, LAPIC stays event-driven (no periodic tick). Later arrival re-arms slice via `preempt_kick` (now priority-aware: shortens Batch 12ms to 4ms for Interactive), or sleeper fires. Previously left `CURRENT=None` while task ran (S0); `yield` removed 2026-08-26.
- `mod.rs:1287-1301`

**SCHED-012 — Bootstrap idle halts:** `enter_userspace` loop after `schedule()` does `if earliest => wait_until(d+1) else halt()` so no 100% spin when INIT is `WAITERS` and no sleeper (S3).
- `mod.rs:1416-1431`

**SCHED-013 — Orphan keeps zombie parent live:** `pid_live()` returns true for `CURRENT/QUEUE/SLEEPING/WAITERS/ZOMBIES` (not `RECLAIM`). A child is orphan (`reap_dead` frees) only when `parent_pid==0 || !pid_live(parent)`. Zombie parent still counts as live, so `kill` of a waiter does not free its child before waiter consumes `exit_code` (S2).
- `mod.rs:961-978,1212-1235`

## Locking & IRQs

**SCHED-L001 — Never hold together:** `SLEEPING` dropped before `QUEUE` (`wake_sleepers`, `sleep_until`), `WAITERS` dropped before `QUEUE` (`wake_waiters_for`), `ZOMBIES` never acquired while holding those (`park_zombie` drops `QUEUE` first, `kill` drops before park). `reap_dead` holds `ZOMBIES` while reading `pid_live` (which scans others) — safe because nothing acquires `ZOMBIES` while holding those and `reap_dead` only runs on idle kernel CR3.
- `mod.rs:494-498,586-592,876-907`

**SCHED-L002 — ISR touches atomics, full preemption (MONIKA invasive):** `universal_timer::tick` never takes scheduler locks; it sets `need_resched` via atomics. After EOI, vector 32/52/49/33-239 handlers call `task::try_preempt_from_irq(_from_user)` which now preempts **both ring-3 and kernel** when `preempt_count==0` and `sched_active` and `Running`. Kernel holders are protected by `IrqMutex` (which does `cli` + `preempt_disable`) or explicit `preempt_disable` around plain locks (HEAP, VMM, NS, BLOCK_CACHE). `from_user` is retained for audit but no longer gates. Further gates: non-null current, `Running`, EOI before switch. This is the full-preemption mode requested (2026-08-30); plain spin locks must be audited to `IrqMutex`/`PreemptMutex` before use in new code.

**SCHED-L005 — Slice + sleeper timers are UniversalTimer entries:** Slice expiry (`arm_slice_timer`, `SLICE_TIMER_SEQ`) and the dedicated sleeper wake timer (`arm_sleep_timer`/`sync_sleep_timer`, `SLEEP_TIMER_SEQ`) are both one-shot `UniversalTimer` entries; `UniversalTimer` is the single LAPIC one-shot owner, so both share the queue. Slice is armed **only while competition remains** after a dispatch; lone task runs untimed but **sleeper timer keeps the lone task preemptible** at the earliest `SLEEPING` deadline (`sync_sleep_timer` at `schedule` entry, MONIKA fix for starvation). Slice expiry and sleeper expiry both set `need_resched` (atomics) and `schedule()->wake_sleepers` drains. `preempt_kick` re-arms slice to `SLICE_INTERACTIVE_NS` even when a slice timer exists if an Interactive becomes Ready (shortens Batch quantum). `cancel_*` on idle/fast-path; fast-path now cancels slice but keeps sleep timer.
- `kernel/src/task/mod.rs` (`SLICE_TIMER_SEQ`, `arm_slice_timer`, `preempt_kick`), `kernel/src/services/universal_timer.rs` (`set_oneshot`/`cancel_timer_id`)

**SCHED-L006 — WRR classes and slices:** `Priority::{Interactive,Batch}`; slices 4ms / 12ms; Interactive holds a 3:1 dispatch budget (`WRR_CREDIT`, reset by each Batch pick; decremented — saturating at 0 — on every Interactive pick, including the no-Batch-ready fallback). The expiry timer is granted per dispatch only under competition; preempted Running tasks requeue as `Ready` at the back of the queue.

**SCHED-L007 — Resched IPI (vector 49):** Registered in `idt::init()` (APIC-008); handler EOIs, sets `need_resched`, calls `try_preempt_from_irq`. Dormant while scheduling stays BSP-only (`sched_active` false on APs), fully wired for per-CPU queues later.
- `kernel/src/arch/x86_64/idt.rs` (`ipi_resched_handler`)

**SCHED-L003 — Deadline-only, IRQ-safe locks, no periodic tick:** All scheduler locks (`QUEUE`/`CURRENT`/`SLEEPING`/`WAITERS`/`ZOMBIES`/`RECLAIM`/`KSTACK_IN_USE`/`LINEAGE`/`INIT_CAPS_STASH`) are `IrqMutex` (local-IRQ disable), so a one-shot universal_timer deadline that sets `need_resched` via atomics can never deadlock against a lock holder. The LAPIC stays **one-shot only — no periodic mode**; `need_resched` is consumed voluntarily in `schedule()`, which also increments `sched_ticks` and wraps itself in `preempt_disable/enable`. `task::init` sets `sched_active(0,true)`. `reap_dead` debug-asserts idle + kernel root. Deduplicated pid scans go through the single `with_task` helper (`process_state`/`task_vm`/`task_parent`/`pid_live`/`task_args`/`caps_snapshot`). Future SMP/preemptive mode must add per-CPU queues and switch `ADDR_SPACES` assumptions accordingly.
- `mod.rs` (IrqMutex statics, `with_task`, `schedule`, `init`, `reap_dead`, `free_kernel_stack` assert), `smp/mod.rs:152-158,542-568`, `mm/usermem.rs` (`ADDR_SPACES` IrqMutex), `mm/vmm/mod.rs` (`current_root`)

**SCHED-L004 — CR3 switch stays mapped:** `switch_to` `cmp cr3,rdx / je` skips reload; higher-half (kernel image, heap physmap, APIC) is in every root via `clone_high_half` shared subtree, so code between `mov cr3` and `ret` stays mapped.
- `switch.rs:113-117`

## Safety Invariants

**SCHED-S001 — Stack lifetime:** `alloc_kernel_stack` maps 4 pages into `kernel_root` (slot 511 shared); `free_kernel_stack` `unmap_range_collect` + `shootdown_tlb` before frames returned. `reap_dead` only runs on idle stack under `kernel_root` (APIC MMIO needs kernel address space).
- `mod.rs:224-236,391-410,1155-1235`

**SCHED-S002 — No panic on device data:** Loader `load.rs:106-250` (`parse_segments`, `map_segment`, `create_process`) returns `Result<&str>`, never `unwrap/expect` on ELF bytes (`panic=abort`). Only OOM `expect` on kernel stack allocation (`mod.rs:1358` bootstrap).

**SCHED-S003 — Box::leak / Box::from_raw paired:** `spawn` `Box::leak(Box::new(task))` as `&'static mut Task`; `reap_one` `Box::from_raw(raw)` after `free_caps_page`/`destroy_root`/`free_kernel_stack`/`unregister`/`detach`. `RECLAIM` tasks are invisible to all scans (`process_state`, `task_vm`, `caps_snapshot`) so `:wait`/`/proc` treat consumed as reaped.
- `mod.rs:522-561,1155-1200`

## API Contracts

**SCHED-API-001 — `spawn(Task)->u64` / `spawn_with_priority(Task, Priority)->u64`:** Assigns `next_id`, `lineage_insert`, `Box::leak`, `proc::attach`, `QUEUE.push_back`. `vm==0` allowed for kernel tasks (`audio_pump`). Priority defaults to Interactive.

**SCHED-API-002 — `schedule()`:** WRR picks first Ready of the credit-selected class in `QUEUE`. Cases: `next+prev=Dead/ZzZ → park_zombie + switch`; `next+prev=Running → prev Ready push_back + switch`; `next=None+prev=Dead/ZzZ → cancel slice timer + park+switch to idle`; `next=None+prev=Running → keep Running, cancel slice timer (S0/SCHED-011)`; `next=None+prev=None → cancel slice timer + return`. A dispatch arms the expiry timer only when Ready tasks remain in the queue after the pick.

**SCHED-API-003 — `sleep_until(deadline_ns)` / `sleep_current(ns)`:** Marks current `ZzZ`, binary-inserts sorted `SLEEPING`, drops lock before `schedule()`.

**SCHED-API-004 — `wait(pid)->Result<code,WaitError>`:** Child-only (Unix `wait`). Consumes `ZOMBIES` → `RECLAIM` if present, else parks in `WAITERS` + `schedule()` loop; re-checks `!zombie_present && !pid_live => NotFound` (S2). Parks with double-check after `WAITERS.push` to close lost-wakeup window where exit raced the check-then-park (MONIKA fix): re-checks `zombie_present`, unparks self synchronously or removes duplicate `QUEUE` entry.

**SCHED-API-005 — `kill(pid)`:** Self-kill diverges via `kill_current`; else scans `QUEUE/SLEEPING/WAITERS` preserving order, marks `Dead`/`KILLED_EXIT_CODE` `0xDEAD_BEEF`, `park_zombie` (which `wake_waiters_for`).

**SCHED-API-006 — `reap_dead(&mut BitmapAllocator)`:** Drains `RECLAIM` → `reap_one`; then scans `ZOMBIES` for `parent_pid==0 || !pid_live(parent)` orphans → `reap_one`; `lineage_gc`.

## Design Notes

- `switch_to` callee-saved + `pushfq/popfq` restores `IF` deterministically; `user_iret` `cli; mov gs:[0]; mov ds/es/fs/gs=0x23; wrmsr GS_BASE; swapgs; iretq` mirrors `syscall_entry` `swapgs; xchg rsp,gs:[off]; ... sti ... sysretq` invariant (`GS.base=PerCpu`, `KERNEL_GS_BASE=user GS` while in kernel).
- `enter_userspace` builds 5-word iretq frame `{RIP,0x2B,0x202,RSP,0x23}` at `kernel_stack_top-40`, `set_user_gs(0)`, `Running`, `Box::leak`, `switch_to(idle,ctx,root)`; resume loop polls `process_state(pid)` for `Dead/None` to drain `INIT` stdout (`/proc/pid/std/out`) before reap.
- `PUMP_QUEUE_CAP=4` bounded; enqueuers park `sleep_current(500µs)` not `HLT` when `current_task` present (AUD-028).
