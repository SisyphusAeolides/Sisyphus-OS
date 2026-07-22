use crate::obsidian::{Fixed, ObsidianError, fixed_hypot};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TelemetrySnapshot {
    pub total_crashes: u64,
    pub thermal_millicelsius: u32,
}

pub struct HorizonPanel {
    telemetry: TelemetrySnapshot,
}

impl HorizonPanel {
    pub const fn new(telemetry: TelemetrySnapshot) -> Self {
        Self { telemetry }
    }

    pub fn update(&mut self, telemetry: TelemetrySnapshot) {
        self.telemetry = telemetry;
    }

    /// Fixed-point SDF for a curved upper horizon.
    pub fn evaluate_sdf(&self, nx: Fixed, ny: Fixed) -> Result<Fixed, ObsidianError> {
        let heat_celsius = self.telemetry.thermal_millicelsius.min(200_000) as i32 / 1000;
        let radius = Fixed::ONE.saturating_sub(Fixed::from_ratio(heat_celsius, 200)?);
        let center_y = Fixed::from_ratio(6, 5)?;
        let mut distance = fixed_hypot(nx, ny.saturating_sub(center_y)).saturating_sub(radius);
        if self.telemetry.total_crashes != 0 {
            distance = distance.saturating_add(fractal_noise(nx)?);
        }
        Ok(distance)
    }

    pub fn shade(&self, distance: Fixed) -> [u8; 4] {
        if distance >= Fixed::ZERO {
            return [0, 0, 0, 0];
        }
        let depth =
            ((u64::from(distance.raw().saturating_abs() as u32) * 255) >> 16).min(255) as u8;
        [10, 20, depth.saturating_add(50), 255]
    }
}

fn fractal_noise(x: Fixed) -> Result<Fixed, ObsidianError> {
    let mut bits = x.raw() as u32;
    bits ^= bits >> 13;
    bits = bits.wrapping_mul(0x5bd1_e995);
    let centered = (bits & 0xffff) as i32 - 0x8000;
    Fixed::from_ratio(centered, 0x8000)?.divide(Fixed::from_integer(20))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thermal_pressure_tightens_the_horizon_without_raw_sensor_access() {
        let cool = HorizonPanel::new(TelemetrySnapshot {
            total_crashes: 0,
            thermal_millicelsius: 40_000,
        });
        let hot = HorizonPanel::new(TelemetrySnapshot {
            total_crashes: 0,
            thermal_millicelsius: 100_000,
        });
        assert!(
            hot.evaluate_sdf(Fixed::ZERO, Fixed::ZERO).unwrap()
                > cool.evaluate_sdf(Fixed::ZERO, Fixed::ZERO).unwrap()
        );
    }
}
