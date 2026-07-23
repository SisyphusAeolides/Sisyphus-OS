//! Recursive least-squares identification of a lifted Koopman predictor.
//!
//! The model predicts the next physical state from nonlinear lifted features
//! and the previously applied scalar queue-pressure control:
//!
//! ```text
//! x_(k+1) = Theta [phi(x_k); u_k] + residual.
//! ```
//!
//! A shared regressor covariance is updated with exponential forgetting.
//! Symmetry, diagonal floors, parameter bounds, and full residual evidence are
//! enforced after every transition.

use super::dictionary::{LiftedFeatures, REGRESSOR_DIMENSION};
use super::state::{ControlState, STATE_DIMENSION, STATE_LIMIT_Q24, mix};
use crate::tensor_decomp::TensorError;
use crate::tensor_decomp::fixed;

pub const MAXIMUM_MODEL_PARAMETERS: usize = STATE_DIMENSION * REGRESSOR_DIMENSION;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RlsConfig {
    pub forgetting_q24: i64,
    pub initial_covariance_q24: i64,
    pub diagonal_floor_q24: i64,
    pub covariance_limit_q24: i64,
    pub parameter_limit_q24: i64,
    pub residual_limit_q24: i64,
}

impl RlsConfig {
    pub const KERNEL_DEFAULT: Self = Self {
        forgetting_q24: fixed::ONE - fixed::ONE / 1024,
        initial_covariance_q24: 8 * fixed::ONE,
        diagonal_floor_q24: fixed::ONE / 65_536,
        covariance_limit_q24: 64 * fixed::ONE,
        parameter_limit_q24: 16 * fixed::ONE,
        residual_limit_q24: 4 * fixed::ONE,
    };

