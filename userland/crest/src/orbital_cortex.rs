use crate::obsidian::{Fixed, ObsidianError, fixed_hypot};

const PI: Fixed = Fixed::from_raw(205_887);
const HALF_PI: Fixed = Fixed::from_raw(102_944);
const TAU: Fixed = Fixed::from_raw(411_775);
const ONE_SIXTH: Fixed = Fixed::from_raw(10_923);
const ONE_ONE_TWENTIETH: Fixed = Fixed::from_raw(546);
const ONE_FIVE_THOUSAND_FORTIETH: Fixed = Fixed::from_raw(13);

pub struct OrbitalDashboard {
    mean_anomaly: Fixed,
    eccentricity: Fixed,
    angular_rate_per_tick: Fixed,
    orbital_velocity_kilometers_per_second: Fixed,
}

impl OrbitalDashboard {
    pub fn leo_earth() -> Result<Self, OrbitalError> {
        Ok(Self {
            mean_anomaly: Fixed::ZERO,
            eccentricity: Fixed::from_ratio(167, 10_000)?,
            angular_rate_per_tick: Fixed::from_ratio(1, 1000)?,
            orbital_velocity_kilometers_per_second: Fixed::from_ratio(766, 100)?,
        })
    }

    pub const fn mean_anomaly(&self) -> Fixed {
        self.mean_anomaly
    }

    pub const fn orbital_velocity(&self) -> Fixed {
        self.orbital_velocity_kilometers_per_second
    }

    pub fn tick_physics(&mut self, delta_ticks: u64) {
        let tau = TAU.raw() as u128;
        let delta = (self.angular_rate_per_tick.raw() as u128 * u128::from(delta_ticks)) % tau;
        let next = (self.mean_anomaly.raw() as u128 + delta) % tau;
        self.mean_anomaly = Fixed::from_raw(next as i32);
    }

    /// Solves Kepler's equation with a fixed six-iteration Newton budget.
    pub fn eccentric_anomaly(&self) -> Result<Fixed, OrbitalError> {
        let mut eccentric_anomaly = self.mean_anomaly;
        for _ in 0..6 {
            let sine = sin_fixed(eccentric_anomaly);
            let cosine = cos_fixed(eccentric_anomaly);
            let residual = eccentric_anomaly
                .saturating_sub(self.eccentricity.multiply(sine))
                .saturating_sub(self.mean_anomaly);
            let derivative = Fixed::ONE.saturating_sub(self.eccentricity.multiply(cosine));
            eccentric_anomaly = eccentric_anomaly.saturating_sub(residual.divide(derivative)?);
        }
        Ok(wrap_angle(eccentric_anomaly))
    }

    pub fn evaluate_sdf(&self, nx: Fixed, ny: Fixed) -> Result<Fixed, OrbitalError> {
        let body = fixed_hypot(nx, ny).saturating_sub(Fixed::from_ratio(3, 10)?);
        let ellipse_x = nx.divide(Fixed::ONE.saturating_add(self.eccentricity))?;
        let orbit = fixed_hypot(ellipse_x, ny).saturating_sub(Fixed::from_ratio(1, 2)?);
        let ring = orbit.abs().saturating_sub(Fixed::from_ratio(1, 100)?);

        let eccentric_anomaly = self.eccentric_anomaly()?;
        let semi_major = Fixed::from_ratio(1, 2)?;
        let sat_x =
            semi_major.multiply(cos_fixed(eccentric_anomaly).saturating_sub(self.eccentricity));
        let one_minus_e2 = Fixed::ONE
            .saturating_sub(self.eccentricity.multiply(self.eccentricity))
            .sqrt()?;
        let sat_y = semi_major
            .multiply(one_minus_e2)
            .multiply(sin_fixed(eccentric_anomaly));
        let satellite = fixed_hypot(nx.saturating_sub(sat_x), ny.saturating_sub(sat_y))
            .saturating_sub(Fixed::from_ratio(3, 100)?);
        Ok(body.min(ring.min(satellite)))
    }

    pub fn shade(&self, distance: Fixed) -> Result<[u8; 4], OrbitalError> {
        if distance < Fixed::from_ratio(5, 1000)? {
            return Ok([0, 255, 50, 255]);
        }
        let denominator = distance.saturating_add(Fixed::from_ratio(1, 1000)?);
        let glow = Fixed::from_ratio(1, 100)?
            .divide(denominator)?
            .max(Fixed::ZERO)
            .min(Fixed::ONE);
        let green = ((u64::from(glow.raw() as u32) * 255) >> 16).min(255) as u8;
        Ok([0, green, green / 5, 255])
    }
}

fn wrap_angle(angle: Fixed) -> Fixed {
    let mut raw = angle.raw() % TAU.raw();
    if raw < 0 {
        raw += TAU.raw();
    }
    Fixed::from_raw(raw)
}

fn sin_fixed(angle: Fixed) -> Fixed {
    let mut x = wrap_angle(angle);
    let mut negative = false;
    if x > PI {
        x = x.saturating_sub(PI);
        negative = true;
    }
    if x > HALF_PI {
        x = PI.saturating_sub(x);
    }
    let x2 = x.multiply(x);
    let x3 = x2.multiply(x);
    let x5 = x3.multiply(x2);
    let x7 = x5.multiply(x2);
    let value = x
        .saturating_sub(x3.multiply(ONE_SIXTH))
        .saturating_add(x5.multiply(ONE_ONE_TWENTIETH))
        .saturating_sub(x7.multiply(ONE_FIVE_THOUSAND_FORTIETH));
    if negative {
        value.saturating_neg()
    } else {
        value
    }
}

fn cos_fixed(angle: Fixed) -> Fixed {
    sin_fixed(angle.saturating_add(HALF_PI))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OrbitalError {
    Arithmetic,
}

impl From<ObsidianError> for OrbitalError {
    fn from(_: ObsidianError) -> Self {
        Self::Arithmetic
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_kepler_solver_satisfies_the_orbital_equation() {
        let mut dashboard = OrbitalDashboard::leo_earth().unwrap();
        dashboard.tick_physics(1000);
        let eccentric = dashboard.eccentric_anomaly().unwrap();
        let reconstructed =
            eccentric.saturating_sub(dashboard.eccentricity.multiply(sin_fixed(eccentric)));
        assert!(
            reconstructed
                .saturating_sub(dashboard.mean_anomaly())
                .abs()
                .raw()
                < 16
        );
    }
}
