use crate::{SyscallError, syscall};

pub mod tachyon;

const SYSCALL_EXIT: usize = 2;
const SYSCALL_YIELD: usize = 3;

/// Cooperatively yields the current process while preserving its execution
/// context.
pub fn yield_now() -> Result<(), SyscallError> {
    yield_with_hint(0)
}

/// Cooperatively yields while attaching a bounded scheduler-policy hint.
///
/// The current Boulder boundary records the scalar hint for future scheduler
/// policy; it does not promise retroactive execution or priority changes.
pub fn yield_with_hint(unfinished_work: u64) -> Result<(), SyscallError> {
    // SAFETY: Yield carries no pointer arguments and follows Slope's native
    // six-register syscall convention.
    unsafe { syscall(SYSCALL_YIELD, [unfinished_work as usize, 0, 0, 0, 0, 0]) }.map(|_| ())
}

/// Requests termination of the current process.
///
/// The call returns an error while the running kernel lacks scheduler-owned
/// teardown. Callers must not assume successful termination until this
/// function stops returning on the target kernel.
pub fn request_exit(status: i32) -> Result<(), SyscallError> {
    let arguments = [status as isize as usize, 0, 0, 0, 0, 0];
    // SAFETY: Exit carries only a scalar status and follows Slope's native
    // six-register syscall convention.
    unsafe { syscall(SYSCALL_EXIT, arguments) }.map(|_| ())
}
