// CLINT registers are at 0x02000000 but PMP-protected by OpenSBI
// — they are NOT accessible from S-mode. Use SBI ecalls instead.
