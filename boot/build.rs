fn main() {
    // Kernel is now loaded from disk at runtime, not embedded.
    // The kernel binary must be built separately before creating the FAT32 image.
    println!("cargo:rerun-if-changed=../kernel/src");
}
