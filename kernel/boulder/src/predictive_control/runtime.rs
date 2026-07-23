//! Predictive containment runtime.
//!
//! Observation remains bounded and only enqueues one transition. Deferred work
//! identifies the lifted model, updates conformal uncertainty, and performs the
//! robust finite-horizon search.

use super::barrier::SafetySet;
use super::conformal::{ConformalCalibrator, ConformalCertificate, ConformalConfig};
use super::mpc::{
    PlanCertificate, PlannerPolicy, PlanningError, PredictiveDirective, plan_robust_mpc,
};
use super::rls::{KoopmanRls, RlsCertificate, RlsConfig, Transition};
use super::state::{ControlState, mix};
use super::transcript::PredictiveSecrets;
use crate::manifold_orchestrator::Actuation;
use crate::tensor_decomp::{MultilinearDirective, TensorError};

pub const MAXIMUM_PENDING_TRANSITIONS: usize = 16;
const STATE_DOMAIN: u64 = 0x5354_4154_455f_524f;
const UPDATE_DOMAIN: u64 = 0x5550_4441_5445_5f52;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PredictivePolicy {
    pub rls: RlsConfig,
    pub conformal: ConformalConfig,
    pub planner: PlannerPolicy,
    pub minimum_model_samples: u16,
    pub plan_period_epochs: u16,
    pub maximum_update_batch: u8,
    pub maximum_rls_residual_q24: u64,
    pub maximum_symmetry_defect_q24: u64,
}

impl PredictivePolicy {
    pub const KERNEL_DEFAULT: Self = Self {
        rls: RlsConfig::KERNEL_DEFAULT,
        conformal: ConformalConfig::KERNEL_DEFAULT,
        planner: PlannerPolicy::KERNEL_DEFAULT,
        minimum_model_samples: 8,
        plan_period_epochs: 2,
        maximum_update_batch: 8,
        maximum_rls_residual_q24: 4 * crate::tensor_decomp::fixed::ONE as u64,
        maximum_symmetry_defect_q24: crate::tensor_decomp::fixed::ONE as u64 / 128,
    };

