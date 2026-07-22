.section .text,"ax"
.code64

.equ USER_DATA_SELECTOR, 0x1b
.equ USER_CODE_SELECTOR, 0x23

.global boulder_enter_ring3_probe
.type boulder_enter_ring3_probe, @function
boulder_enter_ring3_probe:
    pushq %rbp
    pushq %rbx
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15
    movq %rsp, ring3_saved_rsp(%rip)
    movq %cr3, %rax
    movq %rax, ring3_saved_cr3(%rip)
    movq %rdx, %cr3

    pushq $USER_DATA_SELECTOR
    pushq %rsi
    pushfq
    popq %rax
    andq $-13057, %rax
    pushq %rax
    pushq $USER_CODE_SELECTOR
    pushq %rdi
    iretq
.size boulder_enter_ring3_probe, . - boulder_enter_ring3_probe

.global boulder_ring3_probe_return
.type boulder_ring3_probe_return, @function
boulder_ring3_probe_return:
    cli
    movq ring3_saved_cr3(%rip), %rax
    movq %rax, %cr3
    movq ring3_saved_rsp(%rip), %rsp
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %rbx
    popq %rbp
    retq
.size boulder_ring3_probe_return, . - boulder_ring3_probe_return

.global boulder_enter_user_process
.type boulder_enter_user_process, @function
boulder_enter_user_process:
    cli
    movq %rdx, %cr3
    pushq $USER_DATA_SELECTOR
    pushq %rsi
    pushq $0x202
    pushq $USER_CODE_SELECTOR
    pushq %rdi
    iretq
    ud2
.size boulder_enter_user_process, . - boulder_enter_user_process

.section .bss,"aw",@nobits
.balign 8
ring3_saved_rsp:
    .quad 0
ring3_saved_cr3:
    .quad 0
