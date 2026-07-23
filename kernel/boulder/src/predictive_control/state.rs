//! Bounded predictive state extracted from manifold and tensor evidence.

use crate::manifold_orchestrator::Actuation;
use crate::tensor_decomp::fixed;
use crate::tensor_decomp::{MultilinearDirective, TensorError};

pub const STATE_DIMENSION: usize = 8;
pub const STATE_LIMIT_Q24: i64 = 2 * fixed::ONE;

pub mod coordinate {
    pub const QUEUE_CLASS: usize = 0;
    pub const HODGE_ENERGY: usize = 1;
    pub const CEILING_PRESSURE: usize = 2;
    pub const MIGRATION_PRESSURE: usize = 3;
    pub const CONNECTIVITY: usize = 4;
    pub const OBSTRUCTION: usize = 5;
    pub const TROPICAL_PRESSURE: usize = 6;
    pub const TENSOR_ANOMALY: usize = 7;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlState {
    pub epoch: u64,
    pub values_q24: [i64; STATE_DIMENSION],
    pub queue_class: u8,
    pub root: u64,
}

impl ControlState {
    pub const EMPTY: Self = Self {
        epoch: 0,
        values_q24: [0; STATE_DIMENSION],
        queue_class: 0,
        root: 0,
    };

    pub fn from_sources(
        actuation: &Actuation,
        tensor: Option<&MultilinearDirective>,
        secret: u64,
    ) -> Result<Self, TensorError> {
        if secret == 0 {
            return Err(TensorError::ZeroSecret);
        }

        let queue_class = tensor
            .map(|directive| directive.queue_class)
            .unwrap_or_else(|| actuation.fair_class.min(u8::MAX as u16) as u8);

        let mut values = [0_i64; STATE_DIMENSION];
        values[coordinate::QUEUE_CLASS] = unit_ratio(u64::from(queue_class), 63)?;
        values[coordinate::HODGE_ENERGY] = normalized_log2(actuation.energy0)?;
        values[coordinate::CEILING_PRESSURE] = ceiling_pressure(actuation)?;
        values[coordinate::MIGRATION_PRESSURE] = migration_pressure(actuation)?;
        values[coordinate::CONNECTIVITY] =
            q16_to_q24(actuation.fiedler_value_fp)?.clamp(0, fixed::ONE);
        values[coordinate::OBSTRUCTION] = if actuation.cech_obstructed {
            fixed::ONE
        } else {
            unit_ratio(u64::from(actuation.cech_h1_dim), 16)?
        };
        values[coordinate::TROPICAL_PRESSURE] =
            normalized_q16_abs(actuation.tropical_length_fp, 16)?;
        values[coordinate::TENSOR_ANOMALY] = tensor
            .map(|directive| directive.anomaly_q24.clamp(0, fixed::ONE))
            .unwrap_or(0);

        let mut state = Self {
            epoch: actuation.epoch,
            values_q24: values,
            queue_class,
            root: 0,
        };
        state.root = state_root(secret, &state);
        Ok(state)
    }

    pub fn with_values(
        epoch: u64,
        values_q24: [i64; STATE_DIMENSION],
        queue_class: u8,
        secret: u64,
    ) -> Result<Self, TensorError> {
        if secret == 0 {
            return Err(TensorError::ZeroSecret);
        }

        let mut bounded = values_q24;
        for value in &mut bounded {
            *value = (*value).clamp(-STATE_LIMIT_Q24, STATE_LIMIT_Q24);
        }

        let mut state = Self {
            epoch,
            values_q24: bounded,
            queue_class,
            root: 0,
        };
        state.root = state_root(secret, &state);
        Ok(state)
    }

