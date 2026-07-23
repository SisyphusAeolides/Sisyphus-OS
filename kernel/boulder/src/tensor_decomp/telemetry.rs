//! Structured kernel telemetry tensor.
//!
//! Axis 0: retained time window.
//! Axis 1: subsystem family.
//! Axis 2: metric family.
//!
//! The default shape is 8 × 8 × 8 = 512 entries.  Interrupt and scheduler
//! paths only write one bounded time slice.  Decomposition runs elsewhere.

use super::fixed;
use super::tensor::{DenseTensor, MAX_ORDER, TensorError, TensorShape};

pub const TIME_SLOTS: usize = 8;
pub const SUBSYSTEMS: usize = 8;
pub const METRICS: usize = 8;
pub const OBSERVATION_LIMIT_Q24: i64 = 16 * fixed::ONE;

pub mod subsystem {
    pub const CONTROL: usize = 0;
    pub const HODGE: usize = 1;
    pub const TOPOLOGY: usize = 2;
    pub const TROPICAL: usize = 3;
    pub const REWRITE: usize = 4;
    pub const CEILINGS: usize = 5;
    pub const MIGRATION: usize = 6;
    pub const EXTERNAL: usize = 7;
}

pub mod metric {
    pub const LEVEL: usize = 0;
    pub const RATE: usize = 1;
    pub const SPREAD: usize = 2;
    pub const OBSTRUCTION: usize = 3;
    pub const CONNECTIVITY: usize = 4;
    pub const CONCENTRATION: usize = 5;
    pub const EVENT: usize = 6;
    pub const AUXILIARY: usize = 7;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TelemetrySnapshot {
    pub epoch: u64,
    pub active_slot: u8,
    pub occupied_slots: u8,
    pub tensor_root: u64,
}

pub struct KernelTelemetryTensor {
    tensor: DenseTensor,
    active_slot: usize,
    occupied_mask: u8,
    epoch: u64,
    secret: u64,
}

impl KernelTelemetryTensor {
    pub fn new(secret: u64) -> Result<Self, TensorError> {
        if secret == 0 {
            return Err(TensorError::ZeroSecret);
        }

        let shape = TensorShape::new(3, [TIME_SLOTS as u8, SUBSYSTEMS as u8, METRICS as u8, 0])?;

        Ok(Self {
            tensor: DenseTensor::zeros(shape),
            active_slot: 0,
            occupied_mask: 0,
            epoch: 0,
            secret,
        })
    }

    pub fn tensor(&self) -> &DenseTensor {
        &self.tensor
    }

    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    pub const fn occupied_slots(&self) -> u8 {
        self.occupied_mask.count_ones() as u8
    }

    pub fn begin_epoch(&mut self, epoch: u64) -> Result<(), TensorError> {
        let slot = epoch as usize % TIME_SLOTS;
        self.clear_slot(slot)?;
        self.active_slot = slot;
        self.occupied_mask |= 1_u8 << slot;
        self.epoch = epoch;
        Ok(())
    }

    pub fn record_q24(
        &mut self,
        subsystem: usize,
        metric: usize,
        value_q24: i64,
    ) -> Result<(), TensorError> {
        if subsystem >= SUBSYSTEMS || metric >= METRICS {
            return Err(TensorError::Coordinate);
        }

        self.tensor.set(
            &[self.active_slot, subsystem, metric, 0],
            value_q24.clamp(-OBSERVATION_LIMIT_Q24, OBSERVATION_LIMIT_Q24),
        )
    }

    pub fn add_q24(
        &mut self,
        subsystem: usize,
        metric: usize,
        value_q24: i64,
    ) -> Result<(), TensorError> {
        if subsystem >= SUBSYSTEMS || metric >= METRICS {
            return Err(TensorError::Coordinate);
        }

        let coordinates = [self.active_slot, subsystem, metric, 0];
        let current = self.tensor.get(&coordinates)?;
        let next = current
            .checked_add(value_q24)
            .ok_or(TensorError::Arithmetic)?
            .clamp(-OBSERVATION_LIMIT_Q24, OBSERVATION_LIMIT_Q24);
        self.tensor.set(&coordinates, next)
    }

