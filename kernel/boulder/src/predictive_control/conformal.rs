//! Sliding split-conformal residual calibration.
//!
//! The calibrator stores a bounded ring of observed one-step maximum residuals.
//! Its order-statistic radius is distribution-free under exchangeability and
//! is used as a robust uncertainty envelope by every barrier constraint.

use super::state::mix;
use crate::tensor_decomp::TensorError;
use crate::tensor_decomp::fixed;

pub const MAXIMUM_RESIDUAL_SCORES: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConformalConfig {
    pub minimum_samples: u8,
    pub coverage_numerator: u8,
    pub coverage_denominator: u8,
    pub additive_slack_q24: i64,
    pub maximum_radius_q24: i64,
}

impl ConformalConfig {
    pub const KERNEL_DEFAULT: Self = Self {
        minimum_samples: 8,
        coverage_numerator: 19,
        coverage_denominator: 20,
        additive_slack_q24: fixed::ONE / 1024,
        maximum_radius_q24: fixed::ONE,
    };

    fn validate(self) -> Result<(), TensorError> {
        if self.minimum_samples == 0
            || self.minimum_samples as usize > MAXIMUM_RESIDUAL_SCORES
            || self.coverage_denominator == 0
            || self.coverage_numerator == 0
            || self.coverage_numerator >= self.coverage_denominator
            || self.additive_slack_q24 < 0
            || self.maximum_radius_q24 <= 0
        {
            return Err(TensorError::InvalidDimension);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConformalCertificate {
    pub samples: u64,
    pub retained: u8,
    pub quantile_index: u8,
    pub empirical_quantile_q24: i64,
    pub radius_q24: i64,
    pub score_root: u64,
    pub root: u64,
}

impl ConformalCertificate {
    pub const EMPTY: Self = Self {
        samples: 0,
        retained: 0,
        quantile_index: 0,
        empirical_quantile_q24: 0,
        radius_q24: 0,
        score_root: 0,
        root: 0,
    };

    pub fn verify(&self, secret: u64, maximum_radius_q24: i64) -> bool {
        self.retained != 0
            && self.radius_q24 >= self.empirical_quantile_q24
            && self.radius_q24 <= maximum_radius_q24
            && self.root == certificate_root(secret, self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConformalCalibrator {
    scores_q24: [i64; MAXIMUM_RESIDUAL_SCORES],
    length: usize,
    cursor: usize,
    samples: u64,
    config: ConformalConfig,
    root: u64,
}

impl ConformalCalibrator {
    pub fn new(config: ConformalConfig, secret: u64) -> Result<Self, TensorError> {
        config.validate()?;
        if secret == 0 {
            return Err(TensorError::ZeroSecret);
        }

        let mut calibrator = Self {
            scores_q24: [0; MAXIMUM_RESIDUAL_SCORES],
            length: 0,
            cursor: 0,
            samples: 0,
            config,
            root: 0,
        };
        calibrator.seal(secret);
        Ok(calibrator)
    }

    pub const fn samples(&self) -> u64 {
        self.samples
    }

    pub const fn ready(&self) -> bool {
        self.length >= self.config.minimum_samples as usize
    }

    pub const fn root(&self) -> u64 {
        self.root
    }

    pub fn push(
        &mut self,
        score_q24: u64,
        secret: u64,
    ) -> Result<ConformalCertificate, TensorError> {
        if secret == 0 {
            return Err(TensorError::ZeroSecret);
        }

        let bounded = i64::try_from(score_q24)
            .map_err(|_| TensorError::Arithmetic)?
            .clamp(0, self.config.maximum_radius_q24);

        self.scores_q24[self.cursor] = bounded;
        self.cursor = (self.cursor + 1) % MAXIMUM_RESIDUAL_SCORES;
        self.length = (self.length + 1).min(MAXIMUM_RESIDUAL_SCORES);
        self.samples = self.samples.saturating_add(1);
        self.seal(secret);
        self.certificate(secret)
    }

    pub fn certificate(&self, secret: u64) -> Result<ConformalCertificate, TensorError> {
        if secret == 0 || self.length == 0 {
            return Err(TensorError::Arithmetic);
        }

        let mut sorted = [0_i64; MAXIMUM_RESIDUAL_SCORES];
        sorted[..self.length].copy_from_slice(&self.scores_q24[..self.length]);
        sorted[..self.length].sort_unstable();

        let numerator = (self.length + 1)
            .checked_mul(self.config.coverage_numerator as usize)
            .ok_or(TensorError::Arithmetic)?;
        let denominator = self.config.coverage_denominator as usize;
        let rank = numerator
            .checked_add(denominator - 1)
            .ok_or(TensorError::Arithmetic)?
            / denominator;
        let quantile_index = rank.saturating_sub(1).min(self.length - 1);
        let empirical = sorted[quantile_index];
        let radius = empirical
            .checked_add(self.config.additive_slack_q24)
            .ok_or(TensorError::Arithmetic)?
            .min(self.config.maximum_radius_q24);

        let mut certificate = ConformalCertificate {
            samples: self.samples,
            retained: self.length as u8,
            quantile_index: quantile_index as u8,
            empirical_quantile_q24: empirical,
            radius_q24: radius,
            score_root: self.root,
            root: 0,
        };
        certificate.root = certificate_root(secret, &certificate);
        Ok(certificate)
    }

    fn seal(&mut self, secret: u64) {
        let mut root = mix(secret, self.samples);
        root = mix(root, self.length as u64);
        root = mix(root, self.cursor as u64);
        for score in &self.scores_q24[..self.length] {
            root = mix(root, *score as u64);
        }
        self.root = root;
    }
}

fn certificate_root(secret: u64, certificate: &ConformalCertificate) -> u64 {
    let mut root = mix(secret, certificate.samples);
    root = mix(
        root,
        certificate.retained as u64 | ((certificate.quantile_index as u64) << 8),
    );
    root = mix(root, certificate.empirical_quantile_q24 as u64);
    root = mix(root, certificate.radius_q24 as u64);
    mix(root, certificate.score_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantile_tracks_large_residuals() {
        let mut calibrator = ConformalCalibrator::new(ConformalConfig::KERNEL_DEFAULT, 7).unwrap();

        for index in 0..16 {
            calibrator
                .push((index as u64 + 1) * (fixed::ONE as u64 / 64), 7)
                .unwrap();
        }

        let certificate = calibrator.certificate(7).unwrap();
        assert!(certificate.radius_q24 > fixed::ONE / 8);
        assert!(certificate.verify(7, fixed::ONE));
    }
}
