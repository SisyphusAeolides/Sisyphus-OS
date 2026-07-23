use core::sync::atomic::{AtomicBool, Ordering};

use crate::nexus_commit::{CommitError, NexusCommitEngine, apply_prepared};
use aether::effect_program::{EffectIntent, EffectKind, EffectProgram};
use aether::holographic::HolographicTree;
use aether::nexus_wire::{NexusCommand, NexusOpcode, NexusReply, NexusStatus, NexusTelemetry};
use aether::resonance_policy::{POLICY_REPHASE, ResonancePolicy};
use aether::temporal_contract::{
    CONTRACT_REQUIRE_GENERATION_CHANGE, CONTRACT_REQUIRE_ROOT_CHANGE, TemporalContract, effect_bit,
};

use crate::capability::{Capability, ResonanceRight};
use crate::commit_reactor::{CausalCommitReactor, ReactorError, ReactorPoll};
use crate::continuity_vault::{CheckpointId, ContinuityVault};
use crate::counterfactual::CounterfactualUniverse;
use crate::lease_lattice::LeaseError;
use crate::nexus_gateway::{GatewayError, NexusGateway};
use crate::nexus_matrix::{
    MATRIX_HOLOGRAM_LEAVES, MATRIX_HOLOGRAM_NODES, MatrixError, NexusMatrix,
};
use crate::singularity::{ContainmentOrder, StabilitySample};
use crate::sync::SpinLock;
use crate::thermogenesis::Thermogenesis;
use aether::temporal_contract::TemporalObservation;

pub const NEXUS_TASKS: usize = 64;
pub const NEXUS_PAIRS: usize = 256;
pub const NEXUS_CAGES: usize = 256;
pub const NEXUS_MOMENTS: usize = 64;
pub const NEXUS_BINS: usize = 64;
const CEREBRAL_LEASE_QUOTA: u32 = 1_000_000;
const CEREBRAL_LEASE_LIFETIME: u64 = 1_u64 << 40;

type KernelMatrix = NexusMatrix<NEXUS_TASKS, NEXUS_PAIRS, NEXUS_CAGES, NEXUS_MOMENTS, NEXUS_BINS>;

type KernelGateway = NexusGateway<64, 256, 512, 64>;

#[derive(Clone)]
struct RuntimeImage {
    matrix: KernelMatrix,
    thermal_charge: u64,
}

static READY: AtomicBool = AtomicBool::new(false);

static MATRIX: SpinLock<KernelMatrix> = SpinLock::new(KernelMatrix::new(0));

static THERMAL: SpinLock<Option<Thermogenesis>> = SpinLock::new(None);

static CONTINUITY_VAULT: ContinuityVault<RuntimeImage, 4> = ContinuityVault::new();
static HOLOGRAM: SpinLock<HolographicTree<MATRIX_HOLOGRAM_LEAVES, MATRIX_HOLOGRAM_NODES>> =
    SpinLock::new(HolographicTree::new());
static LAST_CHECKPOINT: SpinLock<Option<CheckpointId>> = SpinLock::new(None);

static GATEWAY: KernelGateway = KernelGateway::new(0, 0x51_4e_45_58_55_53_21);

static POLICY_REACTOR: CausalCommitReactor<1, 16, 4> =
    CausalCommitReactor::new(0x4341_5553_414c_5f31);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitializeError {
    AlreadyInitialized,
    LeaseInitialization,
    Gateway(GatewayError),
}

pub fn initialize(
    authority: &Capability<'_, ResonanceRight>,
) -> Result<crate::lease_lattice::LeaseToken, InitializeError> {
    if READY.swap(true, Ordering::AcqRel) {
        return Err(InitializeError::AlreadyInitialized);
    }

    let boot_entropy_word = <crate::arch::Active as crate::arch::Architecture>::counter_sample();
    let now_tick = boot_entropy_word;
    let lease_secret = boot_entropy_word;

    let leases = crate::nexus_gateway::LEASES
        .initialize(crate::lease_lattice::LeaseLattice::new(lease_secret))
        .map_err(|_| InitializeError::LeaseInitialization)?;

    let root = leases
        .issue_root(
            crate::lease_lattice::LeaseRights::ALL,
            now_tick,
            u64::MAX,
            u32::MAX,
            authority,
        )
        .map_err(|error| InitializeError::Gateway(GatewayError::Lease(error)))?;

    let cerebral_rights = crate::lease_lattice::LeaseRights::OBSERVE
        .union(crate::lease_lattice::LeaseRights::SCHEDULE)
        .union(crate::lease_lattice::LeaseRights::RESONANCE)
        .union(crate::lease_lattice::LeaseRights::CONTROL);

    let cerebral_token = leases
        .attenuate(
            root,
            cerebral_rights,
            now_tick.saturating_add(CEREBRAL_LEASE_LIFETIME),
            CEREBRAL_LEASE_QUOTA,
            now_tick,
        )
        .map_err(|error| InitializeError::Gateway(GatewayError::Lease(error)))?;

    *THERMAL.lock() = Some(Thermogenesis::new(4));
    Ok(cerebral_token)
}

