use core::cell::Cell;

use aether::effect_program::PreparedEffects;
use aether::holographic::HolographicTree;
use aether::replay_capsule::ReplayCapsule;
use aether::temporal_contract::TemporalObservation;

use crate::continuity_vault::CheckpointId;
use crate::nexus_commit::apply_prepared;
use crate::nexus_matrix::{MATRIX_HOLOGRAM_LEAVES, MATRIX_HOLOGRAM_NODES, NexusMatrix};
use crate::sync::SpinLock;
use crate::thermogenesis::{MAX_THERMAL_CHARGE, ThermalChargeError, ThermalLedger};

pub const ECHO_MISMATCH_BEFORE_ROOT: u32 = 1 << 0;
pub const ECHO_MISMATCH_BEFORE_GENERATION: u32 = 1 << 1;
pub const ECHO_MISMATCH_AFTER_ROOT: u32 = 1 << 2;
pub const ECHO_MISMATCH_AFTER_GENERATION: u32 = 1 << 3;
pub const ECHO_MISMATCH_AFTER_HEAT: u32 = 1 << 4;
pub const ECHO_MISMATCH_AFTER_PAIRS: u32 = 1 << 5;
pub const ECHO_MISMATCH_AFTER_COLLAPSES: u32 = 1 << 6;
pub const ECHO_MISMATCH_AFTER_PHASE: u32 = 1 << 7;
pub const ECHO_MISMATCH_CONTRACT: u32 = 1 << 8;
pub const ECHO_MISMATCH_EXECUTION: u32 = 1 << 9;
pub const ECHO_MISMATCH_CAPSULE: u32 = 1 << 10;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum EchoVerdict {
    Reproduced = 1,
    Diverged = 2,
    InvalidCapsule = 3,
    ExecutionFault = 4,
    Stale = 5,
}

#[derive(Clone, Copy)]
pub struct PendingEcho<const N: usize> {
    pub due_tick: u64,
    pub checkpoint: CheckpointId,
    pub capsule: ReplayCapsule<N>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct EchoReport {
    pub sequence: u64,
    pub verdict: EchoVerdict,
    pub reserved: [u8; 7],

    pub mismatch_mask: u32,
    pub generation_expected: u32,
    pub generation_observed: u32,
    pub reserved_two: u32,

    pub root_expected: u64,
    pub root_observed: u64,

    pub heat_expected: u64,
    pub heat_observed: u64,

    pub digest: u64,
}

impl EchoReport {
    fn new(
        sequence: u64,
        verdict: EchoVerdict,
        mismatch_mask: u32,
        generation_expected: u32,
        generation_observed: u32,
        root_expected: u64,
        root_observed: u64,
        heat_expected: u64,
        heat_observed: u64,
    ) -> Self {
        let mut report = Self {
            sequence,
            verdict,
            reserved: [0; 7],
            mismatch_mask,
            generation_expected,
            generation_observed,
            reserved_two: 0,
            root_expected,
            root_observed,
            heat_expected,
            heat_observed,
            digest: 0,
        };

        report.digest = report.compute_digest();
        report
    }

    pub const fn reproduced(self) -> bool {
        matches!(self.verdict, EchoVerdict::Reproduced)
    }

    fn compute_digest(&self) -> u64 {
        let mut digest = mix(0x4543_484f_5f52_5054, self.sequence);

        digest = mix(
            digest,
            self.verdict as u64 | (u64::from(self.mismatch_mask) << 8),
        );

        digest = mix(
            digest,
            u64::from(self.generation_expected) | (u64::from(self.generation_observed) << 32),
        );

        digest = mix(digest, self.root_expected);
        digest = mix(digest, self.root_observed);
        digest = mix(digest, self.heat_expected);
        mix(digest, self.heat_observed)
    }
}

#[derive(Clone, Copy)]
struct EchoRecord {
    active: bool,
    report: EchoReport,
}

impl EchoRecord {
    const EMPTY: Self = Self {
        active: false,
        report: EchoReport {
            sequence: 0,
            verdict: EchoVerdict::Stale,
            reserved: [0; 7],
            mismatch_mask: 0,
            generation_expected: 0,
            generation_observed: 0,
            reserved_two: 0,
            root_expected: 0,
            root_observed: 0,
            heat_expected: 0,
            heat_observed: 0,
            digest: 0,
        },
    };
}

struct EchoState<const N: usize, const PENDING: usize, const RECORDS: usize> {
    pending: [Option<PendingEcho<N>>; PENDING],
    pending_head: usize,
    pending_length: usize,

    records: [EchoRecord; RECORDS],
    record_cursor: usize,
    record_length: usize,

