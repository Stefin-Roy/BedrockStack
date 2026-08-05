//! C6 — boot-time capability-separation test (§6.1, §7.5).
//!
//! Runs exactly once from `Kernel::init()` right after bootstrap and before
//! SMP. It proves the DMA provider is reachable only through the boot table:
//! a real allocation succeeds through the endowed `CapId`, an unendowed id is
//! refused with `NoSuchCap`, and a contract the DMA node does not implement is
//! refused by PERMIT with `ObjError::Denied`. It also proves the real
//! physical-world nodes answer per-hook contract rights (READ/WRITE/CALL,
//! §3.3), QUERY surface reads (§4.1), the table node's count hook, and
//! capability-mediated delegation (§3.4.3, §8.24). Arch-neutral.

extern crate alloc;

use alloc::vec;

use super::adapters::{self, DMA_ALLOC_PAGE};
use super::bootstrap::{boot_domain, boot_endowment};
use super::cap_handle::{CapHandle, CapId, HandleState};
use super::contract;
use super::fs;
use super::hook::SURFACE_READ;
use super::memregion::{MEM_REGION_BASE, MEM_REGION_CONTRACT, MEM_REGION_CONTRACT_DEF};
use super::nodes::{
    ADDRSPACE_CONTRACT, ADDRSPACE_ROOT, HEAP_ALLOC, HEAP_CONTRACT, PHYSMEM_ALLOC_FRAMES,
    PHYSMEM_CONTRACT, PHYSMEM_FREE, PHYSMEM_STATS,
};
use super::registry::{REGISTRY_CONTRACT, REGISTRY_LOOKUP, REGISTRY_REGISTER};
use super::rights::{CapRights, ContractRights, Rights};
use super::table::{TABLE_CONTRACT, TABLE_COUNT, TABLE_DELEGATE};
use super::{invoke, Args, ObjError, Reply, Value};
use crate::drivers::serial::SerialPort;

