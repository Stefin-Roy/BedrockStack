//! Interrupt Descriptor Table for x86_64.

use core::sync::atomic::{AtomicPtr, Ordering};

use spin::Once;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};
use x86_64::VirtAddr;

use crate::drivers::serial::SerialPort;
use crate::kerneldump::dump;
use crate::platform::x86_64_pc::apic;

// Placed in .idt section (its own page-aligned area in the linker script)
// so it can be made read-only after init to prevent silent corruption.
#[unsafe(link_section = ".idt")]
static IDT: Once<InterruptDescriptorTable> = Once::new();

// ── IDT integrity canary ─────────────────────────────────────────
// Placed at the end of the .idt page; if anything overwrites it the
// IDT entries are likely corrupted too.  Verified before every IO.
const IDT_GUARD_MAGIC: u64 = 0x1D7_1D7_1D7_1D7;

#[unsafe(link_section = ".idt")]
static IDT_GUARD: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(IDT_GUARD_MAGIC);

pub fn verify_integrity() {
    let g = IDT_GUARD.load(core::sync::atomic::Ordering::Relaxed);
    if g != IDT_GUARD_MAGIC {
        SerialPort::puts("\n[IDT] INTEGRITY CHECK FAILED -- canary overwritten (0x");
        SerialPort::put_hex(g);
        SerialPort::puts(")\n");
        loop { unsafe { core::arch::asm!("cli; hlt"); } }
    }
}

/// Check integrity and return true/false without halting (for diagnostics).
pub fn check_integrity() -> bool {
    let g = IDT_GUARD.load(core::sync::atomic::Ordering::Relaxed);
    g == IDT_GUARD_MAGIC
}

// ── Device interrupt dispatch (vectors 33-48) ─────────────────────
//
// Drivers can register a handler for one of the 16 available device
// interrupt vectors. The handler is called with interrupts disabled
// and must not block. EOI is sent automatically after the handler.

pub const NUM_DEVICE_VECTORS: usize = 16;
pub const DEVICE_VECTOR_BASE: u8 = 33;
static DEVICE_HANDLERS: [AtomicPtr<fn()>; NUM_DEVICE_VECTORS] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; NUM_DEVICE_VECTORS];

/// Register a handler for a device interrupt vector (index 0-15, mapping to
/// IDT vectors 33-48). Returns the allocated vector number or `None` if the
/// slot is already taken.
pub fn register_device_handler(handler: fn()) -> Option<u8> {
    for (i, slot) in DEVICE_HANDLERS.iter().enumerate() {
        if slot.load(Ordering::Acquire).is_null() {
            let ptr = handler as *mut fn();
            if slot.compare_exchange(
                core::ptr::null_mut(), ptr,
                Ordering::Release, Ordering::Relaxed,
            ).is_ok() {
                return Some(DEVICE_VECTOR_BASE + i as u8);
            }
        }
    }
    None
}

/// Register a handler at a specific device interrupt vector.
///
/// Unlike `register_device_handler` which auto-allocates, this stores the
/// handler at the given vector slot directly. Intended for use by the
/// `InterruptManager` capability provider.
pub fn register_device_handler_at(vector: u8, handler: fn()) {
    if vector < DEVICE_VECTOR_BASE || vector >= DEVICE_VECTOR_BASE + NUM_DEVICE_VECTORS as u8 {
        return;
    }
    let idx = (vector - DEVICE_VECTOR_BASE) as usize;
    DEVICE_HANDLERS[idx].store(handler as *mut fn(), Ordering::Release);
}

/// Unregister a previously registered device interrupt handler.
pub fn unregister_device_handler(vector: u8) {
    if vector < DEVICE_VECTOR_BASE || vector >= DEVICE_VECTOR_BASE + NUM_DEVICE_VECTORS as u8 {
        return;
    }
    let idx = (vector - DEVICE_VECTOR_BASE) as usize;
    DEVICE_HANDLERS[idx].store(core::ptr::null_mut(), Ordering::Release);
}

/// Callback invoked by every CPU's APIC timer ISR before EOI.
static TIMER_CALLBACK: AtomicPtr<fn()> = AtomicPtr::new(core::ptr::null_mut());

/// Set the function called on each APIC timer interrupt.
///
/// Used by `UniversalTimer` to wire up queue processing and clockevent
/// reprogramming for the current CPU's base.
pub fn set_timer_callback(cb: fn()) {
    TIMER_CALLBACK.store(cb as *mut fn(), Ordering::Release);
}

