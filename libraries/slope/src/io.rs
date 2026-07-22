use crate::{SyscallError, syscall};

use aether::grimoire;

pub fn write(handle: usize, bytes: &[u8]) -> Result<usize, SyscallError> {
    let arguments = [handle, bytes.as_ptr() as usize, bytes.len(), 0, 0, 0];
    // SAFETY: The kernel copies from the immutable byte slice during the call;
    // the pointer and length remain valid until it returns.
    unsafe { syscall(grimoire::SYS_WRITE, arguments) }
}