    pub fn verify(&self, secret: u64) -> bool {
        self.root == state_root(secret, self)
    }
}

fn ceiling_pressure(actuation: &Actuation) -> Result<i64, TensorError> {
    let length = (actuation.n_ceilings as usize).min(actuation.ceilings.len());
    if length == 0 {
        return Ok(0);
    }

    let mut maximum = 0_i64;
    for value in actuation.ceilings[..length].iter().copied() {
        maximum = maximum.max(i64::from(value).max(0));
    }

    let q24 = maximum.checked_shl(8).ok_or(TensorError::Arithmetic)?;
    fixed::div(q24, 16 * fixed::ONE).map_err(Into::into)
}

fn migration_pressure(actuation: &Actuation) -> Result<i64, TensorError> {
    let length = (actuation.n_migrate as usize).min(actuation.migrate.len());
    if length == 0 {
        return Ok(0);
    }

    let mut sum = 0_i64;
    for value in actuation.migrate[..length].iter().copied() {
        let magnitude = i64::from(value)
            .checked_abs()
            .ok_or(TensorError::Arithmetic)?;
        sum = sum.checked_add(magnitude).ok_or(TensorError::Arithmetic)?;
    }

    let mean_q16 = sum / length as i64;
    let mean_q24 = mean_q16.checked_shl(8).ok_or(TensorError::Arithmetic)?;
    fixed::div(mean_q24, 16 * fixed::ONE)
        .map(|value| value.clamp(0, fixed::ONE))
        .map_err(Into::into)
}

fn normalized_q16_abs(value_q16: i32, scale: i64) -> Result<i64, TensorError> {
    let magnitude = i64::from(value_q16)
        .checked_abs()
        .ok_or(TensorError::Arithmetic)?;
    let q24 = magnitude.checked_shl(8).ok_or(TensorError::Arithmetic)?;
    fixed::div(q24, scale * fixed::ONE)
        .map(|value| value.clamp(0, fixed::ONE))
        .map_err(Into::into)
}

fn q16_to_q24(value_q16: i32) -> Result<i64, TensorError> {
    i64::from(value_q16)
        .checked_shl(8)
        .ok_or(TensorError::Arithmetic)
}

fn normalized_log2(value: u64) -> Result<i64, TensorError> {
    if value == 0 {
        return Ok(0);
    }

    let integer = 63_u32 - value.leading_zeros();
    let base = 1_u64 << integer;
    let remainder = value.saturating_sub(base);
    let fraction = ((remainder as u128)
        .checked_shl(fixed::FRACTION_BITS)
        .ok_or(TensorError::Arithmetic)?
        / base as u128)
        .min(fixed::ONE as u128) as i64;
    let log_q24 = i64::from(integer)
        .checked_mul(fixed::ONE)
        .and_then(|term| term.checked_add(fraction))
        .ok_or(TensorError::Arithmetic)?;
    fixed::div(log_q24, 64 * fixed::ONE)
        .map(|normalized| normalized.clamp(0, fixed::ONE))
        .map_err(Into::into)
}

fn unit_ratio(numerator: u64, denominator: u64) -> Result<i64, TensorError> {
    if denominator == 0 {
        return Err(TensorError::Arithmetic);
    }

    let bounded = numerator.min(denominator);
    let value = (bounded as u128)
        .checked_shl(fixed::FRACTION_BITS)
        .ok_or(TensorError::Arithmetic)?
        / denominator as u128;
    i64::try_from(value).map_err(|_| TensorError::Arithmetic)
}

pub(crate) fn mix(mut state: u64, word: u64) -> u64 {
    state ^= word.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    state ^= state >> 30;
    state = state.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    state ^= state >> 27;
    state = state.wrapping_mul(0x94d0_49bb_1331_11eb);
    state ^ (state >> 31)
}

fn state_root(secret: u64, state: &ControlState) -> u64 {
    let mut root = mix(secret, state.epoch);
    root = mix(root, state.queue_class as u64);
    for value in state.values_q24 {
        root = mix(root, value as u64);
    }
    root
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_state_is_bounded_and_sealed() {
        let state = ControlState::with_values(7, [3 * fixed::ONE; STATE_DIMENSION], 2, 11).unwrap();

        assert!(
            state
                .values_q24
                .iter()
                .all(|value| *value == STATE_LIMIT_Q24)
        );
        assert!(state.verify(11));
    }
}
