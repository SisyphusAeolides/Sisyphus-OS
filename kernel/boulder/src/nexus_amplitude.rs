//! Fixed-point complex amplitudes used by the Nexus matrix.
//!
//! This module contains representation and arithmetic only. It owns no
//! scheduler, thermal, MMIO, quarantine, or capability state.

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct Amplitude {
    /// Real component in signed Q16.16.
    pub re: i32,
    /// Imaginary component in signed Q16.16.
    pub im: i32,
}

impl Amplitude {
    pub const ZERO: Self = Self { re: 0, im: 0 };
    pub const ONE: Self = Self { re: 1 << 16, im: 0 };
    pub const I: Self = Self { re: 0, im: 1 << 16 };

    #[inline(always)]
    pub const fn new(re: i32, im: i32) -> Self {
        Self { re, im }
    }

    #[inline(always)]
    pub fn mag_sq(self) -> u64 {
        let re = i128::from(self.re);
        let im = i128::from(self.im);
        let energy = re
            .checked_mul(re)
            .and_then(|left| im.checked_mul(im).and_then(|right| left.checked_add(right)))
            .unwrap_or(i128::from(u64::MAX) << 16);

        u64::try_from((energy >> 16).min(i128::from(u64::MAX))).unwrap_or(u64::MAX)
    }

    #[inline(always)]
    pub fn mul(self, other: Self) -> Self {
        let ar = i64::from(self.re);
        let ai = i64::from(self.im);
        let br = i64::from(other.re);
        let bi = i64::from(other.im);

        let real = ar.saturating_mul(br).saturating_sub(ai.saturating_mul(bi)) >> 16;
        let imaginary = ar.saturating_mul(bi).saturating_add(ai.saturating_mul(br)) >> 16;

        Self {
            re: real.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
            im: imaginary.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
        }
    }

    #[inline(always)]
    pub fn add(self, other: Self) -> Self {
        Self {
            re: self.re.saturating_add(other.re),
            im: self.im.saturating_add(other.im),
        }
    }

    #[inline(always)]
    pub fn rotate_bin(self, bin: u8) -> Self {
        crate::phase_rotor::rotate(self, bin)
    }

    #[inline(always)]
    pub fn collapse_if_weak(self, threshold: u64) -> Self {
        if self.mag_sq() < threshold {
            Self::ZERO
        } else {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complex_unit_product_is_imaginary_unit() {
        assert_eq!(Amplitude::ONE.mul(Amplitude::I), Amplitude::I);
    }

    #[test]
    fn magnitude_is_monotonic_for_axis_aligned_values() {
        let small = Amplitude::new(1 << 14, 0);
        let large = Amplitude::new(1 << 15, 0);
        assert!(small.mag_sq() < large.mag_sq());
    }

    #[test]
    fn collapse_is_threshold_gated() {
        assert_eq!(
            Amplitude::new(1, 1).collapse_if_weak(1_000),
            Amplitude::ZERO,
        );
        assert_eq!(Amplitude::ONE.collapse_if_weak(1), Amplitude::ONE,);
    }
}
