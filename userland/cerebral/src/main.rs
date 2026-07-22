#![no_std]
#![no_main]

mod quantum_runtime;
mod nexus_reactor;

#[cfg(not(target_os = "none"))]
#[global_allocator]
static ALLOC: slope::memory::GlobalSlabHeap = slope::memory::GlobalSlabHeap::new();

use core::panic::PanicInfo;

use slope::executor::OuroborosExecutor;
use slope::kairos::{features, WorkloadClass};
use slope::runtime::ProcessRuntime;

#[cfg(not(test))]
core::arch::global_asm!(
    ".section .text._start,\"ax\"",
    ".global _start",
    ".type _start,@function",
    "_start:",
    "mov %rsp, %rdi",
    "jmp cerebral_start_with_stack",
    ".size _start, .-_start",
    options(att_syntax)
);

#[cfg(test)]
#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 { 0 }

#[unsafe(no_mangle)]
pub extern "C" fn cerebral_start_with_stack(stack_ptr: *const u8) -> ! {
    let runtime = match unsafe {
        ProcessRuntime::initialize(
            stack_ptr,
            features::SYSCALL_BASIC,
            features::ASYNC_IO
                | features::THERMAL_PAGE
                | features::KAIROS_PAGE
                | features::OFFLOAD_DISPATCH,
            WorkloadClass::Compute,
            2048,
        )
    } {
        Ok(runtime) => runtime,
        Err(_) => terminate(126),
    };

    let capabilities =
        match quantum_runtime::CerebralCapabilities::receive(&runtime.environment) {
            Ok(capabilities) => capabilities,
            Err(_) => terminate(125),
        };

    let mut spawner = OuroborosExecutor::new();

    if quantum_runtime::install(&mut spawner, capabilities).is_err() {
        terminate(124);
    }

    loop {
        if spawner.run_until_stall() == 0 {
            terminate(0);
        }

        if slope::process::yield_now().is_err() {
            core::hint::spin_loop();
        }
    }
}

fn terminate(status: i32) -> ! {
    let _ = slope::process::request_exit(status);

    loop {
        let _ = slope::process::yield_now();
        core::hint::spin_loop();
    }
}

#[cfg(not(test))]
#[panic_handler]
fn panic(_information: &PanicInfo<'_>) -> ! {
    terminate(127)
}