    pub fn observe_manifold(
        &mut self,
        actuation: &crate::manifold_orchestrator::Actuation,
    ) -> Result<TelemetrySnapshot, TensorError> {
        self.begin_epoch(actuation.epoch)?;

        self.record_q24(
            subsystem::CONTROL,
            metric::LEVEL,
            unit_ratio(u64::from(actuation.fair_class), 16)?,
        )?;
        self.record_q24(
            subsystem::CONTROL,
            metric::EVENT,
            unit_ratio(
                if actuation.mutated_node != u16::MAX {
                    1
                } else {
                    0
                },
                1,
            )?,
        )?;
        self.record_q24(
            subsystem::CONTROL,
            metric::RATE,
            unit_ratio(actuation.epoch & 0xff, 255)?,
        )?;

        self.record_q24(
            subsystem::HODGE,
            metric::LEVEL,
            log_unit_u64(actuation.energy0)?,
        )?;

        let migration = migration_statistics(actuation)?;
        self.record_q24(subsystem::HODGE, metric::RATE, migration.mean_absolute_q24)?;
        self.record_q24(
            subsystem::HODGE,
            metric::SPREAD,
            migration.maximum_absolute_q24,
        )?;

        self.record_q24(
            subsystem::TOPOLOGY,
            metric::CONNECTIVITY,
            q16_to_q24(actuation.fiedler_value_fp)?,
        )?;
        self.record_q24(
            subsystem::TOPOLOGY,
            metric::CONCENTRATION,
            unit_ratio(actuation.fiedler_mask.count_ones() as u64, 32)?,
        )?;
        self.record_q24(
            subsystem::TOPOLOGY,
            metric::OBSTRUCTION,
            unit_ratio(u64::from(actuation.cech_h1_dim), 16)?,
        )?;
        self.record_q24(
            subsystem::TOPOLOGY,
            metric::EVENT,
            if actuation.cech_obstructed {
                fixed::ONE
            } else {
                0
            },
        )?;

        self.record_q24(
            subsystem::TROPICAL,
            metric::LEVEL,
            q16_to_q24(actuation.tropical_length_fp)?,
        )?;
        self.record_q24(
            subsystem::TROPICAL,
            metric::CONCENTRATION,
            unit_ratio(u64::from(actuation.tropical_chain_len), 8)?,
        )?;
        self.record_q24(
            subsystem::TROPICAL,
            metric::EVENT,
            if actuation.mutated_node == u16::MAX {
                0
            } else {
                fixed::ONE
            },
        )?;

        let before = u64::from(actuation.zx_edges_before);
        let after = u64::from(actuation.zx_edges_after);
        self.record_q24(subsystem::REWRITE, metric::LEVEL, unit_ratio(before, 64)?)?;
        self.record_q24(subsystem::REWRITE, metric::RATE, unit_ratio(after, 64)?)?;
        self.record_q24(
            subsystem::REWRITE,
            metric::CONCENTRATION,
            unit_ratio(before.saturating_sub(after), before.max(1))?,
        )?;

        let ceilings = ceiling_statistics(actuation)?;
        self.record_q24(subsystem::CEILINGS, metric::LEVEL, ceilings.mean_q24)?;
        self.record_q24(subsystem::CEILINGS, metric::SPREAD, ceilings.spread_q24)?;
        self.record_q24(
            subsystem::CEILINGS,
            metric::CONCENTRATION,
            ceilings.maximum_q24,
        )?;

        self.record_q24(subsystem::MIGRATION, metric::LEVEL, migration.positive_q24)?;
        self.record_q24(subsystem::MIGRATION, metric::RATE, migration.negative_q24)?;
        self.record_q24(
            subsystem::MIGRATION,
            metric::SPREAD,
            migration.mean_absolute_q24,
        )?;
        self.record_q24(
            subsystem::MIGRATION,
            metric::CONCENTRATION,
            migration.maximum_absolute_q24,
        )?;

        self.snapshot()
    }

    pub fn snapshot(&self) -> Result<TelemetrySnapshot, TensorError> {
        Ok(TelemetrySnapshot {
            epoch: self.epoch,
            active_slot: self.active_slot as u8,
            occupied_slots: self.occupied_slots(),
            tensor_root: self.tensor.root(self.secret)?,
        })
    }

    pub fn copy_chronological_into(&self, output: &mut DenseTensor) -> Result<(), TensorError> {
        if !output.shape().same_geometry(self.tensor.shape()) {
            return Err(TensorError::ShapeMismatch);
        }

        output.clear();
        let oldest = (self.active_slot + 1) % TIME_SLOTS;

        for logical_time in 0..TIME_SLOTS {
            let physical_time = (oldest + logical_time) % TIME_SLOTS;
            if self.occupied_mask & (1_u8 << physical_time) == 0 {
                continue;
            }

            for subsystem in 0..SUBSYSTEMS {
                for metric in 0..METRICS {
                    let value = self.tensor.get(&[physical_time, subsystem, metric, 0])?;
                    output.set(&[logical_time, subsystem, metric, 0], value)?;
                }
            }
        }

        Ok(())
    }

