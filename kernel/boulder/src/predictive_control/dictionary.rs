//! Lifted Koopman dictionary.
//!
//! The predictor is linear in this feature space while retaining nonlinear
//! state interactions.

use super::state::{ControlState, STATE_DIMENSION};
use crate::tensor_decomp::TensorError;
use crate::tensor_decomp::fixed;

pub const FEATURE_DIMENSION: usize = 20;
pub const REGRESSOR_DIMENSION: usize = FEATURE_DIMENSION + 1;
pub const CONTROL_INDEX: usize = FEATURE_DIMENSION;
pub const FEATURE_LIMIT_Q24: i64 = 4 * fixed::ONE;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LiftedFeatures {
    pub values_q24: [i64; FEATURE_DIMENSION],
}

impl LiftedFeatures {
    pub const ZERO: Self = Self {
        values_q24: [0; FEATURE_DIMENSION],
    };

    pub fn lift(state: &ControlState) -> Result<Self, TensorError> {
        let x = state.values_q24;
        let mut features = [0_i64; FEATURE_DIMENSION];

        features[0] = fixed::ONE;
        features[1..1 + STATE_DIMENSION].copy_from_slice(&x);

        for index in 0..STATE_DIMENSION {
            features[1 + STATE_DIMENSION + index] =
                fixed::mul(x[index], x[index])?.clamp(0, FEATURE_LIMIT_Q24);
        }

        features[17] = fixed::mul(x[0], x[7])?.clamp(-FEATURE_LIMIT_Q24, FEATURE_LIMIT_Q24);
        features[18] = fixed::mul(x[1], x[3])?.clamp(-FEATURE_LIMIT_Q24, FEATURE_LIMIT_Q24);
        features[19] = fixed::mul(x[4], x[5])?.clamp(-FEATURE_LIMIT_Q24, FEATURE_LIMIT_Q24);

        Ok(Self {
            values_q24: features,
        })
    }

    pub fn regressor(self, control_q24: i64) -> [i64; REGRESSOR_DIMENSION] {
        let mut regressor = [0_i64; REGRESSOR_DIMENSION];
        regressor[..FEATURE_DIMENSION].copy_from_slice(&self.values_q24);
        regressor[CONTROL_INDEX] = control_q24.clamp(0, fixed::ONE);
        regressor
    }
}

#[cfg(test)]
mod tests {
    use super::super::state::ControlState;
    use super::*;

    #[test]
    fn dictionary_contains_linear_and_quadratic_terms() {
        let state = ControlState::with_values(1, [fixed::ONE / 2; STATE_DIMENSION], 0, 7).unwrap();
        let features = LiftedFeatures::lift(&state).unwrap();

        assert_eq!(features.values_q24[0], fixed::ONE);
        assert_eq!(features.values_q24[1 + STATE_DIMENSION], fixed::ONE / 4);
    }
}