    chain_root: u64,
}

impl<const N: usize, const PENDING: usize, const RECORDS: usize> EchoState<N, PENDING, RECORDS> {
    const fn new(seed: u64) -> Self {
        Self {
            pending: [None; PENDING],
            pending_head: 0,
            pending_length: 0,
            records: [EchoRecord::EMPTY; RECORDS],
            record_cursor: 0,
            record_length: 0,
            chain_root: seed,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EchoQueueError {
    ZeroCapacity,
    Full,
}

pub struct TemporalEchoEngine<const N: usize, const PENDING: usize, const RECORDS: usize> {
    state: SpinLock<EchoState<N, PENDING, RECORDS>>,
}

impl<const N: usize, const PENDING: usize, const RECORDS: usize>
    TemporalEchoEngine<N, PENDING, RECORDS>
{
    pub const fn new(seed: u64) -> Self {
        Self {
            state: SpinLock::new(EchoState::new(seed)),
        }
    }

    pub fn schedule(&self, echo: PendingEcho<N>) -> Result<(), EchoQueueError> {
        if PENDING == 0 {
            return Err(EchoQueueError::ZeroCapacity);
        }

        let mut state = self.state.lock();

        if state.pending_length == PENDING {
            return Err(EchoQueueError::Full);
        }

        let tail = (state.pending_head + state.pending_length) % PENDING;

        state.pending[tail] = Some(echo);
        state.pending_length += 1;

        Ok(())
    }

    pub fn take_due(&self, now_tick: u64) -> Option<PendingEcho<N>> {
        if PENDING == 0 {
            return None;
        }

        let mut state = self.state.lock();

        if state.pending_length == 0 {
            return None;
        }

        let head = state.pending_head;
        let echo = state.pending[head]?;

        if echo.due_tick > now_tick {
            return None;
        }

        state.pending[head] = None;
        state.pending_head = (state.pending_head + 1) % PENDING;
        state.pending_length -= 1;

        Some(echo)
    }

    pub fn record(&self, report: EchoReport) -> u64 {
        if RECORDS == 0 {
            return self.state.lock().chain_root;
        }

        let mut state = self.state.lock();

        let chain_before = state.chain_root;

        let chain_after = mix(mix(chain_before, report.sequence), report.digest);

        let cursor = state.record_cursor;

        state.records[cursor] = EchoRecord {
            active: true,
            report,
        };

        state.record_cursor = (state.record_cursor + 1) % RECORDS;

        state.record_length = (state.record_length + 1).min(RECORDS);

        state.chain_root = chain_after;
        chain_after
    }

    pub fn latest(&self) -> Option<EchoReport> {
        if RECORDS == 0 {
            return None;
        }

        let state = self.state.lock();

        if state.record_length == 0 {
            return None;
        }

        let index = (state.record_cursor + RECORDS - 1) % RECORDS;

        state.records[index]
            .active
            .then_some(state.records[index].report)
    }

    pub fn chain_root(&self) -> u64 {
        self.state.lock().chain_root
    }

    pub fn pending_count(&self) -> usize {
        self.state.lock().pending_length
    }
}

pub struct EchoThermal {
    ambient_heat: u64,
    charge: Cell<u64>,
    rebates: Cell<u64>,
}

impl EchoThermal {
    pub fn from_snapshot(total_heat: u64, charge: u64) -> Self {
        Self {
            ambient_heat: total_heat.saturating_sub(charge),
            charge: Cell::new(charge),
            rebates: Cell::new(0),
        }
    }
}

impl ThermalLedger for EchoThermal {
    fn current_heat(&self) -> u64 {
        self.ambient_heat.saturating_add(self.charge.get())
    }

    fn current_charge(&self) -> u64 {
        self.charge.get()
    }

    fn charge(&self, amount: u64) -> Result<u64, ThermalChargeError> {
        let current = self.charge.get();

        let next = current
            .checked_add(amount)
            .filter(|next| *next <= MAX_THERMAL_CHARGE)
            .ok_or(ThermalChargeError::BudgetExceeded {
                current,
                requested: amount,
                maximum: MAX_THERMAL_CHARGE,
            })?;

        self.charge.set(next);
        Ok(next)
    }

    fn credit_collapse_rebate(&self, amount: u64) -> u64 {
        self.rebates.set(self.rebates.get().saturating_add(amount));

        let next = self.charge.get().saturating_sub(amount);

        self.charge.set(next);
        next
    }
}

#[allow(clippy::too_many_arguments)]
pub fn verify_replay<
    const TASKS: usize,
    const PAIRS: usize,
    const CAGES: usize,
    const MOMENTS: usize,
    const BINS: usize,
    const N: usize,
>(
    base_matrix: &NexusMatrix<TASKS, PAIRS, CAGES, MOMENTS, BINS>,
    base_total_heat: u64,
    base_charge: u64,
    capsule: &ReplayCapsule<N>,
    scratch: &mut HolographicTree<MATRIX_HOLOGRAM_LEAVES, MATRIX_HOLOGRAM_NODES>,
) -> EchoReport {
    let certificate = capsule.certificate();

    if capsule.validate().is_err() {
        return EchoReport::new(
            certificate.sequence,
            EchoVerdict::InvalidCapsule,
            ECHO_MISMATCH_CAPSULE,
            certificate.generation_after,
            0,
            certificate.after_root,
            0,
            certificate.heat_after,
            0,
        );
    }

    let mut matrix = base_matrix.clone();
    let thermal = EchoThermal::from_snapshot(base_total_heat, base_charge);

    let before_root = matrix.refresh_hologram(scratch).unwrap_or(0);

    let before_stats = matrix.stats();

    let before = TemporalObservation {
        generation: before_stats.generation,
        pairs_live: before_stats.pairs_live,
        state_root: before_root,
        collapses: before_stats.collapses,
        heat: thermal.current_heat(),
        phase_bin: before_stats.global_phase,
        reserved: 0,
    };

    let mut mismatch = 0_u32;

    if before_root != certificate.before_root {
        mismatch |= ECHO_MISMATCH_BEFORE_ROOT;
    }

    if before_stats.generation != certificate.generation_before {
        mismatch |= ECHO_MISMATCH_BEFORE_GENERATION;
    }

    if capsule
        .contract()
        .verify_before(before, certificate.wall_tick)
        .is_err()
    {
        mismatch |= ECHO_MISMATCH_CONTRACT;
    }

    let prepared: PreparedEffects<N> = capsule.prepared();

    if apply_prepared(&mut matrix, &thermal, &prepared, certificate.wall_tick).is_err() {
        mismatch |= ECHO_MISMATCH_EXECUTION;

        return EchoReport::new(
            certificate.sequence,
            EchoVerdict::ExecutionFault,
            mismatch,
            certificate.generation_after,
            before_stats.generation,
            certificate.after_root,
            before_root,
            certificate.heat_after,
            thermal.current_heat(),
        );
    }

    let after_root = matrix.refresh_hologram(scratch).unwrap_or(0);

    let after_stats = matrix.stats();

    let after = TemporalObservation {
        generation: after_stats.generation,
        pairs_live: after_stats.pairs_live,
        state_root: after_root,
        collapses: after_stats.collapses,
        heat: thermal.current_heat(),
        phase_bin: after_stats.global_phase,
        reserved: 0,
    };

    if after_root != certificate.after_root {
        mismatch |= ECHO_MISMATCH_AFTER_ROOT;
    }

    if after_stats.generation != certificate.generation_after {
        mismatch |= ECHO_MISMATCH_AFTER_GENERATION;
    }

    if after.heat != certificate.heat_after {
        mismatch |= ECHO_MISMATCH_AFTER_HEAT;
    }

    let certified_pairs_available = false;

    if certified_pairs_available && after.pairs_live != certificate_pair_count(certificate) {
        mismatch |= ECHO_MISMATCH_AFTER_PAIRS;
    }

    if after.phase_bin != certificate.phase_after {
        mismatch |= ECHO_MISMATCH_AFTER_PHASE;
    }

    if capsule.contract().verify_after(before, after).is_err() {
        mismatch |= ECHO_MISMATCH_CONTRACT;
    }

    let verdict = if mismatch == 0 {
        EchoVerdict::Reproduced
    } else {
        EchoVerdict::Diverged
    };

    EchoReport::new(
        certificate.sequence,
        verdict,
        mismatch,
        certificate.generation_after,
        after_stats.generation,
        certificate.after_root,
        after_root,
        certificate.heat_after,
        after.heat,
    )
}

fn certificate_pair_count(
    _certificate: aether::transition_certificate::TransitionCertificate,
) -> u32 {
    // TransitionCertificate currently does not carry pair count.
    // Return a sentinel that disables this comparison until the field is
    // added to certificate version 2.
    u32::MAX
}

fn mix(mut state: u64, value: u64) -> u64 {
    state ^= value.wrapping_add(0x517c_c1b7_2722_0a95);
    state = state.rotate_left(31);
    state = state.wrapping_mul(0x9e37_79b1_85eb_ca87);
    state ^ (state >> 28)
}