pub fn run() {
    let table = &boot_domain().table;
    let end = boot_endowment();

    // 1. A genuine allocation through the capability succeeds.
    match invoke(&table, end.dma, adapters::DMA_CONTRACT, DMA_ALLOC_PAGE, &Args::none()) {
        Ok(Reply::Data(vals)) if vals.len() == 3 => {
            match [&vals[0], &vals[1], &vals[2]] {
                [Value::U64(phys), Value::U64(virt), Value::U64(size)] => {
                    assert!(*phys != 0, "separation: zero phys page");
                    assert!(*size == 4096, "separation: DMA page must be one frame");
                    SerialPort::puts("[obj] separation: alloc -> phys=0x");
                    SerialPort::put_hex(*phys);
                    SerialPort::puts(" virt=0x");
                    SerialPort::put_hex(*virt);
                    SerialPort::puts("\n");
                }
                _ => panic!("separation: alloc_page replied non-u64 payload"),
            }
        }
        Ok(_) => panic!("separation: alloc_page replied unexpected shape"),
        Err(e) => panic!("separation: alloc_page through cap failed: {:?}", e),
    }

    // 2. An unendowed capability id cannot reach any node.
    let r2 = invoke(
        &table,
        CapId(u32::MAX as u64),
        adapters::DMA_CONTRACT,
        DMA_ALLOC_PAGE,
        &Args::none(),
    );
    expect_err("unendowed id", r2, ObjError::NoSuchCap);

    // 3. PERMIT refuses a contract the DMA node does not implement.
    let args = Args { vals: vec![Value::Str("sep")] };
    let r3 = invoke(&table, end.dma, adapters::SERIAL_CONTRACT, adapters::SERIAL_PUTS, &args);
    expect_err("foreign contract", r3, ObjError::Denied);

    // 3b. NEGATIVE — the per-hook contract-right dimension (§3.3): a handle
    //     whose contract mask holds READ but lacks the hook's required CALL
    //     must be refused, even though the node implements the contract and
    //     the handle holds INVOKE.
    let narrowed_id = table.insert(CapHandle {
        id: CapId(0),
        node: adapters::dma_node(),
        rights: CapRights::new(Rights::INVOKE, ContractRights::READ),
        state: HandleState::Live,
    });
    let r3b = invoke(&table, narrowed_id, adapters::DMA_CONTRACT, DMA_ALLOC_PAGE, &Args::none());
    expect_err("per-hook contract right", r3b, ObjError::Denied);

    // C8 — first driver domain: disjoint table endowed only with
    // dma+pci_cfg+physmem+addrspace (§6.2, §8.14). The device sweep runs under
    // this domain; it must be able to resolve exactly its endowment, and
    // nothing the boot domain kept.
    let drv = crate::obj::driver::driver_domain();
    let dend = crate::obj::driver::driver_endowment();

    // 4. Endowed DMA resolves from the driver table (PERMIT passes).
    match drv.table.resolve(dend.dma, adapters::DMA_CONTRACT, DMA_ALLOC_PAGE) {
        Ok(_) => SerialPort::puts("[obj] driver sep ok: endowed dma resolves\n"),
        Err(e) => panic!("driver separation FAIL: endowed dma refused: {:?}", e),
    }

    // 5. Endowed PCI-config resolves from the driver table.
    match drv.table.resolve(dend.pci_cfg, adapters::PCI_CONTRACT, adapters::PCI_READ32) {
        Ok(_) => SerialPort::puts("[obj] driver sep ok: endowed pci_cfg resolves\n"),
        Err(e) => panic!("driver separation FAIL: endowed pci_cfg refused: {:?}", e),
    }

    // 6. NEGATIVE — an unendowed id cannot reach DMA from the driver table.
    match drv.table.resolve(CapId(u32::MAX as u64), adapters::DMA_CONTRACT, DMA_ALLOC_PAGE) {
        Err(ObjError::NoSuchCap) => SerialPort::puts("[obj] driver sep ok: unendowed id refused\n"),
        Ok(_) => panic!("driver separation FAIL: unendowed id resolved"),
        Err(e) => panic!("driver separation FAIL: unendowed id -> {:?}", e),
    }

    // 7. The driver domain holds exactly its endowment: four slots, no more.
    //    A driver table of size four cannot name a serial or heap cap the boot
    //    domain kept for itself (§8.14: the kernel cannot reach the driver's
    //    addresses by position — and vice versa).
    assert_eq!(
        drv.table.count(),
        4,
        "driver domain must hold exactly dma + pci_cfg + physmem + addrspace"
    );

    // 8. NEGATIVE — serial is not reachable through the driver domain at all:
    //    the endowed DMA node simply does not implement the serial contract.
    match drv.table.resolve(dend.dma, adapters::SERIAL_CONTRACT, adapters::SERIAL_PUTS) {
        Err(ObjError::Denied) => SerialPort::puts("[obj] driver sep ok: serial not reachable\n"),
        Ok(_) => panic!("driver separation FAIL: dma node implemented serial contract"),
        Err(e) => panic!("driver separation FAIL: serial probe -> {:?}", e),
    }

    // §7.8 — the contract registry: discovery by owned capability, never
    // ambient. The boot domain was endowed with the registry cap; the driver
    // domain was not.

    // 9. NEGATIVE — a driver domain WITHOUT a registry cap cannot consult the
    //    registry: its endowed DMA node does not implement the registry
    //    contract, so PERMIT refuses with `Denied`.
    match drv.table.resolve(dend.dma, REGISTRY_CONTRACT, REGISTRY_LOOKUP) {
        Err(ObjError::Denied) => SerialPort::puts("[obj] driver sep ok: registry not reachable\n"),
        Ok(_) => panic!("driver separation FAIL: dma node implemented registry contract"),
        Err(e) => panic!("driver separation FAIL: registry probe -> {:?}", e),
    }

    // 10. The boot domain, holding the registry cap, CAN look up a registered
    //     contract by id and gets its definition (name + doc) back.
    let lookup = Args { vals: vec![Value::U64(adapters::DMA_CONTRACT.0)] };
    match invoke(&table, end.registry, REGISTRY_CONTRACT, REGISTRY_LOOKUP, &lookup) {
        Ok(Reply::Data(vals)) if vals.len() == 2 => match [&vals[0], &vals[1]] {
            [Value::Str(name), Value::Str(doc)] => {
                assert_eq!(*name, "dma:alloc", "registry lookup: wrong contract name");
                assert_eq!(*doc, adapters::DMA_DOC, "registry lookup: wrong contract doc");
                SerialPort::puts("[obj] registry ok: lookup dma:alloc -> ");
                SerialPort::puts(*name);
                SerialPort::puts("\n");
            }
            _ => panic!("registry lookup replied non-str payload"),
        },
        Ok(_) => panic!("registry lookup replied unexpected shape"),
        Err(e) => panic!("registry lookup failed: {:?}", e),
    }

    // 11. A bogus id returns `Reply::None` — the registry says "unknown".
    let bogus = Args { vals: vec![Value::U64(0xfeed_bacc)] };
    match invoke(&table, end.registry, REGISTRY_CONTRACT, REGISTRY_LOOKUP, &bogus) {
        Ok(Reply::None) => SerialPort::puts("[obj] registry ok: bogus id -> None\n"),
        Ok(_) => panic!("registry lookup: bogus id resolved"),
        Err(e) => panic!("registry lookup: bogus id -> {:?}", e),
    }

    // 12. I10 idempotency — registering the same tuple twice is `Ok`: the
    //     content-addressed identity already covers it.
    let reg = Args { vals: vec![Value::Str("dma:alloc")] };
    match invoke(&table, end.registry, REGISTRY_CONTRACT, REGISTRY_REGISTER, &reg) {
        Ok(_) => SerialPort::puts("[obj] registry ok: re-register idempotent\n"),
        Err(e) => panic!("registry re-register failed: {:?}", e),
    }

    // 13. I10 loud failure — a distinct tuple claiming an already-registered
    //     `ContractId` (same name, different signature) must be refused with
    //     `ContractCollision`, and the genuine entry must survive untouched.
    const FAKE_SURFACE: super::surface::SurfaceDesc = super::surface::SurfaceDesc {
        kind: "dma:alloc",
        attrs: &[],
        events: &[],
    };
    static FAKE_CONTRACT: contract::Contract = contract::Contract {
        id: adapters::DMA_CONTRACT,
        name: "dma:alloc",
        surface: &FAKE_SURFACE,
        hooks: &[],
        doc: "a duplicate-name contract with an empty hook list",
    };
    match contract::contract_registry().register(&FAKE_CONTRACT) {
        Err(ObjError::ContractCollision) => {
            SerialPort::puts("[obj] registry ok: I10 collision refused loudly\n")
        }
        Ok(()) => panic!("registry I10 FAIL: colliding tuple was accepted"),
        Err(e) => panic!("registry I10 FAIL: wrong error {:?}", e),
    }
    assert!(
        contract::contract_registry().lookup(adapters::DMA_CONTRACT).is_some(),
        "registry I10 FAIL: genuine contract lost on collision"
    );

    SerialPort::puts("[obj] registry separation: OK (owned-capability discovery)\n");

    // The physical world as nodes (§7.10). The boot domain was endowed
    // with the five family roots; exercise the frame pool through capability
    // mediation and prove the driver domain (physmem but NO heap) is genuinely
    // cut off from the heap node.

    // 14. POSITIVE — physmem `alloc_frames` from the boot table returns exactly
    //     one capability; that cap resolves `MEM_REGION_BASE` to a non-zero
    //     base. The reply's caps already carry real `CapId`s (invoke inserts
    //     them into the boot table), so we can invoke the region directly.
    match invoke(
        &table,
        end.physmem,
        PHYSMEM_CONTRACT,
        PHYSMEM_ALLOC_FRAMES,
        &Args::none(),
    ) {
        Ok(Reply::Caps(caps)) if caps.len() == 1 => {
            let region_id = caps[0].id;
            match invoke(&table, region_id, MEM_REGION_CONTRACT, MEM_REGION_BASE, &Args::none()) {
                Ok(Reply::Data(vals)) if vals.len() == 1 => match &vals[0] {
                    Value::U64(base) => {
                        assert!(*base != 0, "physmem region base is zero");
                        SerialPort::puts("[obj] physmem ok: alloc_frames -> base=0x");
                        SerialPort::put_hex(*base);
                        SerialPort::puts("\n");
                    }
                    _ => panic!("region base replied non-u64 payload"),
                },
                Ok(_) => panic!("region base replied unexpected shape"),
                Err(e) => panic!("region base through mem:region failed: {:?}", e),
            }
        }
        Ok(_) => panic!("physmem alloc_frames replied unexpected shape"),
        Err(e) => panic!("physmem alloc_frames through cap failed: {:?}", e),
    }

    // 15. NEGATIVE — a driver domain WITHOUT a heap cap cannot resolve the Heap
    //     node: the id provably isn't endowed (the driver table never received
    //     a heap cap), so `resolve` fails `NoSuchCap`/`Denied`. This proves "a
    //     driver domain without a heap cap cannot resolve the Heap node" (§6.2).
    match drv.table.resolve(CapId(u32::MAX as u64), HEAP_CONTRACT, HEAP_ALLOC) {
        Err(ObjError::NoSuchCap) | Err(ObjError::Denied) => {
            SerialPort::puts("[obj] driver sep ok: no heap cap -> Heap node unreachable\n")
        }
        Ok(_) => panic!("driver separation FAIL: heap resolved without a heap cap"),
        Err(e) => panic!("driver separation FAIL: heap probe -> {:?}", e),
    }

    // CapabilityVfs — filesystem capability separation (§7.12.3). The boot domain has
    // DirNode caps from the A: tmpfs mount. A domain holding only a QUERY-only
    // copy cannot readdir (INVOKE required by PERMIT); a domain with no dir
    // cap at all sees nothing via resolve_first.
    //
    // NOTE: the dir-cap check is deferred to `run_post_mount()` because mounts
    // happen in `run()` (after this function returns). The driver-domain
    // negative test runs here since it needs no mount.

    // 18. NEGATIVE — driver domain has no dir cap: resolve_first returns None.
    match drv.table.resolve_first(fs::DIR_CONTRACT, fs::DIR_READDIR) {
        None => {
            SerialPort::puts("[obj] fs sep ok: driver domain resolve_first(DIR_READDIR) -> None\n")
        }
        Some(_) => panic!("fs separation FAIL: driver domain found a dir cap"),
    }

    // §4.1 — QUERY surface reads: the `_read_surface` hook is node-level and
    // gated only by the universal `QUERY` right, so a domain holding a cap over
    // a node can read live attributes off its surface without any contract
    // membership or INVOKE.

    // 19. POSITIVE — surface reads through the real endowed caps answer live
    //     values: physmem's total frame count, the heap arena's committed
    //     bytes, the addrspace page-table root, and the table's slot count.
    let phys_sr = Args { vals: vec![Value::Str("total_frames")] };
    match invoke(&table, end.physmem, PHYSMEM_CONTRACT, SURFACE_READ, &phys_sr) {
        Ok(Reply::Data(vals)) if vals.len() == 1 => match &vals[0] {
            Value::U64(v) => {
                assert!(*v > 0, "physmem surface total_frames must be non-zero");
                SerialPort::puts("[obj] surface ok: physmem total_frames=");
                SerialPort::put_u64(*v);
                SerialPort::puts("\n");
            }
            _ => panic!("surface read: total_frames non-u64"),
        },
        Ok(_) => panic!("surface read: total_frames unexpected shape"),
        Err(e) => panic!("surface read: physmem total_frames -> {:?}", e),
    }
    let heap_sr = Args { vals: vec![Value::Str("arena")] };
    match invoke(&table, end.heap, HEAP_CONTRACT, SURFACE_READ, &heap_sr) {
        Ok(Reply::Data(vals)) if vals.len() == 1 => match &vals[0] {
            Value::U64(v) => {
                assert!(*v > 0, "heap surface arena must be non-zero");
                SerialPort::puts("[obj] surface ok: heap arena=");
                SerialPort::put_u64(*v);
                SerialPort::puts("\n");
            }
            _ => panic!("surface read: arena non-u64"),
        },
        Ok(_) => panic!("surface read: arena unexpected shape"),
        Err(e) => panic!("surface read: heap arena -> {:?}", e),
    }
    let as_sr = Args { vals: vec![Value::Str("root")] };
    match invoke(&table, end.addrspace, ADDRSPACE_CONTRACT, SURFACE_READ, &as_sr) {
        Ok(Reply::Data(vals)) if vals.len() == 1 => match &vals[0] {
            Value::U64(v) => {
                assert!(*v != 0, "addrspace surface root must be non-zero");
                SerialPort::puts("[obj] surface ok: addrspace root=0x");
                SerialPort::put_hex(*v);
                SerialPort::puts("\n");
            }
            _ => panic!("surface read: root non-u64"),
        },
        Ok(_) => panic!("surface read: root unexpected shape"),
        Err(e) => panic!("surface read: addrspace root -> {:?}", e),
    }
    let tbl_sr = Args { vals: vec![Value::Str("slots")] };
    match invoke(&table, end.table, TABLE_CONTRACT, SURFACE_READ, &tbl_sr) {
        Ok(Reply::Data(vals)) if vals.len() == 1 => match &vals[0] {
            Value::U64(v) => {
                assert_eq!(*v as usize, table.count(), "table surface slots must equal count()");
                SerialPort::puts("[obj] surface ok: table slots=");
                SerialPort::put_u64(*v);
                SerialPort::puts("\n");
            }
            _ => panic!("surface read: slots non-u64"),
        },
        Ok(_) => panic!("surface read: slots unexpected shape"),
        Err(e) => panic!("surface read: table slots -> {:?}", e),
    }

    // 20. NEGATIVE — a cap with QUERY dropped cannot read any surface: the
    //     SURFACE_READ path resolves via `resolve_for_query`, which demands the
    //     universal `QUERY` right. INVOKE alone is not enough.
    let no_query = table
        .dup_limited(end.physmem, Rights::INVOKE, ContractRights::empty())
        .expect("dup_limited for no-QUERY physmem cap");
    let r20 = invoke(&table, no_query, PHYSMEM_CONTRACT, SURFACE_READ, &phys_sr);
    expect_err("surface read without QUERY", r20, ObjError::Denied);

    // §3.3 — the per-hook contract-right dimension on the REAL physmem node
    // (mirror of 3b): `free`/`reserve` demand WRITE, `stats` demands READ, so
    // a narrowed mask gates the exact hooks a cap may reach, not just the node.

    // 21. NEGATIVE + POSITIVE — a CALL-only mask reaches neither `free` (WRITE
    //     required) nor `stats` (READ required); the full mask reaches `stats`.
    let call_only = table
        .dup_limited(end.physmem, Rights::INVOKE, ContractRights::CALL)
        .expect("dup_limited for CALL-only physmem cap");
    let r21a = invoke(
        &table,
        call_only,
        PHYSMEM_CONTRACT,
        PHYSMEM_FREE,
        &Args { vals: vec![Value::U64(0)] },
    );
    expect_err("physmem free without WRITE", r21a, ObjError::Denied);
    let full_mask = table
        .dup_limited(
            end.physmem,
            Rights::INVOKE,
            ContractRights::READ.or(ContractRights::WRITE).or(ContractRights::CALL),
        )
        .expect("dup_limited for full-mask physmem cap");
    match invoke(&table, full_mask, PHYSMEM_CONTRACT, PHYSMEM_STATS, &Args::none()) {
        Ok(Reply::Data(vals)) if vals.len() == 2 => {
            SerialPort::puts("[obj] per-hook contract right ok: physmem stats via READ\n")
        }
        Ok(_) => panic!("physmem stats via READ replied unexpected shape"),
        Err(e) => panic!("physmem stats via READ failed: {:?}", e),
    }

    // 22. POSITIVE — the addrspace node answers its `root` hook through the
    //     endowed cap (READ contract right held by the boot mask), giving the
    //     page-table root without a raw VMM call.
    match invoke(&table, end.addrspace, ADDRSPACE_CONTRACT, ADDRSPACE_ROOT, &Args::none()) {
        Ok(Reply::Data(vals)) if vals.len() == 1 => match &vals[0] {
            Value::U64(root) => {
                assert!(*root != 0, "addrspace root is zero");
                SerialPort::puts("[obj] addrspace ok: root=0x");
                SerialPort::put_hex(*root);
                SerialPort::puts("\n");
            }
            _ => panic!("addrspace root replied non-u64 payload"),
        },
        Ok(_) => panic!("addrspace root replied unexpected shape"),
        Err(e) => panic!("addrspace root through cap failed: {:?}", e),
    }

    // 23. POSITIVE — `mem:region` is a registered contract (§7.8): a registry
    //     lookup by id answers its canonical name and doc, exactly like the
    //     fs/device families seeded through the owned registry cap.
    let mem_lookup = Args { vals: vec![Value::U64(MEM_REGION_CONTRACT.0)] };
    match invoke(&table, end.registry, REGISTRY_CONTRACT, REGISTRY_LOOKUP, &mem_lookup) {
        Ok(Reply::Data(vals)) if vals.len() == 2 => match [&vals[0], &vals[1]] {
            [Value::Str(name), Value::Str(doc)] => {
                assert_eq!(*name, "mem:region", "registry lookup: wrong mem:region name");
                assert_eq!(
                    *doc,
                    MEM_REGION_CONTRACT_DEF.doc,
                    "registry lookup: wrong mem:region doc"
                );
                SerialPort::puts("[obj] registry ok: lookup mem:region -> ");
                SerialPort::puts(*name);
                SerialPort::puts("\n");
            }
            _ => panic!("mem:region lookup replied non-str payload"),
        },
        Ok(_) => panic!("mem:region lookup replied unexpected shape"),
        Err(e) => panic!("mem:region lookup failed: {:?}", e),
    }

    // 26. POSITIVE — the table node's `count` hook reports the boot table's
    //     occupied slots through a real capability (infrastructure-as-a-node,
    //     §7.8). Runs before 24 so the count is still pre-delegation.
    match invoke(&table, end.table, TABLE_CONTRACT, TABLE_COUNT, &Args::none()) {
        Ok(Reply::Data(vals)) if vals.len() == 1 => match &vals[0] {
            Value::U64(n) => {
                assert_eq!(*n as usize, table.count(), "table count hook != count()");
                SerialPort::puts("[obj] table ok: count=");
                SerialPort::put_u64(*n);
                SerialPort::puts("\n");
            }
            _ => panic!("table count replied non-u64 payload"),
        },
        Ok(_) => panic!("table count replied unexpected shape"),
        Err(e) => panic!("table count through cap failed: {:?}", e),
    }

    // §3.4.3 / §8.24 — delegation is capability-mediated: the boot domain holds
    // a table cap (INVOKE over `infra:table`), so it hands another domain a
    // rights-preserving clone of one of its own caps through the table node's
    // `delegate` hook. The source handle is untouched; delegation never
    // amplifies.

    // 24. POSITIVE — delegate the boot domain's physmem cap to the driver
    //     domain (id 1) through the `delegate` hook. The driver already holds
    //     its own physmem, so this is a redundant-but-legal second physmem cap
    //     — no capability-category leak. Revoking the clone leaves the source
    //     live. (Deliberately does NOT re-assert drv.table.count() == 4: this
    //     test adds a slot to the driver table, and test 7 already proved the
    //     endowment size.)
    let delegate = Args { vals: vec![Value::U64(1), Value::U64(end.physmem.0)] };
    let new_id = match invoke(&table, end.table, TABLE_CONTRACT, TABLE_DELEGATE, &delegate) {
        Ok(Reply::Data(vals)) if vals.len() == 1 => match &vals[0] {
            Value::U64(id) => CapId(*id),
            _ => panic!("delegate replied non-u64 payload"),
        },
        Ok(_) => panic!("delegate replied unexpected shape"),
        Err(e) => panic!("delegate through cap failed: {:?}", e),
    };
    match drv.table.resolve(new_id, PHYSMEM_CONTRACT, PHYSMEM_STATS) {
        Ok(_) => SerialPort::puts("[obj] delegate ok: driver resolves delegated physmem\n"),
        Err(e) => panic!("delegated physmem refused in driver table: {:?}", e),
    }
    drv.table.revoke(new_id).expect("revoke delegated physmem cap");
    match table.resolve(end.physmem, PHYSMEM_CONTRACT, PHYSMEM_STATS) {
        Ok(_) => SerialPort::puts("[obj] delegate ok: source physmem untouched\n"),
        Err(e) => panic!("source physmem changed after delegation: {:?}", e),
    }

    // 25. NEGATIVE — delegation cannot name an unknown target domain (the hook
    //     refuses `NoSuchCap` before any transfer) and a direct-table delegate
    //     of a bogus id is refused too.
    let r25a = invoke(
        &table,
        end.table,
        TABLE_CONTRACT,
        TABLE_DELEGATE,
        &Args { vals: vec![Value::U64(0xFFFF), Value::U64(end.physmem.0)] },
    );
    expect_err("delegate to unknown domain", r25a, ObjError::NoSuchCap);
    match table.delegate(&boot_domain().table, CapId(u32::MAX as u64)) {
        Err(ObjError::NoSuchCap) => SerialPort::puts("[obj] delegate ok: bogus id refused\n"),
        Ok(_) => panic!("delegate of bogus id succeeded"),
        Err(e) => panic!("delegate of bogus id -> wrong error {:?}", e),
    }

    SerialPort::puts("[obj] driver separation: OK (dma + pci_cfg + physmem + addrspace)\n");

    SerialPort::puts("[obj] separation: OK (cap-mediated alloc enforced)\n");
}

