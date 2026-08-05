// ═══════════════════════════════════════════════════════════════════
// Multiboot2 header (must be within the first 32768 bytes of the image).
// ═══════════════════════════════════════════════════════════════════

.section .multiboot2, "a"
.balign 8
mb2_header_start:
    .long 0xE85250D6
    .long 0                        // architecture = 0 (i386)
    .long mb2_header_end - mb2_header_start
    .long -(0xE85250D6 + 0 + (mb2_header_end - mb2_header_start))

    // Information request tag
mb2_info_req_start:
    .word 1
    .word 0
    .long mb2_info_req_end - mb2_info_req_start
    .long 4                        // MBI: Basic meminfo
    .long 6                        // MBI: Memory map
    .long 8                        // MBI: Framebuffer
    .long 14                       // MBI: ACPI old RSDP
    .long 15                       // MBI: ACPI new RSDP
mb2_info_req_end:
    .balign 8

    // Framebuffer tag: request any 32-bit GOP-backed mode.
    .word 5
    .word 1
    .long 20
    .long 1024                        // width: any
    .long 768                        // height: any
    .long 32                       // depth
    .balign 8

    // Entry address tag (type 3): GRUB jumps EXACTLY to the low physical
    // `_start` (a `.text.boot` symbol whose value == its physical address,
    // the low region being identity-mapped), NOT to the higher-half
    // `e_entry`.  12-byte tag: .word type / .word flags / .long size / .long addr.
    .word 3
    .word 0
    .long 12
    .long _start
    .balign 8

    // End tag
    .word 0
    .word 0
    .long 8
mb2_header_end:

// ═══════════════════════════════════════════════════════════════════
// Data
// ═══════════════════════════════════════════════════════════════════

.section .data.boot, "aw", @progbits

.balign 8
gdt32_start:
    .quad 0                        // null
    .quad 0x00CF9A000000FFFF       // cs32: ring 0, 32-bit, base=0, limit=4G
    .quad 0x00CF92000000FFFF       // ds: ring 0, data, base=0, limit=4G
gdt32_end:

gdt32_ptr:
    .word gdt32_end - gdt32_start - 1
    .long gdt32_start

.balign 8
gdt64_start:
    .quad 0                        // null
    .quad 0x0020980000000000       // cs64: ring 0, 64-bit (L=1, D=0)
    .quad 0x00CF92000000FFFF       // ds: ring 0, data, base=0, limit=4G
gdt64_end:

gdt64_ptr:
    .word gdt64_end - gdt64_start - 1
    .quad gdt64_start

.balign 8
mb2_info_save:
    .quad 0

// Far jump descriptor for entering 64-bit mode
.balign 8
jmp_buf:
    .long _start_64                // 32-bit offset, identity-mapped below 4 GiB
    .word 0x08                     // CS selector
    .word 0                        // padding

// ═══════════════════════════════════════════════════════════════════
// 32-bit Entry — called by GRUB in 32-bit protected mode
// ═══════════════════════════════════════════════════════════════════

.section .text.boot, "ax"
.code32
.globl _start
_start:
    cli
    mov [mb2_info_save], ebx

    // ── Enter long mode on the static higher-half page tables ──────
    // `.boottables` (in the linker script) already identity-maps the low
    // 1 GiB AND maps the higher-half kernel at [KERNEL_VMA, +256 MiB), so
    // no runtime table build is needed: just install CR3 = __boot_pml4.
    // `__boot_pml4` is a low-region symbol whose value == physical address.

    lgdt [gdt64_ptr]

    mov eax, cr4
    or  eax, (1 << 5) | (1 << 7)   // PAE | PGE
    mov cr4, eax

    mov eax, offset __boot_pml4
    mov cr3, eax

    mov ecx, 0xC0000080             // MSR EFER
    rdmsr
    or  eax, (1 << 8) | (1 << 11)   // LME | NXE
    wrmsr

    mov eax, cr0
    or  eax, (1 << 31) | (1 << 16)  // PG | WP
    mov cr0, eax

    // Far jump to 64-bit code through memory indirect pointer
    jmp fword ptr [jmp_buf]

// ═══════════════════════════════════════════════════════════════════
// 64-bit Entry
// ═══════════════════════════════════════════════════════════════════

.section .text.boot64, "ax"
.code64
_start_64:
    // Switch to the kernel's high `.stack` — CR3 = __boot_pml4 was already
    // installed by the 32-bit path, and `.boottables` maps the whole
    // [KERNEL_VMA, +256 MiB) window RW, so `.stack` (ending at `__kernel_end`)
    // is reachable right now.  The low `.bootstack` is dead from here on; the
    // kernel runs its entire life on this high stack, which every domain's
    // cloned high half maps.  (`offset` is required so this assembles to an
    // absolute load, not the `48 a1` moffs memory-load form.)
    movabs rax, offset __stack_end
    mov rsp, rax
    xor rbp, rbp

    mov edi, 0x36d76289             // magic
    mov rsi, [rip + mb2_info_save]  // info ptr (low physical, still identity-mapped)
    // `offset` is required: WITHOUT it, `movabs rax, sym` assembles to the
    // `48 a1` moffs form (mov rax, [sym]) — a memory LOAD from the high VMA,
    // which loads whatever bytes happen to be there instead of the address.
    movabs rax, offset rust_entry_mb2  // kernel-region symbol == high VMA; absolute far jump
    jmp rax

.hang:
    hlt
    jmp .hang
