use crate::quantum_nexus::Amplitude;

pub const PHASE_BINS: usize = 64;
const QUADRANT_BINS: usize = PHASE_BINS / 4;

// sin(0°..90°) in 5.625° steps, Q16.16.
const SIN_Q16_QUARTER: [i32; QUADRANT_BINS + 1] = [
    0, 6_424, 12_785, 19_024, 25_080, 30_893, 36_410, 41_576, 46_341, 50_660, 54_491, 57_798,
    60_547, 62_714, 64_277, 65_220, 65_536,
];

#[inline(always)]
pub fn phasor(bin: u8) -> Amplitude {
    let bin = usize::from(bin) & (PHASE_BINS - 1);
    let quadrant = bin / QUADRANT_BINS;
    let offset = bin % QUADRANT_BINS;

    let sin = SIN_Q16_QUARTER[offset];
    let cos = SIN_Q16_QUARTER[QUADRANT_BINS - offset];

    let (re, im) = match quadrant {
        0 => (cos, sin),
        1 => (-sin, cos),
        2 => (-cos, -sin),
        _ => (sin, -cos),
    };

    Amplitude::new(re, im)
}

#[inline(always)]
pub fn rotate(amplitude: Amplitude, bin: u8) -> Amplitude {
    amplitude.mul(phasor(bin))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cardinal_phases_are_correct() {
        let zero = phasor(0);
        assert_eq!(zero.re, 65_536);
        assert_eq!(zero.im, 0);

        let quarter = phasor(16);
        assert_eq!(quarter.re, 0);
        assert_eq!(quarter.im, 65_536);

        let half = phasor(32);
        assert_eq!(half.re, -65_536);
        assert_eq!(half.im, 0);

        let three_quarters = phasor(48);
        assert_eq!(three_quarters.re, 0);
        assert_eq!(three_quarters.im, -65_536);
    }

    #[test]
    fn opposite_bins_cancel() {
        for bin in 0..32_u8 {
            let a = phasor(bin);
            let b = phasor(bin + 32);

            assert_eq!(a.re, -b.re);
            assert_eq!(a.im, -b.im);
        }
    }
}
