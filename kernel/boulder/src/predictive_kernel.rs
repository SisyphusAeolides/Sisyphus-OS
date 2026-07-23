//! Retained predictive-containment kernel interface.
//!
//! Observation enqueues bounded transitions. Identification and planning are
//! explicit deferred operations. Queue authority remains in the certified
//! mathematical runtime.

use crate::manifold_orchestrator::Actuation;
use crate::predictive_control::{
    ModelUpdateReport, PlanCertificate, PredictiveDirective, PredictivePolicy, PredictiveRuntime,
    PredictiveRuntimeError, PredictiveSecrets,
};
use crate::sync::SpinLock;
use crate::tensor_decomp::MultilinearDirective;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PredictiveKernelError {
    AlreadyInitialized,
    NotInitialized,
    Runtime(PredictiveRuntimeError),
}

impl From<PredictiveRuntimeError> for PredictiveKernelError {
    fn from(error: PredictiveRuntimeError) -> Self {
        Self::Runtime(error)
    }
}

static PREDICTIVE_STATE: SpinLock<Option<PredictiveRuntime>> = SpinLock::new(None);

pub fn initialize(
    secrets: PredictiveSecrets,
    policy: PredictivePolicy,
) -> Result<(), PredictiveKernelError> {
    let mut state = PREDICTIVE_STATE.lock();
    if state.is_some() {
        return Err(PredictiveKernelError::AlreadyInitialized);
    }

    *state = Some(PredictiveRuntime::new(secrets, policy)?);
    Ok(())
}

pub fn observe(
    actuation: &Actuation,
    tensor: Option<&MultilinearDirective>,
) -> Result<(), PredictiveKernelError> {
    let mut state = PREDICTIVE_STATE.lock();
    let runtime = state
        .as_mut()
        .ok_or(PredictiveKernelError::NotInitialized)?;
    runtime.observe(actuation, tensor)?;
    Ok(())
}

pub fn update_model_deferred() -> Result<Option<ModelUpdateReport>, PredictiveKernelError> {
    let mut state = PREDICTIVE_STATE.lock();
    let runtime = state
        .as_mut()
        .ok_or(PredictiveKernelError::NotInitialized)?;
    runtime.update_model_deferred().map_err(Into::into)
}

pub fn plan_deferred()
-> Result<Option<(PredictiveDirective, PlanCertificate)>, PredictiveKernelError> {
    let mut state = PREDICTIVE_STATE.lock();
    let runtime = state
        .as_mut()
        .ok_or(PredictiveKernelError::NotInitialized)?;
    runtime.plan_deferred().map_err(Into::into)
}

pub fn mark_applied(directive: PredictiveDirective) -> Result<(), PredictiveKernelError> {
    let mut state = PREDICTIVE_STATE.lock();
    let runtime = state
        .as_mut()
        .ok_or(PredictiveKernelError::NotInitialized)?;
    runtime.mark_applied(directive).map_err(Into::into)
}

pub fn last_directive() -> Result<Option<PredictiveDirective>, PredictiveKernelError> {
    let state = PREDICTIVE_STATE.lock();
    let runtime = state
        .as_ref()
        .ok_or(PredictiveKernelError::NotInitialized)?;
    Ok(runtime.last_directive())
}

pub fn last_plan() -> Result<Option<PlanCertificate>, PredictiveKernelError> {
    let state = PREDICTIVE_STATE.lock();
    let runtime = state
        .as_ref()
        .ok_or(PredictiveKernelError::NotInitialized)?;
    Ok(runtime.last_plan())
}

pub fn last_update() -> Result<Option<ModelUpdateReport>, PredictiveKernelError> {
    let state = PREDICTIVE_STATE.lock();
    let runtime = state
        .as_ref()
        .ok_or(PredictiveKernelError::NotInitialized)?;
    Ok(runtime.last_update())
}

pub fn initialized() -> bool {
    PREDICTIVE_STATE.lock().is_some()
}
