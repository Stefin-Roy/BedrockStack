//! Interrupt Descriptor Table for x86_64.

use core::sync::atomic::{AtomicPtr, Ordering};

use spin::Once;
use x86_64::VirtAddr;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

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
        loop {
            unsafe {
                core::arch::asm!("cli; hlt");
            }
        }
    }
}

/// Check integrity and return true/false without halting (for diagnostics).
pub fn check_integrity() -> bool {
    let g = IDT_GUARD.load(core::sync::atomic::Ordering::Relaxed);
    g == IDT_GUARD_MAGIC
}

// ── User-mode GS guards ────────────────────────────────────────────
//
// Invariant: the kernel runs with GS.base = the current PerCpu and
// IA32_KERNEL_GS_BASE holding the user GS.  An interrupt taken from user
// mode therefore arrives with GS.base = user GS and must swapgs on entry
// to reach PerCpu state, then swapgs again before returning.  All boot
// frames are RPL0, so these guards no-op during boot.

/// True when the interrupt frame was pushed from ring 3 (user mode).
fn from_user(frame: &InterruptStackFrame) -> bool {
    frame.code_segment.rpl() == x86_64::PrivilegeLevel::Ring3
}

/// Toggle GS.base / IA32_KERNEL_GS_BASE.
fn swapgs() {
    unsafe { core::arch::asm!("swapgs", options(nomem, nostack, preserves_flags)) }
}

// ── Device interrupt dispatch (vectors 33-239) ─────────────────────
//
// Drivers can register a handler for one of the 207 available device
// interrupt vectors. The handler is called with interrupts disabled
// and must not block. EOI is sent automatically after the handler.
// Device vectors cover 33..239 inclusive (207 vectors) – the maximal
// contiguous range before the system vectors at 240-255. The 16-vector
// limit was an artificial SW cap; x86 allows 224 external vectors
// (32-255), IOAPIC and MSI can target any vector in that range.

pub const NUM_DEVICE_VECTORS: usize = 207;
pub const DEVICE_VECTOR_BASE: u8 = 33;
pub const DEVICE_VECTOR_END: u8 = 240; // exclusive
static DEVICE_HANDLERS: [AtomicPtr<fn()>; NUM_DEVICE_VECTORS] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; NUM_DEVICE_VECTORS];

// System vectors above the device window (highest priority, 0xF0+).
pub const IPI_RESCHEDULE_VECTOR: u8 = 240;
pub const IPI_TLB_SHOOTDOWN_VECTOR: u8 = 241;
pub const IPI_HALT_VECTOR: u8 = 242;
pub const IPI_TIMER_VECTOR: u8 = 243;
pub const IOMMU_FAULT_VECTOR: u8 = 244;
pub const IOMMU_QI_VECTOR: u8 = 245;

