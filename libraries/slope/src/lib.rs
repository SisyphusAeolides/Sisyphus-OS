#![no_std]

pub mod bridge;
pub mod io;
pub mod memory;
pub mod net;
pub mod process;
pub mod storage;
pub mod sync;
pub mod time;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyscallError(pub isize);

#[cfg(target_arch = "x86_64")]
/// Enters Boulder through the native syscall instruction.
///
/// # Safety
///
/// Pointer-valued arguments must remain valid for the duration and access mode
/// defined by the selected syscall. The syscall number and argument layout must
/// match the kernel ABI.
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
            in("r8") arguments[4],
            in("r9") arguments[5],
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    if result < 0 {
        Err(SyscallError(result))
    } else {
        Ok(result as usize)
    }
}

#[cfg(not(target_arch = "x86_64"))]
/// Reports that the native syscall path is unavailable on this architecture.
///
/// # Safety
///
/// Callers must still uphold the pointer and argument requirements of the
/// requested syscall so this implementation can be replaced without changing
/// call-site assumptions.
pub unsafe fn syscall(_number: usize, _arguments: [usize; 6]) -> Result<usize, SyscallError> {
    Err(SyscallError(-1))
}
