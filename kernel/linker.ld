ENTRY(_start)

KERNEL_OFFSET = 0x400000;

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
    .rela.dyn : {
        *(.rela.dyn)
    }
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

    .dynamic : {
        *(.dynamic)
    }

    . = ALIGN(4096);

    .bss : {
        __bss_start = .;
        *(.bss)
        *(.bss.*)
        *(COMMON)
        __bss_end = .;
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
