use core::sync::atomic::{AtomicBool, Ordering};

use aether::nexus_wire::{
    NexusCommand, NexusReply, NexusStatus, NexusTelemetry,
};
use aether::resonance_policy::ResonancePolicy;

use crate::capability::{Capability, ResonanceRight};
use crate::nexus_gateway::{
    GatewayError, NexusGateway,
};
use crate::nexus_matrix::{
    MatrixError, NexusMatrix,
};
use crate::singularity::{
    ContainmentOrder, StabilitySample,
};
use crate::sync::SpinLock;
use crate::thermogenesis::Thermogenesis;

pub const NEXUS_TASKS: usize = 64;
pub const NEXUS_PAIRS: usize = 256;
pub const NEXUS_CAGES: usize = 256;
pub const NEXUS_MOMENTS: usize = 64;
pub const NEXUS_BINS: usize = 64;

type KernelMatrix = NexusMatrix<
    NEXUS_TASKS,
    NEXUS_PAIRS,
    NEXUS_CAGES,
    NEXUS_MOMENTS,
    NEXUS_BINS,
>;

type KernelGateway = NexusGateway<64, 256, 512, 64>;

static READY: AtomicBool = AtomicBool::new(false);

static MATRIX: SpinLock<KernelMatrix> =
    SpinLock::new(KernelMatrix::new(0));

static THERMAL: SpinLock<Option<Thermogenesis>> =
    SpinLock::new(None);

static GATEWAY: KernelGateway =
    KernelGateway::new(0, 0x51_4e_45_58_55_53_21);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitializeError {
    AlreadyInitialized,
    Gateway(GatewayError),
}

pub fn initialize(
    authority: &Capability<'_, ResonanceRight>,
) -> Result<crate::lease_lattice::LeaseToken, InitializeError> {
    if READY.swap(true, Ordering::AcqRel) {
        return Err(InitializeError::AlreadyInitialized);
    }

    let seed = <crate::arch::Active as crate::arch::Architecture>::counter_sample();
    crate::nexus_gateway::LEASES.init(seed);

    let token = crate::nexus_gateway::LEASES.issue_root(
        crate::lease_lattice::LeaseRights::ALL,
        0,
        u64::MAX,
        u32::MAX,
        authority,
    ).map_err(|_| InitializeError::Gateway(GatewayError::Capacity))?;

    *THERMAL.lock() = Some(Thermogenesis::new(4));
    Ok(token)
}

pub fn control(
    command: &NexusCommand,
    wall_tick: u64,
) -> NexusReply {
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

    match matrix.execute(
        admission.opcode,
        command.arguments,
        wall_tick,
        thermal,
    ) {
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

pub fn apply_policy(
    policy: ResonancePolicy,
    wall_tick: u64,
) {
    if !READY.load(Ordering::Acquire) {
        return;
    }

    MATRIX.lock().apply_policy(policy, wall_tick);
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
        queue_pressure_q16:
            u32::from(pulse.pairs_live.min(64)) << 10,
        heat_q16:
            stats.heat.min(u32::MAX as u64) as u32,
        fault_rate_q16:
            pulse.collapses.min(u32::MAX as u64) as u32,
        phase_drift_q16:
            u32::from(pulse.eigenphase.abs_diff(stats.global_phase))
                << 6,
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

        ContainmentOrder::Rephase {
            target_phase_bin,
        } => {
            let _ = target_phase_bin;
        }

        ContainmentOrder::Quarantine { .. }
        | ContainmentOrder::Rollback { .. }
        | ContainmentOrder::None => {}
    }
}

pub fn chronicle_is_valid() -> bool {
    GATEWAY.chronicle_is_valid()
}

fn gateway_status(error: GatewayError) -> NexusStatus {
    match error {
        GatewayError::Wire(_) => NexusStatus::BadFrame,
        GatewayError::Replay(_) => NexusStatus::BadFrame,
        GatewayError::Denied => NexusStatus::Denied,
        GatewayError::Expired => NexusStatus::Expired,
        GatewayError::Capacity => NexusStatus::Capacity,
    }
}

fn matrix_status(error: MatrixError) -> NexusStatus {
    match error {
        MatrixError::Capacity => NexusStatus::Capacity,
        MatrixError::ThermalThrottle => NexusStatus::ThermalThrottle,

        MatrixError::InvalidTask
        | MatrixError::InvalidPair
        | MatrixError::InvalidArgument => NexusStatus::InvalidArgument,

        MatrixError::Scheduler => NexusStatus::InternalFault,
    }
}