/// CapabilityVfs — post-mount filesystem separation proof (§7.12.3).
///
/// Called from `Kernel::run()` after the tmpfs and ESP mounts. The dir cap
/// must exist in the boot table by now.
pub fn run_post_mount() {
    let table = &boot_domain().table;

    // 16. POSITIVE — find a DirNode cap in the boot table.
    if let Some(dir_id) = table.resolve_first(fs::DIR_CONTRACT, fs::DIR_READDIR) {
        // 17. NEGATIVE — attune to QUERY-only (no INVOKE): readdir must fail
        //     with Denied because PERMIT requires the INVOKE right.
        let query_only = table
            .dup_limited(dir_id, Rights::QUERY, ContractRights::empty())
            .expect("dup_limited for QUERY-only dir cap");
        match invoke(&table, query_only, fs::DIR_CONTRACT, fs::DIR_READDIR, &Args::none()) {
            Err(ObjError::Denied) => {
                SerialPort::puts("[obj] fs sep ok: QUERY-only dir cap readdir -> Denied\n")
            }
            Ok(_) => panic!("fs separation FAIL: QUERY-only dir cap readdir succeeded"),
            Err(e) => panic!("fs separation FAIL: QUERY-only dir cap readdir -> {:?}", e),
        }
    } else {
        SerialPort::puts("[obj] fs sep ok: no dir cap in boot table (skipped)\n");
    }
}

fn expect_err(tag: &str, res: Result<Reply, ObjError>, want: ObjError) {
    match res {
        Err(e) if e == want => {
            SerialPort::puts("[obj] separation ok: ");
            SerialPort::puts(tag);
            SerialPort::puts(" refused\n");
        }
        Err(e) => panic!("separation FAIL: {} -> wrong error {:?}", tag, e),
        Ok(_) => panic!("separation FAIL: {} was not refused", tag),
    }
}