use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::service::ServiceId;

pub const KNOT_COUNT: usize = 16;

const STATE_IDLE: u32 = 0;
const STATE_WRITING: u32 = 1;
const STATE_REQUESTING: u32 = 2;
const STATE_EVALUATING: u32 = 3;
const STATE_GRANTED: u32 = 4;
const STATE_DENIED: u32 = 5;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum CapabilityKind {
    Network = 1,
    GpuDrm = 2,
    Storage = 3,
}

impl CapabilityKind {
    fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Network),
            2 => Some(Self::GpuDrm),
            3 => Some(Self::Storage),
            _ => None,
        }
    }
}

/// Opaque generation-checked broker handle. It is never a physical address.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapabilityHandle(u64);

impl CapabilityHandle {
    /// Imports a handle minted by the kernel capability broker.
    ///
    /// # Safety
    ///
    /// `raw` must originate from the broker and encode a live generation.
    pub const unsafe fn from_kernel(raw: u64) -> Option<Self> {
        if raw == 0 { None } else { Some(Self(raw)) }
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KnotRequest {
    pub sequence: u64,
    pub service: ServiceId,
    pub capability: CapabilityKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KnotResponse {
    Pending,
    Granted(CapabilityHandle),
    Denied(DenialReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum DenialReason {
    ServiceNotRunning = 1,
    PolicyRejected = 2,
    BrokerUnavailable = 3,
    MalformedRequest = 4,
}

impl DenialReason {
    fn from_raw(raw: u32) -> Self {
        match raw {
            1 => Self::ServiceNotRunning,
            2 => Self::PolicyRejected,
            3 => Self::BrokerUnavailable,
            _ => Self::MalformedRequest,
        }
    }
}

/// One sequence-numbered request/reply slot shared by a service and PID 1.
#[repr(C, align(4096))]
pub struct GordianKnot {
    state: AtomicU32,
    requested_capability: AtomicU32,
    requesting_service: AtomicU32,
    denial_reason: AtomicU32,
    request_sequence: AtomicU64,
    response_sequence: AtomicU64,
    granted_handle: AtomicU64,
}

impl GordianKnot {
    pub const fn new() -> Self {
        Self {
            state: AtomicU32::new(STATE_IDLE),
            requested_capability: AtomicU32::new(0),
            requesting_service: AtomicU32::new(0),
            denial_reason: AtomicU32::new(0),
            request_sequence: AtomicU64::new(0),
            response_sequence: AtomicU64::new(0),
            granted_handle: AtomicU64::new(0),
        }
    }

    pub fn request(
        &self,
        sequence: u64,
        service: ServiceId,
        capability: CapabilityKind,
    ) -> Result<(), KnotError> {
        if sequence == 0 {
            return Err(KnotError::InvalidSequence);
        }
        self.state
            .compare_exchange(
                STATE_IDLE,
                STATE_WRITING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| KnotError::Busy)?;
        self.request_sequence.store(sequence, Ordering::Relaxed);
        self.requesting_service
            .store(service as u32, Ordering::Relaxed);
        self.requested_capability
            .store(capability as u32, Ordering::Relaxed);
        self.granted_handle.store(0, Ordering::Relaxed);
        self.denial_reason.store(0, Ordering::Relaxed);
        self.state.store(STATE_REQUESTING, Ordering::Release);
        Ok(())
    }

    /// Atomically claims one request for Aegis evaluation.
    pub fn claim(&self) -> Result<Option<KnotRequest>, KnotError> {
        if self
            .state
            .compare_exchange(
                STATE_REQUESTING,
                STATE_EVALUATING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return Ok(None);
        }
        let sequence = self.request_sequence.load(Ordering::Relaxed);
        let service = match self.requesting_service.load(Ordering::Relaxed) {
            0 => ServiceId::SlopeNet,
            1 => ServiceId::Corinth,
            2 => ServiceId::Crest,
            _ => {
                self.deny(sequence, DenialReason::MalformedRequest)?;
                return Err(KnotError::MalformedRequest);
            }
        };
        let Some(capability) =
            CapabilityKind::from_raw(self.requested_capability.load(Ordering::Relaxed))
        else {
            self.deny(sequence, DenialReason::MalformedRequest)?;
            return Err(KnotError::MalformedRequest);
        };
        Ok(Some(KnotRequest {
            sequence,
            service,
            capability,
        }))
    }

    pub fn grant(&self, sequence: u64, handle: CapabilityHandle) -> Result<(), KnotError> {
        self.ensure_evaluating(sequence)?;
        self.granted_handle.store(handle.raw(), Ordering::Relaxed);
        self.response_sequence.store(sequence, Ordering::Relaxed);
        self.state.store(STATE_GRANTED, Ordering::Release);
        Ok(())
    }

    pub fn deny(&self, sequence: u64, reason: DenialReason) -> Result<(), KnotError> {
        self.ensure_evaluating(sequence)?;
        self.denial_reason.store(reason as u32, Ordering::Relaxed);
        self.response_sequence.store(sequence, Ordering::Relaxed);
        self.state.store(STATE_DENIED, Ordering::Release);
        Ok(())
    }

    pub fn response(&self, sequence: u64) -> Result<KnotResponse, KnotError> {
        let state = self.state.load(Ordering::Acquire);
        if matches!(
            state,
            STATE_IDLE | STATE_WRITING | STATE_REQUESTING | STATE_EVALUATING
        ) {
            return Ok(KnotResponse::Pending);
        }
        if self.response_sequence.load(Ordering::Relaxed) != sequence {
            return Err(KnotError::StaleSequence);
        }
        match state {
            STATE_GRANTED => {
                let raw = self.granted_handle.load(Ordering::Relaxed);
                // SAFETY: Only Aegis writes this field from a broker-minted
                // handle before publishing STATE_GRANTED.
                let handle = unsafe { CapabilityHandle::from_kernel(raw) }
                    .ok_or(KnotError::MalformedResponse)?;
                Ok(KnotResponse::Granted(handle))
            }
            STATE_DENIED => Ok(KnotResponse::Denied(DenialReason::from_raw(
                self.denial_reason.load(Ordering::Relaxed),
            ))),
            _ => Err(KnotError::MalformedResponse),
        }
    }

    pub fn acknowledge(&self, sequence: u64) -> Result<(), KnotError> {
        if self.response_sequence.load(Ordering::Acquire) != sequence {
            return Err(KnotError::StaleSequence);
        }
        let state = self.state.load(Ordering::Acquire);
        if !matches!(state, STATE_GRANTED | STATE_DENIED) {
            return Err(KnotError::Busy);
        }
        self.state
            .compare_exchange(state, STATE_IDLE, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| KnotError::Busy)?;
        Ok(())
    }

    fn ensure_evaluating(&self, sequence: u64) -> Result<(), KnotError> {
        if self.state.load(Ordering::Acquire) != STATE_EVALUATING {
            return Err(KnotError::NotClaimed);
        }
        if self.request_sequence.load(Ordering::Relaxed) != sequence {
            return Err(KnotError::StaleSequence);
        }
        Ok(())
    }
}

impl Default for GordianKnot {
    fn default() -> Self {
        Self::new()
    }
}

/// Fixed shared-memory bank. Boulder may map individual pages into a service
/// only after authenticating that service and retaining the mapping lease.
pub static SERVICE_KNOTS: [GordianKnot; KNOT_COUNT] = [const { GordianKnot::new() }; KNOT_COUNT];

pub fn service_knot(service: ServiceId) -> &'static GordianKnot {
    &SERVICE_KNOTS[service as usize]
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KnotError {
    Busy,
    InvalidSequence,
    StaleSequence,
    NotClaimed,
    MalformedRequest,
    MalformedResponse,
}
