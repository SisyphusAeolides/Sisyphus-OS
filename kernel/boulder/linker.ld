ENTRY(_start)

KERNEL_VIRTUAL_BASE = 0xffffffff80000000;

SECTIONS
{
    . = 1M;
    __bootstrap_physical_start = .;

    .multiboot_header ALIGN(8) :
    {
        KEEP(*(.multiboot_header))
    }

    .bootstrap_text ALIGN(4K) :
    {
        *(.text.boot)
    }

    .bootstrap_rodata ALIGN(4K) :
    {
        *(.rodata.boot)
    }

    .bootstrap_bss ALIGN(4K) (NOLOAD) :
    {
        *(.bss.boot)
    }

    . = ALIGN(4K);
    __bootstrap_physical_end = .;
    __kernel_physical_start = .;

    . = KERNEL_VIRTUAL_BASE + __kernel_physical_start;
    __kernel_start = .;

    .text ALIGN(4K) : AT(ADDR(.text) - KERNEL_VIRTUAL_BASE)
    {
        *(.text.entry)
        *(.text.interrupts)
        *(.text .text.*)
    }

    .rodata ALIGN(4K) : AT(ADDR(.rodata) - KERNEL_VIRTUAL_BASE)
    {
        *(.rodata.interrupts)
        *(.rodata .rodata.*)
    }

    .data ALIGN(4K) : AT(ADDR(.data) - KERNEL_VIRTUAL_BASE)
    {
        *(.got .got.*)
        *(.data .data.*)
    }

    .bss ALIGN(4K) (NOLOAD) : AT(ADDR(.bss) - KERNEL_VIRTUAL_BASE)
    {
        *(.bss .bss.*)
        *(COMMON)
    }

    . = ALIGN(4K);
    __kernel_end = .;
    __kernel_physical_end = __kernel_end - KERNEL_VIRTUAL_BASE;

    /DISCARD/ :
    {
        *(.eh_frame*)
        *(.comment*)
        *(.note*)
    }
}