/// Callback invoked by the reschedule IPI ISR (vector 52) before EOI.
static TIMER_IPI_CALLBACK: AtomicPtr<fn()> = AtomicPtr::new(core::ptr::null_mut());

/// Set the function called on each timer-reschedule IPI.
///
/// Used by `UniversalTimer` to ask a remote CPU to re-run `tick()` on its
/// own base after a cross-CPU earliest-deadline change.
pub fn set_timer_ipi_callback(cb: fn()) {
    TIMER_IPI_CALLBACK.store(cb as *mut fn(), Ordering::Release);
}

fn device_irq_handler(vector: u8) {
    let idx = (vector - DEVICE_VECTOR_BASE) as usize;
    if idx < NUM_DEVICE_VECTORS {
        let ptr = DEVICE_HANDLERS[idx].load(Ordering::Acquire);
        if !ptr.is_null() {
            let handler: fn() = unsafe { core::mem::transmute(ptr) };
            handler();
        }
    }
    apic::apic_eoi();
}

extern "x86-interrupt" fn irq_33(_sf: InterruptStackFrame) { device_irq_handler(33); }
static mut VEC34_COUNT: u64 = 0;
pub fn vec34_count() -> u64 { unsafe { VEC34_COUNT } }

extern "x86-interrupt" fn irq_34(_sf: InterruptStackFrame) {
    unsafe { VEC34_COUNT += 1; }
    device_irq_handler(34);
}
extern "x86-interrupt" fn irq_35(_sf: InterruptStackFrame) { device_irq_handler(35); }
extern "x86-interrupt" fn irq_36(_sf: InterruptStackFrame) { device_irq_handler(36); }
extern "x86-interrupt" fn irq_37(_sf: InterruptStackFrame) { device_irq_handler(37); }
extern "x86-interrupt" fn irq_38(_sf: InterruptStackFrame) { device_irq_handler(38); }
extern "x86-interrupt" fn irq_39(_sf: InterruptStackFrame) { device_irq_handler(39); }
extern "x86-interrupt" fn irq_40(_sf: InterruptStackFrame) { device_irq_handler(40); }
extern "x86-interrupt" fn irq_41(_sf: InterruptStackFrame) { device_irq_handler(41); }
extern "x86-interrupt" fn irq_42(_sf: InterruptStackFrame) { device_irq_handler(42); }
extern "x86-interrupt" fn irq_43(_sf: InterruptStackFrame) { device_irq_handler(43); }
extern "x86-interrupt" fn irq_44(_sf: InterruptStackFrame) { device_irq_handler(44); }
extern "x86-interrupt" fn irq_45(_sf: InterruptStackFrame) { device_irq_handler(45); }
extern "x86-interrupt" fn irq_46(_sf: InterruptStackFrame) { device_irq_handler(46); }
extern "x86-interrupt" fn irq_47(_sf: InterruptStackFrame) { device_irq_handler(47); }
extern "x86-interrupt" fn irq_48(_sf: InterruptStackFrame) { device_irq_handler(48); }

