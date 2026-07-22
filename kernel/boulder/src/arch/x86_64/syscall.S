.section .text,"ax"
.code64

.global boulder_syscall_entry
.type boulder_syscall_entry, @function
boulder_syscall_entry:
    # SYSCALL leaves the user RSP live. Save it before selecting a kernel-only
    # stack; RCX and R11 already contain the user return RIP and RFLAGS.
    movq %rsp, boulder_syscall_user_rsp(%rip)
    leaq boulder_syscall_stack_top(%rip), %rsp
    subq $80, %rsp

    # struct SyscallFrame, in Slope ABI register order.
    movq %rax, 0(%rsp)
    movq %rdi, 8(%rsp)
    movq %rsi, 16(%rsp)
    movq %rdx, 24(%rsp)
    movq %r10, 32(%rsp)
    movq %r8, 40(%rsp)
    movq %r9, 48(%rsp)
    movq %rcx, 56(%rsp)
    movq %r11, 64(%rsp)
    movq boulder_syscall_user_rsp(%rip), %rax
    movq %rax, 72(%rsp)

    cld
    movq %rsp, %rdi
    call boulder_syscall_dispatch

    # Rust stores the signed result over the syscall number. SYSRET restores
    # only RIP/RFLAGS; the explicit RSP load completes the user frame.
    movq 0(%rsp), %rax
    movq 56(%rsp), %rcx
    movq 64(%rsp), %r11
    movq 72(%rsp), %rsp
    sysretq
.size boulder_syscall_entry, . - boulder_syscall_entry

.section .bss,"aw",@nobits
.balign 8
boulder_syscall_user_rsp:
    .quad 0
.balign 16
boulder_syscall_stack:
    .skip 16384
boulder_syscall_stack_top:
