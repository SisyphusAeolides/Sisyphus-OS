//! Bounded density-operator calculus for latent fault-state estimation.
//!
//! This is quantum probability mathematics, not a claim of quantum hardware.
//! A state is a positive semidefinite Hermitian matrix with unit trace.  State
//! evolution is accepted only through a trace-preserving Kraus channel.

pub const MAX_DENSITY_DIMENSION: usize = 4;
pub const MAX_KRAUS_OPERATORS: usize = 4;
pub const MATRIX_ENTRIES: usize = MAX_DENSITY_DIMENSION * MAX_DENSITY_DIMENSION;
pub const Q30_ONE: i64 = 1_i64 << 30;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DensityError {
    InvalidDimension,
    Arithmetic,
    NonHermitian,
    InvalidTrace,
    InvalidTolerance,
    NonPositive,
    IncompleteChannel,
    Capacity,
    ZeroSecret,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ComplexQ30 {
    pub re: i64,
    pub im: i64,
}

impl ComplexQ30 {
    pub const ZERO: Self = Self { re: 0, im: 0 };
    pub const ONE: Self = Self { re: Q30_ONE, im: 0 };

    pub fn checked_conjugate(self) -> Result<Self, DensityError> {
        Ok(Self {
            re: self.re,
            im: self.im.checked_neg().ok_or(DensityError::Arithmetic)?,
        })
    }

    pub fn checked_add(self, other: Self) -> Result<Self, DensityError> {
        Ok(Self {
            re: self
                .re
                .checked_add(other.re)
                .ok_or(DensityError::Arithmetic)?,
            im: self
                .im
                .checked_add(other.im)
                .ok_or(DensityError::Arithmetic)?,
        })
    }

    pub fn checked_sub(self, other: Self) -> Result<Self, DensityError> {
        Ok(Self {
            re: self
                .re
                .checked_sub(other.re)
                .ok_or(DensityError::Arithmetic)?,
            im: self
                .im
                .checked_sub(other.im)
                .ok_or(DensityError::Arithmetic)?,
        })
    }

    pub fn checked_mul(self, other: Self) -> Result<Self, DensityError> {
        let real = (self.re as i128)
            .checked_mul(other.re as i128)
            .and_then(|value| value.checked_sub((self.im as i128).checked_mul(other.im as i128)?))
            .ok_or(DensityError::Arithmetic)?
            >> 30;
        let imaginary = (self.re as i128)
            .checked_mul(other.im as i128)
            .and_then(|value| value.checked_add((self.im as i128).checked_mul(other.re as i128)?))
            .ok_or(DensityError::Arithmetic)?
            >> 30;

        Ok(Self {
            re: i64::try_from(real).map_err(|_| DensityError::Arithmetic)?,
            im: i64::try_from(imaginary).map_err(|_| DensityError::Arithmetic)?,
        })
    }

    pub fn checked_scale(self, scale_q30: i64) -> Result<Self, DensityError> {
        Ok(Self {
            re: mul_q30(self.re, scale_q30)?,
            im: mul_q30(self.im, scale_q30)?,
        })
    }

    pub fn checked_div_real(self, denominator_q30: i64) -> Result<Self, DensityError> {
        if denominator_q30 == 0 {
            return Err(DensityError::Arithmetic);
        }
        Ok(Self {
            re: div_q30(self.re, denominator_q30)?,
            im: div_q30(self.im, denominator_q30)?,
        })
    }

    pub fn norm_squared_q30(self) -> Result<i64, DensityError> {
        mul_q30(self.re, self.re)?
            .checked_add(mul_q30(self.im, self.im)?)
            .ok_or(DensityError::Arithmetic)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Operator {
    pub entries: [ComplexQ30; MATRIX_ENTRIES],
}

impl Operator {
    pub const ZERO: Self = Self {
        entries: [ComplexQ30::ZERO; MATRIX_ENTRIES],
    };

    pub fn identity(dimension: usize) -> Result<Self, DensityError> {
        validate_dimension(dimension)?;
        let mut operator = Self::ZERO;
        for index in 0..dimension {
            operator.entries[matrix_index(index, index)] = ComplexQ30::ONE;
        }
        Ok(operator)
    }

    pub fn diagonal(dimension: usize, diagonal_q30: &[i64]) -> Result<Self, DensityError> {
        validate_dimension(dimension)?;
        if diagonal_q30.len() < dimension {
            return Err(DensityError::Capacity);
        }

        let mut operator = Self::ZERO;
        for index in 0..dimension {
            operator.entries[matrix_index(index, index)] = ComplexQ30 {
                re: diagonal_q30[index],
                im: 0,
            };
        }
        Ok(operator)
    }

    pub fn adjoint(self, dimension: usize) -> Result<Self, DensityError> {
        let mut output = Self::ZERO;
        for row in 0..dimension {
            for column in 0..dimension {
                output.entries[matrix_index(row, column)] =
                    self.entries[matrix_index(column, row)].checked_conjugate()?;
            }
        }
        Ok(output)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DensityMatrix {
    pub dimension: u8,
    pub entries: [ComplexQ30; MATRIX_ENTRIES],
}

impl DensityMatrix {
    pub const EMPTY: Self = Self {
        dimension: 0,
        entries: [ComplexQ30::ZERO; MATRIX_ENTRIES],
    };

    pub fn from_diagonal(probabilities_q30: &[i64]) -> Result<Self, DensityError> {
        let dimension = probabilities_q30.len();
        validate_dimension(dimension)?;

        let mut trace = 0_i64;
        let mut state = Self {
            dimension: dimension as u8,
            entries: [ComplexQ30::ZERO; MATRIX_ENTRIES],
        };

        for (index, probability) in probabilities_q30.iter().copied().enumerate() {
            if probability < 0 {
                return Err(DensityError::NonPositive);
            }
            trace = trace
                .checked_add(probability)
                .ok_or(DensityError::Arithmetic)?;
            state.entries[matrix_index(index, index)] = ComplexQ30 {
                re: probability,
                im: 0,
            };
        }

        if trace != Q30_ONE {
            return Err(DensityError::InvalidTrace);
        }
        Ok(state)
    }

    pub fn trace_q30(self) -> Result<i64, DensityError> {
        let dimension = self.dimension as usize;
        validate_dimension(dimension)?;

        let mut trace = 0_i64;
        for index in 0..dimension {
            let diagonal = self.entries[matrix_index(index, index)];
            if diagonal.im != 0 {
                return Err(DensityError::NonHermitian);
            }
            trace = trace
                .checked_add(diagonal.re)
                .ok_or(DensityError::Arithmetic)?;
        }
        Ok(trace)
    }

    pub fn purity_q30(self) -> Result<i64, DensityError> {
        let product = matrix_multiply(
            &Operator {
                entries: self.entries,
            },
            &Operator {
                entries: self.entries,
            },
            self.dimension as usize,
        )?;

        let mut trace = 0_i64;
        for index in 0..self.dimension as usize {
            trace = trace
                .checked_add(product.entries[matrix_index(index, index)].re)
                .ok_or(DensityError::Arithmetic)?;
        }
        Ok(trace)
    }

    pub fn validate(self, tolerance_q30: i64) -> Result<DensityValidation, DensityError> {
        let dimension = self.dimension as usize;
        validate_dimension(dimension)?;
        if tolerance_q30 < 0 {
            return Err(DensityError::InvalidTolerance);
        }

        let hermitian_defect = hermitian_defect(&self.entries, dimension)?;
        if hermitian_defect > tolerance_q30 as u64 {
            return Err(DensityError::NonHermitian);
        }

        let trace = self.trace_q30()?;
        let trace_defect = trace.abs_diff(Q30_ONE);
        if trace_defect > tolerance_q30 as u64 {
            return Err(DensityError::InvalidTrace);
        }

        let minimum_pivot = minimum_ldl_pivot(&self.entries, dimension, tolerance_q30)?;
        if minimum_pivot < -tolerance_q30 {
            return Err(DensityError::NonPositive);
        }

        Ok(DensityValidation {
            trace_defect_q30: trace_defect,
            hermitian_defect_q30: hermitian_defect,
            minimum_ldl_pivot_q30: minimum_pivot,
            purity_q30: self.purity_q30()?,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DensityValidation {
    pub trace_defect_q30: u64,
    pub hermitian_defect_q30: u64,
    pub minimum_ldl_pivot_q30: i64,
    pub purity_q30: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KrausChannel {
    pub dimension: u8,
    pub operators: [Operator; MAX_KRAUS_OPERATORS],
    pub operator_count: usize,
}

impl KrausChannel {
    pub const EMPTY: Self = Self {
        dimension: 0,
        operators: [Operator::ZERO; MAX_KRAUS_OPERATORS],
        operator_count: 0,
    };

    pub fn push(&mut self, operator: Operator) -> Result<(), DensityError> {
        validate_dimension(self.dimension as usize)?;
        let destination = self
            .operators
            .get_mut(self.operator_count)
            .ok_or(DensityError::Capacity)?;
        *destination = operator;
        self.operator_count += 1;
        Ok(())
    }

    pub fn completeness_defect_q30(self) -> Result<u64, DensityError> {
        let dimension = self.dimension as usize;
        validate_dimension(dimension)?;
        if self.operator_count == 0 {
            return Err(DensityError::IncompleteChannel);
        }

        let mut accumulated = Operator::ZERO;
        for operator in self.operators[..self.operator_count].iter().copied() {
            let adjoint = operator.adjoint(dimension)?;
            let product = matrix_multiply(&adjoint, &operator, dimension)?;
            matrix_add_assign(&mut accumulated, &product, dimension)?;
        }

        let mut maximum = 0_u64;
        for row in 0..dimension {
            for column in 0..dimension {
                let expected = if row == column {
                    ComplexQ30::ONE
                } else {
                    ComplexQ30::ZERO
                };
                let difference =
                    accumulated.entries[matrix_index(row, column)].checked_sub(expected)?;
                maximum = maximum
                    .max(difference.re.unsigned_abs())
                    .max(difference.im.unsigned_abs());
            }
        }

        Ok(maximum)
    }

    pub fn apply(
        self,
        state: DensityMatrix,
        tolerance_q30: i64,
        secret: u64,
    ) -> Result<(DensityMatrix, DensityChannelCertificate), DensityError> {
        if secret == 0 {
            return Err(DensityError::ZeroSecret);
        }
        if state.dimension != self.dimension {
            return Err(DensityError::InvalidDimension);
        }

        let completeness = self.completeness_defect_q30()?;
        if completeness > tolerance_q30 as u64 {
            return Err(DensityError::IncompleteChannel);
        }

        state.validate(tolerance_q30)?;
        let dimension = self.dimension as usize;
        let state_operator = Operator {
            entries: state.entries,
        };
        let mut output = Operator::ZERO;

        for operator in self.operators[..self.operator_count].iter().copied() {
            let intermediate = matrix_multiply(&operator, &state_operator, dimension)?;
            let adjoint = operator.adjoint(dimension)?;
            let contribution = matrix_multiply(&intermediate, &adjoint, dimension)?;
            matrix_add_assign(&mut output, &contribution, dimension)?;
        }

        let next = DensityMatrix {
            dimension: self.dimension,
            entries: output.entries,
        };
        let validation = next.validate(tolerance_q30)?;

        let mut certificate = DensityChannelCertificate {
            dimension: self.dimension,
            operators: self.operator_count as u8,
            completeness_defect_q30: completeness,
            trace_defect_q30: validation.trace_defect_q30,
            hermitian_defect_q30: validation.hermitian_defect_q30,
            minimum_ldl_pivot_q30: validation.minimum_ldl_pivot_q30,
            input_purity_q30: state.purity_q30()?,
            output_purity_q30: validation.purity_q30,
            input_root: density_root(secret, &state),
            output_root: density_root(secret, &next),
            root: 0,
        };
        certificate.root = channel_certificate_root(secret, &certificate);
        Ok((next, certificate))
    }

    pub fn condition_on(
        self,
        state: DensityMatrix,
        outcome: usize,
        tolerance_q30: i64,
        secret: u64,
    ) -> Result<(DensityMatrix, DensityMeasurementCertificate), DensityError> {
        if secret == 0 {
            return Err(DensityError::ZeroSecret);
        }
        if state.dimension != self.dimension || outcome >= self.operator_count {
            return Err(DensityError::InvalidDimension);
        }

        let completeness = self.completeness_defect_q30()?;
        if completeness > tolerance_q30 as u64 {
            return Err(DensityError::IncompleteChannel);
        }
        state.validate(tolerance_q30)?;

        let dimension = self.dimension as usize;
        let measurement = self.operators[outcome];
        let state_operator = Operator {
            entries: state.entries,
        };
        let intermediate = matrix_multiply(&measurement, &state_operator, dimension)?;
        let measurement_adjoint = measurement.adjoint(dimension)?;
        let unnormalized = matrix_multiply(&intermediate, &measurement_adjoint, dimension)?;

        let mut probability_q30 = 0_i64;
        for index in 0..dimension {
            let diagonal = unnormalized.entries[matrix_index(index, index)];
            if diagonal.im.abs() > tolerance_q30 {
                return Err(DensityError::NonHermitian);
            }
            probability_q30 = probability_q30
                .checked_add(diagonal.re)
                .ok_or(DensityError::Arithmetic)?;
        }

        if probability_q30 <= tolerance_q30 {
            return Err(DensityError::InvalidTrace);
        }

        let mut normalized = DensityMatrix {
            dimension: self.dimension,
            entries: [ComplexQ30::ZERO; MATRIX_ENTRIES],
        };
        for row in 0..dimension {
            for column in 0..dimension {
                let index = matrix_index(row, column);
                normalized.entries[index] =
                    unnormalized.entries[index].checked_div_real(probability_q30)?;
            }
        }

        let validation = normalized.validate(tolerance_q30)?;
        let mut certificate = DensityMeasurementCertificate {
            dimension: self.dimension,
            outcome: outcome as u8,
            probability_q30,
            completeness_defect_q30: completeness,
            trace_defect_q30: validation.trace_defect_q30,
            hermitian_defect_q30: validation.hermitian_defect_q30,
            minimum_ldl_pivot_q30: validation.minimum_ldl_pivot_q30,
            input_root: density_root(secret, &state),
            posterior_root: density_root(secret, &normalized),
            root: 0,
        };
        certificate.root = measurement_certificate_root(secret, &certificate);

        Ok((normalized, certificate))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DensityMeasurementCertificate {
    pub dimension: u8,
    pub outcome: u8,
    pub probability_q30: i64,
    pub completeness_defect_q30: u64,
    pub trace_defect_q30: u64,
    pub hermitian_defect_q30: u64,
    pub minimum_ldl_pivot_q30: i64,
    pub input_root: u64,
    pub posterior_root: u64,
    pub root: u64,
}

impl DensityMeasurementCertificate {
    pub const EMPTY: Self = Self {
        dimension: 0,
        outcome: 0,
        probability_q30: 0,
        completeness_defect_q30: 0,
        trace_defect_q30: 0,
        hermitian_defect_q30: 0,
        minimum_ldl_pivot_q30: 0,
        input_root: 0,
        posterior_root: 0,
        root: 0,
    };

    pub fn verify(&self, secret: u64, tolerance_q30: u64) -> bool {
        self.probability_q30 > 0
            && self.probability_q30 <= Q30_ONE
            && self.completeness_defect_q30 <= tolerance_q30
            && self.trace_defect_q30 <= tolerance_q30
            && self.hermitian_defect_q30 <= tolerance_q30
            && self.minimum_ldl_pivot_q30 >= -(tolerance_q30.min(i64::MAX as u64) as i64)
            && self.root == measurement_certificate_root(secret, self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DensityChannelCertificate {
    pub dimension: u8,
    pub operators: u8,
    pub completeness_defect_q30: u64,
    pub trace_defect_q30: u64,
    pub hermitian_defect_q30: u64,
    pub minimum_ldl_pivot_q30: i64,
    pub input_purity_q30: i64,
    pub output_purity_q30: i64,
    pub input_root: u64,
    pub output_root: u64,
    pub root: u64,
}

impl DensityChannelCertificate {
    pub const EMPTY: Self = Self {
        dimension: 0,
        operators: 0,
        completeness_defect_q30: 0,
        trace_defect_q30: 0,
        hermitian_defect_q30: 0,
        minimum_ldl_pivot_q30: 0,
        input_purity_q30: 0,
        output_purity_q30: 0,
        input_root: 0,
        output_root: 0,
        root: 0,
    };

    pub fn verify(&self, secret: u64, tolerance_q30: u64) -> bool {
        self.completeness_defect_q30 <= tolerance_q30
            && self.trace_defect_q30 <= tolerance_q30
            && self.hermitian_defect_q30 <= tolerance_q30
            && self.minimum_ldl_pivot_q30 >= -(tolerance_q30.min(i64::MAX as u64) as i64)
            && self.root == channel_certificate_root(secret, self)
    }
}

fn matrix_multiply(
    left: &Operator,
    right: &Operator,
    dimension: usize,
) -> Result<Operator, DensityError> {
    let mut output = Operator::ZERO;

    for row in 0..dimension {
        for column in 0..dimension {
            let mut value = ComplexQ30::ZERO;
            for inner in 0..dimension {
                let product = left.entries[matrix_index(row, inner)]
                    .checked_mul(right.entries[matrix_index(inner, column)])?;
                value = value.checked_add(product)?;
            }
            output.entries[matrix_index(row, column)] = value;
        }
    }

    Ok(output)
}

fn matrix_add_assign(
    target: &mut Operator,
    source: &Operator,
    dimension: usize,
) -> Result<(), DensityError> {
    for row in 0..dimension {
        for column in 0..dimension {
            let index = matrix_index(row, column);
            target.entries[index] = target.entries[index].checked_add(source.entries[index])?;
        }
    }
    Ok(())
}

fn hermitian_defect(
    entries: &[ComplexQ30; MATRIX_ENTRIES],
    dimension: usize,
) -> Result<u64, DensityError> {
    let mut maximum = 0_u64;

    for row in 0..dimension {
        for column in 0..dimension {
            let left = entries[matrix_index(row, column)];
            let right = entries[matrix_index(column, row)].checked_conjugate()?;
            let difference = left.checked_sub(right)?;
            maximum = maximum
                .max(difference.re.unsigned_abs())
                .max(difference.im.unsigned_abs());
        }
    }

    Ok(maximum)
}

fn minimum_ldl_pivot(
    entries: &[ComplexQ30; MATRIX_ENTRIES],
    dimension: usize,
    tolerance_q30: i64,
) -> Result<i64, DensityError> {
    let mut lower = [ComplexQ30::ZERO; MATRIX_ENTRIES];
    let mut diagonal = [0_i64; MAX_DENSITY_DIMENSION];
    let mut minimum = i64::MAX;

    for row in 0..dimension {
        lower[matrix_index(row, row)] = ComplexQ30::ONE;
    }

    for pivot in 0..dimension {
        let mut diagonal_value = entries[matrix_index(pivot, pivot)].re;

        for previous in 0..pivot {
            let coefficient = lower[matrix_index(pivot, previous)];
            let norm = coefficient.norm_squared_q30()?;
            diagonal_value = diagonal_value
                .checked_sub(mul_q30(norm, diagonal[previous])?)
                .ok_or(DensityError::Arithmetic)?;
        }

        diagonal[pivot] = diagonal_value;
        minimum = minimum.min(diagonal_value);

        if diagonal_value < -tolerance_q30 {
            return Ok(diagonal_value);
        }

        for row in pivot + 1..dimension {
            let mut numerator = entries[matrix_index(row, pivot)];

            for previous in 0..pivot {
                let left = lower[matrix_index(row, previous)];
                let right = lower[matrix_index(pivot, previous)].checked_conjugate()?;
                let product = left.checked_mul(right)?.checked_scale(diagonal[previous])?;
                numerator = numerator.checked_sub(product)?;
            }

            if diagonal_value.abs() <= tolerance_q30 {
                if numerator.re.abs() > tolerance_q30 || numerator.im.abs() > tolerance_q30 {
                    return Err(DensityError::NonPositive);
                }
                lower[matrix_index(row, pivot)] = ComplexQ30::ZERO;
            } else {
                lower[matrix_index(row, pivot)] = numerator.checked_div_real(diagonal_value)?;
            }
        }
    }

    Ok(minimum)
}

fn mul_q30(left: i64, right: i64) -> Result<i64, DensityError> {
    let value = (left as i128)
        .checked_mul(right as i128)
        .ok_or(DensityError::Arithmetic)?
        >> 30;
    i64::try_from(value).map_err(|_| DensityError::Arithmetic)
}

fn div_q30(numerator: i64, denominator: i64) -> Result<i64, DensityError> {
    if denominator == 0 {
        return Err(DensityError::Arithmetic);
    }
    let value = (numerator as i128)
        .checked_shl(30)
        .ok_or(DensityError::Arithmetic)?
        / denominator as i128;
    i64::try_from(value).map_err(|_| DensityError::Arithmetic)
}

const fn matrix_index(row: usize, column: usize) -> usize {
    row * MAX_DENSITY_DIMENSION + column
}

fn validate_dimension(dimension: usize) -> Result<(), DensityError> {
    if dimension == 0 || dimension > MAX_DENSITY_DIMENSION {
        Err(DensityError::InvalidDimension)
    } else {
        Ok(())
    }
}

fn density_root(secret: u64, state: &DensityMatrix) -> u64 {
    let mut root = mix(secret, state.dimension as u64);
    for row in 0..state.dimension as usize {
        for column in 0..state.dimension as usize {
            let value = state.entries[matrix_index(row, column)];
            root = mix(root, value.re as u64);
            root = mix(root, value.im as u64);
        }
    }
    root
}

fn channel_certificate_root(secret: u64, certificate: &DensityChannelCertificate) -> u64 {
    let mut state = mix(
        secret,
        certificate.dimension as u64 | ((certificate.operators as u64) << 8),
    );
    state = mix(state, certificate.completeness_defect_q30);
    state = mix(state, certificate.trace_defect_q30);
    state = mix(state, certificate.hermitian_defect_q30);
    state = mix(state, certificate.minimum_ldl_pivot_q30 as u64);
    state = mix(state, certificate.input_purity_q30 as u64);
    state = mix(state, certificate.output_purity_q30 as u64);
    state = mix(state, certificate.input_root);
    mix(state, certificate.output_root)
}

fn measurement_certificate_root(secret: u64, certificate: &DensityMeasurementCertificate) -> u64 {
    let mut state = mix(
        secret,
        certificate.dimension as u64 | ((certificate.outcome as u64) << 8),
    );
    state = mix(state, certificate.probability_q30 as u64);
    state = mix(state, certificate.completeness_defect_q30);
    state = mix(state, certificate.trace_defect_q30);
    state = mix(state, certificate.hermitian_defect_q30);
    state = mix(state, certificate.minimum_ldl_pivot_q30 as u64);
    state = mix(state, certificate.input_root);
    mix(state, certificate.posterior_root)
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

    #[test]
    fn projective_measurement_conditions_the_state() {
        let state = DensityMatrix::from_diagonal(&[Q30_ONE / 2, Q30_ONE / 2]).unwrap();

        let mut instrument = KrausChannel {
            dimension: 2,
            ..KrausChannel::EMPTY
        };
        instrument
            .push(Operator::diagonal(2, &[Q30_ONE, 0]).unwrap())
            .unwrap();
        instrument
            .push(Operator::diagonal(2, &[0, Q30_ONE]).unwrap())
            .unwrap();

        let (posterior, certificate) = instrument.condition_on(state, 0, 1 << 12, 11).unwrap();
        assert!(certificate.verify(11, 1 << 12));
        assert_eq!(posterior.entries[matrix_index(0, 0)].re, Q30_ONE);
        assert_eq!(posterior.entries[matrix_index(1, 1)].re, 0);
    }

    #[test]
    fn dephasing_channel_preserves_density_invariants() {
        let state = DensityMatrix::from_diagonal(&[Q30_ONE / 2, Q30_ONE / 2]).unwrap();

        let inverse_sqrt_two = 759_250_125_i64;
        let mut channel = KrausChannel {
            dimension: 2,
            ..KrausChannel::EMPTY
        };
        channel
            .push(Operator::diagonal(2, &[inverse_sqrt_two, inverse_sqrt_two]).unwrap())
            .unwrap();
        channel
            .push(Operator::diagonal(2, &[inverse_sqrt_two, -inverse_sqrt_two]).unwrap())
            .unwrap();

        let (_next, certificate) = channel.apply(state, 1 << 12, 7).unwrap();
        assert!(certificate.verify(7, 1 << 12));
    }
}
