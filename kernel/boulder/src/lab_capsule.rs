use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use aether::blacklab_vm::{ControlField, ExecutionError, LabMetrics, LabProgram, execute, verify};
use aether::causal_policy::{CausalPolicyBatch, PolicyTransition};
use aether::constellation::{ConstellationError, PolicyConstellation};
use aether::resonance_policy::{POLICY_REPHASE, PolicyError, ResonancePolicy};

use crate::capability::{Capability, LearningRight};

const MAXIMUM_REJECTIONS: u32 = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PolicyCommit {
    pub generation: u64,
    pub policy: ResonancePolicy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapsuleError {
    Tripped,
    Uninitialized,
    ProgramVerification,
    Execution(ExecutionError),
    Policy(PolicyError),
    Publication(ConstellationError),
}

pub struct LabCapsule {
    program: PolicyConstellation<LabProgram>,
    policy: PolicyConstellation<ResonancePolicy>,

    initialized: AtomicBool,
    tripped: AtomicBool,
    rejections: AtomicU32,
}

impl LabCapsule {
    pub const fn new() -> Self {
        Self {
            program: PolicyConstellation::new(),
            policy: PolicyConstellation::new(),
            initialized: AtomicBool::new(false),
            tripped: AtomicBool::new(false),
            rejections: AtomicU32::new(0),
        }
    }

    pub fn initialize(
        &self,
        initial_program: LabProgram,
        initial_policy: ResonancePolicy,
        _authority: &Capability<'_, LearningRight>,
    ) -> Result<(), CapsuleError> {
        verify(&initial_program).map_err(|_| CapsuleError::ProgramVerification)?;

        let policy = initial_policy.validate().map_err(CapsuleError::Policy)?;

        self.program
            .initialize(initial_program)
            .map_err(CapsuleError::Publication)?;

        self.policy
            .initialize(policy)
            .map_err(CapsuleError::Publication)?;

        self.initialized.store(true, Ordering::Release);
        Ok(())
    }

    pub fn install_program(
        &self,
        program: LabProgram,
        _authority: &Capability<'_, LearningRight>,
    ) -> Result<u64, CapsuleError> {
        self.require_live()?;

        verify(&program).map_err(|_| CapsuleError::ProgramVerification)?;

        self.program
            .publish(program)
            .map_err(CapsuleError::Publication)
    }

    pub fn evaluate(&self, metrics: LabMetrics) -> Result<Option<PolicyCommit>, CapsuleError> {
        self.require_live()?;

        let program = self.program.read().map_err(CapsuleError::Publication)?;

        let control = match execute(&program, metrics) {
            Ok(control) => control,
            Err(error) => {
                self.reject();
                return Err(CapsuleError::Execution(error));
            }
        };

        if control.mask == 0 {
            return Ok(None);
        }

        let current = self.policy.read().map_err(CapsuleError::Publication)?;

        let mut next = *current;

        if control.contains(ControlField::PriorityMass) {
            next.priority_mass = bounded_u16(control.priority_mass)?;
        }

        if control.contains(ControlField::CollapseThreshold) {
            next.collapse_threshold = bounded_u64(control.collapse_threshold)?;
        }

        if control.contains(ControlField::TargetPhase) {
            next.target_phase = bounded_u16(control.target_phase)?;

            next.flags |= POLICY_REPHASE;
        }

        if control.contains(ControlField::QuarantineTicks) {
            next.quarantine_ticks = bounded_u64(control.quarantine_ticks)?;
        }

        if control.contains(ControlField::Flags) {
            next.flags = bounded_u32(control.flags)?;
        }

        if control.contains(ControlField::HeatCeiling) {
            next.heat_ceiling = bounded_u64(control.heat_ceiling)?;
        }

        let next = match next.validate() {
            Ok(policy) => policy,
            Err(error) => {
                self.reject();
                return Err(CapsuleError::Policy(error));
            }
        };

        if next == *current {
            return Ok(None);
        }

        let generation = self
            .policy
            .publish(next)
            .map_err(CapsuleError::Publication)?;

        self.rejections.store(0, Ordering::Release);

        Ok(Some(PolicyCommit {
            generation,
            policy: next,
        }))
    }

    pub fn current_policy(&self) -> Result<ResonancePolicy, CapsuleError> {
        self.require_live()?;

        self.policy
            .read()
            .map(|policy| *policy)
            .map_err(CapsuleError::Publication)
    }

    pub fn reset_fuse(&self, _authority: &Capability<'_, LearningRight>) {
        self.rejections.store(0, Ordering::Release);
        self.tripped.store(false, Ordering::Release);
    }

    pub fn is_tripped(&self) -> bool {
        self.tripped.load(Ordering::Acquire)
    }

    fn require_live(&self) -> Result<(), CapsuleError> {
        if !self.initialized.load(Ordering::Acquire) {
            return Err(CapsuleError::Uninitialized);
        }

        if self.tripped.load(Ordering::Acquire) {
            return Err(CapsuleError::Tripped);
        }

        Ok(())
    }

    fn reject(&self) {
        let count = self
            .rejections
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);

        if count >= MAXIMUM_REJECTIONS {
            self.tripped.store(true, Ordering::Release);
        }
    }

    pub fn commit_causal_batch(
        &self,
        batch: &CausalPolicyBatch,
        _authority: &Capability<'_, LearningRight>,
    ) -> Result<PolicyTransition, CapsuleError> {
        self.require_live()?;

        let epoch = self.policy.generation();

        let current = self.policy.read().map_err(CapsuleError::Publication)?;

        let transition = batch
            .compile(epoch, *current)
            .map_err(|_| CapsuleError::ProgramVerification)?;

        let observed_epoch = self.policy.generation();

        if observed_epoch != transition.expected_epoch {
            return Err(CapsuleError::Publication(ConstellationError::WriterBusy));
        }

        self.policy
            .publish(transition.policy)
            .map_err(CapsuleError::Publication)?;

        Ok(transition)
    }
}

impl Default for LabCapsule {
    fn default() -> Self {
        Self::new()
    }
}

fn bounded_u64(value: i64) -> Result<u64, CapsuleError> {
    u64::try_from(value).map_err(|_| CapsuleError::Policy(PolicyError::CollapseThreshold))
}

fn bounded_u32(value: i64) -> Result<u32, CapsuleError> {
    u32::try_from(value).map_err(|_| CapsuleError::Policy(PolicyError::Flags))
}

fn bounded_u16(value: i64) -> Result<u16, CapsuleError> {
    u16::try_from(value).map_err(|_| CapsuleError::Policy(PolicyError::TargetPhase))
}
