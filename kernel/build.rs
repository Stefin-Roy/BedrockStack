fn main() {
    // cargo tracks Cargo.toml/Cargo.lock itself, but not the linker scripts which are
    // injected via `rustflags = ["-C", "link-arg=-Tkernel/linker.ld"]` in `.cargo/config.toml`.
    // Emit explicit rerun-if-changed so edits to the scripts correctly invalidate the crate.
    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rerun-if-changed=linker-riscv64.ld");
    println!("cargo:rerun-if-changed=../.cargo/config.toml");
}
