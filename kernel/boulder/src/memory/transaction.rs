const XBEGIN_STARTED: u32 = u32::MAX;
const ABORT_EXPLICIT: u32 = 1 << 0;
const ABORT_RETRY: u32 = 1 << 1;
const ABORT_CONFLICT: u32 = 1 << 2;
const ABORT_CAPACITY: u32 = 1 << 3;
const ABORT_DEBUG: u32 = 1 << 4;
const ABORT_NESTED: u32 = 1 << 5;

core::arch::global_asm!(
    r#"
    .text
    .global boulder_rtm_call
    .type boulder_rtm_call,@function
boulder_rtm_call:
    pushq %r12
    pushq %r13
    pushq %r14
    movq %rdi, %r12
    movq %rsi, %r13
    movq %rdx, %r14
    xbegin 2f
    call *%r12
    movl %eax, (%r13)
    xend
    movl $-1, (%r14)
    jmp 3f
2:
    movl $0, (%r13)
    movl %eax, (%r14)
3:
    popq %r14
    popq %r13
    popq %r12
    ret
    .size boulder_rtm_call, .-boulder_rtm_call
"#,
    options(att_syntax)
);

unsafe extern "C" {
    fn boulder_rtm_call(
        function: unsafe extern "C" fn() -> i32,
        result: *mut i32,
        status: *mut u32,
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransactionAbort {
    pub raw_status: u32,
    pub explicit: bool,
    pub retryable: bool,
    pub conflict: bool,
    pub capacity: bool,
    pub debug: bool,
    pub nested: bool,
    pub code: u8,
}

impl TransactionAbort {
    pub const fn from_raw(status: u32) -> Self {
        Self {
            raw_status: status,
            explicit: status & ABORT_EXPLICIT != 0,
            retryable: status & ABORT_RETRY != 0,
            conflict: status & ABORT_CONFLICT != 0,
            capacity: status & ABORT_CAPACITY != 0,
            debug: status & ABORT_DEBUG != 0,
            nested: status & ABORT_NESTED != 0,
            code: (status >> 24) as u8,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransactionError {
    Unsupported,
    Aborted(TransactionAbort),
}

pub fn rtm_available() -> bool {
    let maximum_leaf = core::arch::x86_64::__cpuid(0).eax;
    maximum_leaf >= 7 && core::arch::x86_64::__cpuid_count(7, 0).ebx & (1 << 11) != 0
}

/// Executes a no-argument foreign call inside an Intel RTM transaction.
///
/// RTM is an optional rollback mechanism for transactional cacheable-memory
/// writes. It is not an isolation boundary: callers must still validate and
/// sandbox the function, and must assume I/O or unsupported instructions can
/// abort without executing the intended operation.
///
/// # Safety
///
/// `function` must obey the System V C ABI, must not unwind, and must be safe
/// to invoke in the current kernel context. Its nontransactional side effects
/// are outside this function's control.
pub unsafe fn execute_transactional_driver_call(
    function: unsafe extern "C" fn() -> i32,
) -> Result<i32, TransactionError> {
    if !rtm_available() {
        return Err(TransactionError::Unsupported);
    }
    let mut result = 0_i32;
    let mut status = 0_u32;
    unsafe { boulder_rtm_call(function, &mut result, &mut status) };
    if status == XBEGIN_STARTED {
        Ok(result)
    } else {
        Err(TransactionError::Aborted(TransactionAbort::from_raw(
            status,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_rtm_abort_status_without_executing_rtm() {
        let status = ABORT_EXPLICIT | ABORT_RETRY | ABORT_CONFLICT | (0x42 << 24);
        let abort = TransactionAbort::from_raw(status);
        assert!(abort.explicit);
        assert!(abort.retryable);
        assert!(abort.conflict);
        assert!(!abort.capacity);
        assert_eq!(abort.code, 0x42);
    }
}
