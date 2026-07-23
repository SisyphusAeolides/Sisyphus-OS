//! Bounded Einstein summation parser and executor.
//!
//! Supported forms are explicit-output unary and binary expressions such as:
//!
//! ```text
//! ij->ji
//! ii->i
//! ij,jk->ik
//! abc,adc->bd
//! ```
//!
//! Labels are lowercase ASCII letters. Repeated labels in one operand select
//! a diagonal. Labels absent from the output are summed. Scalar output is
//! represented by a one-element tensor with shape `[1]`.

use super::fixed;
use super::tensor::{DenseTensor, MAX_ORDER, TensorError, TensorShape, mix};

pub const MAX_EINSUM_LABELS: usize = 8;
const NO_LABEL: u8 = u8::MAX;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EinsumError {
    Syntax,
    UnsupportedOperandCount,
    TooManyLabels,
    DuplicateOutputLabel,
    UnknownOutputLabel,
    DimensionMismatch,
    Tensor(TensorError),
    Fixed(fixed::FixedError),
}

impl From<TensorError> for EinsumError {
    fn from(error: TensorError) -> Self {
        Self::Tensor(error)
    }
}

impl From<fixed::FixedError> for EinsumError {
    fn from(error: fixed::FixedError) -> Self {
        Self::Fixed(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EinsumPlan {
    operand_count: u8,
    operand_lengths: [u8; 2],
    operand_labels: [[u8; MAX_ORDER]; 2],
    output_length: u8,
    output_labels: [u8; MAX_ORDER],
    reduction_count: u8,
    reduction_labels: [u8; MAX_EINSUM_LABELS],
    label_dimensions: [u8; 26],
    output_shape: TensorShape,
}

impl EinsumPlan {
    pub fn unary(expression: &[u8], input: TensorShape) -> Result<Self, EinsumError> {
        Self::parse(expression, input, None)
    }

    pub fn binary(
        expression: &[u8],
        left: TensorShape,
        right: TensorShape,
    ) -> Result<Self, EinsumError> {
        Self::parse(expression, left, Some(right))
    }

    pub const fn output_shape(self) -> TensorShape {
        self.output_shape
    }

    pub const fn operand_count(self) -> usize {
        self.operand_count as usize
    }

    fn parse(
        expression: &[u8],
        left: TensorShape,
        right: Option<TensorShape>,
    ) -> Result<Self, EinsumError> {
        let arrow = find_arrow(expression)?;
        let input_expression = &expression[..arrow];
        let output_expression = &expression[arrow + 2..];

        let comma = input_expression.iter().position(|byte| *byte == b',');

        let operand_count = if comma.is_some() { 2 } else { 1 };
        if operand_count == 1 && right.is_some() || operand_count == 2 && right.is_none() {
            return Err(EinsumError::UnsupportedOperandCount);
        }

        let mut operand_labels = [[NO_LABEL; MAX_ORDER]; 2];
        let mut operand_lengths = [0_u8; 2];

        let left_spec = if let Some(comma) = comma {
            &input_expression[..comma]
        } else {
            input_expression
        };
        parse_label_sequence(left_spec, left.order(), &mut operand_labels[0])?;
        operand_lengths[0] = left.order() as u8;

        if let (Some(comma), Some(right_shape)) = (comma, right) {
            let right_spec = &input_expression[comma + 1..];
            parse_label_sequence(right_spec, right_shape.order(), &mut operand_labels[1])?;
            operand_lengths[1] = right_shape.order() as u8;
        }

        let mut output_labels = [NO_LABEL; MAX_ORDER];
        if output_expression.len() > MAX_ORDER {
            return Err(EinsumError::TooManyLabels);
        }
        parse_output_sequence(output_expression, &mut output_labels)?;

        let mut label_dimensions = [0_u8; 26];
        bind_dimensions(left, &operand_labels[0], &mut label_dimensions)?;
        if let Some(right_shape) = right {
            bind_dimensions(right_shape, &operand_labels[1], &mut label_dimensions)?;
        }

        let mut output_seen = 0_u32;
        for label in output_labels[..output_expression.len()].iter().copied() {
            let bit = 1_u32 << label;
            if output_seen & bit != 0 {
                return Err(EinsumError::DuplicateOutputLabel);
            }
            if label_dimensions[label as usize] == 0 {
                return Err(EinsumError::UnknownOutputLabel);
            }
            output_seen |= bit;
        }

        let mut reduction_labels = [NO_LABEL; MAX_EINSUM_LABELS];
        let mut reduction_count = 0_usize;
        let mut all_seen = 0_u32;

        for operand in 0..operand_count {
            for label in operand_labels[operand][..operand_lengths[operand] as usize]
                .iter()
                .copied()
            {
                let bit = 1_u32 << label;
                if all_seen & bit == 0 {
                    all_seen |= bit;
                    if output_seen & bit == 0 {
                        if reduction_count >= MAX_EINSUM_LABELS {
                            return Err(EinsumError::TooManyLabels);
                        }
                        reduction_labels[reduction_count] = label;
                        reduction_count += 1;
                    }
                }
            }
        }

        let output_shape = if output_expression.is_empty() {
            TensorShape::new(1, [1, 0, 0, 0])?
        } else {
            let mut dimensions = [0_u8; MAX_ORDER];
            for index in 0..output_expression.len() {
                dimensions[index] = label_dimensions[output_labels[index] as usize];
            }
            TensorShape::new(output_expression.len(), dimensions)?
        };

        Ok(Self {
            operand_count: operand_count as u8,
            operand_lengths,
            operand_labels,
            output_length: output_expression.len() as u8,
            output_labels,
            reduction_count: reduction_count as u8,
            reduction_labels,
            label_dimensions,
            output_shape,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EinsumCertificate {
    pub operands: u8,
    pub output_order: u8,
    pub reduction_labels: u8,
    pub multiply_accumulates: u32,
    pub left_root: u64,
    pub right_root: u64,
    pub output_root: u64,
    pub plan_root: u64,
    pub root: u64,
}

impl EinsumCertificate {
    pub fn verify(&self, secret: u64) -> bool {
        self.root == certificate_root(secret, self)
    }
}

pub fn execute_unary(
    plan: EinsumPlan,
    input: &DenseTensor,
    output: &mut DenseTensor,
    secret: u64,
) -> Result<EinsumCertificate, EinsumError> {
    if plan.operand_count() != 1 {
        return Err(EinsumError::UnsupportedOperandCount);
    }
    execute(plan, input, None, output, secret)
}

pub fn execute_binary(
    plan: EinsumPlan,
    left: &DenseTensor,
    right: &DenseTensor,
    output: &mut DenseTensor,
    secret: u64,
) -> Result<EinsumCertificate, EinsumError> {
    if plan.operand_count() != 2 {
        return Err(EinsumError::UnsupportedOperandCount);
    }
    execute(plan, left, Some(right), output, secret)
}

fn execute(
    plan: EinsumPlan,
    left: &DenseTensor,
    right: Option<&DenseTensor>,
    output: &mut DenseTensor,
    secret: u64,
) -> Result<EinsumCertificate, EinsumError> {
    if secret == 0 {
        return Err(TensorError::ZeroSecret.into());
    }
    if !plan.output_shape.same_geometry(output.shape()) {
        return Err(TensorError::ShapeMismatch.into());
    }
    if plan.operand_lengths[0] as usize != left.shape().order() {
        return Err(TensorError::ShapeMismatch.into());
    }
    if let Some(right_tensor) = right {
        if plan.operand_lengths[1] as usize != right_tensor.shape().order() {
            return Err(TensorError::ShapeMismatch.into());
        }
    }

    output.clear();
    let reduction_length = reduction_length(plan)?;
    let mut operations = 0_u32;

    for output_linear in 0..output.shape().length() {
        let output_coordinates = output.shape().unravel(output_linear);
        let mut label_coordinates = [0_usize; 26];

        for index in 0..plan.output_length as usize {
            label_coordinates[plan.output_labels[index] as usize] = output_coordinates[index];
        }

        let mut sum = 0_i64;
        for reduction_linear in 0..reduction_length {
            assign_reduction_coordinates(plan, reduction_linear, &mut label_coordinates);

            let left_coordinates = operand_coordinates(plan, 0, &label_coordinates);
            let mut term = left.get(&left_coordinates)?;

            if let Some(right_tensor) = right {
                let right_coordinates = operand_coordinates(plan, 1, &label_coordinates);
                term = fixed::mul(term, right_tensor.get(&right_coordinates)?)?;
            }

            sum = sum.checked_add(term).ok_or(TensorError::Arithmetic)?;
            operations = operations.saturating_add(1);
        }

        output.set_linear(output_linear, sum)?;
    }

    let plan_root = plan_root(secret, plan);
    let left_root = left.root(mix(secret, 0x4c45_4654))?;
    let right_root = match right {
        Some(tensor) => tensor.root(mix(secret, 0x5249_4748))?,
        None => 0,
    };
    let output_root = output.root(mix(secret, 0x4f55_5450))?;

    let mut certificate = EinsumCertificate {
        operands: plan.operand_count,
        output_order: plan.output_shape.order() as u8,
        reduction_labels: plan.reduction_count,
        multiply_accumulates: operations,
        left_root,
        right_root,
        output_root,
        plan_root,
        root: 0,
    };
    certificate.root = certificate_root(secret, &certificate);
    Ok(certificate)
}

fn find_arrow(expression: &[u8]) -> Result<usize, EinsumError> {
    if expression.len() < 2 {
        return Err(EinsumError::Syntax);
    }

    for index in 0..expression.len() - 1 {
        if expression[index] == b'-' && expression[index + 1] == b'>' {
            if expression[index + 2..].contains(&b'-') || expression[index + 2..].contains(&b'>') {
                return Err(EinsumError::Syntax);
            }
            return Ok(index);
        }
    }

    Err(EinsumError::Syntax)
}

fn parse_label_sequence(
    sequence: &[u8],
    expected_length: usize,
    output: &mut [u8; MAX_ORDER],
) -> Result<(), EinsumError> {
    if sequence.len() != expected_length || sequence.len() > MAX_ORDER {
        return Err(EinsumError::Syntax);
    }

    for (index, byte) in sequence.iter().copied().enumerate() {
        output[index] = label_index(byte)?;
    }
    Ok(())
}

fn parse_output_sequence(sequence: &[u8], output: &mut [u8; MAX_ORDER]) -> Result<(), EinsumError> {
    for (index, byte) in sequence.iter().copied().enumerate() {
        output[index] = label_index(byte)?;
    }
    Ok(())
}

fn label_index(byte: u8) -> Result<u8, EinsumError> {
    if byte.is_ascii_lowercase() {
        Ok(byte - b'a')
    } else {
        Err(EinsumError::Syntax)
    }
}

fn bind_dimensions(
    shape: TensorShape,
    labels: &[u8; MAX_ORDER],
    dimensions: &mut [u8; 26],
) -> Result<(), EinsumError> {
    for mode in 0..shape.order() {
        let label = labels[mode] as usize;
        let dimension = shape.dimension(mode) as u8;
        if dimensions[label] == 0 {
            dimensions[label] = dimension;
        } else if dimensions[label] != dimension {
            return Err(EinsumError::DimensionMismatch);
        }
    }
    Ok(())
}

fn reduction_length(plan: EinsumPlan) -> Result<usize, EinsumError> {
    let mut length = 1_usize;
    for index in 0..plan.reduction_count as usize {
        let label = plan.reduction_labels[index] as usize;
        length = length
            .checked_mul(plan.label_dimensions[label] as usize)
            .ok_or(TensorError::Arithmetic)?;
    }
    Ok(length)
}

fn assign_reduction_coordinates(
    plan: EinsumPlan,
    mut linear: usize,
    coordinates: &mut [usize; 26],
) {
    for index in (0..plan.reduction_count as usize).rev() {
        let label = plan.reduction_labels[index] as usize;
        let dimension = plan.label_dimensions[label] as usize;
        coordinates[label] = linear % dimension;
        linear /= dimension;
    }
}

fn operand_coordinates(
    plan: EinsumPlan,
    operand: usize,
    labels: &[usize; 26],
) -> [usize; MAX_ORDER] {
    let mut coordinates = [0_usize; MAX_ORDER];
    for mode in 0..plan.operand_lengths[operand] as usize {
        coordinates[mode] = labels[plan.operand_labels[operand][mode] as usize];
    }
    coordinates
}

fn plan_root(secret: u64, plan: EinsumPlan) -> u64 {
    let mut state = mix(
        secret,
        plan.operand_count as u64
            | ((plan.output_length as u64) << 8)
            | ((plan.reduction_count as u64) << 16),
    );

    for operand in 0..plan.operand_count as usize {
        for label in plan.operand_labels[operand][..plan.operand_lengths[operand] as usize]
            .iter()
            .copied()
        {
            state = mix(state, label as u64);
        }
    }
    for label in plan.output_labels[..plan.output_length as usize]
        .iter()
        .copied()
    {
        state = mix(state, label as u64);
    }
    for dimension in plan.label_dimensions {
        state = mix(state, dimension as u64);
    }
    state
}

fn certificate_root(secret: u64, certificate: &EinsumCertificate) -> u64 {
    let mut state = mix(
        secret,
        certificate.operands as u64
            | ((certificate.output_order as u64) << 8)
            | ((certificate.reduction_labels as u64) << 16),
    );
    state = mix(state, certificate.multiply_accumulates as u64);
    state = mix(state, certificate.left_root);
    state = mix(state, certificate.right_root);
    state = mix(state, certificate.output_root);
    mix(state, certificate.plan_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_einsum_multiplies_matrices() {
        let shape = TensorShape::new(2, [2, 2, 0, 0]).unwrap();
        let mut left = DenseTensor::zeros(shape);
        let mut right = DenseTensor::zeros(shape);

        for (index, value) in [1, 2, 3, 4].iter().copied().enumerate() {
            left.set_linear(index, value * fixed::ONE).unwrap();
        }
        for (index, value) in [5, 6, 7, 8].iter().copied().enumerate() {
            right.set_linear(index, value * fixed::ONE).unwrap();
        }

        let plan = EinsumPlan::binary(b"ij,jk->ik", shape, shape).unwrap();
        let mut output = DenseTensor::zeros(plan.output_shape());
        let certificate = execute_binary(plan, &left, &right, &mut output, 7).unwrap();

        assert!(certificate.verify(7));
        assert_eq!(output.get(&[0, 0, 0, 0]).unwrap(), 19 * fixed::ONE);
        assert_eq!(output.get(&[1, 1, 0, 0]).unwrap(), 50 * fixed::ONE);
    }

    #[test]
    fn unary_einsum_extracts_diagonal() {
        let shape = TensorShape::new(2, [3, 3, 0, 0]).unwrap();
        let mut matrix = DenseTensor::zeros(shape);
        matrix.set(&[0, 0, 0, 0], fixed::ONE).unwrap();
        matrix.set(&[1, 1, 0, 0], 2 * fixed::ONE).unwrap();
        matrix.set(&[2, 2, 0, 0], 3 * fixed::ONE).unwrap();

        let plan = EinsumPlan::unary(b"ii->i", shape).unwrap();
        let mut output = DenseTensor::zeros(plan.output_shape());
        execute_unary(plan, &matrix, &mut output, 9).unwrap();

        assert_eq!(output.values()[0], fixed::ONE);
        assert_eq!(output.values()[2], 3 * fixed::ONE);
    }

    #[test]
    fn scalar_einsum_uses_one_element_tensor() {
        let shape = TensorShape::new(1, [3, 0, 0, 0]).unwrap();
        let mut vector = DenseTensor::zeros(shape);
        vector
            .values_mut()
            .copy_from_slice(&[fixed::ONE, 2 * fixed::ONE, 3 * fixed::ONE]);

        let plan = EinsumPlan::unary(b"i->", shape).unwrap();
        let mut output = DenseTensor::zeros(plan.output_shape());
        execute_unary(plan, &vector, &mut output, 11).unwrap();
        assert_eq!(output.values()[0], 6 * fixed::ONE);
    }
}
