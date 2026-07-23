//! Robust linear control-barrier constraints.
//!
//! Every safety rule is represented as:
//!
//! ```text
//! a^T x <= b.
//! ```
//!
//! A conformal scalar radius `r` is converted into a worst-case interval bound:
//!
//! ```text
//! a^T x + ||a||_1 r <= b.
//! ```
//!
//! The resulting margin is positive only when the complete uncertainty box is
//! safe.

use super::state::{ControlState, STATE_DIMENSION, coordinate, mix};
use crate::tensor_decomp::TensorError;
use crate::tensor_decomp::fixed;

pub const MAXIMUM_BARRIERS: usize = 12;
pub const NO_VIOLATION: u16 = u16::MAX;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BarrierConstraint {
    pub tag: u16,
    pub coefficients_q24: [i64; STATE_DIMENSION],
    pub bound_q24: i64,
}

impl BarrierConstraint {
    pub const EMPTY: Self = Self {
        tag: 0,
        coefficients_q24: [0; STATE_DIMENSION],
        bound_q24: 0,
    };

    pub const fn upper(tag: u16, coordinate: usize, bound_q24: i64) -> Self {
        let mut coefficients = [0_i64; STATE_DIMENSION];
        if coordinate < STATE_DIMENSION {
            coefficients[coordinate] = fixed::ONE;
        }
        Self {
            tag,
            coefficients_q24: coefficients,
            bound_q24,
        }
    }

