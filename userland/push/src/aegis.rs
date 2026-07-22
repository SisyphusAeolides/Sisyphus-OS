use crate::gordian::{
    CapabilityHandle, CapabilityKind, DenialReason, GordianKnot, KnotError, KnotRequest,
};
use crate::service::{ServiceId, ServiceState, Supervisor};

pub trait HardwareCapabilityBroker {
    fn grant(
        &mut self,
        service: ServiceId,
        capability: CapabilityKind,
    ) -> Result<CapabilityHandle, BrokerError>;
}

pub const fn is_capability_legal(service: ServiceId, capability: CapabilityKind) -> bool {
    matches!(
        (service, capability),
        (ServiceId::Corinth, CapabilityKind::Network) | (ServiceId::Crest, CapabilityKind::GpuDrm)
    )
}

/// Evaluates at most one claimed request and never maps hardware in PID 1.
pub fn evaluate_knot<B: HardwareCapabilityBroker>(
    service: ServiceId,
    supervisor: &Supervisor,
    knot: &GordianKnot,
    broker: &mut B,
) -> Result<Evaluation, KnotError> {
    let Some(request) = knot.claim()? else {
        return Ok(Evaluation::Idle);
    };
    if request.service != service {
        knot.deny(request.sequence, DenialReason::MalformedRequest)?;
        return Ok(Evaluation::Denied(DenialReason::MalformedRequest));
    }
    if supervisor.status(service).state != ServiceState::Running {
        knot.deny(request.sequence, DenialReason::ServiceNotRunning)?;
        return Ok(Evaluation::Denied(DenialReason::ServiceNotRunning));
    }
    if !is_capability_legal(service, request.capability) {
        knot.deny(request.sequence, DenialReason::PolicyRejected)?;
        return Ok(Evaluation::Denied(DenialReason::PolicyRejected));
    }
    complete_broker_request(request, knot, broker)
}

fn complete_broker_request<B: HardwareCapabilityBroker>(
    request: KnotRequest,
    knot: &GordianKnot,
    broker: &mut B,
) -> Result<Evaluation, KnotError> {
    match broker.grant(request.service, request.capability) {
        Ok(handle) => {
            knot.grant(request.sequence, handle)?;
            Ok(Evaluation::Granted(handle))
        }
        Err(_) => {
            knot.deny(request.sequence, DenialReason::BrokerUnavailable)?;
            Ok(Evaluation::Denied(DenialReason::BrokerUnavailable))
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Evaluation {
    Idle,
    Granted(CapabilityHandle),
    Denied(DenialReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrokerError {
    Unavailable,
    Exhausted,
    Rejected,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gordian::KnotResponse;
    use crate::service::SupervisorAction;

    struct Broker;

    impl HardwareCapabilityBroker for Broker {
        fn grant(
            &mut self,
            _service: ServiceId,
            _capability: CapabilityKind,
        ) -> Result<CapabilityHandle, BrokerError> {
            // SAFETY: The test broker models one live kernel handle.
            Ok(unsafe { CapabilityHandle::from_kernel(0x1_0000_0001).unwrap() })
        }
    }

    fn running_corinth() -> Supervisor {
        let mut supervisor = Supervisor::new();
        assert!(matches!(supervisor.tick(), SupervisorAction::Start(_)));
        supervisor.record_started(ServiceId::SlopeNet).unwrap();
        assert!(matches!(supervisor.tick(), SupervisorAction::Start(_)));
        supervisor.record_started(ServiceId::Corinth).unwrap();
        supervisor
    }

    #[test]
    fn grants_an_opaque_network_handle_to_running_corinth() {
        let supervisor = running_corinth();
        let knot = GordianKnot::new();
        knot.request(1, ServiceId::Corinth, CapabilityKind::Network)
            .unwrap();
        let result = evaluate_knot(ServiceId::Corinth, &supervisor, &knot, &mut Broker).unwrap();
        assert!(matches!(result, Evaluation::Granted(_)));
        assert!(matches!(
            knot.response(1).unwrap(),
            KnotResponse::Granted(_)
        ));
        knot.acknowledge(1).unwrap();
    }

    #[test]
    fn denies_gpu_escalation_by_corinth() {
        let supervisor = running_corinth();
        let knot = GordianKnot::new();
        knot.request(2, ServiceId::Corinth, CapabilityKind::GpuDrm)
            .unwrap();
        assert_eq!(
            evaluate_knot(ServiceId::Corinth, &supervisor, &knot, &mut Broker).unwrap(),
            Evaluation::Denied(DenialReason::PolicyRejected)
        );
    }
}