    fn validate(self) -> Result<(), TensorError> {
        if self.minimum_model_samples == 0
            || self.plan_period_epochs == 0
            || self.maximum_update_batch == 0
            || self.maximum_update_batch as usize > MAXIMUM_PENDING_TRANSITIONS
            || self.maximum_rls_residual_q24 == 0
        {
            return Err(TensorError::InvalidDimension);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelUpdateReport {
    pub transitions: u8,
    pub pending_after: u8,
    pub model_samples: u64,
    pub last_rls: RlsCertificate,
    pub last_conformal: ConformalCertificate,
    pub model_root: u64,
    pub conformal_root: u64,
    pub root: u64,
}

impl ModelUpdateReport {
    pub const EMPTY: Self = Self {
        transitions: 0,
        pending_after: 0,
        model_samples: 0,
        last_rls: RlsCertificate::EMPTY,
        last_conformal: ConformalCertificate::EMPTY,
        model_root: 0,
        conformal_root: 0,
        root: 0,
    };

    pub fn verify(&self, secret: u64) -> bool {
        self.transitions != 0
            && self.model_samples != 0
            && self.root == update_report_root(secret, self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PredictiveRuntimeError {
    Tensor(TensorError),
    Planning(PlanningError),
    TransitionCapacity,
    ModelNotReady,
    ConformalNotReady,
    StaleDirective,
    InvalidCertificate,
}

impl From<TensorError> for PredictiveRuntimeError {
    fn from(error: TensorError) -> Self {
        Self::Tensor(error)
    }
}

impl From<PlanningError> for PredictiveRuntimeError {
    fn from(error: PlanningError) -> Self {
        Self::Planning(error)
    }
}

pub struct PredictiveRuntime {
    secrets: PredictiveSecrets,
    state_secret: u64,
    policy: PredictivePolicy,
    model: KoopmanRls,
    model_backup: KoopmanRls,
    calibrator: ConformalCalibrator,
    calibrator_backup: ConformalCalibrator,
    safety: SafetySet,

    transitions: [Option<Transition>; MAXIMUM_PENDING_TRANSITIONS],
    transition_head: usize,
    transition_length: usize,

    last_state: Option<ControlState>,
    last_control_q24: i64,
    last_queue_class: u8,
    last_plan_epoch: u64,

    last_update: Option<ModelUpdateReport>,
    last_plan: Option<PlanCertificate>,
    last_directive: Option<PredictiveDirective>,
}

impl PredictiveRuntime {
    pub fn new(
        secrets: PredictiveSecrets,
        policy: PredictivePolicy,
    ) -> Result<Self, PredictiveRuntimeError> {
        policy.validate()?;
        validate_secrets(secrets)?;

        let state_secret = mix(secrets.certificate, STATE_DOMAIN);
        let model = KoopmanRls::new(policy.rls, secrets.model)?;
        let calibrator = ConformalCalibrator::new(policy.conformal, secrets.conformal)?;
        let safety = SafetySet::kernel_default(secrets.barrier)?;

        Ok(Self {
            secrets,
            state_secret,
            policy,
            model,
            model_backup: model,
            calibrator,
            calibrator_backup: calibrator,
            safety,
            transitions: [None; MAXIMUM_PENDING_TRANSITIONS],
            transition_head: 0,
            transition_length: 0,
            last_state: None,
            last_control_q24: 0,
            last_queue_class: 0,
            last_plan_epoch: 0,
            last_update: None,
            last_plan: None,
            last_directive: None,
        })
    }

    pub fn observe(
        &mut self,
        actuation: &Actuation,
        tensor: Option<&MultilinearDirective>,
    ) -> Result<ControlState, PredictiveRuntimeError> {
        let state = ControlState::from_sources(actuation, tensor, self.state_secret)?;

        if let Some(previous) = self.last_state {
            self.push_transition(Transition {
                previous,
                control_q24: self.last_control_q24,
                next: state,
            })?;
        }

        self.last_queue_class = state.queue_class;
        self.last_state = Some(state);
        Ok(state)
    }

    pub fn update_model_deferred(
        &mut self,
    ) -> Result<Option<ModelUpdateReport>, PredictiveRuntimeError> {
        if self.transition_length == 0 {
            return Ok(None);
        }

        let maximum = (self.policy.maximum_update_batch as usize).min(self.transition_length);
        let mut report = ModelUpdateReport::EMPTY;

        for _ in 0..maximum {
            let transition = self
                .pop_transition()
                .ok_or(PredictiveRuntimeError::InvalidCertificate)?;
            self.model_backup = self.model;
            self.calibrator_backup = self.calibrator;

            let rls = match self
                .model
                .update(transition, self.state_secret, self.secrets.model)
            {
                Ok(certificate) => certificate,
                Err(error) => {
                    self.model = self.model_backup;
                    return Err(error.into());
                }
            };
            if !rls.verify(
                self.secrets.model,
                self.policy.maximum_rls_residual_q24,
                self.policy.maximum_symmetry_defect_q24,
            ) {
                self.model = self.model_backup;
                return Err(PredictiveRuntimeError::InvalidCertificate);
            }

            let conformal = match self
                .calibrator
                .push(rls.maximum_residual_q24, self.secrets.conformal)
            {
                Ok(certificate) => certificate,
                Err(error) => {
                    self.model = self.model_backup;
                    self.calibrator = self.calibrator_backup;
                    return Err(error.into());
                }
            };
            if !conformal.verify(
                self.secrets.conformal,
                self.policy.conformal.maximum_radius_q24,
            ) {
                self.model = self.model_backup;
                self.calibrator = self.calibrator_backup;
                return Err(PredictiveRuntimeError::InvalidCertificate);
            }

            report.transitions = report.transitions.saturating_add(1);
            report.last_rls = rls;
            report.last_conformal = conformal;
        }

        report.pending_after = self.transition_length as u8;
        report.model_samples = self.model.samples();
        report.model_root = self.model.root();
        report.conformal_root = self.calibrator.root();
        report.root = update_report_root(mix(self.secrets.certificate, UPDATE_DOMAIN), &report);

        self.last_update = Some(report);
        Ok(Some(report))
    }

    pub fn plan_deferred(
        &mut self,
    ) -> Result<Option<(PredictiveDirective, PlanCertificate)>, PredictiveRuntimeError> {
        let Some(state) = self.last_state else {
            return Ok(None);
        };

        if self.model.samples() < u64::from(self.policy.minimum_model_samples) {
            return Ok(None);
        }
        if !self.calibrator.ready() {
            return Ok(None);
        }
        if self.last_plan_epoch != 0
            && state.epoch.saturating_sub(self.last_plan_epoch)
                < u64::from(self.policy.plan_period_epochs)
        {
            return Ok(None);
        }

        let conformal = self.calibrator.certificate(self.secrets.conformal)?;
        if !conformal.verify(
            self.secrets.conformal,
            self.policy.conformal.maximum_radius_q24,
        ) {
            return Err(PredictiveRuntimeError::InvalidCertificate);
        }

        let (directive, plan) = plan_robust_mpc(
            &state,
            self.last_queue_class,
            self.last_control_q24,
            conformal.radius_q24,
            conformal.root,
            &self.model,
            &self.safety,
            self.policy.planner,
            self.state_secret,
            self.secrets.planner,
            self.secrets.certificate,
        )?;

        if !directive.verify(self.secrets.planner)
            || !plan.verify(self.secrets.certificate)
            || directive.certificate_root != plan.root
            || plan.directive_root != directive.root
            || directive.model_root != self.model.root()
            || directive.conformal_root != conformal.root
            || directive.safety_root != self.safety.root()
        {
            return Err(PredictiveRuntimeError::InvalidCertificate);
        }

        self.last_plan_epoch = state.epoch;
        self.last_plan = Some(plan);
        self.last_directive = Some(directive);
        Ok(Some((directive, plan)))
    }

    pub fn mark_applied(
        &mut self,
        directive: PredictiveDirective,
    ) -> Result<(), PredictiveRuntimeError> {
        let expected = self
            .last_directive
            .ok_or(PredictiveRuntimeError::StaleDirective)?;
        let plan = self
            .last_plan
            .ok_or(PredictiveRuntimeError::StaleDirective)?;

        if directive != expected
            || !directive.verify(self.secrets.planner)
            || directive.certificate_root != plan.root
            || plan.directive_root != directive.root
        {
            return Err(PredictiveRuntimeError::StaleDirective);
        }

        self.last_control_q24 = directive.control_q24;
        self.last_queue_class = directive.queue_class;
        Ok(())
    }

    pub const fn pending_transitions(&self) -> usize {
        self.transition_length
    }

    pub const fn model_samples(&self) -> u64 {
        self.model.samples()
    }

    pub const fn last_update(&self) -> Option<ModelUpdateReport> {
        self.last_update
    }

    pub const fn last_plan(&self) -> Option<PlanCertificate> {
        self.last_plan
    }

    pub const fn last_directive(&self) -> Option<PredictiveDirective> {
        self.last_directive
    }

    fn push_transition(&mut self, transition: Transition) -> Result<(), PredictiveRuntimeError> {
        if self.transition_length >= MAXIMUM_PENDING_TRANSITIONS {
            return Err(PredictiveRuntimeError::TransitionCapacity);
        }

        let index = (self.transition_head + self.transition_length) % MAXIMUM_PENDING_TRANSITIONS;
        self.transitions[index] = Some(transition);
        self.transition_length += 1;
        Ok(())
    }

    fn pop_transition(&mut self) -> Option<Transition> {
        if self.transition_length == 0 {
            return None;
        }

        let transition = self.transitions[self.transition_head].take();
        self.transition_head = (self.transition_head + 1) % MAXIMUM_PENDING_TRANSITIONS;
        self.transition_length -= 1;
        transition
    }
}

fn validate_secrets(secrets: PredictiveSecrets) -> Result<(), PredictiveRuntimeError> {
    let values = secrets.values();
    for left in 0..values.len() {
        if values[left] == 0 {
            return Err(TensorError::ZeroSecret.into());
        }
        for right in left + 1..values.len() {
            if values[left] == values[right] {
                return Err(PredictiveRuntimeError::InvalidCertificate);
            }
        }
    }
    Ok(())
}

fn update_report_root(secret: u64, report: &ModelUpdateReport) -> u64 {
    let mut root = mix(
        secret,
        report.transitions as u64 | ((report.pending_after as u64) << 8),
    );
    root = mix(root, report.model_samples);
    root = mix(root, report.last_rls.root);
    root = mix(root, report.last_conformal.root);
    root = mix(root, report.model_root);
    mix(root, report.conformal_root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tensor_decomp::fixed;

    fn secrets() -> PredictiveSecrets {
        PredictiveSecrets {
            model: 1,
            conformal: 2,
            barrier: 3,
            planner: 4,
            certificate: 5,
        }
    }

    #[test]
    fn transition_ring_is_bounded() {
        let mut runtime =
            PredictiveRuntime::new(secrets(), PredictivePolicy::KERNEL_DEFAULT).unwrap();

        let previous = ControlState::with_values(
            1,
            [0; super::super::state::STATE_DIMENSION],
            0,
            runtime.state_secret,
        )
        .unwrap();
        let next = ControlState::with_values(
            2,
            [fixed::ONE / 8; super::super::state::STATE_DIMENSION],
            0,
            runtime.state_secret,
        )
        .unwrap();

        for _ in 0..MAXIMUM_PENDING_TRANSITIONS {
            runtime
                .push_transition(Transition {
                    previous,
                    control_q24: 0,
                    next,
                })
                .unwrap();
        }

        assert_eq!(
            runtime.push_transition(Transition {
                previous,
                control_q24: 0,
                next,
            }),
            Err(PredictiveRuntimeError::TransitionCapacity)
        );
    }
}
