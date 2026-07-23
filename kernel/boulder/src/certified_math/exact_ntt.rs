//! Exact fixed-size spectral convolution for bounded kernel control loops.
//!
//! Two NTT-friendly prime fields are combined with the Chinese remainder
//! theorem.  When the checked coefficient bound is below P0*P1, the returned
//! convolution is the exact integer convolution rather than a modular residue.

pub const NTT_LENGTH: usize = 64;
pub const MODULUS_0: u64 = 998_244_353;
pub const MODULUS_1: u64 = 1_004_535_809;
pub const PRIMITIVE_ROOT_0: u64 = 3;
pub const PRIMITIVE_ROOT_1: u64 = 3;
pub const CRT_MODULUS: u128 = MODULUS_0 as u128 * MODULUS_1 as u128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExactNttError {
    CoefficientBound,
    Arithmetic,
    InvalidClassCount,
    ZeroSecret,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Modulus {
    value: u64,
    primitive_root: u64,
}

const MOD_0: Modulus = Modulus {
    value: MODULUS_0,
    primitive_root: PRIMITIVE_ROOT_0,
};

const MOD_1: Modulus = Modulus {
    value: MODULUS_1,
    primitive_root: PRIMITIVE_ROOT_1,
};

#[inline]
fn add_mod(left: u64, right: u64, modulus: u64) -> u64 {
    let sum = left + right;
    if sum >= modulus { sum - modulus } else { sum }
}

#[inline]
fn sub_mod(left: u64, right: u64, modulus: u64) -> u64 {
    if left >= right {
        left - right
    } else {
        left + modulus - right
    }
}

#[inline]
fn mul_mod(left: u64, right: u64, modulus: u64) -> u64 {
    ((left as u128 * right as u128) % modulus as u128) as u64
}

fn pow_mod(mut base: u64, mut exponent: u64, modulus: u64) -> u64 {
    let mut result = 1_u64;
    base %= modulus;

    while exponent != 0 {
        if exponent & 1 != 0 {
            result = mul_mod(result, base, modulus);
        }
        base = mul_mod(base, base, modulus);
        exponent >>= 1;
    }

    result
}

fn inverse_mod(value: u64, modulus: u64) -> u64 {
    pow_mod(value, modulus - 2, modulus)
}

fn bit_reverse(values: &mut [u64; NTT_LENGTH]) {
    let mut index = 1_usize;
    let mut reverse = 0_usize;

    while index < NTT_LENGTH {
        let mut bit = NTT_LENGTH >> 1;
        while reverse & bit != 0 {
            reverse ^= bit;
            bit >>= 1;
        }
        reverse ^= bit;

        if index < reverse {
            values.swap(index, reverse);
        }
        index += 1;
    }
}

fn transform(values: &mut [u64; NTT_LENGTH], modulus: Modulus, inverse: bool) {
    bit_reverse(values);

    let root = pow_mod(
        modulus.primitive_root,
        (modulus.value - 1) / NTT_LENGTH as u64,
        modulus.value,
    );
    let direction_root = if inverse {
        inverse_mod(root, modulus.value)
    } else {
        root
    };

    let mut length = 2_usize;
    while length <= NTT_LENGTH {
        let stage_root = pow_mod(direction_root, (NTT_LENGTH / length) as u64, modulus.value);

        let half = length / 2;
        let mut block = 0_usize;
        while block < NTT_LENGTH {
            let mut twiddle = 1_u64;
            let mut offset = 0_usize;

            while offset < half {
                let even = values[block + offset];
                let odd = mul_mod(values[block + offset + half], twiddle, modulus.value);

                values[block + offset] = add_mod(even, odd, modulus.value);
                values[block + offset + half] = sub_mod(even, odd, modulus.value);
                twiddle = mul_mod(twiddle, stage_root, modulus.value);
                offset += 1;
            }

            block += length;
        }

        length <<= 1;
    }

    if inverse {
        let length_inverse = inverse_mod(NTT_LENGTH as u64, modulus.value);
        for value in values {
            *value = mul_mod(*value, length_inverse, modulus.value);
        }
    }
}

fn convolution_mod(
    left: &[u64; NTT_LENGTH],
    right: &[u64; NTT_LENGTH],
    modulus: Modulus,
) -> [u64; NTT_LENGTH] {
    let mut left_hat = [0_u64; NTT_LENGTH];
    let mut right_hat = [0_u64; NTT_LENGTH];

    for index in 0..NTT_LENGTH {
        left_hat[index] = left[index] % modulus.value;
        right_hat[index] = right[index] % modulus.value;
    }

    transform(&mut left_hat, modulus, false);
    transform(&mut right_hat, modulus, false);

    for index in 0..NTT_LENGTH {
        left_hat[index] = mul_mod(left_hat[index], right_hat[index], modulus.value);
    }

    transform(&mut left_hat, modulus, true);
    left_hat
}

fn coefficient_upper_bound(
    left: &[u64; NTT_LENGTH],
    right: &[u64; NTT_LENGTH],
) -> Result<u128, ExactNttError> {
    let mut left_sum = 0_u128;
    let mut right_sum = 0_u128;
    let mut left_max = 0_u64;
    let mut right_max = 0_u64;

    for value in left.iter().copied() {
        left_sum = left_sum
            .checked_add(value as u128)
            .ok_or(ExactNttError::Arithmetic)?;
        left_max = left_max.max(value);
    }

    for value in right.iter().copied() {
        right_sum = right_sum
            .checked_add(value as u128)
            .ok_or(ExactNttError::Arithmetic)?;
        right_max = right_max.max(value);
    }

    let bound_a = left_sum
        .checked_mul(right_max as u128)
        .ok_or(ExactNttError::Arithmetic)?;
    let bound_b = right_sum
        .checked_mul(left_max as u128)
        .ok_or(ExactNttError::Arithmetic)?;

    Ok(bound_a.min(bound_b))
}

fn crt_pair(residue_0: u64, residue_1: u64) -> u128 {
    let p0_mod_p1 = MODULUS_0 % MODULUS_1;
    let p0_inverse = inverse_mod(p0_mod_p1, MODULUS_1);
    let delta = if residue_1 >= residue_0 % MODULUS_1 {
        residue_1 - residue_0 % MODULUS_1
    } else {
        residue_1 + MODULUS_1 - residue_0 % MODULUS_1
    };
    let multiplier = mul_mod(delta, p0_inverse, MODULUS_1);

    residue_0 as u128 + MODULUS_0 as u128 * multiplier as u128
}

pub fn circular_convolution_exact(
    left: &[u64; NTT_LENGTH],
    right: &[u64; NTT_LENGTH],
) -> Result<[u64; NTT_LENGTH], ExactNttError> {
    let bound = coefficient_upper_bound(left, right)?;
    if bound >= CRT_MODULUS || bound > u64::MAX as u128 {
        return Err(ExactNttError::CoefficientBound);
    }

    let residue_0 = convolution_mod(left, right, MOD_0);
    let residue_1 = convolution_mod(left, right, MOD_1);
    let mut output = [0_u64; NTT_LENGTH];

    for index in 0..NTT_LENGTH {
        let value = crt_pair(residue_0[index], residue_1[index]);
        if value > bound || value > u64::MAX as u128 {
            return Err(ExactNttError::Arithmetic);
        }
        output[index] = value as u64;
    }

    Ok(output)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpectralDecision {
    pub class: u8,
    pub raw_deficit: u64,
    pub smoothed_deficit: u64,
    pub epoch: u64,
    pub root: u64,
}

impl SpectralDecision {
    pub const EMPTY: Self = Self {
        class: 0,
        raw_deficit: 0,
        smoothed_deficit: 0,
        epoch: 0,
        root: 0,
    };

    pub fn verify(&self, secret: u64) -> bool {
        self.root == decision_root(secret, self)
    }
}

pub struct ExactSpectralFairQueue {
    deficits: [u64; NTT_LENGTH],
    kernel: [u64; NTT_LENGTH],
    classes: u8,
    cursor: u8,
    epoch: u64,
    secret: u64,
}

impl ExactSpectralFairQueue {
    pub fn new(classes: u8, secret: u64) -> Result<Self, ExactNttError> {
        if classes == 0 || classes as usize > NTT_LENGTH {
            return Err(ExactNttError::InvalidClassCount);
        }
        if secret == 0 {
            return Err(ExactNttError::ZeroSecret);
        }

        let mut kernel = [0_u64; NTT_LENGTH];
        kernel[0] = 8;
        kernel[1] = 4;
        kernel[2] = 2;
        kernel[3] = 1;
        kernel[NTT_LENGTH - 1] = 4;
        kernel[NTT_LENGTH - 2] = 2;
        kernel[NTT_LENGTH - 3] = 1;

        Ok(Self {
            deficits: [0; NTT_LENGTH],
            kernel,
            classes,
            cursor: 0,
            epoch: 1,
            secret,
        })
    }

    pub fn charge(&mut self, class: usize, amount: u64) -> Result<(), ExactNttError> {
        if class >= self.classes as usize {
            return Err(ExactNttError::InvalidClassCount);
        }

        self.deficits[class] = self.deficits[class]
            .checked_add(amount)
            .ok_or(ExactNttError::Arithmetic)?;
        self.epoch = self.epoch.wrapping_add(1).max(1);
        Ok(())
    }

    pub fn credit(&mut self, class: usize, amount: u64) -> Result<(), ExactNttError> {
        if class >= self.classes as usize {
            return Err(ExactNttError::InvalidClassCount);
        }

        self.deficits[class] = self.deficits[class].saturating_sub(amount);
        self.epoch = self.epoch.wrapping_add(1).max(1);
        Ok(())
    }

    pub fn smoothed(&self) -> Result<[u64; NTT_LENGTH], ExactNttError> {
        circular_convolution_exact(&self.deficits, &self.kernel)
    }

    pub fn select(&mut self) -> Result<SpectralDecision, ExactNttError> {
        let smoothed = self.smoothed()?;
        let classes = self.classes as usize;
        let start = self.cursor as usize % classes;

        let mut selected = start;
        let mut selected_value = smoothed[start];

        for distance in 1..classes {
            let candidate = (start + distance) % classes;
            let value = smoothed[candidate];
            if value > selected_value {
                selected = candidate;
                selected_value = value;
            }
        }

        self.cursor = ((selected + 1) % classes) as u8;
        self.epoch = self.epoch.wrapping_add(1).max(1);

        let mut decision = SpectralDecision {
            class: selected as u8,
            raw_deficit: self.deficits[selected],
            smoothed_deficit: selected_value,
            epoch: self.epoch,
            root: 0,
        };
        decision.root = decision_root(self.secret, &decision);
        Ok(decision)
    }

    pub fn serve(&mut self, amount: u64) -> Result<SpectralDecision, ExactNttError> {
        let decision = self.select()?;
        self.credit(decision.class as usize, amount)?;
        Ok(decision)
    }

    pub fn deficits(&self) -> &[u64] {
        &self.deficits[..self.classes as usize]
    }
}

fn decision_root(secret: u64, decision: &SpectralDecision) -> u64 {
    let mut state = mix(secret, decision.class as u64);
    state = mix(state, decision.raw_deficit);
    state = mix(state, decision.smoothed_deficit);
    mix(state, decision.epoch)
}

fn mix(mut state: u64, word: u64) -> u64 {
    state ^= word.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    state ^= state >> 30;
    state = state.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    state ^= state >> 27;
    state = state.wrapping_mul(0x94d0_49bb_1331_11eb);
    state ^ (state >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn naive(left: &[u64; NTT_LENGTH], right: &[u64; NTT_LENGTH]) -> [u64; NTT_LENGTH] {
        let mut output = [0_u64; NTT_LENGTH];
        for i in 0..NTT_LENGTH {
            for j in 0..NTT_LENGTH {
                let index = (i + j) % NTT_LENGTH;
                output[index] += left[i] * right[j];
            }
        }
        output
    }

    #[test]
    fn exact_convolution_matches_integer_reference() {
        let mut left = [0_u64; NTT_LENGTH];
        let mut right = [0_u64; NTT_LENGTH];
        left[0] = 1_000_000;
        left[7] = 19;
        right[0] = 11;
        right[63] = 3;

        let exact = circular_convolution_exact(&left, &right).unwrap();
        assert_eq!(exact, naive(&left, &right));
    }

    #[test]
    fn queue_does_not_wrap_at_a_small_field_modulus() {
        let mut queue = ExactSpectralFairQueue::new(8, 7).unwrap();
        queue.charge(5, 1_000_000).unwrap();
        queue.charge(1, 193).unwrap();
        assert_eq!(queue.select().unwrap().class, 5);
    }
}
