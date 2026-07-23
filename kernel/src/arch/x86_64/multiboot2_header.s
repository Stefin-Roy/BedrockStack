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

.balign 4096
pml4_page:
    .space 4096
.balign 4096
pdp_page:
    .space 4096
.balign 4096
pd_page:
    .space 4096

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

    // ── Zero BSS ───────────────────────────────────────────────
    mov edi, offset __bss_start
    mov ecx, offset __bss_end
    sub ecx, edi
    xor eax, eax
    cld
    rep stosb

    // ── Build page tables (identity-map first 16 MB) ──────────

    // Zero PML4, PDP, PD pages (3 pages = 12 KB / 4 = 3072 dwords)
    mov edi, offset pml4_page
    mov ecx, 3072
    xor eax, eax
    rep stosd

    // PML4[0] → PDP (present | writable)
    mov eax, offset pdp_page
    or  eax, 0x03
    mov [pml4_page], eax

    // PDP[0] → PD (present | writable)
    mov eax, offset pd_page
    or  eax, 0x03
    mov [pdp_page], eax

    // PD[0..511] → 2 MiB pages (present | writable | PS). This temporary
    // 1 GiB identity map covers GRUB's allocator/page-table handoff.
    mov ecx, 512
    xor ebx, ebx
    mov edi, offset pd_page
1:  mov eax, ebx
    or  eax, 0x83
    mov [edi], eax
    add ebx, 0x200000
    add edi, 8
    dec ecx
    jnz 1b

    // ── Enter long mode ────────────────────────────────────────

    lgdt [gdt64_ptr]

    mov eax, cr4
    or  eax, (1 << 5) | (1 << 7)   // PAE | PGE
    mov cr4, eax

    mov eax, offset pml4_page
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
    lea rsp, [rip + __stack_end]
    xor rbp, rbp

    mov edi, 0x36d76289             // magic
    mov rsi, [rip + mb2_info_save]  // info ptr
    call rust_entry_mb2

.hang:
    hlt
    jmp .hang