    pub const fn lower(tag: u16, coordinate: usize, bound_q24: i64) -> Self {
        let mut coefficients = [0_i64; STATE_DIMENSION];
        if coordinate < STATE_DIMENSION {
            coefficients[coordinate] = -fixed::ONE;
        }
        Self {
            tag,
            coefficients_q24: coefficients,
            bound_q24: -bound_q24,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BarrierEvaluation {
    pub safe: bool,
    pub minimum_margin_q24: i64,
    pub violated_tag: u16,
    pub active_tag: u16,
    pub uncertainty_q24: i64,
    pub state_root: u64,
    pub barrier_root: u64,
    pub root: u64,
}

impl BarrierEvaluation {
    pub const EMPTY: Self = Self {
        safe: false,
        minimum_margin_q24: i64::MIN,
        violated_tag: NO_VIOLATION,
        active_tag: NO_VIOLATION,
        uncertainty_q24: 0,
        state_root: 0,
        barrier_root: 0,
        root: 0,
    };

    pub fn verify(&self, secret: u64) -> bool {
        self.root == evaluation_root(secret, self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SafetySet {
    barriers: [BarrierConstraint; MAXIMUM_BARRIERS],
    length: usize,
    root: u64,
}

impl SafetySet {
    pub fn kernel_default(secret: u64) -> Result<Self, TensorError> {
        let mut set = Self::new(secret)?;

        set.push(BarrierConstraint::upper(
            1,
            coordinate::QUEUE_CLASS,
            fixed::ONE,
        ))?;
        set.push(BarrierConstraint::upper(
            2,
            coordinate::HODGE_ENERGY,
            3 * fixed::ONE / 4,
        ))?;
        set.push(BarrierConstraint::upper(
            3,
            coordinate::CEILING_PRESSURE,
            9 * fixed::ONE / 10,
        ))?;
        set.push(BarrierConstraint::upper(
            4,
            coordinate::MIGRATION_PRESSURE,
            4 * fixed::ONE / 5,
        ))?;
        set.push(BarrierConstraint::lower(
            5,
            coordinate::CONNECTIVITY,
            fixed::ONE / 20,
        ))?;
        set.push(BarrierConstraint::upper(
            6,
            coordinate::OBSTRUCTION,
            fixed::ONE / 2,
        ))?;
        set.push(BarrierConstraint::upper(
            7,
            coordinate::TROPICAL_PRESSURE,
            9 * fixed::ONE / 10,
        ))?;
        set.push(BarrierConstraint::upper(
            8,
            coordinate::TENSOR_ANOMALY,
            fixed::ONE / 2,
        ))?;

        let mut energy_migration = [0_i64; STATE_DIMENSION];
        energy_migration[coordinate::HODGE_ENERGY] = fixed::ONE;
        energy_migration[coordinate::MIGRATION_PRESSURE] = fixed::ONE;
        set.push(BarrierConstraint {
            tag: 9,
            coefficients_q24: energy_migration,
            bound_q24: 6 * fixed::ONE / 5,
        })?;

        let mut topology_anomaly = [0_i64; STATE_DIMENSION];
        topology_anomaly[coordinate::OBSTRUCTION] = fixed::ONE;
        topology_anomaly[coordinate::TENSOR_ANOMALY] = fixed::ONE;
        set.push(BarrierConstraint {
            tag: 10,
            coefficients_q24: topology_anomaly,
            bound_q24: 4 * fixed::ONE / 5,
        })?;

        set.seal(secret);
        Ok(set)
    }

    pub fn new(secret: u64) -> Result<Self, TensorError> {
        if secret == 0 {
            return Err(TensorError::ZeroSecret);
        }
        let mut set = Self {
            barriers: [BarrierConstraint::EMPTY; MAXIMUM_BARRIERS],
            length: 0,
            root: 0,
        };
        set.seal(secret);
        Ok(set)
    }

    pub const fn len(&self) -> usize {
        self.length
    }

    pub const fn root(&self) -> u64 {
        self.root
    }

    pub fn barriers(&self) -> &[BarrierConstraint] {
        &self.barriers[..self.length]
    }

    pub fn push(&mut self, barrier: BarrierConstraint) -> Result<(), TensorError> {
        if self.length >= MAXIMUM_BARRIERS
            || barrier.tag == NO_VIOLATION
            || barrier
                .coefficients_q24
                .iter()
                .all(|coefficient| *coefficient == 0)
        {
            return Err(TensorError::Capacity);
        }

        if self.barriers[..self.length]
            .iter()
            .any(|existing| existing.tag == barrier.tag)
        {
            return Err(TensorError::InvalidDimension);
        }

        self.barriers[self.length] = barrier;
        self.length += 1;
        Ok(())
    }

    pub fn evaluate(
        &self,
        state: &ControlState,
        uncertainty_q24: i64,
        secret: u64,
    ) -> Result<BarrierEvaluation, TensorError> {
        if secret == 0 || self.length == 0 || uncertainty_q24 < 0 {
            return Err(TensorError::Arithmetic);
        }

        let mut minimum_margin = i64::MAX;
        let mut active_tag = NO_VIOLATION;
        let mut violated_tag = NO_VIOLATION;

        for barrier in self.barriers() {
            let mut left = 0_i64;
            let mut norm_l1 = 0_i64;

            for coordinate in 0..STATE_DIMENSION {
                left = left
                    .checked_add(fixed::mul(
                        barrier.coefficients_q24[coordinate],
                        state.values_q24[coordinate],
                    )?)
                    .ok_or(TensorError::Arithmetic)?;
                norm_l1 = norm_l1
                    .checked_add(
                        barrier.coefficients_q24[coordinate]
                            .checked_abs()
                            .ok_or(TensorError::Arithmetic)?,
                    )
                    .ok_or(TensorError::Arithmetic)?;
            }

            let robust_uncertainty = fixed::mul(norm_l1, uncertainty_q24)?;
            let margin = barrier
                .bound_q24
                .checked_sub(left)
                .and_then(|value| value.checked_sub(robust_uncertainty))
                .ok_or(TensorError::Arithmetic)?;

            if margin < minimum_margin {
                minimum_margin = margin;
                active_tag = barrier.tag;
            }
            if margin < 0 && violated_tag == NO_VIOLATION {
                violated_tag = barrier.tag;
            }
        }

        let mut evaluation = BarrierEvaluation {
            safe: violated_tag == NO_VIOLATION,
            minimum_margin_q24: minimum_margin,
            violated_tag,
            active_tag,
            uncertainty_q24,
            state_root: state.root,
            barrier_root: self.root,
            root: 0,
        };
        evaluation.root = evaluation_root(secret, &evaluation);
        Ok(evaluation)
    }

    fn seal(&mut self, secret: u64) {
        let mut root = mix(secret, self.length as u64);
        for barrier in self.barriers() {
            root = mix(root, barrier.tag as u64);
            root = mix(root, barrier.bound_q24 as u64);
            for coefficient in barrier.coefficients_q24 {
                root = mix(root, coefficient as u64);
            }
        }
        self.root = root;
    }
}

fn evaluation_root(secret: u64, evaluation: &BarrierEvaluation) -> u64 {
    let mut root = mix(secret, u64::from(evaluation.safe));
    root = mix(root, evaluation.minimum_margin_q24 as u64);
    root = mix(
        root,
        evaluation.violated_tag as u64 | ((evaluation.active_tag as u64) << 16),
    );
    root = mix(root, evaluation.uncertainty_q24 as u64);
    root = mix(root, evaluation.state_root);
    mix(root, evaluation.barrier_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncertainty_can_turn_nominal_state_unsafe() {
        let set = SafetySet::kernel_default(7).unwrap();
        let mut values = [0_i64; STATE_DIMENSION];
        values[coordinate::HODGE_ENERGY] = 3 * fixed::ONE / 4 - fixed::ONE / 64;
        values[coordinate::CONNECTIVITY] = fixed::ONE / 2;
        let state = ControlState::with_values(1, values, 0, 11).unwrap();

        let nominal = set.evaluate(&state, 0, 7).unwrap();
        let robust = set.evaluate(&state, fixed::ONE / 32, 7).unwrap();

        assert!(nominal.safe);
        assert!(!robust.safe);
    }
}
