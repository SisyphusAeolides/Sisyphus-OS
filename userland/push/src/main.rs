#![no_std]
#![no_main]

use core::future::Future;
use core::panic::PanicInfo;
use core::pin::Pin;
use core::task::{Context, Poll};
use push::aegis::{BrokerError, HardwareCapabilityBroker, evaluate_knot};
use push::gordian::{CapabilityHandle, CapabilityKind, service_knot};
use push::service::{FailureReason, ServiceId, Supervisor, SupervisorAction};
use slope::kairos::{WorkloadClass, features};
use slope::runtime::ProcessRuntime;
use slope::thermogenesis::{ThermalGuard, ThermalPage, ThermalPolicy, throttled_batch_size};

#[global_allocator]
static HEAP: slope::memory::GlobalSlabHeap = slope::memory::GlobalSlabHeap::new();

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

pub const THERMAL_PAGE_ADDRESS: usize = 0x0080_0000;
fn thermal_page() -> &'static ThermalPage {
    unsafe { &*(THERMAL_PAGE_ADDRESS as *const ThermalPage) }
}

struct DispatchProbe {
    remaining: u8,
    units: usize,
}

impl Future for DispatchProbe {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let batch = throttled_batch_size(thermal_page(), self.units);
        self.units = self.units.saturating_sub(batch.max(1));
        if self.remaining == 0 || self.units == 0 {
            Poll::Ready(())
        } else {
            self.remaining -= 1;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

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
    let runtime = match unsafe {
        ProcessRuntime::initialize(
            stack_ptr,
            features::SYSCALL_BASIC,
            features::ASYNC_IO
                | features::THERMAL_PAGE
                | features::KAIROS_PAGE
                | features::OFFLOAD_DISPATCH
                | features::HOLOGRAM_FS,
            WorkloadClass::Compute,
            1024,
        )
    } {
        Ok(runtime) => runtime,
        Err(error) => {
            push::push_log!("[PID 1] runtime initialization failed: {:?}", error);
            let _ = slope::process::request_exit(127);
            loop {
                core::hint::spin_loop();
            }
        }
    };
    push::push_log!(
        "[PID 1] argv={} topology cpus={} domains={} affinity domain={} kind={:?} partitions={} optional_missing={:#x}",
        runtime.argv.len(),
        runtime.kairos.topology.cpus().len(),
        runtime.kairos.topology.domains().len(),
        runtime.affinity.domain_id,
        runtime.affinity.preferred_kind,
        runtime.partition.count,
        runtime.kairos.features.unavailable_lo,
    );

    let mut executor = slope::executor::OuroborosExecutor::new();
    let mut dispatch = DispatchProbe {
        remaining: 4,
        units: 1024,
    };
    unsafe {
        executor
            .spawn_raw(&mut dispatch)
            .expect("executor arena exhausted");
    }
    let mut thermal = ThermalPolicy::new(thermal_page());
    while executor.run_until_stall() != 0 {
        let _guard = ThermalGuard::enter(thermal_page());
        thermal.check_transition();
        push::push_log!(
            "[PID 1] dispatch pass alive={} thermal={:?} hint={:?} partition_units={}",
            executor.task_count(),
            thermal.current_zone(),
            thermal_page().hint(),
            throttled_batch_size(
                thermal_page(),
                runtime
                    .partition
                    .iter()
                    .next()
                    .map(|s| s.len())
                    .unwrap_or(0)
            ),
        );
    }
    push::push_log!("[PID 1] Kairos-dispatched workload complete");

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
                match slope::process::spawn(0, service.id as u8) {
                    Ok(pid) => {
                        push::push_log!("[PID 1] spawned service {:?} as PID {}", service.id, pid);
                    }
                    Err(_) => {
                        let _ =
                            supervisor.record_failure(service.id, FailureReason::LaunchUnavailable);
                    }
                }
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
        match slope::process::wait_nohang() {
            Ok(Some((pid, status))) => {
                push::push_log!("[PID 1] child {} exited with status {}", pid, status);
                // Simple demonstration; in a real OS we'd map PID to service.id
            }
            _ => {}
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