    fn validate(self) -> Result<(), TensorError> {
        if self.forgetting_q24 <= 0
            || self.forgetting_q24 > fixed::ONE
            || self.initial_covariance_q24 <= 0
            || self.diagonal_floor_q24 <= 0
            || self.covariance_limit_q24 < self.initial_covariance_q24
            || self.parameter_limit_q24 <= fixed::ONE
            || self.residual_limit_q24 <= 0
        {
            return Err(TensorError::InvalidDimension);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Transition {
    pub previous: ControlState,
    pub control_q24: i64,
    pub next: ControlState,
}

impl Transition {
    pub fn verify(&self, state_secret: u64) -> bool {
        self.previous.verify(state_secret)
            && self.next.verify(state_secret)
            && self.next.epoch > self.previous.epoch
            && (0..=fixed::ONE).contains(&self.control_q24)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RlsCertificate {
    pub sample: u64,
    pub previous_root: u64,
    pub next_root: u64,
    pub prediction_root: u64,
    pub maximum_residual_q24: u64,
    pub gain_norm_q24: i64,
    pub covariance_symmetry_defect_q24: u64,
    pub covariance_diagonal_floor_q24: i64,
    pub model_root: u64,
    pub root: u64,
}

impl RlsCertificate {
    pub const EMPTY: Self = Self {
        sample: 0,
        previous_root: 0,
        next_root: 0,
        prediction_root: 0,
        maximum_residual_q24: 0,
        gain_norm_q24: 0,
        covariance_symmetry_defect_q24: 0,
        covariance_diagonal_floor_q24: 0,
        model_root: 0,
        root: 0,
    };

    pub fn verify(
        &self,
        secret: u64,
        maximum_residual_q24: u64,
        maximum_symmetry_defect_q24: u64,
    ) -> bool {
        self.sample != 0
            && self.maximum_residual_q24 <= maximum_residual_q24
            && self.covariance_symmetry_defect_q24 <= maximum_symmetry_defect_q24
            && self.covariance_diagonal_floor_q24 > 0
            && self.root == certificate_root(secret, self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KoopmanRls {
    theta_q24: [[i64; REGRESSOR_DIMENSION]; STATE_DIMENSION],
    covariance_q24: [[i64; REGRESSOR_DIMENSION]; REGRESSOR_DIMENSION],
    config: RlsConfig,
    samples: u64,
    root: u64,
}

impl KoopmanRls {
    pub fn new(config: RlsConfig, secret: u64) -> Result<Self, TensorError> {
        config.validate()?;
        if secret == 0 {
            return Err(TensorError::ZeroSecret);
        }

        let mut theta = [[0_i64; REGRESSOR_DIMENSION]; STATE_DIMENSION];
        for state in 0..STATE_DIMENSION {
            theta[state][1 + state] = fixed::ONE;
        }

        let mut covariance = [[0_i64; REGRESSOR_DIMENSION]; REGRESSOR_DIMENSION];
        for index in 0..REGRESSOR_DIMENSION {
            covariance[index][index] = config.initial_covariance_q24;
        }

        let mut model = Self {
            theta_q24: theta,
            covariance_q24: covariance,
            config,
            samples: 0,
            root: 0,
        };
        model.seal(secret);
        Ok(model)
    }

    pub const fn samples(&self) -> u64 {
        self.samples
    }

    pub const fn root(&self) -> u64 {
        self.root
    }

    pub fn coefficient(&self, output: usize, regressor: usize) -> Result<i64, TensorError> {
        self.theta_q24
            .get(output)
            .and_then(|row| row.get(regressor))
            .copied()
            .ok_or(TensorError::Coordinate)
    }

    pub fn predict(
        &self,
        current: &ControlState,
        control_q24: i64,
        state_secret: u64,
    ) -> Result<ControlState, TensorError> {
        let features = LiftedFeatures::lift(current)?;
        let regressor = features.regressor(control_q24.clamp(0, fixed::ONE));
        let values = self.predict_regressor(&regressor)?;

        ControlState::with_values(
            current.epoch.saturating_add(1),
            values,
            current.queue_class,
            state_secret,
        )
    }

    pub fn update(
        &mut self,
        transition: Transition,
        state_secret: u64,
        model_secret: u64,
    ) -> Result<RlsCertificate, TensorError> {
        if model_secret == 0 || !transition.verify(state_secret) {
            return Err(TensorError::Arithmetic);
        }

        let features = LiftedFeatures::lift(&transition.previous)?;
        let regressor = features.regressor(transition.control_q24);
        let prediction_values = self.predict_regressor(&regressor)?;
        let prediction = ControlState::with_values(
            transition.next.epoch,
            prediction_values,
            transition.previous.queue_class,
            state_secret,
        )?;

        let mut covariance_times_regressor = [0_i64; REGRESSOR_DIMENSION];
        for row in 0..REGRESSOR_DIMENSION {
            let mut value = 0_i64;
            for column in 0..REGRESSOR_DIMENSION {
                value = value
                    .checked_add(fixed::mul(
                        self.covariance_q24[row][column],
                        regressor[column],
                    )?)
                    .ok_or(TensorError::Arithmetic)?;
            }
            covariance_times_regressor[row] = value;
        }

        let mut denominator = self.config.forgetting_q24;
        for index in 0..REGRESSOR_DIMENSION {
            denominator = denominator
                .checked_add(fixed::mul(
                    regressor[index],
                    covariance_times_regressor[index],
                )?)
                .ok_or(TensorError::Arithmetic)?;
        }
        if denominator <= self.config.diagonal_floor_q24 {
            return Err(TensorError::Arithmetic);
        }

        let mut gain = [0_i64; REGRESSOR_DIMENSION];
        let mut gain_norm_squared = 0_i64;
        for index in 0..REGRESSOR_DIMENSION {
            gain[index] = fixed::div(covariance_times_regressor[index], denominator)?;
            gain_norm_squared = gain_norm_squared
                .checked_add(fixed::mul(gain[index], gain[index])?)
                .ok_or(TensorError::Arithmetic)?;
        }

        let mut maximum_residual = 0_u64;
        let mut residuals = [0_i64; STATE_DIMENSION];
        for output in 0..STATE_DIMENSION {
            let raw_residual = transition.next.values_q24[output]
                .checked_sub(prediction_values[output])
                .ok_or(TensorError::Arithmetic)?;
            maximum_residual = maximum_residual.max(raw_residual.unsigned_abs());
            residuals[output] = raw_residual.clamp(
                -self.config.residual_limit_q24,
                self.config.residual_limit_q24,
            );
        }

        for output in 0..STATE_DIMENSION {
            for index in 0..REGRESSOR_DIMENSION {
                let correction = fixed::mul(residuals[output], gain[index])?;
                self.theta_q24[output][index] = self.theta_q24[output][index]
                    .checked_add(correction)
                    .ok_or(TensorError::Arithmetic)?
                    .clamp(
                        -self.config.parameter_limit_q24,
                        self.config.parameter_limit_q24,
                    );
            }
        }

        for row in 0..REGRESSOR_DIMENSION {
            for column in 0..REGRESSOR_DIMENSION {
                let correction = fixed::mul(gain[row], covariance_times_regressor[column])?;
                let reduced = self.covariance_q24[row][column]
                    .checked_sub(correction)
                    .ok_or(TensorError::Arithmetic)?;
                self.covariance_q24[row][column] = fixed::div(reduced, self.config.forgetting_q24)?
                    .clamp(
                        -self.config.covariance_limit_q24,
                        self.config.covariance_limit_q24,
                    );
            }
        }

        let symmetry_defect = self.symmetrize_and_floor()?;
        self.samples = self.samples.saturating_add(1);
        self.seal(model_secret);

        let mut certificate = RlsCertificate {
            sample: self.samples,
            previous_root: transition.previous.root,
            next_root: transition.next.root,
            prediction_root: prediction.root,
            maximum_residual_q24: maximum_residual,
            gain_norm_q24: fixed::sqrt(gain_norm_squared.max(0))?,
            covariance_symmetry_defect_q24: symmetry_defect,
            covariance_diagonal_floor_q24: self.minimum_covariance_diagonal(),
            model_root: self.root,
            root: 0,
        };
        certificate.root = certificate_root(model_secret, &certificate);
        Ok(certificate)
    }

    fn predict_regressor(
        &self,
        regressor: &[i64; REGRESSOR_DIMENSION],
    ) -> Result<[i64; STATE_DIMENSION], TensorError> {
        let mut output = [0_i64; STATE_DIMENSION];

        for state in 0..STATE_DIMENSION {
            let mut value = 0_i64;
            for index in 0..REGRESSOR_DIMENSION {
                value = value
                    .checked_add(fixed::mul(self.theta_q24[state][index], regressor[index])?)
                    .ok_or(TensorError::Arithmetic)?;
            }
            output[state] = value.clamp(-STATE_LIMIT_Q24, STATE_LIMIT_Q24);
        }

        Ok(output)
    }

    fn symmetrize_and_floor(&mut self) -> Result<u64, TensorError> {
        let mut maximum_defect = 0_u64;

        for row in 0..REGRESSOR_DIMENSION {
            for column in row + 1..REGRESSOR_DIMENSION {
                let left = self.covariance_q24[row][column];
                let right = self.covariance_q24[column][row];
                maximum_defect = maximum_defect.max(left.abs_diff(right));
                let average = left.checked_add(right).ok_or(TensorError::Arithmetic)? / 2;
                self.covariance_q24[row][column] = average;
                self.covariance_q24[column][row] = average;
            }

            self.covariance_q24[row][row] = self.covariance_q24[row][row]
                .max(self.config.diagonal_floor_q24)
                .min(self.config.covariance_limit_q24);
        }

        Ok(maximum_defect)
    }

    fn minimum_covariance_diagonal(&self) -> i64 {
        let mut minimum = i64::MAX;
        for index in 0..REGRESSOR_DIMENSION {
            minimum = minimum.min(self.covariance_q24[index][index]);
        }
        minimum
    }

    fn seal(&mut self, secret: u64) {
        let mut root = mix(secret, self.samples);
        for output in 0..STATE_DIMENSION {
            for coefficient in self.theta_q24[output] {
                root = mix(root, coefficient as u64);
            }
        }
        for row in 0..REGRESSOR_DIMENSION {
            for value in self.covariance_q24[row] {
                root = mix(root, value as u64);
            }
        }
        self.root = root;
    }
}

fn certificate_root(secret: u64, certificate: &RlsCertificate) -> u64 {
    let mut root = mix(secret, certificate.sample);
    root = mix(root, certificate.previous_root);
    root = mix(root, certificate.next_root);
    root = mix(root, certificate.prediction_root);
    root = mix(root, certificate.maximum_residual_q24);
    root = mix(root, certificate.gain_norm_q24 as u64);
    root = mix(root, certificate.covariance_symmetry_defect_q24);
    root = mix(root, certificate.covariance_diagonal_floor_q24 as u64);
    mix(root, certificate.model_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_seed_predicts_stationary_state() {
        let model = KoopmanRls::new(RlsConfig::KERNEL_DEFAULT, 7).unwrap();
        let state = ControlState::with_values(1, [fixed::ONE / 4; STATE_DIMENSION], 2, 11).unwrap();

        let prediction = model.predict(&state, 0, 11).unwrap();
        assert_eq!(prediction.values_q24, state.values_q24);
    }

    #[test]
    fn update_produces_a_model_certificate() {
        let mut model = KoopmanRls::new(RlsConfig::KERNEL_DEFAULT, 7).unwrap();
        let previous =
            ControlState::with_values(1, [fixed::ONE / 4; STATE_DIMENSION], 1, 11).unwrap();
        let next = ControlState::with_values(2, [fixed::ONE / 3; STATE_DIMENSION], 1, 11).unwrap();

        let certificate = model
            .update(
                Transition {
                    previous,
                    control_q24: fixed::ONE / 4,
                    next,
                },
                11,
                7,
            )
            .unwrap();

        assert_eq!(certificate.sample, 1);
        assert_ne!(certificate.model_root, 0);
    }
}
