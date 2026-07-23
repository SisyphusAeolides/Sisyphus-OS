//! Retained multilinear kernel integration.
//!
//! Observation only writes one bounded telemetry slice. SGD and full
//! decomposition are explicit deferred operations.

use crate::manifold_orchestrator::Actuation;
use crate::sync::SpinLock;
use crate::tensor_decomp::{
    MultilinearCertificate, MultilinearDirective, MultilinearError, MultilinearPolicy,
    MultilinearRuntime, SgdCertificate, TensorError,
};

#[derive(Debug, Eq, PartialEq)]
pub enum TensorKernelError {
    AlreadyInitialized,
    NotInitialized,
    Tensor(TensorError),
    Multilinear(MultilinearError),
    InvalidDirective,
}

impl From<TensorError> for TensorKernelError {
    fn from(error: TensorError) -> Self {
        Self::Tensor(error)
    }
}

impl From<MultilinearError> for TensorKernelError {
    fn from(error: MultilinearError) -> Self {
        Self::Multilinear(error)
    }
}

struct TensorKernelState {
    runtime: MultilinearRuntime,
    secret: u64,
}

static TENSOR_STATE: SpinLock<Option<TensorKernelState>> = SpinLock::new(None);

pub fn initialize(secret: u64, policy: MultilinearPolicy) -> Result<(), TensorKernelError> {
    let mut state = TENSOR_STATE.lock();
    if state.is_some() {
        return Err(TensorKernelError::AlreadyInitialized);
    }

    let runtime = MultilinearRuntime::new(secret, policy)?;
    *state = Some(TensorKernelState { runtime, secret });
    Ok(())
}

pub fn observe(actuation: &Actuation) -> Result<(), TensorKernelError> {
    let mut state = TENSOR_STATE.lock();
    let state = state.as_mut().ok_or(TensorKernelError::NotInitialized)?;
    state.runtime.observe_manifold(actuation)?;
    Ok(())
}

pub fn record_external_q24(metric: usize, value_q24: i64) -> Result<(), TensorKernelError> {
    let mut state = TENSOR_STATE.lock();
    let state = state.as_mut().ok_or(TensorKernelError::NotInitialized)?;
    state.runtime.record_external_q24(metric, value_q24)?;
    Ok(())
}

pub fn update_online_deferred() -> Result<Option<SgdCertificate>, TensorKernelError> {
    let mut state = TENSOR_STATE.lock();
    let state = state.as_mut().ok_or(TensorKernelError::NotInitialized)?;
    state.runtime.update_online_deferred().map_err(Into::into)
}

pub fn analyze_deferred() -> Result<Option<MultilinearDirective>, TensorKernelError> {
    let mut state = TENSOR_STATE.lock();
    let state = state.as_mut().ok_or(TensorKernelError::NotInitialized)?;

    let Some(directive) = state.runtime.analyze_full_deferred()? else {
        return Ok(None);
    };
    let certificate = state
        .runtime
        .last_certificate()
        .ok_or(TensorKernelError::InvalidDirective)?;

    let expected_cp_root = certificate
        .ccd
        .map(|value| value.model_root)
        .unwrap_or(certificate.cp.model_root);

    if !directive.verify(state.secret)
        || directive.certificate_root != certificate.root
        || certificate.directive_root != directive.root
        || directive.cp_model_root != expected_cp_root
        || directive.tucker_model_root != certificate.hooi.model_root
        || directive.train_root != certificate.tt.train_root
    {
        return Err(TensorKernelError::InvalidDirective);
    }

    Ok(Some(directive))
}

pub fn last_directive() -> Result<Option<MultilinearDirective>, TensorKernelError> {
    let state = TENSOR_STATE.lock();
    let state = state.as_ref().ok_or(TensorKernelError::NotInitialized)?;
    Ok(state.runtime.last_directive())
}

pub fn last_certificate() -> Result<Option<MultilinearCertificate>, TensorKernelError> {
    let state = TENSOR_STATE.lock();
    let state = state.as_ref().ok_or(TensorKernelError::NotInitialized)?;
    Ok(state.runtime.last_certificate())
}

pub fn initialized() -> bool {
    TENSOR_STATE.lock().is_some()
}
