use super::dispatch::{AttemptOutcome, DispatchResolution, FaultCode};
use super::fingerprint::GpuFingerprint;
use super::model::{DriverStrategy, OracleDecision};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum DriverNetEventKind {
    Fingerprint = 1,
    OracleDecision = 2,
    CandidateAttempt = 3,
    Rollback = 4,
    Commit = 5,
    PrimarySelected = 6,
    FirmwareFallback = 7,
    Quarantine = 8,
    InventoryOverflow = 9,
    NoDisplay = 10,
    ConfigurationIncomplete = 11,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DriverNetEvent {
    pub tick: u64,
    pub kind: DriverNetEventKind,
    pub severity: u8,
    pub strategy: DriverStrategy,
    pub address: u32,
    pub fingerprint_root: u64,
    pub decision_root: u64,
    pub data0: u64,
    pub data1: u64,
    pub root: u64,
}

impl DriverNetEvent {
    pub const EMPTY: Self = Self {
        tick: 0,
        kind: DriverNetEventKind::Fingerprint,
        severity: 0,
        strategy: DriverStrategy::Quarantine,
        address: 0,
        fingerprint_root: 0,
        decision_root: 0,
        data0: 0,
        data1: 0,
        root: 0,
    };

    pub fn seal(&mut self, secret: u64) {
        self.root = 0;
        self.root = event_root(secret, self);
    }

    pub fn verify(&self, secret: u64) -> bool {
        self.root == event_root(secret, self)
    }
}

pub trait DriverNetObserver {
    fn observe(&mut self, event: DriverNetEvent);
}

pub struct NullObserver;

impl DriverNetObserver for NullObserver {
    fn observe(&mut self, event: DriverNetEvent) {
        let _ = event.root;
    }
}

pub fn fingerprint_event(secret: u64, tick: u64, fingerprint: &GpuFingerprint) -> DriverNetEvent {
    let mut event = DriverNetEvent {
        tick,
        kind: DriverNetEventKind::Fingerprint,
        severity: 0,
        strategy: DriverStrategy::Quarantine,
        address: packed_address(fingerprint),
        fingerprint_root: fingerprint.evidence_root,
        decision_root: 0,
        data0: u64::from(fingerprint.vendor_id)
            | (u64::from(fingerprint.device_id) << 16)
            | (u64::from(fingerprint.subsystem_vendor_id) << 32)
            | (u64::from(fingerprint.subsystem_device_id) << 48),
        data1: u64::from(fingerprint.capability_flags)
            | (u64::from(fingerprint.topology_flags) << 32),
        root: 0,
    };
    event.seal(secret);
    event
}

pub fn decision_event(
    secret: u64,
    tick: u64,
    fingerprint: &GpuFingerprint,
    decision: &OracleDecision,
) -> DriverNetEvent {
    let strategy = decision
        .candidates()
        .first()
        .map(|candidate| candidate.strategy)
        .unwrap_or(DriverStrategy::Quarantine);

    let mut event = DriverNetEvent {
        tick,
        kind: DriverNetEventKind::OracleDecision,
        severity: u8::from(decision.confidence_q16 < 12_000),
        strategy,
        address: packed_address(fingerprint),
        fingerprint_root: fingerprint.evidence_root,
        decision_root: decision.decision_root,
        data0: u64::from(decision.confidence_q16) | (u64::from(decision.reasons) << 16),
        data1: decision.best_margin as u64,
        root: 0,
    };
    event.seal(secret);
    event
}

pub fn resolution_events(
    secret: u64,
    fingerprint: &GpuFingerprint,
    resolution: &DispatchResolution,
    output: &mut [DriverNetEvent],
) -> usize {
    let mut length = 0_usize;

    for attempt in resolution.attempts() {
        let Some(slot) = output.get_mut(length) else {
            return length;
        };

        let kind = if attempt.outcome == AttemptOutcome::RolledBack {
            DriverNetEventKind::Rollback
        } else {
            DriverNetEventKind::CandidateAttempt
        };
        let severity = match attempt.outcome {
            AttemptOutcome::Committed => 0,
            AttemptOutcome::Skipped => 1,
            AttemptOutcome::RolledBack => 2,
            AttemptOutcome::ProbeRejected
            | AttemptOutcome::ActivationRejected
            | AttemptOutcome::HealthRejected
            | AttemptOutcome::CommitRejected => 3,
        };

        *slot = DriverNetEvent {
            tick: attempt.ended_tick,
            kind,
            severity,
            strategy: attempt.strategy,
            address: packed_address(fingerprint),
            fingerprint_root: fingerprint.evidence_root,
            decision_root: resolution.decision_root,
            data0: u64::from(attempt.evidence_mask)
                | ((attempt.stage as u8 as u64) << 32)
                | ((attempt.outcome as u8 as u64) << 40)
                | ((attempt.fault as u16 as u64) << 48),
            data1: attempt.detail,
            root: 0,
        };
        slot.seal(secret);
        length += 1;
    }

    if let Some(slot) = output.get_mut(length) {
        *slot = DriverNetEvent {
            tick: resolution.lease.committed_tick,
            kind: match resolution.active_strategy {
                DriverStrategy::FirmwareFramebuffer => DriverNetEventKind::FirmwareFallback,
                DriverStrategy::Quarantine => DriverNetEventKind::Quarantine,
                _ => DriverNetEventKind::Commit,
            },
            severity: u8::from(resolution.active_strategy == DriverStrategy::Quarantine)
                .saturating_mul(4),
            strategy: resolution.active_strategy,
            address: packed_address(fingerprint),
            fingerprint_root: fingerprint.evidence_root,
            decision_root: resolution.decision_root,
            data0: resolution.lease.handle,
            data1: resolution.resolution_root,
            root: 0,
        };
        slot.seal(secret);
        length += 1;
    }

    length
}

pub fn terminal_event(
    secret: u64,
    tick: u64,
    kind: DriverNetEventKind,
    strategy: DriverStrategy,
    fingerprint: &GpuFingerprint,
    decision_root: u64,
    fault: FaultCode,
    detail: u64,
) -> DriverNetEvent {
    let mut event = DriverNetEvent {
        tick,
        kind,
        severity: 4,
        strategy,
        address: packed_address(fingerprint),
        fingerprint_root: fingerprint.evidence_root,
        decision_root,
        data0: u64::from(fault as u16),
        data1: detail,
        root: 0,
    };
    event.seal(secret);
    event
}

pub fn packed_address(fingerprint: &GpuFingerprint) -> u32 {
    u32::from(fingerprint.segment)
        | (u32::from(fingerprint.bus) << 16)
        | (u32::from(fingerprint.slot) << 24)
        | (u32::from(fingerprint.function) << 29)
}

fn event_root(secret: u64, event: &DriverNetEvent) -> u64 {
    let mut state = mix(secret, event.tick);
    state = mix(state, event.kind as u16 as u64);
    state = mix(state, u64::from(event.severity));
    state = mix(state, event.strategy.index() as u64);
    state = mix(state, u64::from(event.address));
    state = mix(state, event.fingerprint_root);
    state = mix(state, event.decision_root);
    state = mix(state, event.data0);
    mix(state, event.data1)
}

fn mix(mut state: u64, word: u64) -> u64 {
    state ^= word.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    state ^= state >> 30;
    state = state.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    state ^= state >> 27;
    state = state.wrapping_mul(0x94d0_49bb_1331_11eb);
    state ^ (state >> 31)
}
