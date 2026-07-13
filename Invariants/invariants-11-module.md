# Module System — Invariants

**Version:** 0.2.0
**Source:** `kernel/src/module/{mod,registry,vfs_test}.rs`
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
