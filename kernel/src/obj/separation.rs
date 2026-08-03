//! C6 — boot-time capability-separation test (§6.1, §7.5).
//!
//! Runs exactly once from `Kernel::init()` right after bootstrap and before
//! SMP. It proves the DMA provider is reachable only through the boot table:
//! a real allocation succeeds through the endowed `CapId`, an unendowed id is
//! refused with `NoSuchCap`, and a contract the DMA node does not implement is
//! refused by PERMIT with `ObjError::Denied`. Arch-neutral.

extern crate alloc;

use alloc::vec;

use super::adapters::{self, DMA_ALLOC_PAGE};
use super::bootstrap::{boot_domain, boot_endowment};
use super::cap_handle::CapId;
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

    // C8 — first driver domain: disjoint table endowed only with dma+pci_cfg
    // (§6.2, §8.14). The device sweep runs under this domain; it must be able
    // to resolve exactly its endowment, and nothing the boot domain kept.
    let drv = crate::obj::driver::driver_domain();
    let dend = crate::obj::driver::driver_endowment();

    // 4. Endowed DMA resolves from the driver table (PERMIT passes).
    match drv.table.resolve(dend.dma, adapters::DMA_CONTRACT) {
        Ok(_) => SerialPort::puts("[obj] driver sep ok: endowed dma resolves\n"),
        Err(e) => panic!("driver separation FAIL: endowed dma refused: {:?}", e),
    }

    // 5. Endowed PCI-config resolves from the driver table.
    match drv.table.resolve(dend.pci_cfg, adapters::PCI_CONTRACT) {
        Ok(_) => SerialPort::puts("[obj] driver sep ok: endowed pci_cfg resolves\n"),
        Err(e) => panic!("driver separation FAIL: endowed pci_cfg refused: {:?}", e),
    }

    // 6. NEGATIVE — an unendowed id cannot reach DMA from the driver table.
    match drv.table.resolve(CapId(u32::MAX as u64), adapters::DMA_CONTRACT) {
        Err(ObjError::NoSuchCap) => SerialPort::puts("[obj] driver sep ok: unendowed id refused\n"),
        Ok(_) => panic!("driver separation FAIL: unendowed id resolved"),
        Err(e) => panic!("driver separation FAIL: unendowed id -> {:?}", e),
    }

    // 7. The driver domain holds exactly its endowment: two slots, no more.
    //    A driver table of size two cannot name a serial or heap cap the boot
    //    domain kept for itself (§8.14: the kernel cannot reach the driver's
    //    addresses by position — and vice versa).
    assert_eq!(drv.table.count(), 2, "driver domain must hold exactly dma + pci_cfg");

    // 8. NEGATIVE — serial is not reachable through the driver domain at all:
    //    the endowed DMA node simply does not implement the serial contract.
    match drv.table.resolve(dend.dma, adapters::SERIAL_CONTRACT) {
        Err(ObjError::Denied) => SerialPort::puts("[obj] driver sep ok: serial not reachable\n"),
        Ok(_) => panic!("driver separation FAIL: dma node implemented serial contract"),
        Err(e) => panic!("driver separation FAIL: serial probe -> {:?}", e),
    }

    SerialPort::puts("[obj] driver separation: OK (dma + pci_cfg only)\n");

    SerialPort::puts("[obj] separation: OK (cap-mediated alloc enforced)\n");
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