/// Initialize and load the IDT.
///
/// # Safety
/// Must be called after GDT init (the double-fault handler relies on the IST
/// entry configured there).
pub fn init() {
    let idt = IDT.call_once(|| {
        let mut idt = InterruptDescriptorTable::new();

        idt.divide_error.set_handler_fn(divide_error_handler).disable_interrupts(true);
        idt.breakpoint.set_handler_fn(breakpoint_handler).disable_interrupts(true);
        idt.invalid_opcode.set_handler_fn(invalid_opcode_handler).disable_interrupts(true);
        idt.invalid_tss.set_handler_fn(invalid_tss_handler).disable_interrupts(true);
        idt.segment_not_present
            .set_handler_fn(segment_not_present_handler).disable_interrupts(true);
        idt.stack_segment_fault
            .set_handler_fn(stack_segment_fault_handler).disable_interrupts(true);
        idt.general_protection_fault.set_handler_fn(gpf_handler).disable_interrupts(true);
        idt.page_fault.set_handler_fn(page_fault_handler).disable_interrupts(true);

        unsafe {
            idt.double_fault
                .set_handler_fn(double_fault_handler)
                .set_stack_index(crate::arch::x86_64::gdt::DOUBLE_FAULT_IST_INDEX)
                .disable_interrupts(true);
        }

        // Register APIC timer interrupt at vector 32 (interrupt gate, clears IF).
        idt[32].set_handler_fn(timer_handler).disable_interrupts(true);

        // Register the timer-reschedule IPI at vector 52 (interrupt gate).
        idt[52].set_handler_fn(ipi_timer_handler).disable_interrupts(true);

        // Register the cross-CPU TLB-shootdown IPI at vector 50 (interrupt
        // gate).  Every online CPU flushes its TLB and acknowledges here, so
        // the initiator can safely release unmapped frames to the allocator.
        idt[50].set_handler_fn(ipi_tlb_shootdown_handler).disable_interrupts(true);

        // Register device interrupt vectors 33-48 (interrupt gates, clears IF).
        idt[33].set_handler_fn(irq_33).disable_interrupts(true);
        idt[34].set_handler_fn(irq_34).disable_interrupts(true);
        idt[35].set_handler_fn(irq_35).disable_interrupts(true);
        idt[36].set_handler_fn(irq_36).disable_interrupts(true);
        idt[37].set_handler_fn(irq_37).disable_interrupts(true);
        idt[38].set_handler_fn(irq_38).disable_interrupts(true);
        idt[39].set_handler_fn(irq_39).disable_interrupts(true);
        idt[40].set_handler_fn(irq_40).disable_interrupts(true);
        idt[41].set_handler_fn(irq_41).disable_interrupts(true);
        idt[42].set_handler_fn(irq_42).disable_interrupts(true);
        idt[43].set_handler_fn(irq_43).disable_interrupts(true);
        idt[44].set_handler_fn(irq_44).disable_interrupts(true);
        idt[45].set_handler_fn(irq_45).disable_interrupts(true);
        idt[46].set_handler_fn(irq_46).disable_interrupts(true);
        idt[47].set_handler_fn(irq_47).disable_interrupts(true);
        idt[48].set_handler_fn(irq_48).disable_interrupts(true);

        idt
    });

    idt.load();

    // Arm integrity canary — must stay alive through every IO operation.
    IDT_GUARD.store(IDT_GUARD_MAGIC, core::sync::atomic::Ordering::Relaxed);
}

/// Timer interrupt handler (vector 32).
///
/// Runs on whichever CPU's LAPIC fired.  Each CPU processes its own
/// universal-timer base (drains expired timers, reprograms the LAPIC
/// one-shot), then signals EOI.
extern "x86-interrupt" fn timer_handler(_stack_frame: InterruptStackFrame) {
    let ptr = TIMER_CALLBACK.load(Ordering::Acquire);
    if !ptr.is_null() {
        let handler: fn() = unsafe { core::mem::transmute(ptr) };
        handler();
    }
    apic::apic_eoi();
}

/// Timer-reschedule IPI handler (vector 52).
///
/// Sent by a remote CPU when this CPU's base got an earlier earliest
/// deadline.  Runs the same tick routine so the local base re-arms, then
/// signals EOI.  A missed IPI is non-fatal — the LAPIC is still armed for
/// the previous earliest and `tick()` re-arms after it fires.
extern "x86-interrupt" fn ipi_timer_handler(_stack_frame: InterruptStackFrame) {
    let ptr = TIMER_IPI_CALLBACK.load(Ordering::Acquire);
    if !ptr.is_null() {
        let handler: fn() = unsafe { core::mem::transmute(ptr) };
        handler();
    }
    apic::apic_eoi();
}

/// Cross-CPU TLB-shootdown IPI handler (vector 50).
///
/// Flushes this CPU's entire TLB and acknowledges the shootdown generation, so
/// the initiating CPU may then release the just-unmapped frames to the
/// allocator.  Runs with interrupts disabled (interrupt gate); does not touch
/// any VMM lock, so it can never deadlock against an in-progress shootdown.
extern "x86-interrupt" fn ipi_tlb_shootdown_handler(_stack_frame: InterruptStackFrame) {
    crate::mm::vmm::tlb_shootdown_on_this_cpu();
    apic::apic_eoi();
}

extern "x86-interrupt" fn divide_error_handler(frame: InterruptStackFrame) {
    let rsp: u64;
    unsafe { core::arch::asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack)); }
    crate::drivers::serial::dump_puts("[#DE] RSP = 0x");
    crate::drivers::serial::dump_put_hex(rsp);
    crate::drivers::serial::dump_puts("\n");
    crate::drivers::serial::dump_puts("[#DE] frame RIP = 0x");
    crate::drivers::serial::dump_put_hex(frame.instruction_pointer.as_u64());
    crate::drivers::serial::dump_puts("\n");
    crate::kerneldump::dump_full_fault(&frame, 0, 0);
}