    fn clear_slot(&mut self, slot: usize) -> Result<(), TensorError> {
        for subsystem in 0..SUBSYSTEMS {
            for metric in 0..METRICS {
                self.tensor.set(&[slot, subsystem, metric, 0], 0)?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MigrationStatistics {
    positive_q24: i64,
    negative_q24: i64,
    mean_absolute_q24: i64,
    maximum_absolute_q24: i64,
}

fn migration_statistics(
    actuation: &crate::manifold_orchestrator::Actuation,
) -> Result<MigrationStatistics, TensorError> {
    let length = (actuation.n_migrate as usize).min(actuation.migrate.len());
    if length == 0 {
        return Ok(MigrationStatistics {
            positive_q24: 0,
            negative_q24: 0,
            mean_absolute_q24: 0,
            maximum_absolute_q24: 0,
        });
    }

    let mut positive = 0_i64;
    let mut negative = 0_i64;
    let mut absolute = 0_i64;
    let mut maximum = 0_i64;

    for value in actuation.migrate[..length].iter().copied() {
        let value_q24 = q16_to_q24(value)?;
        if value_q24 >= 0 {
            positive = positive
                .checked_add(value_q24)
                .ok_or(TensorError::Arithmetic)?;
        } else {
            negative = negative
                .checked_add(value_q24.checked_neg().ok_or(TensorError::Arithmetic)?)
                .ok_or(TensorError::Arithmetic)?;
        }

        let magnitude = value_q24.checked_abs().ok_or(TensorError::Arithmetic)?;
        absolute = absolute
            .checked_add(magnitude)
            .ok_or(TensorError::Arithmetic)?;
        maximum = maximum.max(magnitude);
    }

    Ok(MigrationStatistics {
        positive_q24: positive / length as i64,
        negative_q24: negative / length as i64,
        mean_absolute_q24: absolute / length as i64,
        maximum_absolute_q24: maximum,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CeilingStatistics {
    mean_q24: i64,
    spread_q24: i64,
    maximum_q24: i64,
}

fn ceiling_statistics(
    actuation: &crate::manifold_orchestrator::Actuation,
) -> Result<CeilingStatistics, TensorError> {
    let length = (actuation.n_ceilings as usize).min(actuation.ceilings.len());
    if length == 0 {
        return Ok(CeilingStatistics {
            mean_q24: 0,
            spread_q24: 0,
            maximum_q24: 0,
        });
    }

    let mut sum = 0_i64;
    let mut minimum = i64::MAX;
    let mut maximum = i64::MIN;

    for value in actuation.ceilings[..length].iter().copied() {
        let value_q24 = i64::from(value)
            .checked_shl(8)
            .ok_or(TensorError::Arithmetic)?;
        sum = sum.checked_add(value_q24).ok_or(TensorError::Arithmetic)?;
        minimum = minimum.min(value_q24);
        maximum = maximum.max(value_q24);
    }

    Ok(CeilingStatistics {
        mean_q24: sum / length as i64,
        spread_q24: maximum
            .checked_sub(minimum)
            .ok_or(TensorError::Arithmetic)?,
        maximum_q24: maximum,
    })
}

fn q16_to_q24(value_q16: i32) -> Result<i64, TensorError> {
    i64::from(value_q16)
        .checked_shl(8)
        .ok_or(TensorError::Arithmetic)
}

fn unit_ratio(numerator: u64, denominator: u64) -> Result<i64, TensorError> {
    if denominator == 0 {
        return Err(TensorError::Arithmetic);
    }
    let bounded = numerator.min(denominator);
    let scaled = (bounded as u128)
        .checked_shl(fixed::FRACTION_BITS)
        .ok_or(TensorError::Arithmetic)?
        / denominator as u128;
    i64::try_from(scaled).map_err(|_| TensorError::Arithmetic)
}

fn log_unit_u64(value: u64) -> Result<i64, TensorError> {
    if value == 0 {
        return Ok(0);
    }

    let integer = 63_u32 - value.leading_zeros();
    let base = 1_u64 << integer;
    let remainder = value.saturating_sub(base);
    let fraction = if base == 0 {
        0
    } else {
        ((remainder as u128)
            .checked_shl(fixed::FRACTION_BITS)
            .ok_or(TensorError::Arithmetic)?
            / base as u128)
            .min(fixed::ONE as u128) as i64
    };

    let log_q24 = i64::from(integer)
        .checked_mul(fixed::ONE)
        .and_then(|value| value.checked_add(fraction))
        .ok_or(TensorError::Arithmetic)?;

    fixed::div(log_q24, 64 * fixed::ONE).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chronological_copy_starts_after_the_active_slot() {
        let mut telemetry = KernelTelemetryTensor::new(11).unwrap();

        for epoch in 0_u64..=8 {
            telemetry.begin_epoch(epoch).unwrap();
            telemetry
                .record_q24(subsystem::CONTROL, metric::LEVEL, epoch as i64 * fixed::ONE)
                .unwrap();
        }

        let mut ordered = DenseTensor::zeros(telemetry.tensor().shape());
        telemetry.copy_chronological_into(&mut ordered).unwrap();

        assert_eq!(
            ordered
                .get(&[0, subsystem::CONTROL, metric::LEVEL, 0,])
                .unwrap(),
            fixed::ONE
        );
        assert_eq!(
            ordered
                .get(&[TIME_SLOTS - 1, subsystem::CONTROL, metric::LEVEL, 0,])
                .unwrap(),
            8 * fixed::ONE
        );
    }

    #[test]
    fn time_ring_overwrites_one_slice_only() {
        let mut telemetry = KernelTelemetryTensor::new(7).unwrap();
        telemetry.begin_epoch(1).unwrap();
        telemetry.record_q24(2, 3, fixed::ONE).unwrap();

        telemetry.begin_epoch(2).unwrap();
        assert_eq!(telemetry.tensor().get(&[1, 2, 3, 0]).unwrap(), fixed::ONE);
        assert_eq!(telemetry.tensor().get(&[2, 2, 3, 0]).unwrap(), 0);
    }
}
