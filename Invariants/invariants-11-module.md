# Module System — Invariants

**Version:** 0.5.0
**Date:** 2026-08-01
**Source:** `kernel/src/module/{mod,registry,fat32_test,fat32_ls,msix_test,usb_test,ps2_test,vfs_test}.rs`
**Status:** Stable

---

## State Invariants

**MOD-001 — Each module is initialized at most once:**
`init_all()` iterates `MODULES` linearly, calling `module.init(display)`
for each. No module registers itself or is registered dynamically.
- Location: `kernel/src/module/registry.rs:34-47`

**MOD-002 — If a module fails to init, subsequent modules are skipped:**
The loop `break`s after the first `Err(msg)`, logging the failure.
- Location: `kernel/src/module/registry.rs:36-46`

**MOD-003 — Module name/version are `'static` string slices:**
All module metadata is compile-time constant.
- Location: `kernel/src/module/registry.rs:13-18`

**MOD-004 — Registry lists `Fat32Ls` on both arches, x86_64 adds `MsixTest`, `UsbTest`, and `Ps2Test`:**
- `#[cfg(target_arch = "x86_64")]`: `[HelloModule, Fat32Test, MsixTest, UsbTest, Fat32Ls, VfsTest, Ps2Test]`
- otherwise: `[HelloModule, Fat32Test, Fat32Ls, VfsTest]`
`Fat32Ls` (lists the FAT32 root directory on `B>`) was added in the block/FS
registry work; it runs on riscv64 too but is skipped gracefully when no
ESP is mounted. `Ps2Test` (interactive keyboard echo, halts on Esc) is x86_64
only and is deliberately **last**: its `init()` never returns once a keyboard
is present, so it is the terminal step of the boot sequence.
- Location: `kernel/src/module/registry.rs:37-47`

---

## API Contracts

**MOD-API-001 — `Module` trait:**
```rust
pub trait Module: Sync {
    fn name(&self) -> &str;
    fn version(&self) -> &str;
    fn init(&self, display: &mut Framebuffer) -> Result<(), &'static str>;
}
```
- `name()` must return valid UTF-8.
- `init()` may only mutate the provided `display` reference.
- `init()` is called once during kernel startup.
- Location: `kernel/src/module/mod.rs:12-23`

---

## Design Notes

- Modules are statically defined (not dynamically loaded). The `MODULES`
  slice is built at compile time.
- The `VfsTest` module exercises the VFS subsystem during kernel init.
- No module unloading is supported.