/// Register a handler for a device interrupt vector (index 0-206, mapping to
/// IDT vectors 33-239). Returns the allocated vector number or `None` if the
/// slot is already taken.
pub fn register_device_handler(handler: fn()) -> Option<u8> {
    for (i, slot) in DEVICE_HANDLERS.iter().enumerate() {
        if slot.load(Ordering::Acquire).is_null() {
            let ptr = handler as *mut fn();
            if slot
                .compare_exchange(
                    core::ptr::null_mut(),
                    ptr,
                    Ordering::Release,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
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

/// Register `count` contiguous handlers atomically, aligned to `count` when
/// `count` is power-of-two (MSI requirement: vector % count == 0). Returns
/// base vector on success.
pub fn register_device_handlers(count: usize, handler: fn()) -> Option<u8> {
    if count == 0 || count > NUM_DEVICE_VECTORS {
        return None;
    }
    let need_align = count.is_power_of_two();
    let max_start = NUM_DEVICE_VECTORS - count;
    for start in 0..=max_start {
        let base_vec = DEVICE_VECTOR_BASE as usize + start;
        if need_align && (base_vec % count != 0) {
            continue;
        }
        // Fast check: all free?
        let mut all_free = true;
        for j in 0..count {
            if !DEVICE_HANDLERS[start + j].load(Ordering::Acquire).is_null() {
                all_free = false;
                break;
            }
        }
        if !all_free {
            continue;
        }
        // Try to claim each slot with CAS; rollback on failure.
        let ptr = handler as *mut fn();
        let mut claimed = 0usize;
        for j in 0..count {
            let slot = &DEVICE_HANDLERS[start + j];
            if slot
                .compare_exchange(
                    core::ptr::null_mut(),
                    ptr,
                    Ordering::Release,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                claimed += 1;
            } else {
                break;
            }
        }
        if claimed == count {
            return Some(base_vec as u8);
        }
        // rollback partial
        for j in 0..claimed {
            DEVICE_HANDLERS[start + j].store(core::ptr::null_mut(), Ordering::Release);
        }
    }
    None
}

/// Unregister `count` contiguous vectors starting at `base`.
pub fn unregister_device_handlers(base: u8, count: usize) {
    if count == 0 {
        return;
    }
    if base < DEVICE_VECTOR_BASE || (base as usize + count) > DEVICE_VECTOR_END as usize {
        return;
    }
    let start = (base - DEVICE_VECTOR_BASE) as usize;
    for j in 0..count {
        if start + j < NUM_DEVICE_VECTORS {
            DEVICE_HANDLERS[start + j].store(core::ptr::null_mut(), Ordering::Release);
        }
    }
}

/// Number of currently allocated device vectors (diagnostic).
pub fn allocated_device_vectors() -> usize {
    DEVICE_HANDLERS
        .iter()
        .filter(|s| !s.load(Ordering::Acquire).is_null())
        .count()
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

/// Callback invoked by the reschedule IPI ISR (vector 243) before EOI.
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

macro_rules! device_irq_guard {
    ($fnname:ident, $vec:expr) => {
        extern "x86-interrupt" fn $fnname(frame: InterruptStackFrame) {
            let u = from_user(&frame);
            if u {
                swapgs();
            }
            device_irq_handler($vec);
            // Full preemption: check for pending reschedule after every device IRQ,
            // after EOI so switched-away context does not leave unacked LAPIC.
            crate::task::try_preempt_from_irq(u);
            if u {
                swapgs();
            }
        }
    };
}

device_irq_guard!(irq_33, 33);
device_irq_guard!(irq_35, 35);
device_irq_guard!(irq_36, 36);
device_irq_guard!(irq_37, 37);
device_irq_guard!(irq_38, 38);
device_irq_guard!(irq_39, 39);
device_irq_guard!(irq_40, 40);
device_irq_guard!(irq_41, 41);
device_irq_guard!(irq_42, 42);
device_irq_guard!(irq_43, 43);
device_irq_guard!(irq_44, 44);
device_irq_guard!(irq_45, 45);
device_irq_guard!(irq_46, 46);
device_irq_guard!(irq_47, 47);
device_irq_guard!(irq_48, 48);
device_irq_guard!(irq_49, 49);
device_irq_guard!(irq_50, 50);
device_irq_guard!(irq_51, 51);
device_irq_guard!(irq_52, 52);
device_irq_guard!(irq_53, 53);
device_irq_guard!(irq_54, 54);
device_irq_guard!(irq_55, 55);
device_irq_guard!(irq_56, 56);
device_irq_guard!(irq_57, 57);
device_irq_guard!(irq_58, 58);
device_irq_guard!(irq_59, 59);
device_irq_guard!(irq_60, 60);
device_irq_guard!(irq_61, 61);
device_irq_guard!(irq_62, 62);
device_irq_guard!(irq_63, 63);
device_irq_guard!(irq_64, 64);
device_irq_guard!(irq_65, 65);
device_irq_guard!(irq_66, 66);
device_irq_guard!(irq_67, 67);
device_irq_guard!(irq_68, 68);
device_irq_guard!(irq_69, 69);
device_irq_guard!(irq_70, 70);
device_irq_guard!(irq_71, 71);
device_irq_guard!(irq_72, 72);
device_irq_guard!(irq_73, 73);
device_irq_guard!(irq_74, 74);
device_irq_guard!(irq_75, 75);
device_irq_guard!(irq_76, 76);
device_irq_guard!(irq_77, 77);
device_irq_guard!(irq_78, 78);
device_irq_guard!(irq_79, 79);
device_irq_guard!(irq_80, 80);
device_irq_guard!(irq_81, 81);
device_irq_guard!(irq_82, 82);
device_irq_guard!(irq_83, 83);
device_irq_guard!(irq_84, 84);
device_irq_guard!(irq_85, 85);
device_irq_guard!(irq_86, 86);
device_irq_guard!(irq_87, 87);
device_irq_guard!(irq_88, 88);
device_irq_guard!(irq_89, 89);
device_irq_guard!(irq_90, 90);
device_irq_guard!(irq_91, 91);
device_irq_guard!(irq_92, 92);
device_irq_guard!(irq_93, 93);
device_irq_guard!(irq_94, 94);
device_irq_guard!(irq_95, 95);
device_irq_guard!(irq_96, 96);
device_irq_guard!(irq_97, 97);
device_irq_guard!(irq_98, 98);
device_irq_guard!(irq_99, 99);
device_irq_guard!(irq_100, 100);
device_irq_guard!(irq_101, 101);
device_irq_guard!(irq_102, 102);
device_irq_guard!(irq_103, 103);
device_irq_guard!(irq_104, 104);
device_irq_guard!(irq_105, 105);
device_irq_guard!(irq_106, 106);
device_irq_guard!(irq_107, 107);
device_irq_guard!(irq_108, 108);
device_irq_guard!(irq_109, 109);
device_irq_guard!(irq_110, 110);
device_irq_guard!(irq_111, 111);
device_irq_guard!(irq_112, 112);
device_irq_guard!(irq_113, 113);
device_irq_guard!(irq_114, 114);
device_irq_guard!(irq_115, 115);
device_irq_guard!(irq_116, 116);
device_irq_guard!(irq_117, 117);
device_irq_guard!(irq_118, 118);
device_irq_guard!(irq_119, 119);
device_irq_guard!(irq_120, 120);
device_irq_guard!(irq_121, 121);
device_irq_guard!(irq_122, 122);
device_irq_guard!(irq_123, 123);
device_irq_guard!(irq_124, 124);
device_irq_guard!(irq_125, 125);
device_irq_guard!(irq_126, 126);
device_irq_guard!(irq_127, 127);
device_irq_guard!(irq_128, 128);
device_irq_guard!(irq_129, 129);
device_irq_guard!(irq_130, 130);
device_irq_guard!(irq_131, 131);
device_irq_guard!(irq_132, 132);
device_irq_guard!(irq_133, 133);
device_irq_guard!(irq_134, 134);
device_irq_guard!(irq_135, 135);
device_irq_guard!(irq_136, 136);
device_irq_guard!(irq_137, 137);
device_irq_guard!(irq_138, 138);
device_irq_guard!(irq_139, 139);
device_irq_guard!(irq_140, 140);
device_irq_guard!(irq_141, 141);
device_irq_guard!(irq_142, 142);
device_irq_guard!(irq_143, 143);
device_irq_guard!(irq_144, 144);
device_irq_guard!(irq_145, 145);
device_irq_guard!(irq_146, 146);
device_irq_guard!(irq_147, 147);
device_irq_guard!(irq_148, 148);
device_irq_guard!(irq_149, 149);
device_irq_guard!(irq_150, 150);
device_irq_guard!(irq_151, 151);
device_irq_guard!(irq_152, 152);
device_irq_guard!(irq_153, 153);
device_irq_guard!(irq_154, 154);
device_irq_guard!(irq_155, 155);
device_irq_guard!(irq_156, 156);
device_irq_guard!(irq_157, 157);
device_irq_guard!(irq_158, 158);
device_irq_guard!(irq_159, 159);
device_irq_guard!(irq_160, 160);
device_irq_guard!(irq_161, 161);
device_irq_guard!(irq_162, 162);
device_irq_guard!(irq_163, 163);
device_irq_guard!(irq_164, 164);
device_irq_guard!(irq_165, 165);
device_irq_guard!(irq_166, 166);
device_irq_guard!(irq_167, 167);
device_irq_guard!(irq_168, 168);
device_irq_guard!(irq_169, 169);
device_irq_guard!(irq_170, 170);
device_irq_guard!(irq_171, 171);
device_irq_guard!(irq_172, 172);
device_irq_guard!(irq_173, 173);
device_irq_guard!(irq_174, 174);
device_irq_guard!(irq_175, 175);
device_irq_guard!(irq_176, 176);
device_irq_guard!(irq_177, 177);
device_irq_guard!(irq_178, 178);
device_irq_guard!(irq_179, 179);
device_irq_guard!(irq_180, 180);
device_irq_guard!(irq_181, 181);
device_irq_guard!(irq_182, 182);
device_irq_guard!(irq_183, 183);
device_irq_guard!(irq_184, 184);
device_irq_guard!(irq_185, 185);
device_irq_guard!(irq_186, 186);
device_irq_guard!(irq_187, 187);
device_irq_guard!(irq_188, 188);
device_irq_guard!(irq_189, 189);
device_irq_guard!(irq_190, 190);
device_irq_guard!(irq_191, 191);
device_irq_guard!(irq_192, 192);
device_irq_guard!(irq_193, 193);
device_irq_guard!(irq_194, 194);
device_irq_guard!(irq_195, 195);
device_irq_guard!(irq_196, 196);
device_irq_guard!(irq_197, 197);
device_irq_guard!(irq_198, 198);
device_irq_guard!(irq_199, 199);
device_irq_guard!(irq_200, 200);
device_irq_guard!(irq_201, 201);
device_irq_guard!(irq_202, 202);
device_irq_guard!(irq_203, 203);
device_irq_guard!(irq_204, 204);
device_irq_guard!(irq_205, 205);
device_irq_guard!(irq_206, 206);
device_irq_guard!(irq_207, 207);
device_irq_guard!(irq_208, 208);
device_irq_guard!(irq_209, 209);
device_irq_guard!(irq_210, 210);
device_irq_guard!(irq_211, 211);
device_irq_guard!(irq_212, 212);
device_irq_guard!(irq_213, 213);
device_irq_guard!(irq_214, 214);
device_irq_guard!(irq_215, 215);
device_irq_guard!(irq_216, 216);
device_irq_guard!(irq_217, 217);
device_irq_guard!(irq_218, 218);
device_irq_guard!(irq_219, 219);
device_irq_guard!(irq_220, 220);
device_irq_guard!(irq_221, 221);
device_irq_guard!(irq_222, 222);
device_irq_guard!(irq_223, 223);
device_irq_guard!(irq_224, 224);
device_irq_guard!(irq_225, 225);
device_irq_guard!(irq_226, 226);
device_irq_guard!(irq_227, 227);
device_irq_guard!(irq_228, 228);
device_irq_guard!(irq_229, 229);
device_irq_guard!(irq_230, 230);
device_irq_guard!(irq_231, 231);
device_irq_guard!(irq_232, 232);
device_irq_guard!(irq_233, 233);
device_irq_guard!(irq_234, 234);
device_irq_guard!(irq_235, 235);
device_irq_guard!(irq_236, 236);
device_irq_guard!(irq_237, 237);
device_irq_guard!(irq_238, 238);
device_irq_guard!(irq_239, 239);
static VEC34_COUNT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub fn vec34_count() -> u64 {
    VEC34_COUNT.load(core::sync::atomic::Ordering::Relaxed)
}

static PF_COUNT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub fn pf_count() -> u64 {
    PF_COUNT.load(core::sync::atomic::Ordering::Relaxed)
}

extern "x86-interrupt" fn irq_34(frame: InterruptStackFrame) {
    let u = from_user(&frame);
    if u {
        swapgs();
    }
    VEC34_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    device_irq_handler(34);
    crate::task::try_preempt_from_irq(u);
    if u {
        swapgs();
    }
}

/// Initialize and load the IDT.
///
/// # Safety
/// Must be called after GDT init (the double-fault handler relies on the IST
/// entry configured there).
pub fn init() {
    let idt = IDT.call_once(|| {
        let mut idt = InterruptDescriptorTable::new();

        idt.divide_error
            .set_handler_fn(divide_error_handler)
            .disable_interrupts(true);
        idt.breakpoint
            .set_handler_fn(breakpoint_handler)
            .disable_interrupts(true);
        idt.invalid_opcode
            .set_handler_fn(invalid_opcode_handler)
            .disable_interrupts(true);
        idt.invalid_tss
            .set_handler_fn(invalid_tss_handler)
            .disable_interrupts(true);
        idt.segment_not_present
            .set_handler_fn(segment_not_present_handler)
            .disable_interrupts(true);
        idt.stack_segment_fault
            .set_handler_fn(stack_segment_fault_handler)
            .disable_interrupts(true);
        idt.general_protection_fault
            .set_handler_fn(gpf_handler)
            .disable_interrupts(true);
        idt.page_fault
            .set_handler_fn(page_fault_handler)
            .disable_interrupts(true);

        unsafe {
            idt.double_fault
                .set_handler_fn(double_fault_handler)
                .set_stack_index(crate::arch::x86_64::gdt::DOUBLE_FAULT_IST_INDEX)
                .disable_interrupts(true);
            idt.machine_check
                .set_handler_fn(machine_check_handler)
                .set_stack_index(crate::arch::x86_64::gdt::MCE_IST_INDEX)
                .disable_interrupts(true);
            idt.non_maskable_interrupt
                .set_handler_fn(crate::watchdog::nmi_handler)
                .set_stack_index(crate::arch::x86_64::gdt::NMI_IST_INDEX)
                .disable_interrupts(true);
        }

        // Register APIC timer interrupt at vector 32 (interrupt gate, clears IF).
        idt[32]
            .set_handler_fn(timer_handler)
            .disable_interrupts(true);

        // System vectors at top (240-245) — highest priority.
        // Timer-reschedule IPI at 243 (was 52), resched at 240 (was 49),
        // TLB shootdown at 241 (was 50). Halt at 242 (was 51, now wired).
        idt[IPI_TIMER_VECTOR]
            .set_handler_fn(ipi_timer_handler)
            .disable_interrupts(true);

        idt[IPI_RESCHEDULE_VECTOR]
            .set_handler_fn(ipi_resched_handler)
            .disable_interrupts(true);

        idt[IPI_TLB_SHOOTDOWN_VECTOR]
            .set_handler_fn(ipi_tlb_shootdown_handler)
            .disable_interrupts(true);

        idt[IPI_HALT_VECTOR]
            .set_handler_fn(ipi_halt_handler)
            .disable_interrupts(true);

        // IOMMU fault (244) — non-halting fault logger. Each VT-d unit
        // drains its FSTS/FRCD queue here, then EOI. Always present; no-op when
        // IOMMU is disabled.
        idt[IOMMU_FAULT_VECTOR]
            .set_handler_fn(iommu_fault_handler)
            .disable_interrupts(true);
        // QI completion (245) — currently polled, but reserved for the
        // invalidation completion interrupt when QI ECAP so indicates.
        idt[IOMMU_QI_VECTOR]
            .set_handler_fn(iommu_qi_handler)
            .disable_interrupts(true);

        // Register device interrupt vectors 33-239 (207 vectors, interrupt gates, clears IF).
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
        idt[49].set_handler_fn(irq_49).disable_interrupts(true);
        idt[50].set_handler_fn(irq_50).disable_interrupts(true);
        idt[51].set_handler_fn(irq_51).disable_interrupts(true);
        idt[52].set_handler_fn(irq_52).disable_interrupts(true);
        idt[53].set_handler_fn(irq_53).disable_interrupts(true);
        idt[54].set_handler_fn(irq_54).disable_interrupts(true);
        idt[55].set_handler_fn(irq_55).disable_interrupts(true);
        idt[56].set_handler_fn(irq_56).disable_interrupts(true);
        idt[57].set_handler_fn(irq_57).disable_interrupts(true);
        idt[58].set_handler_fn(irq_58).disable_interrupts(true);
        idt[59].set_handler_fn(irq_59).disable_interrupts(true);
        idt[60].set_handler_fn(irq_60).disable_interrupts(true);
        idt[61].set_handler_fn(irq_61).disable_interrupts(true);
        idt[62].set_handler_fn(irq_62).disable_interrupts(true);
        idt[63].set_handler_fn(irq_63).disable_interrupts(true);
        idt[64].set_handler_fn(irq_64).disable_interrupts(true);
        idt[65].set_handler_fn(irq_65).disable_interrupts(true);
        idt[66].set_handler_fn(irq_66).disable_interrupts(true);
        idt[67].set_handler_fn(irq_67).disable_interrupts(true);
        idt[68].set_handler_fn(irq_68).disable_interrupts(true);
        idt[69].set_handler_fn(irq_69).disable_interrupts(true);
        idt[70].set_handler_fn(irq_70).disable_interrupts(true);
        idt[71].set_handler_fn(irq_71).disable_interrupts(true);
        idt[72].set_handler_fn(irq_72).disable_interrupts(true);
        idt[73].set_handler_fn(irq_73).disable_interrupts(true);
        idt[74].set_handler_fn(irq_74).disable_interrupts(true);
        idt[75].set_handler_fn(irq_75).disable_interrupts(true);
        idt[76].set_handler_fn(irq_76).disable_interrupts(true);
        idt[77].set_handler_fn(irq_77).disable_interrupts(true);
        idt[78].set_handler_fn(irq_78).disable_interrupts(true);
        idt[79].set_handler_fn(irq_79).disable_interrupts(true);
        idt[80].set_handler_fn(irq_80).disable_interrupts(true);
        idt[81].set_handler_fn(irq_81).disable_interrupts(true);
        idt[82].set_handler_fn(irq_82).disable_interrupts(true);
        idt[83].set_handler_fn(irq_83).disable_interrupts(true);
        idt[84].set_handler_fn(irq_84).disable_interrupts(true);
        idt[85].set_handler_fn(irq_85).disable_interrupts(true);
        idt[86].set_handler_fn(irq_86).disable_interrupts(true);
        idt[87].set_handler_fn(irq_87).disable_interrupts(true);
        idt[88].set_handler_fn(irq_88).disable_interrupts(true);
        idt[89].set_handler_fn(irq_89).disable_interrupts(true);
        idt[90].set_handler_fn(irq_90).disable_interrupts(true);
        idt[91].set_handler_fn(irq_91).disable_interrupts(true);
        idt[92].set_handler_fn(irq_92).disable_interrupts(true);
        idt[93].set_handler_fn(irq_93).disable_interrupts(true);
        idt[94].set_handler_fn(irq_94).disable_interrupts(true);
        idt[95].set_handler_fn(irq_95).disable_interrupts(true);
        idt[96].set_handler_fn(irq_96).disable_interrupts(true);
        idt[97].set_handler_fn(irq_97).disable_interrupts(true);
        idt[98].set_handler_fn(irq_98).disable_interrupts(true);
        idt[99].set_handler_fn(irq_99).disable_interrupts(true);
        idt[100].set_handler_fn(irq_100).disable_interrupts(true);
        idt[101].set_handler_fn(irq_101).disable_interrupts(true);
        idt[102].set_handler_fn(irq_102).disable_interrupts(true);
        idt[103].set_handler_fn(irq_103).disable_interrupts(true);
        idt[104].set_handler_fn(irq_104).disable_interrupts(true);
        idt[105].set_handler_fn(irq_105).disable_interrupts(true);
        idt[106].set_handler_fn(irq_106).disable_interrupts(true);
        idt[107].set_handler_fn(irq_107).disable_interrupts(true);
        idt[108].set_handler_fn(irq_108).disable_interrupts(true);
        idt[109].set_handler_fn(irq_109).disable_interrupts(true);
        idt[110].set_handler_fn(irq_110).disable_interrupts(true);
        idt[111].set_handler_fn(irq_111).disable_interrupts(true);
        idt[112].set_handler_fn(irq_112).disable_interrupts(true);
        idt[113].set_handler_fn(irq_113).disable_interrupts(true);
        idt[114].set_handler_fn(irq_114).disable_interrupts(true);
        idt[115].set_handler_fn(irq_115).disable_interrupts(true);
        idt[116].set_handler_fn(irq_116).disable_interrupts(true);
        idt[117].set_handler_fn(irq_117).disable_interrupts(true);
        idt[118].set_handler_fn(irq_118).disable_interrupts(true);
        idt[119].set_handler_fn(irq_119).disable_interrupts(true);
        idt[120].set_handler_fn(irq_120).disable_interrupts(true);
        idt[121].set_handler_fn(irq_121).disable_interrupts(true);
        idt[122].set_handler_fn(irq_122).disable_interrupts(true);
        idt[123].set_handler_fn(irq_123).disable_interrupts(true);
        idt[124].set_handler_fn(irq_124).disable_interrupts(true);
        idt[125].set_handler_fn(irq_125).disable_interrupts(true);
        idt[126].set_handler_fn(irq_126).disable_interrupts(true);
        idt[127].set_handler_fn(irq_127).disable_interrupts(true);
        idt[128].set_handler_fn(irq_128).disable_interrupts(true);
        idt[129].set_handler_fn(irq_129).disable_interrupts(true);
        idt[130].set_handler_fn(irq_130).disable_interrupts(true);
        idt[131].set_handler_fn(irq_131).disable_interrupts(true);
        idt[132].set_handler_fn(irq_132).disable_interrupts(true);
        idt[133].set_handler_fn(irq_133).disable_interrupts(true);
        idt[134].set_handler_fn(irq_134).disable_interrupts(true);
        idt[135].set_handler_fn(irq_135).disable_interrupts(true);
        idt[136].set_handler_fn(irq_136).disable_interrupts(true);
        idt[137].set_handler_fn(irq_137).disable_interrupts(true);
        idt[138].set_handler_fn(irq_138).disable_interrupts(true);
        idt[139].set_handler_fn(irq_139).disable_interrupts(true);
        idt[140].set_handler_fn(irq_140).disable_interrupts(true);
        idt[141].set_handler_fn(irq_141).disable_interrupts(true);
        idt[142].set_handler_fn(irq_142).disable_interrupts(true);
        idt[143].set_handler_fn(irq_143).disable_interrupts(true);
        idt[144].set_handler_fn(irq_144).disable_interrupts(true);
        idt[145].set_handler_fn(irq_145).disable_interrupts(true);
        idt[146].set_handler_fn(irq_146).disable_interrupts(true);
        idt[147].set_handler_fn(irq_147).disable_interrupts(true);
        idt[148].set_handler_fn(irq_148).disable_interrupts(true);
        idt[149].set_handler_fn(irq_149).disable_interrupts(true);
        idt[150].set_handler_fn(irq_150).disable_interrupts(true);
        idt[151].set_handler_fn(irq_151).disable_interrupts(true);
        idt[152].set_handler_fn(irq_152).disable_interrupts(true);
        idt[153].set_handler_fn(irq_153).disable_interrupts(true);
        idt[154].set_handler_fn(irq_154).disable_interrupts(true);
        idt[155].set_handler_fn(irq_155).disable_interrupts(true);
        idt[156].set_handler_fn(irq_156).disable_interrupts(true);
        idt[157].set_handler_fn(irq_157).disable_interrupts(true);
        idt[158].set_handler_fn(irq_158).disable_interrupts(true);
        idt[159].set_handler_fn(irq_159).disable_interrupts(true);
        idt[160].set_handler_fn(irq_160).disable_interrupts(true);
        idt[161].set_handler_fn(irq_161).disable_interrupts(true);
        idt[162].set_handler_fn(irq_162).disable_interrupts(true);
        idt[163].set_handler_fn(irq_163).disable_interrupts(true);
        idt[164].set_handler_fn(irq_164).disable_interrupts(true);
        idt[165].set_handler_fn(irq_165).disable_interrupts(true);
        idt[166].set_handler_fn(irq_166).disable_interrupts(true);
        idt[167].set_handler_fn(irq_167).disable_interrupts(true);
        idt[168].set_handler_fn(irq_168).disable_interrupts(true);
        idt[169].set_handler_fn(irq_169).disable_interrupts(true);
        idt[170].set_handler_fn(irq_170).disable_interrupts(true);
        idt[171].set_handler_fn(irq_171).disable_interrupts(true);
        idt[172].set_handler_fn(irq_172).disable_interrupts(true);
        idt[173].set_handler_fn(irq_173).disable_interrupts(true);
        idt[174].set_handler_fn(irq_174).disable_interrupts(true);
        idt[175].set_handler_fn(irq_175).disable_interrupts(true);
        idt[176].set_handler_fn(irq_176).disable_interrupts(true);
        idt[177].set_handler_fn(irq_177).disable_interrupts(true);
        idt[178].set_handler_fn(irq_178).disable_interrupts(true);
        idt[179].set_handler_fn(irq_179).disable_interrupts(true);
        idt[180].set_handler_fn(irq_180).disable_interrupts(true);
        idt[181].set_handler_fn(irq_181).disable_interrupts(true);
        idt[182].set_handler_fn(irq_182).disable_interrupts(true);
        idt[183].set_handler_fn(irq_183).disable_interrupts(true);
        idt[184].set_handler_fn(irq_184).disable_interrupts(true);
        idt[185].set_handler_fn(irq_185).disable_interrupts(true);
        idt[186].set_handler_fn(irq_186).disable_interrupts(true);
        idt[187].set_handler_fn(irq_187).disable_interrupts(true);
        idt[188].set_handler_fn(irq_188).disable_interrupts(true);
        idt[189].set_handler_fn(irq_189).disable_interrupts(true);
        idt[190].set_handler_fn(irq_190).disable_interrupts(true);
        idt[191].set_handler_fn(irq_191).disable_interrupts(true);
        idt[192].set_handler_fn(irq_192).disable_interrupts(true);
        idt[193].set_handler_fn(irq_193).disable_interrupts(true);
        idt[194].set_handler_fn(irq_194).disable_interrupts(true);
        idt[195].set_handler_fn(irq_195).disable_interrupts(true);
        idt[196].set_handler_fn(irq_196).disable_interrupts(true);
        idt[197].set_handler_fn(irq_197).disable_interrupts(true);
        idt[198].set_handler_fn(irq_198).disable_interrupts(true);
        idt[199].set_handler_fn(irq_199).disable_interrupts(true);
        idt[200].set_handler_fn(irq_200).disable_interrupts(true);
        idt[201].set_handler_fn(irq_201).disable_interrupts(true);
        idt[202].set_handler_fn(irq_202).disable_interrupts(true);
        idt[203].set_handler_fn(irq_203).disable_interrupts(true);
        idt[204].set_handler_fn(irq_204).disable_interrupts(true);
        idt[205].set_handler_fn(irq_205).disable_interrupts(true);
        idt[206].set_handler_fn(irq_206).disable_interrupts(true);
        idt[207].set_handler_fn(irq_207).disable_interrupts(true);
        idt[208].set_handler_fn(irq_208).disable_interrupts(true);
        idt[209].set_handler_fn(irq_209).disable_interrupts(true);
        idt[210].set_handler_fn(irq_210).disable_interrupts(true);
        idt[211].set_handler_fn(irq_211).disable_interrupts(true);
        idt[212].set_handler_fn(irq_212).disable_interrupts(true);
        idt[213].set_handler_fn(irq_213).disable_interrupts(true);
        idt[214].set_handler_fn(irq_214).disable_interrupts(true);
        idt[215].set_handler_fn(irq_215).disable_interrupts(true);
        idt[216].set_handler_fn(irq_216).disable_interrupts(true);
        idt[217].set_handler_fn(irq_217).disable_interrupts(true);
        idt[218].set_handler_fn(irq_218).disable_interrupts(true);
        idt[219].set_handler_fn(irq_219).disable_interrupts(true);
        idt[220].set_handler_fn(irq_220).disable_interrupts(true);
        idt[221].set_handler_fn(irq_221).disable_interrupts(true);
        idt[222].set_handler_fn(irq_222).disable_interrupts(true);
        idt[223].set_handler_fn(irq_223).disable_interrupts(true);
        idt[224].set_handler_fn(irq_224).disable_interrupts(true);
        idt[225].set_handler_fn(irq_225).disable_interrupts(true);
        idt[226].set_handler_fn(irq_226).disable_interrupts(true);
        idt[227].set_handler_fn(irq_227).disable_interrupts(true);
        idt[228].set_handler_fn(irq_228).disable_interrupts(true);
        idt[229].set_handler_fn(irq_229).disable_interrupts(true);
        idt[230].set_handler_fn(irq_230).disable_interrupts(true);
        idt[231].set_handler_fn(irq_231).disable_interrupts(true);
        idt[232].set_handler_fn(irq_232).disable_interrupts(true);
        idt[233].set_handler_fn(irq_233).disable_interrupts(true);
        idt[234].set_handler_fn(irq_234).disable_interrupts(true);
        idt[235].set_handler_fn(irq_235).disable_interrupts(true);
        idt[236].set_handler_fn(irq_236).disable_interrupts(true);
        idt[237].set_handler_fn(irq_237).disable_interrupts(true);
        idt[238].set_handler_fn(irq_238).disable_interrupts(true);
        idt[239].set_handler_fn(irq_239).disable_interrupts(true);

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
/// one-shot), then signals EOI.  Also pets the NMI watchdog (lock-free).
extern "x86-interrupt" fn timer_handler(frame: InterruptStackFrame) {
    let u = from_user(&frame);
    if u {
        swapgs();
    }
    // Pet NMI watchdog before any queue lock — hangs that wedge the queue
    // still get a heartbeat from the timer ISR path.
    crate::watchdog::pet();
    let ptr = TIMER_CALLBACK.load(Ordering::Acquire);
    if !ptr.is_null() {
        let handler: fn() = unsafe { core::mem::transmute(ptr) };
        handler();
    }
    apic::apic_eoi();
    // Slice/deadline expiry may demand a switch; preempt only after EOI so
    // a switched-away context never leaves an unacknowledged interrupt.
    crate::task::try_preempt_from_irq(u);
    if u {
        swapgs();
    }
}

/// Timer-reschedule IPI handler (vector 243).
///
/// Sent by a remote CPU when this CPU's base got an earlier earliest
/// deadline.  Runs the same tick routine so the local base re-arms, then
/// signals EOI.  A missed IPI is non-fatal — the LAPIC is still armed for
/// the previous earliest and `tick()` re-arms after it fires.
extern "x86-interrupt" fn ipi_timer_handler(frame: InterruptStackFrame) {
    let u = from_user(&frame);
    if u {
        swapgs();
    }
    crate::watchdog::pet();
    let ptr = TIMER_IPI_CALLBACK.load(Ordering::Acquire);
    if !ptr.is_null() {
        let handler: fn() = unsafe { core::mem::transmute(ptr) };
        handler();
    }
    apic::apic_eoi();
    crate::task::try_preempt_from_irq(u);
    if u {
        swapgs();
    }
}

/// Reschedule IPI handler (vector 240).
///
/// Reserved cross-CPU "please reschedule" doorbell (APIC-008). Dormant while
/// scheduling stays BSP-only (`try_preempt_from_irq` gates on
/// `sched_active`, which APs never raise), fully functional for per-CPU
/// queues later.
extern "x86-interrupt" fn ipi_resched_handler(frame: InterruptStackFrame) {
    let u = from_user(&frame);
    if u {
        swapgs();
    }
    apic::apic_eoi();
    crate::smp::set_need_resched();
    crate::task::try_preempt_from_irq(u);
    if u {
        swapgs();
    }
}

/// Cross-CPU TLB-shootdown IPI handler (vector 241).
///
/// Flushes this CPU's entire TLB and acknowledges the shootdown generation, so
/// the initiating CPU may then release the just-unmapped frames to the
/// allocator.  Runs with interrupts disabled (interrupt gate); does not touch
/// any VMM lock, so it can never deadlock against an in-progress shootdown.
extern "x86-interrupt" fn ipi_tlb_shootdown_handler(frame: InterruptStackFrame) {
    let u = from_user(&frame);
    if u {
        swapgs();
    }
    crate::mm::vmm::tlb_shootdown_on_this_cpu();
    apic::apic_eoi();
    if u {
        swapgs();
    }
}

/// Halt IPI handler (vector 242).
///
/// Broadcast halt for panic/reboot paths. EOI then hlt loop; never returns
/// in the halting path but defined as normal handler to allow EOI.
extern "x86-interrupt" fn ipi_halt_handler(frame: InterruptStackFrame) {
    let u = from_user(&frame);
    if u {
        swapgs();
    }
    apic::apic_eoi();
    // Halt loop � caller (panic/reboot) will spin; we just park.
    loop {
        unsafe { core::arch::asm!("cli; hlt", options(nomem, nostack)) }
    }
}

extern "x86-interrupt" fn iommu_fault_handler(frame: InterruptStackFrame) {
    let u = from_user(&frame);
    if u {
        swapgs();
    }
    #[cfg(target_arch = "x86_64")]
    crate::iommu::fault_handler();
    apic::apic_eoi();
    if u {
        swapgs();
    }
}

extern "x86-interrupt" fn iommu_qi_handler(frame: InterruptStackFrame) {
    let u = from_user(&frame);
    if u {
        swapgs();
    }
    // QI completion: currently polled, but EOI is required if hardware fires it.
    // Drain faults as well as a belt-and-suspenders.
    #[cfg(target_arch = "x86_64")]
    crate::iommu::fault_handler();
    apic::apic_eoi();
    if u {
        swapgs();
    }
}

extern "x86-interrupt" fn divide_error_handler(frame: InterruptStackFrame) {
    let u = from_user(&frame);
    if u {
        swapgs();
    }
    if u {
        crate::task::kill_user_fault();
    }
    let rsp: u64;
    unsafe {
        core::arch::asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack));
    }
    crate::drivers::serial::dump_puts("[#DE] RSP = 0x");
    crate::drivers::serial::dump_put_hex(rsp);
    crate::drivers::serial::dump_puts("\n");
    crate::drivers::serial::dump_puts("[#DE] frame RIP = 0x");
    crate::drivers::serial::dump_put_hex(frame.instruction_pointer.as_u64());
    crate::drivers::serial::dump_puts("\n");
    crate::kerneldump::dump_full_fault(&frame, 0, 0);
}

extern "x86-interrupt" fn breakpoint_handler(frame: InterruptStackFrame) {
    let u = from_user(&frame);
    if u {
        swapgs();
    }
    if u {
        crate::task::kill_user_fault();
    }
    crate::kerneldump::dump_full_fault(&frame, 0, 3);
}

extern "x86-interrupt" fn invalid_opcode_handler(frame: InterruptStackFrame) {
    let u = from_user(&frame);
    if u {
        swapgs();
    }
    if u {
        crate::task::kill_user_fault();
    }
    crate::kerneldump::dump_full_fault(&frame, 0, 6);
}

extern "x86-interrupt" fn invalid_tss_handler(frame: InterruptStackFrame, error_code: u64) {
    let u = from_user(&frame);
    if u {
        swapgs();
    }
    if u {
        crate::task::kill_user_fault();
    }
    crate::kerneldump::dump_full_fault(&frame, error_code, 10);
}

extern "x86-interrupt" fn segment_not_present_handler(frame: InterruptStackFrame, error_code: u64) {
    let u = from_user(&frame);
    if u {
        swapgs();
    }
    if u {
        crate::task::kill_user_fault();
    }
    crate::kerneldump::dump_full_fault(&frame, error_code, 11);
}

extern "x86-interrupt" fn stack_segment_fault_handler(frame: InterruptStackFrame, error_code: u64) {
    let u = from_user(&frame);
    if u {
        swapgs();
    }
    if u {
        crate::task::kill_user_fault();
    }
    crate::kerneldump::dump_full_fault(&frame, error_code, 12);
}

extern "x86-interrupt" fn double_fault_handler(frame: InterruptStackFrame, error_code: u64) -> ! {
    let u = from_user(&frame);
    if u {
        swapgs();
    }
    crate::kerneldump::dump_full_fault(&frame, error_code, 8);
}

extern "x86-interrupt" fn machine_check_handler(frame: InterruptStackFrame) -> ! {
    let u = from_user(&frame);
    if u {
        swapgs();
    }
    // Immediate banner onto VRAM + serial before heavy dump — ASAP visibility.
    // `panic_screen_init` clears VRAM to MCE red and claims ownership (first CPU wins).
    #[cfg(target_arch = "x86_64")]
    {
        // `panic_screen_init` is lock-free, no heap. If no fb, it's a no-op.
        let _ = crate::kerneldump::screen::panic_screen_init();
        if crate::kerneldump::screen::is_ready() {
            crate::kerneldump::screen::panic_puts("!!! FATAL MACHINE CHECK (#MC 18) !!!\n");
            crate::kerneldump::screen::panic_puts("CPU halting — see serial for full dump\n");
        }
        // Always emit on serial as well (lock-free dump path).
        crate::drivers::serial::dump_puts("\n!!! FATAL MACHINE CHECK (#MC 18) !!!\n");
        crate::drivers::serial::dump_puts("CPU halting — dumping MCA + registers\n");
    }
    // Delegate to the full dump (which will also emit MCA banks, regs, stack, code).
    // `dump_full_fault` never returns (cli;hlt loop) and mirrors to screen via DumpWriter.
    crate::kerneldump::dump_full_fault(&frame, 0, 18);
}

extern "x86-interrupt" fn page_fault_handler(
    mut frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    PF_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let u = from_user(&frame);
    if u {
        swapgs();
    }
    // MONIKA INVASIVE: log supervisor I-fetch at low VA (RIP==CR3) before dump
    if !u {
        let rip = frame.instruction_pointer.as_u64();
        let cr3: u64;
        unsafe { core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack)) };
        if rip < 0x0000_8000_0000_0000 && error_code.contains(PageFaultErrorCode::INSTRUCTION_FETCH) {
            crate::drivers::serial::dump_puts("\n[PF] supervisor I-fetch low RIP=0x");
            crate::drivers::serial::dump_put_hex(rip);
            crate::drivers::serial::dump_puts(" CR3=0x");
            crate::drivers::serial::dump_put_hex(cr3);
            crate::drivers::serial::dump_puts(" err=0x");
            crate::drivers::serial::dump_put_hex(error_code.bits());
            crate::drivers::serial::dump_puts(" pid=");
            crate::drivers::serial::dump_put_hex(crate::task::current_pid().unwrap_or(0));
            crate::drivers::serial::dump_puts("\n");
            if rip == (cr3 & !0xFFF) {
                crate::drivers::serial::dump_puts("[PF] RIP==CR3 detected - TaskContext corruption!\n");
            }
        }
    }
    if u {
        // Ring-3 page fault. Demand paging and COW make #PF a legitimate
        // allocation event now: try to resolve and retry the faulting
        // instruction first. Anything unresolved (bad pointer, guard-page
        // overflow, real permission breach, OOM) kills the task instead of
        // bricking the kernel.
        let cr2: u64;
        unsafe {
            core::arch::asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack));
        }
        if crate::mm::fault::resolve_user_fault(cr2, error_code) {
            // Resolved: iretq returns to ring 3, so GS must be swapped back
            // (entry swapgs'd to PerCpu; sysret paths restore it explicitly).
            if u {
                swapgs();
            }
            return;
        }
        crate::task::kill_user_fault();
    }
    // ── PF recovery during a kernel dump ───────────────────────────
    if dump::is_dump_in_progress() {
        let cpu = crate::smp::current_cpu_id() as usize;
        let idx = if cpu < crate::smp::MAX_CPUS { cpu } else { 0 };
        let target = dump::PF_RECOVERY_RIP[idx].load(Ordering::Relaxed);
        if target != 0 {
            let cr2: u64;
            unsafe {
                core::arch::asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack));
            }
            dump::PF_FAULT_ADDR[idx].store(cr2, Ordering::Relaxed);
            dump::PF_ERROR_CODE[idx].store(error_code.bits(), Ordering::Relaxed);
            unsafe {
                frame
                    .as_mut()
                    .update(|val| val.instruction_pointer = VirtAddr::new(target));
            }
            if u {
                swapgs();
            }
            return;
        }
        crate::drivers::serial::dump_puts(
            "\n[DUMP] Nested PF during dump (no recovery target) — halting\n",
        );
        loop {
            unsafe {
                core::arch::asm!("cli; hlt", options(nomem, nostack));
            }
        }
    }

    crate::kerneldump::dump_full_fault(&frame, error_code.bits(), 14);
}

extern "x86-interrupt" fn gpf_handler(frame: InterruptStackFrame, error_code: u64) {
    let u = from_user(&frame);
    if u {
        swapgs();
    }
    if u {
        crate::task::kill_user_fault();
    }
    let ecx: u32;
    unsafe {
        core::arch::asm!("mov {0:e}, ecx", out(reg) ecx);
    }
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
    unsafe {
        core::arch::asm!(
            "mov rax, cr3; mov cr3, rax",
            options(nostack, preserves_flags)
        );
    }
}

/// Reload the IDT on an Application Processor (IDTR is per-CPU).
///
/// Must be called after the BSP has called `init()` and before the AP
/// enables interrupts.
pub fn init_ap() {
    let idt = IDT.get().expect("IDT not initialised on BSP");
    idt.load();
}
