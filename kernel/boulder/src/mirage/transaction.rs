use crate::memory::transaction::{TransactionAbort, rtm_available};

const XBEGIN_STARTED: u32 = u32::MAX;

core::arch::global_asm!(
    r#"
    .text
    .global boulder_rtm_call_win64_context
    .type boulder_rtm_call_win64_context,@function
boulder_rtm_call_win64_context:
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15
    movq %rdi, %r12
    movq %rsi, %r13
    movq %rdx, %r14
    movq %rcx, %r15
    xbegin 2f
    subq $40, %rsp
    movq %r13, %rcx
    call *%r12
    addq $40, %rsp
    movl %eax, (%r14)
    xend
    movl $-1, (%r15)
    jmp 3f
2:
    movl $0, (%r14)
    movl %eax, (%r15)
3:
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    ret
    .size boulder_rtm_call_win64_context, .-boulder_rtm_call_win64_context
"#,
    options(att_syntax)
);

unsafe extern "C" {
    fn boulder_rtm_call_win64_context(
        function: unsafe extern "win64" fn(*mut u8) -> i32,
        context: *mut u8,
        result: *mut i32,
        status: *mut u32,
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionResult {
    Committed(i32),
    RolledBack(TransactionAbort),
    Unsupported,
}

/// Executes one Win64 callback in an optional RTM rollback region.
///
/// This is a rollback aid, not a privilege or memory-isolation boundary.
///
/// # Safety
///
/// The callback and context must be live, must not unwind, and must satisfy the
/// selected personality's execution-context contract.
pub unsafe fn execute_time_capsule(
    function: unsafe extern "win64" fn(*mut u8) -> i32,
    context: *mut u8,
) -> ExecutionResult {
    if !rtm_available() {
        return ExecutionResult::Unsupported;
    }
    let mut result = 0_i32;
    let mut status = 0_u32;
    unsafe {
        boulder_rtm_call_win64_context(function, context, &mut result, &mut status);
    }
    if status == XBEGIN_STARTED {
        ExecutionResult::Committed(result)
    } else {
        ExecutionResult::RolledBack(TransactionAbort::from_raw(status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_result_keeps_abort_details() {
        let result = ExecutionResult::RolledBack(TransactionAbort::from_raw(1 | (0x99 << 24)));
        assert!(matches!(
            result,
            ExecutionResult::RolledBack(TransactionAbort {
                explicit: true,
                code: 0x99,
                ..
            })
        ));
    }
}
