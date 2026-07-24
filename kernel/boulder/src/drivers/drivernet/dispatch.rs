use super::fingerprint::GpuFingerprint;
use super::model::{DriverStrategy, OracleDecision, RankedCandidate};
use super::registry::{
    MAXIMUM_SHIMS, ProbeSemantic, ProbeStep, RegistryError, SHIM_FLAG_PRESERVE_FIRMWARE_DISPLAY,
    ShimDescriptor, ShimRegistry,
};

pub const MAXIMUM_ATTEMPTS: usize = MAXIMUM_SHIMS;
pub const MAXIMUM_PROBE_OBSERVATIONS: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum DispatchStage {
    CandidateGate = 1,
    BeginTransaction = 2,
    Probe = 3,
    Activate = 4,
    HealthCheck = 5,
    Commit = 6,
    Rollback = 7,
    Complete = 8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum AttemptOutcome {
    Skipped = 1,
    ProbeRejected = 2,
    ActivationRejected = 3,
    HealthRejected = 4,
    CommitRejected = 5,
    RolledBack = 6,
    Committed = 7,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum FaultCode {
    None = 0,
    CandidateInadmissible = 1,
    DescriptorRejected = 2,
    TransactionRejected = 3,
    ProbeTimeout = 4,
    ProbeFault = 5,
    MissingProbeEvidence = 6,
    ActivationTimeout = 7,
    ActivationFault = 8,
    HealthTimeout = 9,
    HealthFault = 10,
    Unhealthy = 11,
    CommitFault = 12,
    RollbackFault = 13,
    DecisionCorrupt = 14,
    FingerprintMismatch = 15,
    NoSafeStrategy = 16,
    TimeRegression = 17,
    RegistryFault = 18,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BackendFault {
    pub code: FaultCode,
    pub retryable: bool,
    pub detail: u64,
}

impl BackendFault {
    pub const fn new(code: FaultCode, retryable: bool, detail: u64) -> Self {
        Self {
            code,
            retryable,
            detail,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProbeObservation {
    pub semantic: ProbeSemantic,
    pub evidence_bit: u32,
    pub value: u64,
    pub tick: u64,
    pub root: u64,
}

impl ProbeObservation {
    pub const EMPTY: Self = Self {
        semantic: ProbeSemantic::ValidateIdentity,
        evidence_bit: 0,
        value: 0,
        tick: 0,
        root: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProbeTransaction {
    pub token: u64,
    pub fingerprint_root: u64,
    pub strategy: DriverStrategy,
    pub started_tick: u64,
    pub deadline_tick: u64,
    pub evidence_mask: u32,
    pub observation_root: u64,
    pub firmware_preserved: bool,
    pub backend_state: [u64; 8],
}

impl ProbeTransaction {
    pub const EMPTY: Self = Self {
        token: 0,
        fingerprint_root: 0,
        strategy: DriverStrategy::Quarantine,
        started_tick: 0,
        deadline_tick: 0,
        evidence_mask: 0,
        observation_root: 0,
        firmware_preserved: false,
        backend_state: [0; 8],
    };

    pub const fn valid(self) -> bool {
        self.token != 0 && self.fingerprint_root != 0 && self.deadline_tick > self.started_tick
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActivationReceipt {
    pub token: u64,
    pub strategy: DriverStrategy,
    pub activation_epoch: u64,
    pub transport_root: u64,
    pub framebuffer_object: u64,
    pub backend_state: [u64; 8],
}

impl ActivationReceipt {
    pub const EMPTY: Self = Self {
        token: 0,
        strategy: DriverStrategy::Quarantine,
        activation_epoch: 0,
        transport_root: 0,
        framebuffer_object: 0,
        backend_state: [0; 8],
    };

    pub const fn valid(self) -> bool {
        self.token != 0 && self.activation_epoch != 0 && self.transport_root != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HealthReceipt {
    pub healthy: bool,
    pub tick: u64,
    pub heartbeat: u64,
    pub fault_mask: u64,
    pub root: u64,
}

impl HealthReceipt {
    pub const EMPTY: Self = Self {
        healthy: false,
        tick: 0,
        heartbeat: 0,
        fault_mask: 0,
        root: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DriverLease {
    pub handle: u64,
    pub generation: u32,
    pub strategy: DriverStrategy,
    pub fingerprint_root: u64,
    pub decision_root: u64,
    pub activation_root: u64,
    pub health_root: u64,
    pub framebuffer_object: u64,
    pub committed_tick: u64,
}

impl DriverLease {
    pub const EMPTY: Self = Self {
        handle: 0,
        generation: 0,
        strategy: DriverStrategy::Quarantine,
        fingerprint_root: 0,
        decision_root: 0,
        activation_root: 0,
        health_root: 0,
        framebuffer_object: 0,
        committed_tick: 0,
    };

    pub const fn valid(self) -> bool {
        self.handle != 0
            && self.generation != 0
            && self.fingerprint_root != 0
            && self.decision_root != 0
            && self.activation_root != 0
            && self.health_root != 0
            && self.committed_tick != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttemptRecord {
    pub strategy: DriverStrategy,
    pub stage: DispatchStage,
    pub outcome: AttemptOutcome,
    pub score: i64,
    pub confidence_q16: u16,
    pub started_tick: u64,
    pub ended_tick: u64,
    pub evidence_mask: u32,
    pub fault: FaultCode,
    pub detail: u64,
    pub root: u64,
}

impl AttemptRecord {
    pub const EMPTY: Self = Self {
        strategy: DriverStrategy::Quarantine,
        stage: DispatchStage::CandidateGate,
        outcome: AttemptOutcome::Skipped,
        score: i64::MIN,
        confidence_q16: 0,
        started_tick: 0,
        ended_tick: 0,
        evidence_mask: 0,
        fault: FaultCode::None,
        detail: 0,
        root: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DispatchResolution {
    pub fingerprint_root: u64,
    pub decision_root: u64,
    pub active_strategy: DriverStrategy,
    pub lease: DriverLease,
    pub attempts: [AttemptRecord; MAXIMUM_ATTEMPTS],
    pub attempt_count: usize,
    pub retained_firmware_display: bool,
    pub resolution_root: u64,
}

impl DispatchResolution {
    pub fn attempts(&self) -> &[AttemptRecord] {
        &self.attempts[..self.attempt_count]
    }

    pub fn verify(&self, secret: u64) -> bool {
        self.attempt_count <= self.attempts.len()
            && self.lease.valid()
            && self.active_strategy == self.lease.strategy
            && self.resolution_root == resolution_root(secret, self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchError {
    CorruptDecision,
    FingerprintMismatch,
    Registry(RegistryError),
    TimeRegression,
    NoSafeStrategy {
        attempts: [AttemptRecord; MAXIMUM_ATTEMPTS],
        attempt_count: usize,
    },
}

impl From<RegistryError> for DispatchError {
    fn from(error: RegistryError) -> Self {
        Self::Registry(error)
    }
}

pub trait DriverNetBackend {
    fn now_tick(&self) -> u64;

    fn begin(
        &mut self,
        fingerprint: &GpuFingerprint,
        descriptor: &ShimDescriptor,
        decision: &OracleDecision,
    ) -> Result<ProbeTransaction, BackendFault>;

    fn probe_step(
        &mut self,
        transaction: &mut ProbeTransaction,
        fingerprint: &GpuFingerprint,
        descriptor: &ShimDescriptor,
        step: ProbeStep,
        attempt: u8,
    ) -> Result<ProbeObservation, BackendFault>;

    fn activate(
        &mut self,
        transaction: &mut ProbeTransaction,
        fingerprint: &GpuFingerprint,
        descriptor: &ShimDescriptor,
    ) -> Result<ActivationReceipt, BackendFault>;

    fn health_check(
        &mut self,
        transaction: &mut ProbeTransaction,
        activation: &ActivationReceipt,
        descriptor: &ShimDescriptor,
    ) -> Result<HealthReceipt, BackendFault>;

    fn commit(
        &mut self,
        transaction: &mut ProbeTransaction,
        activation: &ActivationReceipt,
        health: &HealthReceipt,
        decision: &OracleDecision,
    ) -> Result<DriverLease, BackendFault>;

    fn rollback(
        &mut self,
        transaction: &mut ProbeTransaction,
        stage: DispatchStage,
        fault: BackendFault,
    ) -> Result<(), BackendFault>;
}

pub struct DriverDispatcher {
    secret: u64,
    decision_secret: u64,
    last_tick: u64,
    resolutions: u64,
    rollbacks: u64,
}

impl DriverDispatcher {
    pub fn new(secret: u64, decision_secret: u64) -> Result<Self, DispatchError> {
        if secret == 0 || decision_secret == 0 || secret == decision_secret {
            return Err(DispatchError::CorruptDecision);
        }

        Ok(Self {
            secret,
            decision_secret,
            last_tick: 0,
            resolutions: 0,
            rollbacks: 0,
        })
    }

    pub fn resolve(
        &mut self,
        fingerprint: &GpuFingerprint,
        decision: &OracleDecision,
        registry: &ShimRegistry,
        backend: &mut dyn DriverNetBackend,
    ) -> Result<DispatchResolution, DispatchError> {
        if !decision.verify(self.decision_secret) {
            return Err(DispatchError::CorruptDecision);
        }
        if decision.fingerprint_root != fingerprint.evidence_root {
            return Err(DispatchError::FingerprintMismatch);
        }

        let now = backend.now_tick();
        if self.last_tick != 0 && now < self.last_tick {
            return Err(DispatchError::TimeRegression);
        }
        self.last_tick = now;

        let mut attempts = [AttemptRecord::EMPTY; MAXIMUM_ATTEMPTS];
        let mut attempt_count = 0_usize;
        let mut retained_firmware_display = fingerprint.firmware_display_usable();

        for candidate in decision.candidates().iter().copied() {
            if attempt_count >= attempts.len() {
                break;
            }

            let descriptor = registry.descriptor(candidate.strategy)?;
            let started_tick = backend.now_tick();

            if !candidate.admissible || !descriptor.accepts(fingerprint, decision.confidence_q16) {
                attempts[attempt_count] = seal_attempt(
                    self.secret,
                    AttemptRecord {
                        strategy: candidate.strategy,
                        stage: DispatchStage::CandidateGate,
                        outcome: AttemptOutcome::Skipped,
                        score: candidate.score,
                        confidence_q16: decision.confidence_q16,
                        started_tick,
                        ended_tick: backend.now_tick(),
                        evidence_mask: 0,
                        fault: if candidate.admissible {
                            FaultCode::DescriptorRejected
                        } else {
                            FaultCode::CandidateInadmissible
                        },
                        detail: u64::from(candidate.gate_reason),
                        root: 0,
                    },
                );
                attempt_count += 1;
                continue;
            }

            match self.try_candidate(fingerprint, decision, candidate, descriptor, backend) {
                Ok((lease, record, firmware_preserved)) => {
                    attempts[attempt_count] = record;
                    attempt_count += 1;
                    retained_firmware_display &= firmware_preserved;
                    self.resolutions = self.resolutions.saturating_add(1);
                    self.last_tick = lease.committed_tick;

                    let mut resolution = DispatchResolution {
                        fingerprint_root: fingerprint.evidence_root,
                        decision_root: decision.decision_root,
                        active_strategy: lease.strategy,
                        lease,
                        attempts,
                        attempt_count,
                        retained_firmware_display,
                        resolution_root: 0,
                    };
                    resolution.resolution_root = resolution_root(self.secret, &resolution);
                    return Ok(resolution);
                }
                Err(record) => {
                    attempts[attempt_count] = record;
                    attempt_count += 1;
                    if matches!(record.outcome, AttemptOutcome::RolledBack) {
                        self.rollbacks = self.rollbacks.saturating_add(1);
                    }
                }
            }
        }

        Err(DispatchError::NoSafeStrategy {
            attempts,
            attempt_count,
        })
    }

    pub const fn totals(&self) -> (u64, u64) {
        (self.resolutions, self.rollbacks)
    }

    fn try_candidate(
        &mut self,
        fingerprint: &GpuFingerprint,
        decision: &OracleDecision,
        candidate: RankedCandidate,
        descriptor: &ShimDescriptor,
        backend: &mut dyn DriverNetBackend,
    ) -> Result<(DriverLease, AttemptRecord, bool), AttemptRecord> {
        let started_tick = backend.now_tick();

        let mut transaction = match backend.begin(fingerprint, descriptor, decision) {
            Ok(transaction) if transaction.valid() => transaction,
            Ok(_) => {
                return Err(seal_attempt(
                    self.secret,
                    failure_record(
                        candidate,
                        decision,
                        started_tick,
                        backend.now_tick(),
                        DispatchStage::BeginTransaction,
                        AttemptOutcome::ActivationRejected,
                        FaultCode::TransactionRejected,
                        0,
                        0,
                    ),
                ));
            }
            Err(fault) => {
                return Err(seal_attempt(
                    self.secret,
                    failure_record(
                        candidate,
                        decision,
                        started_tick,
                        backend.now_tick(),
                        DispatchStage::BeginTransaction,
                        AttemptOutcome::ActivationRejected,
                        fault.code,
                        fault.detail,
                        0,
                    ),
                ));
            }
        };

        if transaction.fingerprint_root != fingerprint.evidence_root
            || transaction.strategy != candidate.strategy
        {
            let fault = BackendFault::new(
                FaultCode::FingerprintMismatch,
                false,
                transaction.fingerprint_root,
            );
            let rollback =
                backend.rollback(&mut transaction, DispatchStage::BeginTransaction, fault);
            return Err(self.rollback_record(
                candidate,
                decision,
                started_tick,
                backend.now_tick(),
                DispatchStage::BeginTransaction,
                transaction.evidence_mask,
                fault,
                rollback,
            ));
        }

        if descriptor.flags & SHIM_FLAG_PRESERVE_FIRMWARE_DISPLAY != 0
            && fingerprint.firmware_display_usable()
            && !transaction.firmware_preserved
        {
            let fault = BackendFault::new(
                FaultCode::TransactionRejected,
                false,
                SHIM_FLAG_PRESERVE_FIRMWARE_DISPLAY as u64,
            );
            let rollback =
                backend.rollback(&mut transaction, DispatchStage::BeginTransaction, fault);
            return Err(self.rollback_record(
                candidate,
                decision,
                started_tick,
                backend.now_tick(),
                DispatchStage::BeginTransaction,
                transaction.evidence_mask,
                fault,
                rollback,
            ));
        }

        if let Err(fault) =
            self.run_probe_program(&mut transaction, fingerprint, descriptor, backend)
        {
            let rollback = backend.rollback(&mut transaction, DispatchStage::Probe, fault);
            return Err(self.rollback_record(
                candidate,
                decision,
                started_tick,
                backend.now_tick(),
                DispatchStage::Probe,
                transaction.evidence_mask,
                fault,
                rollback,
            ));
        }

        if transaction.evidence_mask & descriptor.program.required_evidence
            != descriptor.program.required_evidence
        {
            let missing = descriptor.program.required_evidence & !transaction.evidence_mask;
            let fault =
                BackendFault::new(FaultCode::MissingProbeEvidence, false, u64::from(missing));
            let rollback = backend.rollback(&mut transaction, DispatchStage::Probe, fault);
            return Err(self.rollback_record(
                candidate,
                decision,
                started_tick,
                backend.now_tick(),
                DispatchStage::Probe,
                transaction.evidence_mask,
                fault,
                rollback,
            ));
        }

        let activation_deadline = backend
            .now_tick()
            .saturating_add(descriptor.activation_budget_ticks);
        let activation = match backend.activate(&mut transaction, fingerprint, descriptor) {
            Ok(receipt) if receipt.valid() && receipt.strategy == descriptor.strategy => receipt,
            Ok(_) => {
                let fault = BackendFault::new(FaultCode::ActivationFault, false, 0);
                let rollback = backend.rollback(&mut transaction, DispatchStage::Activate, fault);
                return Err(self.rollback_record(
                    candidate,
                    decision,
                    started_tick,
                    backend.now_tick(),
                    DispatchStage::Activate,
                    transaction.evidence_mask,
                    fault,
                    rollback,
                ));
            }
            Err(fault) => {
                let rollback = backend.rollback(&mut transaction, DispatchStage::Activate, fault);
                return Err(self.rollback_record(
                    candidate,
                    decision,
                    started_tick,
                    backend.now_tick(),
                    DispatchStage::Activate,
                    transaction.evidence_mask,
                    fault,
                    rollback,
                ));
            }
        };

        if backend.now_tick() > activation_deadline {
            let fault = BackendFault::new(FaultCode::ActivationTimeout, false, activation_deadline);
            let rollback = backend.rollback(&mut transaction, DispatchStage::Activate, fault);
            return Err(self.rollback_record(
                candidate,
                decision,
                started_tick,
                backend.now_tick(),
                DispatchStage::Activate,
                transaction.evidence_mask,
                fault,
                rollback,
            ));
        }

        let health_deadline = backend
            .now_tick()
            .saturating_add(descriptor.health_budget_ticks);
        let health = match backend.health_check(&mut transaction, &activation, descriptor) {
            Ok(receipt) if receipt.healthy && receipt.root != 0 => receipt,
            Ok(receipt) => {
                let fault = BackendFault::new(FaultCode::Unhealthy, false, receipt.fault_mask);
                let rollback =
                    backend.rollback(&mut transaction, DispatchStage::HealthCheck, fault);
                return Err(self.rollback_record(
                    candidate,
                    decision,
                    started_tick,
                    backend.now_tick(),
                    DispatchStage::HealthCheck,
                    transaction.evidence_mask,
                    fault,
                    rollback,
                ));
            }
            Err(fault) => {
                let rollback =
                    backend.rollback(&mut transaction, DispatchStage::HealthCheck, fault);
                return Err(self.rollback_record(
                    candidate,
                    decision,
                    started_tick,
                    backend.now_tick(),
                    DispatchStage::HealthCheck,
                    transaction.evidence_mask,
                    fault,
                    rollback,
                ));
            }
        };

        if backend.now_tick() > health_deadline {
            let fault = BackendFault::new(FaultCode::HealthTimeout, false, health_deadline);
            let rollback = backend.rollback(&mut transaction, DispatchStage::HealthCheck, fault);
            return Err(self.rollback_record(
                candidate,
                decision,
                started_tick,
                backend.now_tick(),
                DispatchStage::HealthCheck,
                transaction.evidence_mask,
                fault,
                rollback,
            ));
        }

        let lease = match backend.commit(&mut transaction, &activation, &health, decision) {
            Ok(lease)
                if lease.valid()
                    && lease.strategy == descriptor.strategy
                    && lease.fingerprint_root == fingerprint.evidence_root
                    && lease.decision_root == decision.decision_root =>
            {
                lease
            }
            Ok(_) => {
                let fault = BackendFault::new(FaultCode::CommitFault, false, 0);
                let rollback = backend.rollback(&mut transaction, DispatchStage::Commit, fault);
                return Err(self.rollback_record(
                    candidate,
                    decision,
                    started_tick,
                    backend.now_tick(),
                    DispatchStage::Commit,
                    transaction.evidence_mask,
                    fault,
                    rollback,
                ));
            }
            Err(fault) => {
                let rollback = backend.rollback(&mut transaction, DispatchStage::Commit, fault);
                return Err(self.rollback_record(
                    candidate,
                    decision,
                    started_tick,
                    backend.now_tick(),
                    DispatchStage::Commit,
                    transaction.evidence_mask,
                    fault,
                    rollback,
                ));
            }
        };

        let record = seal_attempt(
            self.secret,
            AttemptRecord {
                strategy: candidate.strategy,
                stage: DispatchStage::Complete,
                outcome: AttemptOutcome::Committed,
                score: candidate.score,
                confidence_q16: decision.confidence_q16,
                started_tick,
                ended_tick: lease.committed_tick,
                evidence_mask: transaction.evidence_mask,
                fault: FaultCode::None,
                detail: lease.handle,
                root: 0,
            },
        );

        Ok((lease, record, transaction.firmware_preserved))
    }

    fn run_probe_program(
        &self,
        transaction: &mut ProbeTransaction,
        fingerprint: &GpuFingerprint,
        descriptor: &ShimDescriptor,
        backend: &mut dyn DriverNetBackend,
    ) -> Result<(), BackendFault> {
        let program_deadline = backend
            .now_tick()
            .saturating_add(descriptor.program.maximum_total_ticks);
        let mut observations = [ProbeObservation::EMPTY; MAXIMUM_PROBE_OBSERVATIONS];
        let mut observation_count = 0_usize;

        for step in descriptor.program.steps().iter().copied() {
            let step_start = backend.now_tick();
            let step_deadline = step_start.saturating_add(u64::from(step.maximum_ticks));
            let mut attempt = 0_u8;

            loop {
                match backend.probe_step(transaction, fingerprint, descriptor, step, attempt) {
                    Ok(observation)
                        if observation.semantic == step.semantic
                            && observation.evidence_bit & step.evidence_bit
                                == step.evidence_bit
                            && observation.root != 0 =>
                    {
                        transaction.evidence_mask |= observation.evidence_bit;
                        transaction.observation_root =
                            mix(transaction.observation_root, observation.root);

                        if let Some(slot) = observations.get_mut(observation_count) {
                            *slot = observation;
                            observation_count += 1;
                        }
                        break;
                    }
                    Ok(observation) => {
                        let fault =
                            BackendFault::new(FaultCode::ProbeFault, false, observation.root);
                        return Err(fault);
                    }
                    Err(fault) if fault.retryable && attempt < step.retries => {
                        attempt = attempt.saturating_add(1);
                    }
                    Err(fault) => return Err(fault),
                }

                let now = backend.now_tick();
                if now > step_deadline || now > program_deadline {
                    return Err(BackendFault::new(FaultCode::ProbeTimeout, false, now));
                }
            }

            let now = backend.now_tick();
            if now > step_deadline || now > program_deadline {
                return Err(BackendFault::new(FaultCode::ProbeTimeout, false, now));
            }
        }

        let mut root = transaction.observation_root;
        root = mix(root, observation_count as u64);
        for observation in observations[..observation_count].iter().copied() {
            root = mix(root, observation.root);
            root = mix(root, u64::from(observation.evidence_bit));
            root = mix(root, observation.tick);
        }
        transaction.observation_root = root;
        Ok(())
    }

    fn rollback_record(
        &self,
        candidate: RankedCandidate,
        decision: &OracleDecision,
        started_tick: u64,
        ended_tick: u64,
        stage: DispatchStage,
        evidence_mask: u32,
        fault: BackendFault,
        rollback: Result<(), BackendFault>,
    ) -> AttemptRecord {
        let (outcome, final_fault, detail) = match rollback {
            Ok(()) => (AttemptOutcome::RolledBack, fault.code, fault.detail),
            Err(rollback_fault) => (
                AttemptOutcome::CommitRejected,
                FaultCode::RollbackFault,
                rollback_fault.detail,
            ),
        };

        seal_attempt(
            self.secret,
            failure_record(
                candidate,
                decision,
                started_tick,
                ended_tick,
                stage,
                outcome,
                final_fault,
                detail,
                evidence_mask,
            ),
        )
    }
}

fn failure_record(
    candidate: RankedCandidate,
    decision: &OracleDecision,
    started_tick: u64,
    ended_tick: u64,
    stage: DispatchStage,
    outcome: AttemptOutcome,
    fault: FaultCode,
    detail: u64,
    evidence_mask: u32,
) -> AttemptRecord {
    AttemptRecord {
        strategy: candidate.strategy,
        stage,
        outcome,
        score: candidate.score,
        confidence_q16: decision.confidence_q16,
        started_tick,
        ended_tick,
        evidence_mask,
        fault,
        detail,
        root: 0,
    }
}

fn seal_attempt(secret: u64, mut attempt: AttemptRecord) -> AttemptRecord {
    attempt.root = 0;
    attempt.root = attempt_root(secret, &attempt);
    attempt
}

fn attempt_root(secret: u64, attempt: &AttemptRecord) -> u64 {
    let mut state = mix(secret, attempt.strategy.index() as u64);
    state = mix(state, attempt.stage as u8 as u64);
    state = mix(state, attempt.outcome as u8 as u64);
    state = mix(state, attempt.score as u64);
    state = mix(state, u64::from(attempt.confidence_q16));
    state = mix(state, attempt.started_tick);
    state = mix(state, attempt.ended_tick);
    state = mix(state, u64::from(attempt.evidence_mask));
    state = mix(state, attempt.fault as u16 as u64);
    mix(state, attempt.detail)
}

fn resolution_root(secret: u64, resolution: &DispatchResolution) -> u64 {
    let mut state = mix(secret, resolution.fingerprint_root);
    state = mix(state, resolution.decision_root);
    state = mix(state, resolution.active_strategy.index() as u64);
    state = mix(state, resolution.lease.handle);
    state = mix(state, u64::from(resolution.lease.generation));
    state = mix(state, resolution.lease.activation_root);
    state = mix(state, resolution.lease.health_root);
    state = mix(state, resolution.lease.framebuffer_object);
    state = mix(state, resolution.lease.committed_tick);
    state = mix(state, resolution.attempt_count as u64);
    state = mix(state, resolution.retained_firmware_display as u64);
    for attempt in resolution.attempts() {
        state = mix(state, attempt.root);
    }
    state
}

fn mix(mut state: u64, word: u64) -> u64 {
    state ^= word.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    state ^= state >> 30;
    state = state.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    state ^= state >> 27;
    state = state.wrapping_mul(0x94d0_49bb_1331_11eb);
    state ^ (state >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::drivernet::fingerprint::{
        FirmwareFramebufferEvidence, FirmwareFramebufferKind, GpuFingerprint,
        TOPOLOGY_FIRMWARE_FRAMEBUFFER,
    };
    use crate::drivers::drivernet::model::{CompatibilityOracle, OraclePolicy};

    struct Backend {
        tick: u64,
    }

    impl Backend {
        fn advance(&mut self) -> u64 {
            self.tick = self.tick.saturating_add(1);
            self.tick
        }
    }

    impl DriverNetBackend for Backend {
        fn now_tick(&self) -> u64 {
            self.tick
        }

        fn begin(
            &mut self,
            fingerprint: &GpuFingerprint,
            descriptor: &ShimDescriptor,
            _decision: &OracleDecision,
        ) -> Result<ProbeTransaction, BackendFault> {
            let tick = self.advance();
            Ok(ProbeTransaction {
                token: tick,
                fingerprint_root: fingerprint.evidence_root,
                strategy: descriptor.strategy,
                started_tick: tick,
                deadline_tick: tick.saturating_add(10_000),
                evidence_mask: 0,
                observation_root: tick,
                firmware_preserved: true,
                backend_state: [0; 8],
            })
        }

        fn probe_step(
            &mut self,
            _transaction: &mut ProbeTransaction,
            _fingerprint: &GpuFingerprint,
            _descriptor: &ShimDescriptor,
            step: ProbeStep,
            _attempt: u8,
        ) -> Result<ProbeObservation, BackendFault> {
            let tick = self.advance();
            Ok(ProbeObservation {
                semantic: step.semantic,
                evidence_bit: step.evidence_bit,
                value: 1,
                tick,
                root: tick,
            })
        }

        fn activate(
            &mut self,
            _transaction: &mut ProbeTransaction,
            _fingerprint: &GpuFingerprint,
            descriptor: &ShimDescriptor,
        ) -> Result<ActivationReceipt, BackendFault> {
            let tick = self.advance();
            Ok(ActivationReceipt {
                token: tick,
                strategy: descriptor.strategy,
                activation_epoch: tick,
                transport_root: tick,
                framebuffer_object: if descriptor.strategy == DriverStrategy::FirmwareFramebuffer {
                    99
                } else {
                    0
                },
                backend_state: [0; 8],
            })
        }

        fn health_check(
            &mut self,
            _transaction: &mut ProbeTransaction,
            _activation: &ActivationReceipt,
            _descriptor: &ShimDescriptor,
        ) -> Result<HealthReceipt, BackendFault> {
            let tick = self.advance();
            Ok(HealthReceipt {
                healthy: true,
                tick,
                heartbeat: tick,
                fault_mask: 0,
                root: tick,
            })
        }

        fn commit(
            &mut self,
            transaction: &mut ProbeTransaction,
            activation: &ActivationReceipt,
            health: &HealthReceipt,
            decision: &OracleDecision,
        ) -> Result<DriverLease, BackendFault> {
            let tick = self.advance();
            Ok(DriverLease {
                handle: tick,
                generation: 1,
                strategy: transaction.strategy,
                fingerprint_root: transaction.fingerprint_root,
                decision_root: decision.decision_root,
                activation_root: activation.transport_root,
                health_root: health.root,
                framebuffer_object: activation.framebuffer_object,
                committed_tick: tick,
            })
        }

        fn rollback(
            &mut self,
            _transaction: &mut ProbeTransaction,
            _stage: DispatchStage,
            _fault: BackendFault,
        ) -> Result<(), BackendFault> {
            self.advance();
            Ok(())
        }
    }

    fn firmware_fingerprint() -> GpuFingerprint {
        let mut fingerprint = GpuFingerprint::EMPTY;
        fingerprint.firmware_framebuffer = FirmwareFramebufferEvidence {
            kind: FirmwareFramebufferKind::UefiGop,
            physical_address: 0xe000_0000,
            width: 1920,
            height: 1080,
            pitch: 7680,
            format: 1,
            byte_length: 8_294_400,
            owner: None,
            retained: true,
        };
        fingerprint.topology_flags = TOPOLOGY_FIRMWARE_FRAMEBUFFER;
        fingerprint.evidence_root = 7;
        fingerprint
    }

    #[test]
    fn distinct_oracle_and_dispatch_secrets_resolve_safely() {
        let oracle_secret = 11;
        let dispatch_secret = 22;
        let fingerprint = firmware_fingerprint();
        let oracle = CompatibilityOracle::new(oracle_secret, OraclePolicy::BLACK_LAB).unwrap();
        let decision = oracle.classify(&fingerprint).unwrap();
        let registry = ShimRegistry::black_lab().unwrap();
        let mut dispatcher = DriverDispatcher::new(dispatch_secret, oracle_secret).unwrap();
        let mut backend = Backend { tick: 1 };

        let resolution = dispatcher
            .resolve(&fingerprint, &decision, &registry, &mut backend)
            .unwrap();

        assert_eq!(
            resolution.active_strategy,
            DriverStrategy::FirmwareFramebuffer
        );
        assert_eq!(resolution.lease.framebuffer_object, 99);
        assert!(resolution.verify(dispatch_secret));
    }

    #[test]
    fn wrong_oracle_secret_is_rejected_before_hardware_work() {
        let fingerprint = firmware_fingerprint();
        let oracle = CompatibilityOracle::new(11, OraclePolicy::BLACK_LAB).unwrap();
        let decision = oracle.classify(&fingerprint).unwrap();
        let registry = ShimRegistry::black_lab().unwrap();
        let mut dispatcher = DriverDispatcher::new(22, 33).unwrap();
        let mut backend = Backend { tick: 1 };

        assert_eq!(
            dispatcher.resolve(&fingerprint, &decision, &registry, &mut backend,),
            Err(DispatchError::CorruptDecision)
        );
        assert_eq!(backend.tick, 1);
    }
}
