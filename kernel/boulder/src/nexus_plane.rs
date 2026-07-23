use core::sync::atomic::{AtomicU64, Ordering};

use aether::blacklab_vm::LabMetrics;
use aether::event_horizon::EVENT_HORIZON_PROGRAM;
use aether::policy_crystal::PolicyCrystal;
use aether::resonance_policy::ResonancePolicy;
use aether::resonance_split::{ResonanceIngressPage, ResonanceObservationPage};

use crate::capability::{Capability, LearningRight};
use crate::lab_capsule::{CapsuleError, LabCapsule};
use crate::nexus_runtime;

pub const COMMAND_BUDGET_PER_HEARTBEAT: usize = 16;

static INGRESS_PAGE: ResonanceIngressPage = ResonanceIngressPage::new();
static OBSERVATION_PAGE: ResonanceObservationPage = ResonanceObservationPage::new();

static LAB_CAPSULE: LabCapsule = LabCapsule::new();
static POLICY_CRYSTAL: PolicyCrystal = PolicyCrystal::new();

static LAST_COLLAPSES: AtomicU64 = AtomicU64::new(0);

static mut PRIVATE_CURSOR: u64 = 0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaneDriverInitError {
    Capsule(CapsuleError),
    PolicyCrystal,
}

pub fn initialize(authority: &Capability<'_, LearningRight>) -> Result<(), PlaneDriverInitError> {
    INGRESS_PAGE.initialize(1);
    OBSERVATION_PAGE.initialize(1);

    LAB_CAPSULE
        .initialize(EVENT_HORIZON_PROGRAM, ResonancePolicy::DEFAULT, authority)
        .map_err(PlaneDriverInitError::Capsule)?;

    POLICY_CRYSTAL
        .publish(ResonancePolicy::DEFAULT)
        .map_err(|_| PlaneDriverInitError::PolicyCrystal)?;

    Ok(())
}

pub(crate) fn drive_once(wall_tick: u64, absorbed_ticks: u64) {
    drain_commands(wall_tick);

    nexus_runtime::heartbeat_batch(wall_tick, absorbed_ticks.min(64));

    evaluate_policy_and_publish(wall_tick);
}

fn evaluate_policy_and_publish(wall_tick: u64) {
    let sequence = OBSERVATION_PAGE.epoch().wrapping_add(1);

    let telemetry = nexus_runtime::telemetry(sequence, wall_tick);

    let previous_collapses = LAST_COLLAPSES.swap(telemetry.collapses, Ordering::AcqRel);

    let collapse_delta = telemetry.collapses.saturating_sub(previous_collapses);

    let target_phase = LAB_CAPSULE
        .current_policy()
        .map(|policy| policy.target_phase)
        .unwrap_or(0);

    let kernel_phase = telemetry.global_phase as u16 & 1023;

    let direct_drift = kernel_phase.abs_diff(target_phase);

    let phase_drift = direct_drift.min(1024 - direct_drift);

    let metrics = LabMetrics {
        heat: telemetry.heat.min(i64::MAX as u64) as i64,

        queue_pressure: 0,

        collapse_rate: collapse_delta.min(i64::MAX as u64) as i64,

        phase_drift: i64::from(phase_drift),

        replay_pressure: 0,

        coherence: 1024_i64.saturating_sub(i64::from(phase_drift)),

        kernel_phase: i64::from(kernel_phase),
    };

    if let Ok(Some(commit)) = LAB_CAPSULE.evaluate(metrics) {
        if POLICY_CRYSTAL.publish(commit.policy).is_ok() {
            if let Ok(majority) = POLICY_CRYSTAL.snapshot() {
                let _ = nexus_runtime::propose_policy(majority.policy, wall_tick);
            }
        }
    }

    let _ = nexus_runtime::service_policy_commit(wall_tick);

    let (state_root, checkpoint_generation, witness_root) = nexus_runtime::continuity_state();

    OBSERVATION_PAGE.publish_state_root(state_root);
    OBSERVATION_PAGE.publish_checkpoint_generation(checkpoint_generation);
    OBSERVATION_PAGE.publish_witness_root(witness_root);
    OBSERVATION_PAGE.publish_telemetry(&telemetry);
}

pub fn ingress() -> &'static ResonanceIngressPage {
    &INGRESS_PAGE
}

pub fn observation() -> &'static ResonanceObservationPage {
    &OBSERVATION_PAGE
}

fn drain_commands(wall_tick: u64) {
    for _ in 0..COMMAND_BUDGET_PER_HEARTBEAT {
        let cursor_ref = unsafe { &mut *core::ptr::addr_of_mut!(PRIVATE_CURSOR) };
        let command = INGRESS_PAGE.take_new(cursor_ref);
        let Some(command) = command else {
            break;
        };

        let reply = nexus_runtime::control(&command, wall_tick);

        OBSERVATION_PAGE.publish_reply(&reply);
    }
}
