//! Robust finite-horizon model-predictive queue controller.
//!
//! The action space is deliberately finite. Every length-four sequence from a
//! five-level control lattice is simulated through the learned Koopman model.
//! A sequence is admissible only when every conformal uncertainty box satisfies
//! every barrier. The minimum-cost admissible sequence is sealed.

use super::barrier::{NO_VIOLATION, SafetySet};
use super::rls::KoopmanRls;
use super::state::{ControlState, STATE_DIMENSION, mix};
use crate::tensor_decomp::TensorError;
use crate::tensor_decomp::fixed;

pub const PLANNING_HORIZON: usize = 4;
pub const ACTION_COUNT: usize = 5;
pub const CANDIDATE_COUNT: usize = ACTION_COUNT * ACTION_COUNT * ACTION_COUNT * ACTION_COUNT;

pub const ACTION_LEVELS_Q24: [i64; ACTION_COUNT] = [
    0,
    fixed::ONE / 16,
    fixed::ONE / 8,
    fixed::ONE / 4,
    fixed::ONE / 2,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlannerPolicy {
    pub state_weights_q24: [i64; STATE_DIMENSION],
    pub state_targets_q24: [i64; STATE_DIMENSION],
    pub control_weight_q24: i64,
    pub switching_weight_q24: i64,
    pub terminal_multiplier_q24: i64,
    pub maximum_queue_charge: u32,
    pub maximum_uncertainty_q24: i64,
}

impl PlannerPolicy {
    pub const KERNEL_DEFAULT: Self = Self {
        state_weights_q24: [
            fixed::ONE / 16,
            4 * fixed::ONE,
            2 * fixed::ONE,
            2 * fixed::ONE,
            2 * fixed::ONE,
            4 * fixed::ONE,
            fixed::ONE,
            4 * fixed::ONE,
        ],
        state_targets_q24: [
            0,
            fixed::ONE / 8,
            fixed::ONE / 4,
            fixed::ONE / 4,
            fixed::ONE / 2,
            0,
            fixed::ONE / 4,
            0,
        ],
        control_weight_q24: fixed::ONE / 8,
        switching_weight_q24: fixed::ONE / 4,
        terminal_multiplier_q24: 2 * fixed::ONE,
        maximum_queue_charge: 4096,
        maximum_uncertainty_q24: fixed::ONE / 2,
    };

    fn validate(self) -> Result<(), TensorError> {
        if self.control_weight_q24 < 0
            || self.switching_weight_q24 < 0
            || self.terminal_multiplier_q24 < fixed::ONE
            || self.maximum_queue_charge == 0
            || self.maximum_uncertainty_q24 <= 0
            || self.state_weights_q24.iter().any(|weight| *weight < 0)
        {
            return Err(TensorError::InvalidDimension);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlanCertificate {
    pub epoch: u64,
    pub candidates_evaluated: u16,
    pub safe_candidates: u16,
    pub selected_sequence_q24: [i64; PLANNING_HORIZON],
    pub selected_cost_q24: i64,
    pub minimum_margin_q24: i64,
    pub active_barrier_tag: u16,
    pub forecast_root: u64,
    pub model_root: u64,
    pub conformal_root: u64,
    pub safety_root: u64,
    pub directive_root: u64,
    pub root: u64,
}

impl PlanCertificate {
    pub const EMPTY: Self = Self {
        epoch: 0,
        candidates_evaluated: 0,
        safe_candidates: 0,
        selected_sequence_q24: [0; PLANNING_HORIZON],
        selected_cost_q24: 0,
        minimum_margin_q24: i64::MIN,
        active_barrier_tag: NO_VIOLATION,
        forecast_root: 0,
        model_root: 0,
        conformal_root: 0,
        safety_root: 0,
        directive_root: 0,
        root: 0,
    };

    pub fn verify(&self, secret: u64) -> bool {
        self.candidates_evaluated == CANDIDATE_COUNT as u16
            && self.safe_candidates != 0
            && self.minimum_margin_q24 >= 0
            && self.root == certificate_root(secret, self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PredictiveDirective {
    pub epoch: u64,
    pub queue_class: u8,
    pub queue_charge: u32,
    pub control_q24: i64,
    pub predicted_cost_q24: i64,
    pub minimum_margin_q24: i64,
    pub active_barrier_tag: u16,
    pub model_root: u64,
    pub conformal_root: u64,
    pub safety_root: u64,
    pub forecast_root: u64,
    pub certificate_root: u64,
    pub root: u64,
}

impl PredictiveDirective {
    pub const EMPTY: Self = Self {
        epoch: 0,
        queue_class: 0,
        queue_charge: 0,
        control_q24: 0,
        predicted_cost_q24: 0,
        minimum_margin_q24: 0,
        active_barrier_tag: NO_VIOLATION,
        model_root: 0,
        conformal_root: 0,
        safety_root: 0,
        forecast_root: 0,
        certificate_root: 0,
        root: 0,
    };

    pub fn verify(&self, secret: u64) -> bool {
        self.control_q24 >= 0
            && self.control_q24 <= fixed::ONE
            && self.root == directive_root(secret, self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanningError {
    NoSafeSequence,
    Tensor(TensorError),
    Fixed(crate::tensor_decomp::fixed::FixedError),
}

impl From<TensorError> for PlanningError {
    fn from(error: TensorError) -> Self {
        Self::Tensor(error)
    }
}

impl From<crate::tensor_decomp::fixed::FixedError> for PlanningError {
    fn from(error: crate::tensor_decomp::fixed::FixedError) -> Self {
        Self::Fixed(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Candidate {
    sequence_q24: [i64; PLANNING_HORIZON],
    cost_q24: i64,
    minimum_margin_q24: i64,
    active_barrier_tag: u16,
    forecast_root: u64,
}

pub fn plan_robust_mpc(
    initial: &ControlState,
    queue_class: u8,
    previous_control_q24: i64,
    conformal_radius_q24: i64,
    conformal_root: u64,
    model: &KoopmanRls,
    safety: &SafetySet,
    policy: PlannerPolicy,
    state_secret: u64,
    planner_secret: u64,
    certificate_secret: u64,
) -> Result<(PredictiveDirective, PlanCertificate), PlanningError> {
    policy.validate()?;
    if state_secret == 0
        || planner_secret == 0
        || certificate_secret == 0
        || conformal_root == 0
        || model.root() == 0
        || safety.root() == 0
        || conformal_radius_q24 < 0
    {
        return Err(TensorError::Arithmetic.into());
    }

    let mut best: Option<Candidate> = None;
    let mut safe_candidates = 0_u16;

    for encoded in 0..CANDIDATE_COUNT {
        let sequence = decode_sequence(encoded);
        let candidate = evaluate_candidate(
            initial,
            previous_control_q24,
            conformal_radius_q24,
            model,
            safety,
            policy,
            state_secret,
            planner_secret,
            sequence,
        )?;

        let Some(candidate) = candidate else {
            continue;
        };
        safe_candidates = safe_candidates.saturating_add(1);

        let replace = best
            .map(|current| {
                candidate.cost_q24 < current.cost_q24
                    || (candidate.cost_q24 == current.cost_q24
                        && lexicographically_less(&candidate.sequence_q24, &current.sequence_q24))
            })
            .unwrap_or(true);
        if replace {
            best = Some(candidate);
        }
    }

    let best = best.ok_or(PlanningError::NoSafeSequence)?;
    let first_control = best.sequence_q24[0];
    let queue_charge = scale_charge(first_control, policy.maximum_queue_charge)?;

    let mut directive = PredictiveDirective {
        epoch: initial.epoch,
        queue_class,
        queue_charge,
        control_q24: first_control,
        predicted_cost_q24: best.cost_q24,
        minimum_margin_q24: best.minimum_margin_q24,
        active_barrier_tag: best.active_barrier_tag,
        model_root: model.root(),
        conformal_root,
        safety_root: safety.root(),
        forecast_root: best.forecast_root,
        certificate_root: 0,
        root: 0,
    };
    directive.root = directive_root(planner_secret, &directive);

    let mut certificate = PlanCertificate {
        epoch: initial.epoch,
        candidates_evaluated: CANDIDATE_COUNT as u16,
        safe_candidates,
        selected_sequence_q24: best.sequence_q24,
        selected_cost_q24: best.cost_q24,
        minimum_margin_q24: best.minimum_margin_q24,
        active_barrier_tag: best.active_barrier_tag,
        forecast_root: best.forecast_root,
        model_root: model.root(),
        conformal_root,
        safety_root: safety.root(),
        directive_root: directive.root,
        root: 0,
    };
    certificate.root = certificate_root(certificate_secret, &certificate);
    directive.certificate_root = certificate.root;

    Ok((directive, certificate))
}

#[allow(clippy::too_many_arguments)]
fn evaluate_candidate(
    initial: &ControlState,
    previous_control_q24: i64,
    conformal_radius_q24: i64,
    model: &KoopmanRls,
    safety: &SafetySet,
    policy: PlannerPolicy,
    state_secret: u64,
    planner_secret: u64,
    sequence: [i64; PLANNING_HORIZON],
) -> Result<Option<Candidate>, PlanningError> {
    let mut state = *initial;
    let mut previous_control = previous_control_q24.clamp(0, fixed::ONE);
    let mut cost = 0_i64;
    let mut minimum_margin = i64::MAX;
    let mut active_tag = NO_VIOLATION;
    let mut forecast_root = mix(planner_secret, initial.root);

    for step in 0..PLANNING_HORIZON {
        let control = sequence[step];
        state = model.predict(&state, control, state_secret)?;

        let uncertainty = conformal_radius_q24
            .checked_mul((step + 1) as i64)
            .ok_or(TensorError::Arithmetic)?
            .min(policy.maximum_uncertainty_q24);
        let evaluation = safety.evaluate(&state, uncertainty, planner_secret)?;
        if !evaluation.safe {
            return Ok(None);
        }

        if evaluation.minimum_margin_q24 < minimum_margin {
            minimum_margin = evaluation.minimum_margin_q24;
            active_tag = evaluation.active_tag;
        }

        cost = cost
            .checked_add(state_cost(&state, policy)?)
            .and_then(|value| {
                control_cost(control, previous_control, policy)
                    .ok()
                    .and_then(|term| value.checked_add(term))
            })
            .ok_or(TensorError::Arithmetic)?;

        forecast_root = mix(forecast_root, state.root);
        forecast_root = mix(forecast_root, evaluation.root);
        forecast_root = mix(forecast_root, control as u64);
        previous_control = control;
    }

    let terminal = fixed::mul(state_cost(&state, policy)?, policy.terminal_multiplier_q24)?;
    cost = cost.checked_add(terminal).ok_or(TensorError::Arithmetic)?;

    Ok(Some(Candidate {
        sequence_q24: sequence,
        cost_q24: cost,
        minimum_margin_q24: minimum_margin,
        active_barrier_tag: active_tag,
        forecast_root,
    }))
}

fn state_cost(state: &ControlState, policy: PlannerPolicy) -> Result<i64, TensorError> {
    let mut cost = 0_i64;

    for index in 0..STATE_DIMENSION {
        let error = state.values_q24[index]
            .checked_sub(policy.state_targets_q24[index])
            .ok_or(TensorError::Arithmetic)?;
        let square = fixed::mul(error, error)?;
        let weighted = fixed::mul(policy.state_weights_q24[index], square)?;
        cost = cost.checked_add(weighted).ok_or(TensorError::Arithmetic)?;
    }

    Ok(cost)
}

fn control_cost(
    control_q24: i64,
    previous_q24: i64,
    policy: PlannerPolicy,
) -> Result<i64, TensorError> {
    let control_square = fixed::mul(control_q24, control_q24)?;
    let difference = control_q24
        .checked_sub(previous_q24)
        .ok_or(TensorError::Arithmetic)?;
    let switching_square = fixed::mul(difference, difference)?;

    fixed::mul(policy.control_weight_q24, control_square)?
        .checked_add(fixed::mul(policy.switching_weight_q24, switching_square)?)
        .ok_or(TensorError::Arithmetic)
}

fn decode_sequence(mut encoded: usize) -> [i64; PLANNING_HORIZON] {
    let mut sequence = [0_i64; PLANNING_HORIZON];
    for step in 0..PLANNING_HORIZON {
        sequence[step] = ACTION_LEVELS_Q24[encoded % ACTION_COUNT];
        encoded /= ACTION_COUNT;
    }
    sequence
}

fn lexicographically_less(left: &[i64; PLANNING_HORIZON], right: &[i64; PLANNING_HORIZON]) -> bool {
    for index in 0..PLANNING_HORIZON {
        if left[index] < right[index] {
            return true;
        }
        if left[index] > right[index] {
            return false;
        }
    }
    false
}

fn scale_charge(control_q24: i64, maximum: u32) -> Result<u32, TensorError> {
    let scaled = (control_q24.clamp(0, fixed::ONE) as u128)
        .checked_mul(maximum as u128)
        .ok_or(TensorError::Arithmetic)?
        >> fixed::FRACTION_BITS;
    Ok(scaled.min(maximum as u128) as u32)
}

fn directive_root(secret: u64, directive: &PredictiveDirective) -> u64 {
    let mut root = mix(secret, directive.epoch);
    root = mix(
        root,
        directive.queue_class as u64 | ((directive.queue_charge as u64) << 8),
    );
    root = mix(root, directive.control_q24 as u64);
    root = mix(root, directive.predicted_cost_q24 as u64);
    root = mix(root, directive.minimum_margin_q24 as u64);
    root = mix(root, directive.active_barrier_tag as u64);
    root = mix(root, directive.model_root);
    root = mix(root, directive.conformal_root);
    root = mix(root, directive.safety_root);
    mix(root, directive.forecast_root)
}

fn certificate_root(secret: u64, certificate: &PlanCertificate) -> u64 {
    let mut root = mix(secret, certificate.epoch);
    root = mix(
        root,
        certificate.candidates_evaluated as u64 | ((certificate.safe_candidates as u64) << 16),
    );
    for control in certificate.selected_sequence_q24 {
        root = mix(root, control as u64);
    }
    root = mix(root, certificate.selected_cost_q24 as u64);
    root = mix(root, certificate.minimum_margin_q24 as u64);
    root = mix(root, certificate.active_barrier_tag as u64);
    root = mix(root, certificate.forecast_root);
    root = mix(root, certificate.model_root);
    root = mix(root, certificate.conformal_root);
    root = mix(root, certificate.safety_root);
    mix(root, certificate.directive_root)
}

#[cfg(test)]
mod tests {
    use super::super::barrier::SafetySet;
    use super::super::rls::{KoopmanRls, RlsConfig};
    use super::super::state::{ControlState, coordinate};
    use super::*;

    #[test]
    fn planner_selects_a_safe_sequence() {
        let model = KoopmanRls::new(RlsConfig::KERNEL_DEFAULT, 7).unwrap();
        let safety = SafetySet::kernel_default(9).unwrap();
        let mut values = [0_i64; STATE_DIMENSION];
        values[coordinate::CONNECTIVITY] = fixed::ONE / 2;
        let state = ControlState::with_values(10, values, 2, 11).unwrap();

        let (directive, certificate) = plan_robust_mpc(
            &state,
            2,
            0,
            fixed::ONE / 1024,
            13,
            &model,
            &safety,
            PlannerPolicy::KERNEL_DEFAULT,
            11,
            17,
            19,
        )
        .unwrap();

        assert!(directive.verify(17));
        assert!(certificate.verify(19));
    }
}
