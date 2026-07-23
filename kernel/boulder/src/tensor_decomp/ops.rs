//! Exact bounded tensor contractions and mode products.
//!
//! All operations use checked Q24 arithmetic and caller-provided retained
//! output tensors. A zero-order scalar result is represented by a one-element
//! tensor with shape `[1]`.

use super::fixed;
use super::linalg::SmallMatrix;
use super::tensor::{DenseTensor, MAX_ORDER, TensorError, TensorShape, mix};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AxisPairs {
    pub count: u8,
    pub left: [u8; MAX_ORDER],
    pub right: [u8; MAX_ORDER],
}

impl AxisPairs {
    pub const EMPTY: Self = Self {
        count: 0,
        left: [0; MAX_ORDER],
        right: [0; MAX_ORDER],
    };

    pub fn new(left: &[u8], right: &[u8]) -> Result<Self, TensorError> {
        if left.len() != right.len() || left.len() > MAX_ORDER {
            return Err(TensorError::InvalidDimension);
        }

        let mut pairs = Self::EMPTY;
        pairs.count = left.len() as u8;
        pairs.left[..left.len()].copy_from_slice(left);
        pairs.right[..right.len()].copy_from_slice(right);
        Ok(pairs)
    }

    pub const fn len(self) -> usize {
        self.count as usize
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContractionCertificate {
    pub contracted_axes: u8,
    pub output_order: u8,
    pub multiply_accumulates: u32,
    pub left_root: u64,
    pub right_root: u64,
    pub output_root: u64,
    pub root: u64,
}

impl ContractionCertificate {
    pub const EMPTY: Self = Self {
        contracted_axes: 0,
        output_order: 0,
        multiply_accumulates: 0,
        left_root: 0,
        right_root: 0,
        output_root: 0,
        root: 0,
    };

    pub fn verify(&self, secret: u64) -> bool {
        self.root == contraction_root(secret, self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModeProductCertificate {
    pub mode: u8,
    pub transpose: bool,
    pub input_root: u64,
    pub matrix_root: u64,
    pub output_root: u64,
    pub multiply_accumulates: u32,
    pub root: u64,
}

impl ModeProductCertificate {
    pub fn verify(&self, secret: u64) -> bool {
        self.root == mode_product_root(secret, self)
    }
}

pub fn tensordot_output_shape(
    left: TensorShape,
    right: TensorShape,
    axes: AxisPairs,
) -> Result<TensorShape, TensorError> {
    validate_axis_pairs(left, right, axes)?;

    let output_order = left
        .order()
        .checked_add(right.order())
        .and_then(|value| value.checked_sub(2 * axes.len()))
        .ok_or(TensorError::Arithmetic)?;

    if output_order > MAX_ORDER {
        return Err(TensorError::Capacity);
    }

    if output_order == 0 {
        return TensorShape::new(1, [1, 0, 0, 0]);
    }

    let mut dimensions = [0_u8; MAX_ORDER];
    let mut write = 0_usize;

    for mode in 0..left.order() {
        if !contains_axis(&axes.left[..axes.len()], mode) {
            dimensions[write] = left.dimension(mode) as u8;
            write += 1;
        }
    }

    for mode in 0..right.order() {
        if !contains_axis(&axes.right[..axes.len()], mode) {
            dimensions[write] = right.dimension(mode) as u8;
            write += 1;
        }
    }

    TensorShape::new(output_order, dimensions)
}

pub fn tensordot_into(
    left: &DenseTensor,
    right: &DenseTensor,
    axes: AxisPairs,
    output: &mut DenseTensor,
    secret: u64,
) -> Result<ContractionCertificate, TensorError> {
    if secret == 0 {
        return Err(TensorError::ZeroSecret);
    }

    let expected = tensordot_output_shape(left.shape(), right.shape(), axes)?;
    if !expected.same_geometry(output.shape()) {
        return Err(TensorError::ShapeMismatch);
    }

    output.clear();

    let mut left_free = [0_u8; MAX_ORDER];
    let mut right_free = [0_u8; MAX_ORDER];
    let left_free_count = collect_free_axes(
        left.shape().order(),
        &axes.left[..axes.len()],
        &mut left_free,
    );
    let right_free_count = collect_free_axes(
        right.shape().order(),
        &axes.right[..axes.len()],
        &mut right_free,
    );

    let contraction_length = contracted_length(left.shape(), &axes.left[..axes.len()])?;
    let mut operations = 0_u32;

    for output_linear in 0..output.shape().length() {
        let output_coordinates = output.shape().unravel(output_linear);
        let mut left_coordinates = [0_usize; MAX_ORDER];
        let mut right_coordinates = [0_usize; MAX_ORDER];

        for index in 0..left_free_count {
            left_coordinates[left_free[index] as usize] = output_coordinates[index];
        }
        for index in 0..right_free_count {
            right_coordinates[right_free[index] as usize] =
                output_coordinates[left_free_count + index];
        }

        let mut sum = 0_i64;
        for contraction_linear in 0..contraction_length {
            let contraction_coordinates =
                unravel_contracted(contraction_linear, left.shape(), &axes.left[..axes.len()]);

            for pair in 0..axes.len() {
                let coordinate = contraction_coordinates[pair];
                left_coordinates[axes.left[pair] as usize] = coordinate;
                right_coordinates[axes.right[pair] as usize] = coordinate;
            }

            let product = fixed::mul(left.get(&left_coordinates)?, right.get(&right_coordinates)?)?;
            sum = sum.checked_add(product).ok_or(TensorError::Arithmetic)?;
            operations = operations.saturating_add(1);
        }

        output.set_linear(output_linear, sum)?;
    }

    let mut certificate = ContractionCertificate {
        contracted_axes: axes.count,
        output_order: expected.order() as u8,
        multiply_accumulates: operations,
        left_root: left.root(mix(secret, 0x4c45_4654))?,
        right_root: right.root(mix(secret, 0x5249_4748))?,
        output_root: output.root(mix(secret, 0x4f55_5450))?,
        root: 0,
    };
    certificate.root = contraction_root(secret, &certificate);
    Ok(certificate)
}

pub fn mode_product_output_shape(
    input: TensorShape,
    mode: usize,
    matrix: &SmallMatrix,
    transpose: bool,
) -> Result<TensorShape, TensorError> {
    if mode >= input.order() {
        return Err(TensorError::Coordinate);
    }

    let (output_dimension, contracted_dimension) = if transpose {
        (matrix.columns(), matrix.rows())
    } else {
        (matrix.rows(), matrix.columns())
    };

    if contracted_dimension != input.dimension(mode) {
        return Err(TensorError::ShapeMismatch);
    }

    let mut dimensions = input.dimensions();
    dimensions[mode] = output_dimension as u8;
    TensorShape::new(input.order(), dimensions)
}

pub fn mode_product_into(
    input: &DenseTensor,
    mode: usize,
    matrix: &SmallMatrix,
    transpose: bool,
    output: &mut DenseTensor,
    secret: u64,
) -> Result<ModeProductCertificate, TensorError> {
    if secret == 0 {
        return Err(TensorError::ZeroSecret);
    }

    let expected = mode_product_output_shape(input.shape(), mode, matrix, transpose)?;
    if !expected.same_geometry(output.shape()) {
        return Err(TensorError::ShapeMismatch);
    }

    output.clear();
    let contracted_dimension = input.shape().dimension(mode);
    let mut operations = 0_u32;

    for output_linear in 0..output.shape().length() {
        let output_coordinates = output.shape().unravel(output_linear);
        let output_mode_coordinate = output_coordinates[mode];
        let mut input_coordinates = output_coordinates;
        let mut sum = 0_i64;

        for contracted in 0..contracted_dimension {
            input_coordinates[mode] = contracted;
            let coefficient = if transpose {
                matrix.get(contracted, output_mode_coordinate)?
            } else {
                matrix.get(output_mode_coordinate, contracted)?
            };
            let product = fixed::mul(coefficient, input.get(&input_coordinates)?)?;
            sum = sum.checked_add(product).ok_or(TensorError::Arithmetic)?;
            operations = operations.saturating_add(1);
        }

        output.set_linear(output_linear, sum)?;
    }

    let mut certificate = ModeProductCertificate {
        mode: mode as u8,
        transpose,
        input_root: input.root(mix(secret, 0x494e_5055))?,
        matrix_root: matrix.root(mix(secret, 0x4d41_5452))?,
        output_root: output.root(mix(secret, 0x4f55_5450))?,
        multiply_accumulates: operations,
        root: 0,
    };
    certificate.root = mode_product_root(secret, &certificate);
    Ok(certificate)
}

pub fn copy_tensor(source: &DenseTensor, destination: &mut DenseTensor) -> Result<(), TensorError> {
    if !source.shape().same_geometry(destination.shape()) {
        return Err(TensorError::ShapeMismatch);
    }
    destination.values_mut().copy_from_slice(source.values());
    Ok(())
}

fn validate_axis_pairs(
    left: TensorShape,
    right: TensorShape,
    axes: AxisPairs,
) -> Result<(), TensorError> {
    let mut left_seen = 0_u16;
    let mut right_seen = 0_u16;

    for pair in 0..axes.len() {
        let left_axis = axes.left[pair] as usize;
        let right_axis = axes.right[pair] as usize;

        if left_axis >= left.order() || right_axis >= right.order() {
            return Err(TensorError::Coordinate);
        }
        if left_seen & (1_u16 << left_axis) != 0 || right_seen & (1_u16 << right_axis) != 0 {
            return Err(TensorError::InvalidDimension);
        }
        if left.dimension(left_axis) != right.dimension(right_axis) {
            return Err(TensorError::ShapeMismatch);
        }

        left_seen |= 1_u16 << left_axis;
        right_seen |= 1_u16 << right_axis;
    }

    Ok(())
}

fn contains_axis(axes: &[u8], axis: usize) -> bool {
    axes.iter().any(|candidate| *candidate as usize == axis)
}

fn collect_free_axes(order: usize, contracted: &[u8], output: &mut [u8; MAX_ORDER]) -> usize {
    let mut count = 0_usize;
    for axis in 0..order {
        if !contains_axis(contracted, axis) {
            output[count] = axis as u8;
            count += 1;
        }
    }
    count
}

fn contracted_length(shape: TensorShape, axes: &[u8]) -> Result<usize, TensorError> {
    let mut length = 1_usize;
    for axis in axes {
        length = length
            .checked_mul(shape.dimension(*axis as usize))
            .ok_or(TensorError::Arithmetic)?;
    }
    Ok(length)
}

fn unravel_contracted(mut linear: usize, shape: TensorShape, axes: &[u8]) -> [usize; MAX_ORDER] {
    let mut output = [0_usize; MAX_ORDER];

    for index in (0..axes.len()).rev() {
        let dimension = shape.dimension(axes[index] as usize);
        output[index] = linear % dimension;
        linear /= dimension;
    }

    output
}

fn contraction_root(secret: u64, certificate: &ContractionCertificate) -> u64 {
    let mut state = mix(
        secret,
        certificate.contracted_axes as u64 | ((certificate.output_order as u64) << 8),
    );
    state = mix(state, certificate.multiply_accumulates as u64);
    state = mix(state, certificate.left_root);
    state = mix(state, certificate.right_root);
    mix(state, certificate.output_root)
}

fn mode_product_root(secret: u64, certificate: &ModeProductCertificate) -> u64 {
    let mut state = mix(
        secret,
        certificate.mode as u64 | ((u64::from(certificate.transpose)) << 8),
    );
    state = mix(state, certificate.input_root);
    state = mix(state, certificate.matrix_root);
    state = mix(state, certificate.output_root);
    mix(state, certificate.multiply_accumulates as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_multiplication_is_tensordot() {
        let matrix_shape = TensorShape::new(2, [2, 2, 0, 0]).unwrap();
        let mut left = DenseTensor::zeros(matrix_shape);
        let mut right = DenseTensor::zeros(matrix_shape);
        for (index, value) in [1, 2, 3, 4].iter().copied().enumerate() {
            left.set_linear(index, value * fixed::ONE).unwrap();
        }
        for (index, value) in [5, 6, 7, 8].iter().copied().enumerate() {
            right.set_linear(index, value * fixed::ONE).unwrap();
        }

        let axes = AxisPairs::new(&[1], &[0]).unwrap();
        let output_shape = tensordot_output_shape(matrix_shape, matrix_shape, axes).unwrap();
        let mut output = DenseTensor::zeros(output_shape);
        tensordot_into(&left, &right, axes, &mut output, 7).unwrap();

        assert_eq!(output.get(&[0, 0, 0, 0]).unwrap(), 19 * fixed::ONE);
        assert_eq!(output.get(&[1, 1, 0, 0]).unwrap(), 50 * fixed::ONE);
    }

    #[test]
    fn mode_product_changes_one_dimension() {
        let shape = TensorShape::new(3, [2, 3, 2, 0]).unwrap();
        let input = DenseTensor::zeros(shape);
        let matrix = SmallMatrix::zeros(1, 3).unwrap();
        let output_shape = mode_product_output_shape(shape, 1, &matrix, false).unwrap();
        assert_eq!(output_shape.dimensions(), [2, 1, 2, 0]);
        let mut output = DenseTensor::zeros(output_shape);
        mode_product_into(&input, 1, &matrix, false, &mut output, 9).unwrap();
    }
}
