pub mod gdt;
pub mod idt;

/// Initialize x86_64 architecture.
///
/// Called once at kernel startup.
/// Order: GDT then IDT.
pub fn init() {
    gdt::init();
    idt::init();
}