extern "x86-interrupt" fn breakpoint_handler(frame: InterruptStackFrame) {
    crate::kerneldump::dump_full_fault(&frame, 0, 3);
}

extern "x86-interrupt" fn invalid_opcode_handler(frame: InterruptStackFrame) {
    crate::kerneldump::dump_full_fault(&frame, 0, 6);
}

extern "x86-interrupt" fn invalid_tss_handler(frame: InterruptStackFrame, error_code: u64) {
    crate::kerneldump::dump_full_fault(&frame, error_code, 10);
}

extern "x86-interrupt" fn segment_not_present_handler(
    frame: InterruptStackFrame,
    error_code: u64,
) {
    crate::kerneldump::dump_full_fault(&frame, error_code, 11);
}

extern "x86-interrupt" fn stack_segment_fault_handler(
    frame: InterruptStackFrame,
    error_code: u64,
) {
    crate::kerneldump::dump_full_fault(&frame, error_code, 12);
}

extern "x86-interrupt" fn double_fault_handler(
    frame: InterruptStackFrame,
    error_code: u64,
) -> ! {
    crate::kerneldump::dump_full_fault(&frame, error_code, 8);
}

extern "x86-interrupt" fn page_fault_handler(
    mut frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    // ── PF recovery during a kernel dump ───────────────────────────
    if dump::is_dump_in_progress() {
        let target = dump::PF_RECOVERY_RIP.load(Ordering::Relaxed);
        if target != 0 {
            let cr2: u64;
            unsafe { core::arch::asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack)); }
            dump::PF_FAULT_ADDR.store(cr2, Ordering::Relaxed);
            dump::PF_ERROR_CODE.store(error_code.bits(), Ordering::Relaxed);
            unsafe {
                frame
                    .as_mut()
                    .update(|val| val.instruction_pointer = VirtAddr::new(target));
            }
            return;
        }
        crate::drivers::serial::dump_puts(
            "\n[DUMP] Nested PF during dump (no recovery target) — halting\n",
        );
        loop { unsafe { core::arch::asm!("cli; hlt", options(nomem, nostack)); } }
    }

    crate::kerneldump::dump_full_fault(&frame, error_code.bits(), 14);
}

extern "x86-interrupt" fn gpf_handler(frame: InterruptStackFrame, error_code: u64) {
    let ecx: u32;
    unsafe { core::arch::asm!("mov {0:e}, ecx", out(reg) ecx); }
    crate::drivers::serial::dump_puts("[GPF] ECX (MSR) = 0x");
    crate::drivers::serial::dump_put_hex(ecx as u64);
    crate::drivers::serial::dump_puts("   RIP = 0x");
    crate::drivers::serial::dump_put_hex(frame.instruction_pointer.as_u64());
    crate::drivers::serial::dump_puts("\n");
    crate::kerneldump::dump_full_fault(&frame, error_code, 13);
}

/// Make the .idt pages read-only in the page tables so any wild write
/// to the IDT or its canary triggers an immediate page fault.
/// Must be called after all IDT initialisation is complete.
///
/// `idt_start` / `idt_end` are the kernel-region VMAs of the `.idt` section
/// (from `KernelLayout`, which now holds higher-half link addresses); the
/// kernel is mapped once at `KERNEL_VMA`, so these are the VAs to protect.
pub fn protect_idt(root: u64, idt_start: u64, idt_end: u64) {
    let mut page = idt_start & !0xFFF;
    let end = (idt_end + 0xFFF) & !0xFFF;
    while page < end {
        crate::mm::vmm::make_read_only(root, page);
        page += 0x1000;
    }
    // Flush TLB after modifying page tables.
    use core::sync::atomic::Ordering;
    core::sync::atomic::fence(Ordering::SeqCst);
    unsafe { core::arch::asm!("mov rax, cr3; mov cr3, rax", options(nostack, preserves_flags)); }
}

/// Reload the IDT on an Application Processor (IDTR is per-CPU).
///
/// Must be called after the BSP has called `init()` and before the AP
/// enables interrupts.
pub fn init_ap() {
    let idt = IDT.get().expect("IDT not initialised on BSP");
    idt.load();
}
