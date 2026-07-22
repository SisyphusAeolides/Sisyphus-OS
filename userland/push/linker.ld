ENTRY(_start)

PHDRS
{
    text PT_LOAD FLAGS(5);
    rodata PT_LOAD FLAGS(4);
    data PT_LOAD FLAGS(6);
}

SECTIONS
{
    . = 0x1000;

    .text ALIGN(4K) :
    {
        *(.text .text.*)
    } :text

    .rodata ALIGN(4K) :
    {
        *(.rodata .rodata.*)
    } :rodata

    .data ALIGN(4K) :
    {
        *(.got .got.*)
        *(.data .data.*)
    } :data

    .bss ALIGN(4K) (NOLOAD) :
    {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /DISCARD/ :
    {
        *(.eh_frame*)
        *(.comment*)
        *(.note*)
        *(.dynamic*)
        *(.dynsym*)
        *(.dynstr*)
        *(.hash*)
        *(.gnu.hash*)
        *(.rela*)
        *(.rel*)
    }
}
