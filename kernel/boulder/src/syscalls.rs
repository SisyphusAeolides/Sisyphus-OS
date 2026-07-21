pub const SYSCALL_WRITE: usize = 1;
pub const SYSCALL_EXIT: usize = 2;

pub fn dispatch(number: usize, _arguments: [usize; 6]) -> isize {
    match number {
        SYSCALL_WRITE | SYSCALL_EXIT => 0,
        _ => -1,
    }
}
