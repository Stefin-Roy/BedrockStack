/* ═══════════════════════════════════════════════════════════════════════
 * BedrockOS `init` — flat user-space ELF64.
 *
 * Fixed ET_EXEC placed at 0x400000 (below the kernel's 0xFFFFFFFF80000000
 * and well inside the user low half). The kernel ELF loader maps each
 * PT_LOAD segment and `sysretq`s to e_entry (`_start`), so this script
 * keeps every section page-aligned and loadable.
 * ═══════════════════════════════════════════════════════════════════════ */

ENTRY(_start)

BASE = 0x400000;

SECTIONS
{
    . = BASE;

    .text : {
        *(.text)
        *(.text.*)
    }
    . = ALIGN(4096);

    .rodata : {
        *(.rodata)
        *(.rodata.*)
    }
    . = ALIGN(4096);

    .data : {
        *(.data)
        *(.data.*)
        *(.got)
        *(.got.*)
    }
    . = ALIGN(4096);

    .bss (NOLOAD) : {
        *(.bss)
        *(.bss.*)
        *(COMMON)
    }
    . = ALIGN(4096);

    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.comment)
        *(.note*)
        *(.rela.dyn)
        *(.rela.*)
        *(.rel.*)
    }
}
