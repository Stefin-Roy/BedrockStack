/* User-space INIT: fixed-position ET_EXEC loaded at 0x400000.
 *
 * The ELF header/program headers live below the first PT_LOAD's p_offset, so
 * they are never loaded. `.text` is forced to a 4 KiB boundary so p_offset is
 * exactly 0x1000 with p_vaddr 0x400000 and `_start` at both — the loader maps
 * mem[0x400000] = file[p_offset] = _start.
 */
ENTRY(_start)

SECTIONS
{
    . = 0x400000;

    .text : ALIGN(0x1000) {
        *(.text.entry)
        *(.text*)
    }

    .rodata : ALIGN(0x1000) {
        *(.rodata*)
    }

    .data : ALIGN(0x1000) {
        *(.data*)
        *(.got*)
    }

    .bss : ALIGN(0x1000) {
        *(COMMON)
        *(.bss*)
    }

    /DISCARD/ : {
        *(.note.gnu.property)
        *(.note.GNU-stack)
        *(.note*)
        *(.eh_frame)
        *(.comment)
    }
}