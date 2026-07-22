use core::sync::atomic::{
    AtomicU64, Ordering,
};

use aether::blacklab_vm::LabMetrics;
use aether::event_horizon::EVENT_HORIZON_PROGRAM;
use aether::lockfree::QueueError;
use aether::resonance_plane::{
    PlaneInitError, ResonancePlane,
};
use aether::resonance_policy::ResonancePolicy;

use crate::capability::{
    Capability, LearningRight,
};
use crate::lab_capsule::{
    CapsuleError, LabCapsule,
};
use crate::nexus_runtime;

pub const COMMAND_BUDGET_PER_HEARTBEAT: usize = 16;

static RESONANCE_PLANE: ResonancePlane =
    ResonancePlane::new();

static LAB_CAPSULE: LabCapsule =
    LabCapsule::new();

static LAST_COLLAPSES: AtomicU64 =
    AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaneDriverInitError {
    Plane(PlaneInitError),
    Capsule(CapsuleError),
}

pub fn initialize(
    authority: &Capability<'_, LearningRight>,
) -> Result<(), PlaneDriverInitError> {
    RESONANCE_PLANE
        .initialize()
        .map_err(PlaneDriverInitError::Plane)?;

    LAB_CAPSULE
        .initialize(
            EVENT_HORIZON_PROGRAM,
            ResonancePolicy::DEFAULT,
            authority,
        )
        .map_err(PlaneDriverInitError::Capsule)?;

    Ok(())
}

pub fn drive(wall_tick: u64) {
    drain_commands(wall_tick);

    nexus_runtime::heartbeat(wall_tick);

    let sequence =
        RESONANCE_PLANE.epoch().wrapping_add(1);

    let telemetry =
        nexus_runtime::telemetry(sequence, wall_tick);

    let previous_collapses =
        LAST_COLLAPSES.swap(
            telemetry.collapses,
            Ordering::AcqRel,
        );

    let collapse_delta =
        telemetry.collapses.saturating_sub(previous_collapses);

    let target_phase = LAB_CAPSULE
        .current_policy()
        .map(|policy| policy.target_phase)
        .unwrap_or(0);

    let kernel_phase =
        telemetry.global_phase as u16 & 1023;

    let direct_drift =
        kernel_phase.abs_diff(target_phase);

    let phase_drift =
        direct_drift.min(1024 - direct_drift);

    let metrics = LabMetrics {
        heat: telemetry.heat.min(i64::MAX as u64) as i64,

        queue_pressure:
            RESONANCE_PLANE
                .command_depth_approximate()
                .min(i64::MAX as usize) as i64,

        collapse_rate:
            collapse_delta.min(i64::MAX as u64) as i64,

        phase_drift: i64::from(phase_drift),

        replay_pressure:
            RESONANCE_PLANE
                .dropped_commands()
                .min(i64::MAX as u64) as i64,

        coherence:
            1024_i64.saturating_sub(i64::from(phase_drift)),

        kernel_phase: i64::from(kernel_phase),
    };

    if let Ok(Some(commit)) =
        LAB_CAPSULE.evaluate(metrics)
    {
        nexus_runtime::apply_policy(
            commit.policy,
            wall_tick,
        );
    }

    RESONANCE_PLANE.publish_telemetry(&telemetry);
}

pub fn plane() -> &'static ResonancePlane {
    &RESONANCE_PLANE
}

fn drain_commands(wall_tick: u64) {
    for _ in 0..COMMAND_BUDGET_PER_HEARTBEAT {
        let command = match RESONANCE_PLANE.take_command() {
            Ok(command) => command,
            Err(QueueError::Empty) => break,
            Err(_) => break,
        };

        let reply =
            nexus_runtime::control(&command, wall_tick);

        let _ = RESONANCE_PLANE.publish_reply(reply);
    }
}