pub fn control(command: &NexusCommand, wall_tick: u64) -> NexusReply {
    if !READY.load(Ordering::Acquire) {
        return NexusReply::new(
            NexusStatus::NotReady,
            command.sequence,
            wall_tick,
            0,
            command.opcode,
            [0; 3],
        );
    }

    let admission = match GATEWAY.admit(command, wall_tick) {
        Ok(admission) => admission,
        Err(error) => {
            return NexusReply::new(
                gateway_status(error),
                command.sequence,
                wall_tick,
                MATRIX.lock().stats().generation,
                command.opcode as u16,
                [0; 3],
            );
        }
    };

    let mut matrix = MATRIX.lock();
    let mut thermal_guard = THERMAL.lock();

    let Some(thermal) = thermal_guard.as_mut() else {
        return GATEWAY.finish(
            admission,
            NexusStatus::NotReady,
            matrix.stats().generation,
            [0; 3],
        );
    };

    let checkpoint = opcode_mutates(admission.opcode)
        .then(|| checkpoint_runtime(&matrix, thermal, wall_tick))
        .flatten();

    let result = matrix.execute(admission.opcode, command.arguments, wall_tick, thermal);

    if result.is_err() {
        if let Some(checkpoint) = checkpoint {
            let _ = restore_checkpoint(checkpoint, &mut matrix, thermal);
        }
    }

    match result {
        Ok(values) => GATEWAY.finish(
            admission,
            NexusStatus::Ok,
            matrix.stats().generation,
            values,
        ),

        Err(error) => GATEWAY.finish(
            admission,
            matrix_status(error),
            matrix.stats().generation,
            [0; 3],
        ),
    }
}
pub fn telemetry(sequence: u64, _wall_tick: u64) -> NexusTelemetry {
    let mut telemetry = MATRIX.lock().snapshot_telemetry();
    telemetry.sequence = sequence;
    telemetry
}

pub fn policy_effects(
    policy: ResonancePolicy,
    generation: u32,
    state_root: u64,
) -> EffectProgram<4> {
    let mut program = EffectProgram::new(generation, state_root, policy.heat_ceiling);

    let _ = program.push(EffectIntent::new(
        EffectKind::SetCollapseThreshold,
        0,
        [policy.collapse_threshold, 0, 0, 0],
    ));

    let _ = program.push(EffectIntent::new(
        EffectKind::SetPriorityMass,
        1 << 0,
        [u64::from(policy.priority_mass), 0, 0, 0],
    ));

    if policy.flags & POLICY_REPHASE != 0 {
        let _ = program.push(EffectIntent::new(
            EffectKind::Rephase,
            (1 << 0) | (1 << 1),
            [u64::from(policy.target_phase), 0, 0, 0],
        ));
    }

    program.seal()
}

pub fn policy_contract(
    policy: ResonancePolicy,
    generation: u32,
    state_root: u64,
    wall_tick: u64,
) -> TemporalContract {
    let mut allowed = effect_bit(EffectKind::SetCollapseThreshold as u8)
        | effect_bit(EffectKind::SetPriorityMass as u8);

    if policy.flags & POLICY_REPHASE != 0 {
        allowed |= effect_bit(EffectKind::Rephase as u8);
    }

    TemporalContract {
        expected_generation: generation,

        flags: CONTRACT_REQUIRE_ROOT_CHANGE | CONTRACT_REQUIRE_GENERATION_CHANGE,

        expected_state_root: state_root,
        deadline_tick: wall_tick.saturating_add(4096),

        maximum_heat: policy.heat_ceiling,
        maximum_generation_delta: 4,
        maximum_pair_growth: 0,

        maximum_collapse_growth: 0,
        maximum_phase_distance: 512,
        reserved: 0,

        allowed_effects: allowed,
    }
}

