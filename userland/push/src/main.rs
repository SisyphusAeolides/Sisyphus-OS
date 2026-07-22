#![no_std]
#![no_main]

use core::panic::PanicInfo;
use push::aegis::{BrokerError, HardwareCapabilityBroker, evaluate_knot};
use push::gordian::{CapabilityHandle, CapabilityKind, service_knot};
use push::service::{FailureReason, ServiceId, Supervisor, SupervisorAction};

core::arch::global_asm!(
    ".section .text._start,\"ax\"",
    ".global _start",
    ".type _start,@function",
    "_start:",
    "mov %rsp, %rdi",
    "jmp push_start_with_stack",
    ".size _start, .-_start",
    options(att_syntax)
);

struct UnavailableBroker;

impl HardwareCapabilityBroker for UnavailableBroker {
    fn grant(
        &mut self,
        _service: ServiceId,
        _capability: CapabilityKind,
    ) -> Result<CapabilityHandle, BrokerError> {
        Err(BrokerError::Unavailable)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn push_start_with_stack(stack_ptr: *const u8) -> ! {
    push::push_log!("[PID 1] measured push engine online");
    let _ = stack_ptr;
    push::push_log!("[PID 1] argv/envp ABI accepted; Kairos runtime handoff pending kernel user-copy fix");

    let mut supervisor = Supervisor::new();
    let mut broker = UnavailableBroker;
    loop {
        for service in [ServiceId::SlopeNet, ServiceId::Corinth, ServiceId::Crest] {
            let _ = evaluate_knot(service, &supervisor, service_knot(service), &mut broker);
        }
        match supervisor.tick() {
            SupervisorAction::Start(service) => {
                push::push_log!(
                    "[PID 1] requesting '{}' critical={} restart={}/{}",
                    service.name,
                    service.critical,
                    supervisor.status(service.id).restart_count,
                    service.maximum_restarts,
                );
                // Spawn is intentionally not forged. Until Boulder provides a
                // process capability, this explicit failure exercises bounded
                // restart and recovery policy without claiming a child exists.
                let _ = supervisor.record_failure(service.id, FailureReason::LaunchUnavailable);
            }
            SupervisorAction::EnterRecovery { failed_service } => {
                push::push_log!(
                    "[PID 1] critical service {:?} exhausted; entering recovery mode",
                    failed_service,
                );
            }
            SupervisorAction::Deadlock { service } => {
                push::push_log!(
                    "[PID 1] thermodynamic deadlock; failure mass={} service={:?}",
                    supervisor.failure_mass(service),
                    service,
                );
                let _ = supervisor.record_failure(service, FailureReason::Unresponsive);
            }
            SupervisorAction::Idle => {}
        }
        if slope::process::yield_now().is_err() {
            core::hint::spin_loop();
        }
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    push::push_log!("[PID 1] unrecoverable panic");
    let _ = slope::process::request_exit(1);
    loop {
        let _ = slope::process::yield_now();
    }
}
