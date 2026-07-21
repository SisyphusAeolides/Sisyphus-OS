.section .text.interrupts,"ax"
.code64

.macro ISR_NO_ERROR vector
.global isr_\vector
isr_\vector:
    pushq $0
    pushq $\vector
    jmp interrupt_common
.endm

.macro ISR_ERROR vector
.global isr_\vector
isr_\vector:
    pushq $\vector
    jmp interrupt_common
.endm

ISR_NO_ERROR 0
ISR_NO_ERROR 1
ISR_NO_ERROR 2
ISR_NO_ERROR 3
ISR_NO_ERROR 4
ISR_NO_ERROR 5
ISR_NO_ERROR 6
ISR_NO_ERROR 7
ISR_ERROR 8
ISR_NO_ERROR 9
ISR_ERROR 10
ISR_ERROR 11
ISR_ERROR 12
ISR_ERROR 13
ISR_ERROR 14
ISR_NO_ERROR 15
ISR_NO_ERROR 16
ISR_ERROR 17
ISR_NO_ERROR 18
ISR_NO_ERROR 19
ISR_NO_ERROR 20
ISR_ERROR 21
ISR_NO_ERROR 22
ISR_NO_ERROR 23
ISR_NO_ERROR 24
ISR_NO_ERROR 25
ISR_NO_ERROR 26
ISR_NO_ERROR 27
ISR_NO_ERROR 28
ISR_ERROR 29
ISR_ERROR 30
ISR_NO_ERROR 31
ISR_NO_ERROR 32
ISR_NO_ERROR 33
ISR_NO_ERROR 34
ISR_NO_ERROR 35
ISR_NO_ERROR 36
ISR_NO_ERROR 37
ISR_NO_ERROR 38
ISR_NO_ERROR 39
ISR_NO_ERROR 40
ISR_NO_ERROR 41
ISR_NO_ERROR 42
ISR_NO_ERROR 43
ISR_NO_ERROR 44
ISR_NO_ERROR 45
ISR_NO_ERROR 46
ISR_NO_ERROR 47
ISR_NO_ERROR 48

.global isr_unhandled
isr_unhandled:
    pushq $0
    pushq $255
    jmp interrupt_common

interrupt_common:
    cld
    pushq %rax
    pushq %rbx
    pushq %rcx
    pushq %rdx
    pushq %rsi
    pushq %rdi
    pushq %rbp
    pushq %r8
    pushq %r9
    pushq %r10
    pushq %r11
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15

    movq %rsp, %r12
    movq %rsp, %rdi
    andq $-16, %rsp
    call boulder_interrupt_dispatch
    movq %r12, %rsp

    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %r11
    popq %r10
    popq %r9
    popq %r8
    popq %rbp
    popq %rdi
    popq %rsi
    popq %rdx
    popq %rcx
    popq %rbx
    popq %rax
    addq $16, %rsp
    iretq

.section .rodata.interrupts,"a"
.balign 8
.global isr_stub_table
isr_stub_table:
    .quad isr_0, isr_1, isr_2, isr_3, isr_4, isr_5, isr_6, isr_7
    .quad isr_8, isr_9, isr_10, isr_11, isr_12, isr_13, isr_14, isr_15
    .quad isr_16, isr_17, isr_18, isr_19, isr_20, isr_21, isr_22, isr_23
    .quad isr_24, isr_25, isr_26, isr_27, isr_28, isr_29, isr_30, isr_31
    .quad isr_32, isr_33, isr_34, isr_35, isr_36, isr_37, isr_38, isr_39
    .quad isr_40, isr_41, isr_42, isr_43, isr_44, isr_45, isr_46, isr_47
    .quad isr_48
