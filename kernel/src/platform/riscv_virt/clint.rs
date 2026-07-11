// CLINT (Core Local Interruptor) memory map for QEMU riscv-virt.
//
// These registers are at physical address 0x02000000 but are
// **PMP-protected by OpenSBI** — they are NOT accessible from S-mode.
// All timer and IPI operations MUST go through SBI ecalls.
//
// Use the SBI interface (crate::arch::riscv64::sbi) instead:
//   - Timer:  sbi::set_timer(deadline)
//   - IPI:    sbi::send_ipi(hart_mask)
//
// The constants below are for reference / documentation purposes.
//
// Register layout per hart:
//   msip[hart] (u32) at CLINT_BASE + 0x0000 + hart*4
//   mtimecmp[hart] (u64) at CLINT_BASE + 0x4000 + hart*8
//   mtime (u64)            at CLINT_BASE + 0xBFF8  (read-only via `time` CSR)

pub const CLINT_BASE: u64 = 0x02000000;
pub const CLINT_MSIP_OFFSET: u64 = 0x0000;
pub const CLINT_MTIMECMP_OFFSET: u64 = 0x4000;
pub const CLINT_MTIME_OFFSET: u64 = 0xBFF8;
