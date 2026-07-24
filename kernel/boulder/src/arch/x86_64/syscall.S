.section .text,"ax"
.code64

.global boulder_syscall_entry
.type boulder_syscall_entry, @function
boulder_syscall_entry:
    # SYSCALL leaves the user RSP live. Save it before selecting a kernel-only
    # stack; RCX and R11 already contain the user return RIP and RFLAGS.
    movq %rsp, boulder_syscall_user_rsp(%rip)
    leaq boulder_syscall_stack_top(%rip), %rsp
    subq $144, %rsp

    # struct SavedUserContext. The register prefix has the same offsets as
    # InterruptFrame, followed by the SYSRET control triple.
    movq %r15, 0(%rsp)
    movq %r14, 8(%rsp)
    movq %r13, 16(%rsp)
    movq %r12, 24(%rsp)
    movq %r11, 32(%rsp)
    movq %r10, 40(%rsp)
    movq %r9, 48(%rsp)
    movq %r8, 56(%rsp)
    movq %rbp, 64(%rsp)
    movq %rdi, 72(%rsp)
    movq %rsi, 80(%rsp)
    movq %rdx, 88(%rsp)
    movq %rcx, 96(%rsp)
    movq %rbx, 104(%rsp)
    movq %rax, 112(%rsp)
    movq %rcx, 120(%rsp)
    movq %r11, 128(%rsp)
    movq boulder_syscall_user_rsp(%rip), %rax
    movq %rax, 136(%rsp)

    cld
    movq %rsp, %rdi
    call boulder_syscall_dispatch

    # Rust may replace the complete frame with another process context.
    # SYSRET consumes RCX/R11; the other registers and RSP are explicit.
    movq 0(%rsp), %r15
    movq 8(%rsp), %r14
    movq 16(%rsp), %r13
    movq 24(%rsp), %r12
    movq 40(%rsp), %r10
    movq 48(%rsp), %r9
    movq 56(%rsp), %r8
    movq 64(%rsp), %rbp
    movq 72(%rsp), %rdi
    movq 80(%rsp), %rsi
    movq 88(%rsp), %rdx
    movq 104(%rsp), %rbx
    movq 112(%rsp), %rax
    movq 120(%rsp), %rcx
    movq 128(%rsp), %r11
    movq 136(%rsp), %rsp
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