pub fn propose_policy(policy: ResonancePolicy, wall_tick: u64) -> Result<(), ReactorError> {
    let matrix = MATRIX.lock();
    let thermal_guard = THERMAL.lock();

    let Some(thermal) = thermal_guard.as_ref() else {
        return Ok(());
    };

    let mut hologram = HOLOGRAM.lock();

    let root = matrix.refresh_hologram(&mut hologram).unwrap_or(0);

    let stats = matrix.stats();

    let observation = TemporalObservation {
        generation: stats.generation,
        pairs_live: stats.pairs_live,
        state_root: root,
        collapses: stats.collapses,
        heat: thermal.current_heat(),
        phase_bin: stats.global_phase,
        reserved: 0,
    };

    let program = policy_effects(policy, stats.generation, root);

    let prepared = program
        .prepare(stats.generation, root, thermal.current_heat())
        .map_err(|error| ReactorError::Commit(CommitError::Effects(error)))?;

    let contract = policy_contract(policy, stats.generation, root, wall_tick);

    POLICY_REACTOR.propose(prepared, contract, observation, 1, wall_tick)?;

    POLICY_REACTOR.acknowledge(0, observation, wall_tick)?;

    Ok(())
}

pub fn service_policy_commit(wall_tick: u64) -> Result<bool, ReactorError> {
    match POLICY_REACTOR.poll(wall_tick)? {
        ReactorPoll::Idle | ReactorPoll::Waiting { .. } => Ok(false),

        ReactorPoll::Expired(pending) => {
            let contract = pending.contract;

            let _ = POLICY_REACTOR.finalize_abort(
                pending,
                contract.expected_state_root,
                contract.expected_generation,
                wall_tick,
                false,
            );

            Ok(false)
        }

        ReactorPoll::Ready(pending) => {
            let mut matrix = MATRIX.lock();
            let thermal_guard = THERMAL.lock();

            let Some(thermal) = thermal_guard.as_ref() else {
                let _ = POLICY_REACTOR.finalize_abort(
                    pending,
                    pending.contract.expected_state_root,
                    pending.contract.expected_generation,
                    wall_tick,
                    false,
                );

                return Ok(false);
            };

            let mut hologram = HOLOGRAM.lock();

            let live_root = matrix.refresh_hologram(&mut hologram).unwrap_or(0);

            let universe = CounterfactualUniverse::simulate(
                &matrix,
                thermal,
                &pending.prepared,
                pending.contract,
                wall_tick,
                live_root,
            )
            .map_err(|_| ReactorError::Commit(CommitError::Thermal))?;

            let before = universe.before();
            let predicted_after = universe.after();

            let receipt = universe
                .commit(&mut matrix, thermal, live_root)
                .map_err(|_| ReactorError::Commit(CommitError::Thermal))?;

            let committed_root = matrix
                .refresh_hologram(&mut hologram)
                .unwrap_or(receipt.after.state_root);

            debug_assert_eq!(committed_root, predicted_after.state_root,);

            let commit = POLICY_REACTOR.finalize_success(
                pending,
                before.state_root,
                committed_root,
                before.generation,
                receipt.after.generation,
                wall_tick,
            )?;

            crate::nexus_plane::observation().publish_witness_root(commit.witness_root);

            Ok(true)
        }
    }
}

