//! Checked fixed-point arithmetic used by the tensor kernels.

pub const FRACTION_BITS: u32 = 24;
pub const ONE: i64 = 1_i64 << FRACTION_BITS;
pub const HALF: i64 = ONE / 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FixedError {
    Overflow,
    DivisionByZero,
    NegativeSquareRoot,
}

#[inline]
pub fn mul(left: i64, right: i64) -> Result<i64, FixedError> {
    let product = (left as i128)
        .checked_mul(right as i128)
        .ok_or(FixedError::Overflow)?
        >> FRACTION_BITS;
    i64::try_from(product).map_err(|_| FixedError::Overflow)
}

#[inline]
pub fn div(numerator: i64, denominator: i64) -> Result<i64, FixedError> {
    if denominator == 0 {
        return Err(FixedError::DivisionByZero);
    }
    let quotient = (numerator as i128)
        .checked_shl(FRACTION_BITS)
        .ok_or(FixedError::Overflow)?
        / denominator as i128;
    i64::try_from(quotient).map_err(|_| FixedError::Overflow)
}

#[inline]
pub fn square(value: i64) -> Result<i64, FixedError> {
    mul(value, value)
}

pub fn sqrt(value: i64) -> Result<i64, FixedError> {
    if value < 0 {
        return Err(FixedError::NegativeSquareRoot);
    }
    if value == 0 {
        return Ok(0);
    }

    let radicand = (value as u128)
        .checked_shl(FRACTION_BITS)
        .ok_or(FixedError::Overflow)?;
    let root = integer_sqrt(radicand);
    i64::try_from(root).map_err(|_| FixedError::Overflow)
}

pub fn reciprocal(value: i64) -> Result<i64, FixedError> {
    div(ONE, value)
}

pub fn abs(value: i64) -> Result<i64, FixedError> {
    value.checked_abs().ok_or(FixedError::Overflow)
}

pub fn from_integer(value: i64) -> Result<i64, FixedError> {
    value.checked_shl(FRACTION_BITS).ok_or(FixedError::Overflow)
}

pub fn ratio_u128(numerator: u128, denominator: u128) -> Result<i64, FixedError> {
    if denominator == 0 {
        return Err(FixedError::DivisionByZero);
    }
    let scaled = numerator
        .checked_shl(FRACTION_BITS)
        .ok_or(FixedError::Overflow)?
        / denominator;
    i64::try_from(scaled).map_err(|_| FixedError::Overflow)
}

pub fn integer_sqrt(value: u128) -> u128 {
    if value < 2 {
        return value;
    }

    let significant_bits = 128_u32 - value.leading_zeros();
    let mut current = 1_u128 << significant_bits.div_ceil(2);

    loop {
        let next = (current + value / current) / 2;
        if next >= current {
            return current;
        }
        current = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn square_root_round_trip() {
        let four = from_integer(4).unwrap();
        let two = from_integer(2).unwrap();
        assert_eq!(sqrt(four).unwrap(), two);
    }

    #[test]
    fn multiply_and_divide_round_trip() {
        let three = from_integer(3).unwrap();
        let five = from_integer(5).unwrap();
        let product = mul(three, five).unwrap();
        assert_eq!(div(product, five).unwrap(), three);
    }
}
