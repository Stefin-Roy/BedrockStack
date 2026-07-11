ENTRY(_start)

KERNEL_OFFSET = 0x80200000;

SECTIONS
{
    . = KERNEL_OFFSET;
    __kernel_start = .;

    __text_start = .;
    .text : AT(KERNEL_OFFSET) {
        *(.text.boot)
        *(.text)
        *(.text.*)
    }

    . = ALIGN(4096);
    __text_end = .;

    __rela_dyn_start = .;
    __rela_dyn_end = .;

    . = ALIGN(4096);

    __rodata_start = .;
    .rodata : {
        *(.rodata)
        *(.rodata.*)
    }

    . = ALIGN(4096);
    __rodata_end = .;

    __data_start = .;
    .data : {
        *(.data)
        *(.data.*)
    }

    . = ALIGN(4096);

    .bss : {
        __bss_start = .;
        *(.bss)
        *(.bss.*)
        *(COMMON)
        __bss_end = .;
    }

    .stack : {
        __stack_start = .;
        . += 0x8000;
        __stack_end = .;
    }

    . = ALIGN(4096);
    __kernel_end = .;

    /DISCARD/ : {
        *(.eh_frame)
        *(.comment)
        *(.note*)
        *(.gnu.hash)
        *(.dynsym)
        *(.dynstr)
    }
}
