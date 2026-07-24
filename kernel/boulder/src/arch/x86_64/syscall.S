.section .text,"ax"
.code64

.global boulder_syscall_entry
.type boulder_syscall_entry, @function
boulder_syscall_entry:
    # Each CPU receives IA32_KERNEL_GS_BASE only after its CPUID/APIC identity,
    # TSS, generation, and dedicated entry stack have been registered. EFER.SCE
    # is programmed after that publication, so an unregistered CPU has no live
    # syscall gate. SWAPGS selects the immutable record for this CPU.
    swapgs
    cmpl $1, %gs:76
    jne .Lsyscall_authority_failure
    cmpq $0, %gs:8
    je .Lsyscall_authority_failure
    cmpq $0, %gs:24
    je .Lsyscall_authority_failure
    cmpl $0, %gs:72
    jne .Lsyscall_authority_failure
    movl $1, %gs:72

    # SYSCALL leaves the user RSP live. Save it in this CPU's record before
    # selecting its registered kernel-only stack.
    movq %rsp, %gs:16
    movq %gs:8, %rsp
    subq $176, %rsp

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
    movq %gs:16, %rax
    movq %rax, 136(%rsp)
    # DispatchContext CR3/RSP0 and the lifecycle generation/epoch authority
    # are output-only. Zeroing them prevents any caller register value from
    # masquerading as kernel-issued return authority.
    movq $0, 144(%rsp)
    movq $0, 152(%rsp)
    movq $0, 160(%rsp)
    movq $0, 168(%rsp)

    # The generation armed at the previous user return must still be current.
    # Registers are safe to use after their complete user image is on-stack.
    movq %gs:40, %rax
    testq %rax, %rax
    je .Lsyscall_authority_failure
    cmpq %gs:56, %rax
    jne .Lsyscall_authority_failure
    movq %rax, %gs:48

    cld
    movq %rsp, %rdi
    call boulder_syscall_dispatch

    # Rust revalidates the exact generation and epoch, publishes target RSP0
    # to the active TSS, and switches CR3 while this frame remains reachable
    # through the inherited higher-half kernel mapping.
    movq %rsp, %rdi
    call boulder_syscall_activate

    # Activation must atomically publish a fresh generation alongside the new
    # TSS RSP0 before assembly can release nesting and return to user mode.
    movq %gs:40, %rdx
    testq %rdx, %rdx
    je .Lsyscall_authority_failure
    cmpq %gs:56, %rdx
    jne .Lsyscall_authority_failure
    cmpl $1, %gs:72
    jne .Lsyscall_authority_failure
    movq $0, %gs:48
    movl $0, %gs:72

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
    swapgs
    sysretq

.Lsyscall_authority_failure:
    # No recovery may trust the current stack or return state. Interrupts were
    # masked by FMASK; remain halted on this CPU without touching memory.
    cli
1:
    hlt
    jmp 1b
.size boulder_syscall_entry, . - boulder_syscall_entry
