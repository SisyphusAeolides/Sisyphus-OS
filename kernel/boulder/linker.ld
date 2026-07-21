ENTRY(_start)

SECTIONS
{
    . = 1M;
    __kernel_start = .;

    .multiboot_header ALIGN(8) :
    {
        KEEP(*(.multiboot_header))
    }

    .text ALIGN(4K) :
    {
        *(.text.boot)
        *(.text.interrupts)
        *(.text .text.*)
    }

    .rodata ALIGN(4K) :
    {
        *(.rodata.boot)
        *(.rodata.interrupts)
        *(.rodata .rodata.*)
    }

    .data ALIGN(4K) :
    {
        *(.data .data.*)
    }

    .bss ALIGN(4K) (NOLOAD) :
    {
        *(.bss.boot)
        *(.bss .bss.*)
        *(COMMON)
    }

    . = ALIGN(4K);
    __kernel_end = .;

    /DISCARD/ :
    {
        *(.eh_frame*)
        *(.comment*)
        *(.note*)
    }
}
