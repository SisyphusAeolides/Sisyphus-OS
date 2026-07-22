#![no_std]

pub mod bridge;
pub mod env;
pub mod executor;
pub mod fabric;
pub mod fs;
pub mod io;
pub mod ipc;
pub mod kairos;
pub mod memory;
pub mod net;
pub mod process;
pub mod runtime;
pub mod signal;
pub mod storage;
pub mod sync;
pub mod syscalls;
pub mod thermogenesis;
pub mod time;
pub mod capability;
pub mod scheduler;
pub mod nexus;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyscallError(pub isize);

#[cfg(target_arch = "x86_64")]
pub unsafe fn syscall(number: usize, arguments: [usize; 6]) -> Result<usize, SyscallError> {
    let result: isize;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") arguments[0],
            in("rsi") arguments[1],
            in("rdx") arguments[2],
            in("r10") arguments[3],
            in("r8")  arguments[4],
            in("r9")  arguments[5],
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    if result < 0 { Err(SyscallError(result)) } else { Ok(result as usize) }
}

#[cfg(not(target_arch = "x86_64"))]
pub unsafe fn syscall(_number: usize, _arguments: [usize; 6]) -> Result<usize, SyscallError> {
    Err(SyscallError(-1))
}