pub fn heartbeat_batch(wall_tick: u64, batch_size: u64) {
    if !READY.load(Ordering::Acquire) {
        return;
    }

    let mut matrix = MATRIX.lock();
    let mut thermal_guard = THERMAL.lock();

    let Some(thermal) = thermal_guard.as_mut() else {
        return;
    };

    let mut pulse = None;
    for _ in 0..batch_size.max(1) {
        pulse = Some(matrix.heartbeat(wall_tick, thermal));
    }
    let pulse = pulse.unwrap();
    let stats = matrix.stats();

    let sample = StabilitySample {
        queue_pressure_q16: u32::from(pulse.pairs_live.min(64)) << 10,
        heat_q16: stats.heat.min(u32::MAX as u64) as u32,
        fault_rate_q16: pulse.collapses.min(u32::MAX as u64) as u32,
        phase_drift_q16: u32::from(pulse.eigenphase.abs_diff(stats.global_phase)) << 6,
        replay_pressure_q16: 0,
        phase_bin: pulse.eigenphase,
        checkpoint: stats.generation,
    };

    let decision = GATEWAY.observe_stability(sample);

    match decision.order {
        ContainmentOrder::Throttle {
            priority_mass_ceiling,
        } => {
            let _ = matrix.execute(
                aether::nexus_wire::NexusOpcode::SetPriorityMass,
                [u64::from(priority_mass_ceiling), 0, 0, 0],
                wall_tick,
                thermal,
            );
        }

        ContainmentOrder::Rephase { target_phase_bin } => {
            let _ = target_phase_bin;
        }

        ContainmentOrder::Rollback { .. } => {
            let _ = rollback_latest(&mut matrix, thermal);
        }

        ContainmentOrder::Quarantine { .. } | ContainmentOrder::None => {}
    }
}

pub fn chronicle_is_valid() -> bool {
    GATEWAY.chronicle_is_valid()
}

pub fn continuity_state() -> (u64, u64, u64) {
    let root = HOLOGRAM.lock().root();

    let checkpoint_generation = match *LAST_CHECKPOINT.lock() {
        Some(checkpoint) => checkpoint.generation,
        None => 0,
    };

    (
        root,
        checkpoint_generation as u64,
        POLICY_REACTOR.witness_root(),
    )
}

fn checkpoint_runtime(
    matrix: &KernelMatrix,
    thermal: &Thermogenesis,
    wall_tick: u64,
) -> Option<CheckpointId> {
    let mut hologram = HOLOGRAM.lock();
    let root = matrix.refresh_hologram(&mut hologram).ok()?;

    let image = RuntimeImage {
        matrix: matrix.clone(),
        thermal_charge: thermal.current_charge(),
    };

    let checkpoint = CONTINUITY_VAULT.checkpoint(&image, root, wall_tick).ok()?;

    *LAST_CHECKPOINT.lock() = Some(checkpoint);

    Some(checkpoint)
}

fn rollback_latest(matrix: &mut KernelMatrix, thermal: &Thermogenesis) -> bool {
    let Some(checkpoint) = *LAST_CHECKPOINT.lock() else {
        return false;
    };

    let Ok(image) = CONTINUITY_VAULT.restore(checkpoint) else {
        return false;
    };

    *matrix = image.matrix;
    thermal.restore_charge(image.thermal_charge);

    true
}

fn restore_checkpoint(
    checkpoint: CheckpointId,
    matrix: &mut KernelMatrix,
    thermal: &Thermogenesis,
) -> bool {
    let Ok(image) = CONTINUITY_VAULT.restore(checkpoint) else {
        return false;
    };

    *matrix = image.matrix;
    thermal.restore_charge(image.thermal_charge);

    true
}

fn opcode_mutates(opcode: NexusOpcode) -> bool {
    !matches!(
        opcode,
        NexusOpcode::QueryStats | NexusOpcode::QueryTelemetry
    )
}

fn gateway_status(error: GatewayError) -> NexusStatus {
    match error {
        GatewayError::Wire(_) | GatewayError::Replay(_) => NexusStatus::BadFrame,

        GatewayError::NotReady => NexusStatus::NotReady,

        GatewayError::Expired
        | GatewayError::Lease(LeaseError::Expired | LeaseError::NotYetValid) => {
            NexusStatus::Expired
        }

        GatewayError::Capacity | GatewayError::Lease(LeaseError::Capacity) => NexusStatus::Capacity,

        GatewayError::Denied | GatewayError::Lease(_) => NexusStatus::Denied,
    }
}

fn matrix_status(error: MatrixError) -> NexusStatus {
    match error {
        MatrixError::Capacity => NexusStatus::Capacity,
        MatrixError::ThermalThrottle => NexusStatus::ThermalThrottle,

        MatrixError::InvalidTask | MatrixError::InvalidPair | MatrixError::InvalidArgument => {
            NexusStatus::InvalidArgument
        }

        MatrixError::Scheduler => NexusStatus::InternalFault,
    }
}
