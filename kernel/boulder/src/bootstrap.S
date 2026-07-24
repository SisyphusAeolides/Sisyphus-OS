.section .multiboot_header,"a"
.balign 8
multiboot_header:
    .long 0xe85250d6
    .long 0
    .long multiboot_header_end - multiboot_header
    .long -(0xe85250d6 + 0 + (multiboot_header_end - multiboot_header))
    .short 0
    .short 0
    .long 8
multiboot_header_end:

.section .text.boot,"ax"
.equ KERNEL_VIRTUAL_BASE, 0xffffffff80000000
.equ HIGHER_HALF_DIRECT_MAP_BASE, 0xffff800000000000

.code32
.global _start
.type _start, @function
_start:
    cli
    cmpl $0x36d76289, %eax
    jne boot_hang

    movl %ebx, %edi
    movl $stack_top, %esp

    movl $p3_table, %eax
    orl $3, %eax
    movl %eax, p4_table

    movl $p2_table, %eax
    orl $3, %eax
    movl %eax, p3_table

    movl $p3_table, %eax
    orl $3, %eax
    movl %eax, p4_table+2048
    movl $0, p4_table+2052

    movl $mmio_p3_table, %eax
    orl $3, %eax
    movl %eax, p4_table+2056
    movl $0, p4_table+2060

    movl $mmio_p2_table, %eax
    orl $3, %eax
    movl %eax, mmio_p3_table

    movl $bootstrap_mmio_p1_table, %eax
    orl $3, %eax
    movl %eax, mmio_p2_table

    movl $kernel_p3_table, %eax
    orl $3, %eax
    movl %eax, p4_table+4088
    movl $0, p4_table+4092

    movl $p2_table, %eax
    orl $3, %eax
    movl %eax, kernel_p3_table+4080
    movl $0, kernel_p3_table+4084

    xorl %ecx, %ecx
map_p2:
    movl %ecx, %eax
    shll $21, %eax
    orl $0x83, %eax
    movl %eax, p2_table(,%ecx,8)
    movl $0, p2_table+4(,%ecx,8)
    incl %ecx
    cmpl $512, %ecx
    jne map_p2

    movl %cr4, %eax
    orl $0x20, %eax
    movl %eax, %cr4

    movl $p4_table, %eax
    movl %eax, %cr3

    movl $0xc0000080, %ecx
    rdmsr
    orl $0x100, %eax
    wrmsr

    movl %cr0, %eax
    orl $0x80000000, %eax
    movl %eax, %cr0

    lgdt gdt64_pointer
    ljmpl $0x08, $long_mode_start

boot_hang:
    hlt
    jmp boot_hang

.code64
long_mode_start:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    xorw %ax, %ax
    movw %ax, %fs
    movw %ax, %gs
    lgdt gdt64_high_pointer
    movabsq $high_half_start, %rax
    jmp *%rax

.section .text.entry,"ax"
.code64
high_half_start:
    movabsq $(stack_top + KERNEL_VIRTUAL_BASE), %rsp
    xorq %rbp, %rbp

    movq %rdi, %rsi
    movabsq $HIGHER_HALF_DIRECT_MAP_BASE, %rax
    addq %rax, %rdi

    movabsq $(p4_table + KERNEL_VIRTUAL_BASE), %rax
    movq $0, (%rax)
    movabsq $p4_table, %rax
    movq %rax, %cr3

    call boulder_main

long_mode_hang:
    cli
    hlt
    jmp long_mode_hang

.section .rodata.boot,"a"
.balign 8
gdt64:
    .quad 0
    .quad 0x00af9a000000ffff
    .quad 0x00cf92000000ffff
gdt64_end:
gdt64_pointer:
    .word gdt64_end - gdt64 - 1
    .quad gdt64
gdt64_high_pointer:
    .word gdt64_end - gdt64 - 1
    .quad gdt64 + KERNEL_VIRTUAL_BASE

.section .bss.boot,"aw",@nobits
.balign 4096
p4_table:
    .skip 4096
p3_table:
    .skip 4096
p2_table:
    .skip 4096
mmio_p3_table:
    .skip 4096
mmio_p2_table:
    .skip 4096
bootstrap_mmio_p1_table:
    .skip 4096
kernel_p3_table:
    .skip 4096
.balign 16
stack_bottom:
    .skip 8388608
stack_top:

.global mmio_p1_table
.set mmio_p1_table, bootstrap_mmio_p1_table + KERNEL_VIRTUAL_BASE
