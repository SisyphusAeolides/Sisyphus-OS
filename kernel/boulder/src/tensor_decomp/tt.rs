//! Tensor-train decomposition by bounded TT-SVD.
//!
//! The input may have up to eight modes while containing at most 512 explicit
//! samples. Successive left unfoldings are compressed into three-index cores:
//! ```text
//!     G_k[r_k, i_k, r_{k+1}].
//! ```
//!
//! The retained residual is `U_k^T M_k`, so singular scale is propagated into
//! the next core. TT is also a real-valued matrix-product-state representation.

use super::fixed;
use super::tensor::{DenseTensor, TensorError, mix};

pub const MAX_TT_ORDER: usize = 8;
pub const MAX_TT_DIMENSION: usize = 8;
pub const MAX_TT_ENTRIES: usize = 512;
pub const MAX_TT_RANK: usize = 8;
pub const MAX_TT_MATRIX_ROWS: usize = MAX_TT_RANK * MAX_TT_DIMENSION;
pub const MAX_TT_CORE_ENTRIES: usize = MAX_TT_RANK * MAX_TT_DIMENSION * MAX_TT_RANK;
pub const MAX_TT_STORAGE: usize = MAX_TT_ORDER * MAX_TT_CORE_ENTRIES;
const MAX_COVARIANCE_ENTRIES: usize = MAX_TT_MATRIX_ROWS * MAX_TT_MATRIX_ROWS;
const MAX_BASIS_ENTRIES: usize = MAX_TT_MATRIX_ROWS * MAX_TT_RANK;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TtShape {
    order: u8,
    dimensions: [u8; MAX_TT_ORDER],
    strides: [usize; MAX_TT_ORDER],
    length: usize,
}

impl TtShape {
    pub fn new(order: usize, dimensions: [u8; MAX_TT_ORDER]) -> Result<Self, TensorError> {
        if order < 2 || order > MAX_TT_ORDER {
            return Err(TensorError::InvalidOrder);
        }

        let mut length = 1_usize;
        let mut strides = [0_usize; MAX_TT_ORDER];
        for mode in (0..order).rev() {
            let dimension = dimensions[mode] as usize;
            if dimension == 0 || dimension > MAX_TT_DIMENSION {
                return Err(TensorError::InvalidDimension);
            }
            strides[mode] = length;
            length = length
                .checked_mul(dimension)
                .ok_or(TensorError::Arithmetic)?;
            if length > MAX_TT_ENTRIES {
                return Err(TensorError::Capacity);
            }
        }

        if dimensions[order..].iter().any(|dimension| *dimension != 0) {
            return Err(TensorError::InvalidDimension);
        }

        Ok(Self {
            order: order as u8,
            dimensions,
            strides,
            length,
        })
    }

    pub const fn order(self) -> usize {
        self.order as usize
    }

    pub const fn dimensions(self) -> [u8; MAX_TT_ORDER] {
        self.dimensions
    }

    pub const fn dimension(self, mode: usize) -> usize {
        if mode < self.order as usize {
            self.dimensions[mode] as usize
        } else {
            0
        }
    }

    pub const fn length(self) -> usize {
        self.length
    }

    pub fn offset(self, coordinates: &[usize; MAX_TT_ORDER]) -> Result<usize, TensorError> {
        let mut offset = 0_usize;
        for mode in 0..self.order() {
            if coordinates[mode] >= self.dimension(mode) {
                return Err(TensorError::Coordinate);
            }
            offset = offset
                .checked_add(
                    coordinates[mode]
                        .checked_mul(self.strides[mode])
                        .ok_or(TensorError::Arithmetic)?,
                )
                .ok_or(TensorError::Arithmetic)?;
        }
        if coordinates[self.order()..]
            .iter()
            .any(|coordinate| *coordinate != 0)
        {
            return Err(TensorError::Coordinate);
        }
        Ok(offset)
    }

    pub fn unravel(self, mut offset: usize) -> [usize; MAX_TT_ORDER] {
        let mut coordinates = [0_usize; MAX_TT_ORDER];
        for mode in 0..self.order() {
            coordinates[mode] = offset / self.strides[mode];
            offset %= self.strides[mode];
        }
        coordinates
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct TtDense {
    shape: TtShape,
    values_q24: [i64; MAX_TT_ENTRIES],
}

impl TtDense {
    pub fn zeros(shape: TtShape) -> Self {
        Self {
            shape,
            values_q24: [0; MAX_TT_ENTRIES],
        }
    }

    pub fn from_dense(source: &DenseTensor) -> Result<Self, TensorError> {
        let mut dimensions = [0_u8; MAX_TT_ORDER];
        for mode in 0..source.shape().order() {
            dimensions[mode] = source.shape().dimension(mode) as u8;
        }
        let shape = TtShape::new(source.shape().order(), dimensions)?;
        let mut output = Self::zeros(shape);
        output.values_q24[..shape.length()].copy_from_slice(source.values());
        Ok(output)
    }

    pub const fn shape(&self) -> TtShape {
        self.shape
    }

    pub fn values(&self) -> &[i64] {
        &self.values_q24[..self.shape.length()]
    }

    pub fn values_mut(&mut self) -> &mut [i64] {
        &mut self.values_q24[..self.shape.length()]
    }

    pub fn copy_from_dense(&mut self, source: &DenseTensor) -> Result<(), TensorError> {
        if source.shape().order() != self.shape.order()
            || source.shape().length() != self.shape.length()
        {
            return Err(TensorError::ShapeMismatch);
        }
        for mode in 0..self.shape.order() {
            if source.shape().dimension(mode) != self.shape.dimension(mode) {
                return Err(TensorError::ShapeMismatch);
            }
        }
        self.values_mut().copy_from_slice(source.values());
        Ok(())
    }

    pub fn get(&self, coordinates: &[usize; MAX_TT_ORDER]) -> Result<i64, TensorError> {
        Ok(self.values_q24[self.shape.offset(coordinates)?])
    }

    pub fn set_linear(&mut self, index: usize, value_q24: i64) -> Result<(), TensorError> {
        if index >= self.shape.length() {
            return Err(TensorError::Coordinate);
        }
        self.values_q24[index] = value_q24;
        Ok(())
    }

    pub fn frobenius_squared_q48(&self) -> Result<u128, TensorError> {
        let mut sum = 0_u128;
        for value in self.values().iter().copied() {
            let magnitude = value.unsigned_abs() as u128;
            sum = sum
                .checked_add(
                    magnitude
                        .checked_mul(magnitude)
                        .ok_or(TensorError::Arithmetic)?,
                )
                .ok_or(TensorError::Arithmetic)?;
        }
        Ok(sum)
    }

    pub fn root(&self, secret: u64) -> Result<u64, TensorError> {
        if secret == 0 {
            return Err(TensorError::ZeroSecret);
        }

        let mut state = mix(secret, self.shape.order as u64);
        for dimension in self.shape.dimensions {
            state = mix(state, dimension as u64);
        }
        for value in self.values() {
            state = mix(state, *value as u64);
        }
        Ok(state)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TtConfig {
    pub maximum_rank: u8,
    pub eigenspace_iterations: u16,
    pub eigenspace_tolerance_q24: i64,
}

impl TtConfig {
    pub const KERNEL_DEFAULT: Self = Self {
        maximum_rank: 6,
        eigenspace_iterations: 64,
        eigenspace_tolerance_q24: fixed::ONE / 4096,
    };

    fn validate(self) -> Result<(), TensorError> {
        if self.maximum_rank == 0
            || self.maximum_rank as usize > MAX_TT_RANK
            || self.eigenspace_iterations == 0
            || self.eigenspace_tolerance_q24 < 0
        {
            return Err(TensorError::InvalidDimension);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TtCertificate {
    pub order: u8,
    pub ranks: [u8; MAX_TT_ORDER + 1],
    pub maximum_iterations: u16,
    pub relative_error_q24: i64,
    pub maximum_orthogonality_defect_q24: u64,
    pub maximum_relative_eigen_residual_q24: u64,
    pub source_parameters: u16,
    pub train_parameters: u16,
    pub tensor_root: u64,
    pub train_root: u64,
    pub root: u64,
}

impl TtCertificate {
    pub const EMPTY: Self = Self {
        order: 0,
        ranks: [0; MAX_TT_ORDER + 1],
        maximum_iterations: 0,
        relative_error_q24: 0,
        maximum_orthogonality_defect_q24: 0,
        maximum_relative_eigen_residual_q24: 0,
        source_parameters: 0,
        train_parameters: 0,
        tensor_root: 0,
        train_root: 0,
        root: 0,
    };

    pub const fn compresses(self) -> bool {
        self.train_parameters < self.source_parameters
    }

    pub fn verify(
        &self,
        secret: u64,
        maximum_relative_error_q24: i64,
        maximum_orthogonality_q24: u64,
        maximum_relative_eigen_residual_q24: u64,
    ) -> bool {
        self.order >= 2
            && self.ranks[0] == 1
            && self.ranks[self.order as usize] == 1
            && self.relative_error_q24 <= maximum_relative_error_q24
            && self.maximum_orthogonality_defect_q24 <= maximum_orthogonality_q24
            && self.maximum_relative_eigen_residual_q24 <= maximum_relative_eigen_residual_q24
            && self.root == certificate_root(secret, self)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct TensorTrain {
    shape: TtShape,
    ranks: [u8; MAX_TT_ORDER + 1],
    core_offsets: [u16; MAX_TT_ORDER],
    core_lengths: [u16; MAX_TT_ORDER],
    cores_q24: [i64; MAX_TT_STORAGE],
    root: u64,
}

impl TensorTrain {
    pub fn new(shape: TtShape) -> Self {
        let mut ranks = [0_u8; MAX_TT_ORDER + 1];
        ranks[0] = 1;
        ranks[shape.order()] = 1;

        Self {
            shape,
            ranks,
            core_offsets: [0; MAX_TT_ORDER],
            core_lengths: [0; MAX_TT_ORDER],
            cores_q24: [0; MAX_TT_STORAGE],
            root: 0,
        }
    }

    pub const fn shape(&self) -> TtShape {
        self.shape
    }

    pub const fn ranks(&self) -> [u8; MAX_TT_ORDER + 1] {
        self.ranks
    }

    pub const fn root(&self) -> u64 {
        self.root
    }

    pub fn parameter_count(&self) -> usize {
        let mut total = 0_usize;
        for mode in 0..self.shape.order() {
            total += self.core_lengths[mode] as usize;
        }
        total
    }

    pub fn core_value(
        &self,
        mode: usize,
        left_rank: usize,
        physical: usize,
        right_rank: usize,
    ) -> Result<i64, TensorError> {
        if mode >= self.shape.order()
            || left_rank >= self.ranks[mode] as usize
            || physical >= self.shape.dimension(mode)
            || right_rank >= self.ranks[mode + 1] as usize
        {
            return Err(TensorError::Coordinate);
        }

        let right_dimension = self.ranks[mode + 1] as usize;
        let index = self.core_offsets[mode] as usize
            + (left_rank * self.shape.dimension(mode) + physical) * right_dimension
            + right_rank;
        Ok(self.cores_q24[index])
    }

    pub fn value(&self, coordinates: &[usize; MAX_TT_ORDER]) -> Result<i64, TensorError> {
        self.shape.offset(coordinates)?;

        let mut state = [0_i64; MAX_TT_RANK];
        state[0] = fixed::ONE;
        let mut left_rank = 1_usize;

        for mode in 0..self.shape.order() {
            let right_rank = self.ranks[mode + 1] as usize;
            let mut next = [0_i64; MAX_TT_RANK];

            for right in 0..right_rank {
                for left in 0..left_rank {
                    let product = fixed::mul(
                        state[left],
                        self.core_value(mode, left, coordinates[mode], right)?,
                    )?;
                    next[right] = next[right]
                        .checked_add(product)
                        .ok_or(TensorError::Arithmetic)?;
                }
            }

            state = next;
            left_rank = right_rank;
        }

        Ok(state[0])
    }

    pub fn reconstruct_into(&self, output: &mut TtDense) -> Result<(), TensorError> {
        if output.shape() != self.shape {
            return Err(TensorError::ShapeMismatch);
        }

        for linear in 0..self.shape.length() {
            let coordinates = self.shape.unravel(linear);
            output.set_linear(linear, self.value(&coordinates)?)?;
        }
        Ok(())
    }

    fn install_core(
        &mut self,
        mode: usize,
        left_rank: usize,
        physical_dimension: usize,
        right_rank: usize,
        values: &[i64],
    ) -> Result<(), TensorError> {
        if mode >= self.shape.order()
            || left_rank > MAX_TT_RANK
            || right_rank > MAX_TT_RANK
            || physical_dimension != self.shape.dimension(mode)
        {
            return Err(TensorError::InvalidDimension);
        }

        let length = left_rank
            .checked_mul(physical_dimension)
            .and_then(|value| value.checked_mul(right_rank))
            .ok_or(TensorError::Arithmetic)?;
        if length > MAX_TT_CORE_ENTRIES || values.len() < length {
            return Err(TensorError::Capacity);
        }

        let offset = mode
            .checked_mul(MAX_TT_CORE_ENTRIES)
            .ok_or(TensorError::Arithmetic)?;
        self.cores_q24[offset..offset + length].copy_from_slice(&values[..length]);
        self.cores_q24[offset + length..offset + MAX_TT_CORE_ENTRIES].fill(0);
        self.core_offsets[mode] = offset as u16;
        self.core_lengths[mode] = length as u16;
        self.ranks[mode] = left_rank as u8;
        self.ranks[mode + 1] = right_rank as u8;
        Ok(())
    }

    fn seal(&mut self, secret: u64) -> Result<(), TensorError> {
        if secret == 0 {
            return Err(TensorError::ZeroSecret);
        }

        let mut state = mix(secret, self.shape.order as u64);
        for mode in 0..self.shape.order() {
            state = mix(state, self.shape.dimension(mode) as u64);
            state = mix(
                state,
                self.ranks[mode] as u64 | ((self.ranks[mode + 1] as u64) << 8),
            );
            let offset = self.core_offsets[mode] as usize;
            let length = self.core_lengths[mode] as usize;
            for value in &self.cores_q24[offset..offset + length] {
                state = mix(state, *value as u64);
            }
        }
        self.root = state;
        Ok(())
    }
}

pub struct TtWorkspace {
    current: [i64; MAX_TT_ENTRIES],
    next: [i64; MAX_TT_ENTRIES],
    covariance: [i64; MAX_COVARIANCE_ENTRIES],
    basis: [i64; MAX_BASIS_ENTRIES],
    next_basis: [i64; MAX_BASIS_ENTRIES],
    eigenvalues: [i64; MAX_TT_RANK],
    reconstruction: TtDense,
}

impl TtWorkspace {
    pub fn new(shape: TtShape) -> Self {
        Self {
            current: [0; MAX_TT_ENTRIES],
            next: [0; MAX_TT_ENTRIES],
            covariance: [0; MAX_COVARIANCE_ENTRIES],
            basis: [0; MAX_BASIS_ENTRIES],
            next_basis: [0; MAX_BASIS_ENTRIES],
            eigenvalues: [0; MAX_TT_RANK],
            reconstruction: TtDense::zeros(shape),
        }
    }

    pub fn reconstruction(&self) -> &TtDense {
        &self.reconstruction
    }
}

pub fn fit_tt_svd(
    tensor: &TtDense,
    train: &mut TensorTrain,
    workspace: &mut TtWorkspace,
    config: TtConfig,
    secret: u64,
) -> Result<TtCertificate, TensorError> {
    config.validate()?;
    if secret == 0 {
        return Err(TensorError::ZeroSecret);
    }
    if train.shape() != tensor.shape() || workspace.reconstruction.shape() != tensor.shape() {
        return Err(TensorError::ShapeMismatch);
    }

    workspace.current.fill(0);
    workspace.current[..tensor.shape().length()].copy_from_slice(tensor.values());
    train.cores_q24.fill(0);
    train.core_lengths.fill(0);
    train.core_offsets.fill(0);
    train.ranks.fill(0);
    train.ranks[0] = 1;
    train.ranks[tensor.shape().order()] = 1;

    let mut left_rank = 1_usize;
    let mut columns = tensor.shape().length();
    let mut maximum_orthogonality = 0_u64;
    let mut maximum_residual = 0_u64;
    let mut maximum_iterations = 0_u16;

    for mode in 0..tensor.shape().order() - 1 {
        let physical_dimension = tensor.shape().dimension(mode);
        let rows = left_rank
            .checked_mul(physical_dimension)
            .ok_or(TensorError::Arithmetic)?;
        if columns % physical_dimension != 0 {
            return Err(TensorError::ShapeMismatch);
        }
        columns /= physical_dimension;

        if rows > MAX_TT_MATRIX_ROWS
            || rows.checked_mul(columns).ok_or(TensorError::Arithmetic)? > MAX_TT_ENTRIES
        {
            return Err(TensorError::Capacity);
        }

        let rank = (config.maximum_rank as usize).min(rows).min(columns).max(1);

        build_covariance(&workspace.current, rows, columns, &mut workspace.covariance)?;

        let eigenspace = dominant_wide_eigenspace(
            &workspace.covariance,
            rows,
            rank,
            config.eigenspace_iterations,
            config.eigenspace_tolerance_q24,
            mix(secret, 0x5454_5356 ^ mode as u64),
            &mut workspace.basis,
            &mut workspace.next_basis,
            &mut workspace.eigenvalues,
        )?;
        maximum_orthogonality = maximum_orthogonality.max(eigenspace.orthogonality_defect_q24);
        maximum_residual = maximum_residual.max(eigenspace.maximum_relative_residual_q24);
        maximum_iterations = maximum_iterations.max(eigenspace.iterations);

        train.install_core(
            mode,
            left_rank,
            physical_dimension,
            rank,
            &workspace.basis[..rows * rank],
        )?;

        workspace.next.fill(0);
        for component in 0..rank {
            for column in 0..columns {
                let mut value = 0_i64;
                for row in 0..rows {
                    value = value
                        .checked_add(fixed::mul(
                            workspace.basis[row * rank + component],
                            workspace.current[row * columns + column],
                        )?)
                        .ok_or(TensorError::Arithmetic)?;
                }
                workspace.next[component * columns + column] = value;
            }
        }

        core::mem::swap(&mut workspace.current, &mut workspace.next);
        left_rank = rank;
    }

    let final_mode = tensor.shape().order() - 1;
    let final_dimension = tensor.shape().dimension(final_mode);
    if columns != final_dimension
        || left_rank
            .checked_mul(final_dimension)
            .ok_or(TensorError::Arithmetic)?
            > MAX_TT_CORE_ENTRIES
    {
        return Err(TensorError::ShapeMismatch);
    }

    train.install_core(
        final_mode,
        left_rank,
        final_dimension,
        1,
        &workspace.current[..left_rank * final_dimension],
    )?;
    train.seal(secret)?;
    train.reconstruct_into(&mut workspace.reconstruction)?;

    let source_energy = tensor.frobenius_squared_q48()?.max(1);
    let error = tt_squared_error_q48(tensor, &workspace.reconstruction)?;
    let relative_error = fixed::ratio_u128(error, source_energy)?;

    let mut certificate = TtCertificate {
        order: tensor.shape().order() as u8,
        ranks: train.ranks(),
        maximum_iterations,
        relative_error_q24: relative_error,
        maximum_orthogonality_defect_q24: maximum_orthogonality,
        maximum_relative_eigen_residual_q24: maximum_residual,
        source_parameters: tensor.shape().length().min(u16::MAX as usize) as u16,
        train_parameters: train.parameter_count().min(u16::MAX as usize) as u16,
        tensor_root: tensor.root(mix(secret, 0x534f_5552))?,
        train_root: train.root(),
        root: 0,
    };
    certificate.root = certificate_root(secret, &certificate);
    Ok(certificate)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WideEigenspaceCertificate {
    iterations: u16,
    orthogonality_defect_q24: u64,
    maximum_relative_residual_q24: u64,
}

fn build_covariance(
    matrix: &[i64; MAX_TT_ENTRIES],
    rows: usize,
    columns: usize,
    covariance: &mut [i64; MAX_COVARIANCE_ENTRIES],
) -> Result<(), TensorError> {
    covariance.fill(0);

    for left in 0..rows {
        for right in left..rows {
            let mut value = 0_i64;
            for column in 0..columns {
                value = value
                    .checked_add(fixed::mul(
                        matrix[left * columns + column],
                        matrix[right * columns + column],
                    )?)
                    .ok_or(TensorError::Arithmetic)?;
            }
            covariance[left * MAX_TT_MATRIX_ROWS + right] = value;
            covariance[right * MAX_TT_MATRIX_ROWS + left] = value;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn dominant_wide_eigenspace(
    covariance: &[i64; MAX_COVARIANCE_ENTRIES],
    dimension: usize,
    rank: usize,
    iterations: u16,
    tolerance_q24: i64,
    secret: u64,
    basis: &mut [i64; MAX_BASIS_ENTRIES],
    next: &mut [i64; MAX_BASIS_ENTRIES],
    eigenvalues: &mut [i64; MAX_TT_RANK],
) -> Result<WideEigenspaceCertificate, TensorError> {
    if dimension == 0
        || dimension > MAX_TT_MATRIX_ROWS
        || rank == 0
        || rank > MAX_TT_RANK
        || rank > dimension
    {
        return Err(TensorError::InvalidDimension);
    }

    basis.fill(0);
    next.fill(0);
    eigenvalues.fill(0);

    for column in 0..rank {
        for row in 0..dimension {
            let word = mix(secret ^ ((column as u64) << 32), row as u64);
            let signed = ((word >> 32) as i32) as i64;
            basis[row * rank + column] = (signed >> 8).clamp(-fixed::ONE, fixed::ONE);
        }
        orthonormalize_column(basis, dimension, rank, column, tolerance_q24)?;
    }

    let mut completed = 0_u16;
    for iteration in 0..iterations {
        next.fill(0);

        for column in 0..rank {
            for row in 0..dimension {
                let mut value = 0_i64;
                for inner in 0..dimension {
                    value = value
                        .checked_add(fixed::mul(
                            covariance[row * MAX_TT_MATRIX_ROWS + inner],
                            basis[inner * rank + column],
                        )?)
                        .ok_or(TensorError::Arithmetic)?;
                }
                next[row * rank + column] = value;
            }
            orthonormalize_column(next, dimension, rank, column, tolerance_q24)?;
        }

        let change = basis_change(basis, next, dimension, rank)?;
        basis[..dimension * rank].copy_from_slice(&next[..dimension * rank]);
        completed = iteration.saturating_add(1);

        if change <= tolerance_q24.unsigned_abs() {
            break;
        }
    }

    let mut maximum_residual = 0_u64;
    for column in 0..rank {
        let mut image = [0_i64; MAX_TT_MATRIX_ROWS];
        for row in 0..dimension {
            for inner in 0..dimension {
                image[row] = image[row]
                    .checked_add(fixed::mul(
                        covariance[row * MAX_TT_MATRIX_ROWS + inner],
                        basis[inner * rank + column],
                    )?)
                    .ok_or(TensorError::Arithmetic)?;
            }
        }

        let mut eigenvalue = 0_i64;
        for row in 0..dimension {
            eigenvalue = eigenvalue
                .checked_add(fixed::mul(basis[row * rank + column], image[row])?)
                .ok_or(TensorError::Arithmetic)?;
        }
        eigenvalues[column] = eigenvalue;

        let mut residual_squared = 0_i64;
        for row in 0..dimension {
            let residual = image[row]
                .checked_sub(fixed::mul(eigenvalue, basis[row * rank + column])?)
                .ok_or(TensorError::Arithmetic)?;
            residual_squared = residual_squared
                .checked_add(fixed::mul(residual, residual)?)
                .ok_or(TensorError::Arithmetic)?;
        }
        let residual_norm = fixed::sqrt(residual_squared.max(0))?;
        let denominator = eigenvalue
            .checked_abs()
            .ok_or(TensorError::Arithmetic)?
            .max(tolerance_q24.max(1));
        let relative = fixed::div(residual_norm, denominator)?;
        maximum_residual = maximum_residual.max(relative.unsigned_abs());
    }

    sort_wide_eigenspace(basis, eigenvalues, dimension, rank);

    let orthogonality = wide_orthogonality_defect(basis, dimension, rank)?;

    Ok(WideEigenspaceCertificate {
        iterations: completed,
        orthogonality_defect_q24: orthogonality,
        maximum_relative_residual_q24: maximum_residual,
    })
}

fn orthonormalize_column(
    basis: &mut [i64; MAX_BASIS_ENTRIES],
    dimension: usize,
    rank: usize,
    column: usize,
    floor_q24: i64,
) -> Result<(), TensorError> {
    for previous in 0..column {
        let mut coefficient = 0_i64;
        for row in 0..dimension {
            coefficient = coefficient
                .checked_add(fixed::mul(
                    basis[row * rank + previous],
                    basis[row * rank + column],
                )?)
                .ok_or(TensorError::Arithmetic)?;
        }
        for row in 0..dimension {
            basis[row * rank + column] = basis[row * rank + column]
                .checked_sub(fixed::mul(coefficient, basis[row * rank + previous])?)
                .ok_or(TensorError::Arithmetic)?;
        }
    }

    let mut squared = 0_i64;
    for row in 0..dimension {
        let value = basis[row * rank + column];
        squared = squared
            .checked_add(fixed::mul(value, value)?)
            .ok_or(TensorError::Arithmetic)?;
    }
    let magnitude = fixed::sqrt(squared.max(0))?;

    if magnitude <= floor_q24 {
        for row in 0..dimension {
            basis[row * rank + column] = 0;
        }
        basis[(column % dimension) * rank + column] = fixed::ONE;

        for previous in 0..column {
            let mut coefficient = 0_i64;
            for row in 0..dimension {
                coefficient = coefficient
                    .checked_add(fixed::mul(
                        basis[row * rank + previous],
                        basis[row * rank + column],
                    )?)
                    .ok_or(TensorError::Arithmetic)?;
            }
            for row in 0..dimension {
                basis[row * rank + column] = basis[row * rank + column]
                    .checked_sub(fixed::mul(coefficient, basis[row * rank + previous])?)
                    .ok_or(TensorError::Arithmetic)?;
            }
        }
    }

    let mut normalized_squared = 0_i64;
    for row in 0..dimension {
        let value = basis[row * rank + column];
        normalized_squared = normalized_squared
            .checked_add(fixed::mul(value, value)?)
            .ok_or(TensorError::Arithmetic)?;
    }
    let normalized_magnitude = fixed::sqrt(normalized_squared.max(0))?;
    if normalized_magnitude <= 0 {
        return Err(TensorError::Arithmetic);
    }

    for row in 0..dimension {
        basis[row * rank + column] = fixed::div(basis[row * rank + column], normalized_magnitude)?;
    }
    Ok(())
}

fn basis_change(
    left: &[i64; MAX_BASIS_ENTRIES],
    right: &[i64; MAX_BASIS_ENTRIES],
    dimension: usize,
    rank: usize,
) -> Result<u64, TensorError> {
    let mut maximum = 0_u64;

    for column in 0..rank {
        let mut overlap = 0_i64;
        for row in 0..dimension {
            overlap = overlap
                .checked_add(fixed::mul(
                    left[row * rank + column],
                    right[row * rank + column],
                )?)
                .ok_or(TensorError::Arithmetic)?;
        }
        maximum = maximum.max(fixed::ONE.abs_diff(overlap.abs()));
    }
    Ok(maximum)
}

fn wide_orthogonality_defect(
    basis: &[i64; MAX_BASIS_ENTRIES],
    dimension: usize,
    rank: usize,
) -> Result<u64, TensorError> {
    let mut maximum = 0_u64;
    for left in 0..rank {
        for right in 0..rank {
            let mut inner = 0_i64;
            for row in 0..dimension {
                inner = inner
                    .checked_add(fixed::mul(
                        basis[row * rank + left],
                        basis[row * rank + right],
                    )?)
                    .ok_or(TensorError::Arithmetic)?;
            }
            let expected = if left == right { fixed::ONE } else { 0 };
            maximum = maximum.max(inner.abs_diff(expected));
        }
    }
    Ok(maximum)
}

fn sort_wide_eigenspace(
    basis: &mut [i64; MAX_BASIS_ENTRIES],
    eigenvalues: &mut [i64; MAX_TT_RANK],
    dimension: usize,
    rank: usize,
) {
    for left in 0..rank {
        let mut best = left;
        for right in left + 1..rank {
            if eigenvalues[right] > eigenvalues[best] {
                best = right;
            }
        }
        if best != left {
            eigenvalues.swap(left, best);
            for row in 0..dimension {
                basis.swap(row * rank + left, row * rank + best);
            }
        }
    }
}

fn tt_squared_error_q48(left: &TtDense, right: &TtDense) -> Result<u128, TensorError> {
    if left.shape() != right.shape() {
        return Err(TensorError::ShapeMismatch);
    }

    let mut error = 0_u128;
    for index in 0..left.shape().length() {
        let difference = left.values()[index]
            .checked_sub(right.values()[index])
            .ok_or(TensorError::Arithmetic)?;
        let magnitude = difference.unsigned_abs() as u128;
        error = error
            .checked_add(
                magnitude
                    .checked_mul(magnitude)
                    .ok_or(TensorError::Arithmetic)?,
            )
            .ok_or(TensorError::Arithmetic)?;
    }
    Ok(error)
}

fn certificate_root(secret: u64, certificate: &TtCertificate) -> u64 {
    let mut state = mix(
        secret,
        certificate.order as u64 | ((certificate.maximum_iterations as u64) << 8),
    );
    for rank in certificate.ranks {
        state = mix(state, rank as u64);
    }
    state = mix(state, certificate.relative_error_q24 as u64);
    state = mix(state, certificate.maximum_orthogonality_defect_q24);
    state = mix(state, certificate.maximum_relative_eigen_residual_q24);
    state = mix(
        state,
        certificate.source_parameters as u64 | ((certificate.train_parameters as u64) << 16),
    );
    state = mix(state, certificate.tensor_root);
    mix(state, certificate.train_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tt_svd_compresses_a_separable_six_mode_tensor() {
        let shape = TtShape::new(6, [2, 2, 2, 2, 2, 2, 0, 0]).unwrap();
        let mut tensor = TtDense::zeros(shape);

        for linear in 0..shape.length() {
            let coordinate = shape.unravel(linear);
            let mut value = fixed::ONE;
            for mode in 0..shape.order() {
                value = fixed::mul(value, (coordinate[mode] as i64 + 1) * fixed::ONE).unwrap();
            }
            tensor.set_linear(linear, value).unwrap();
        }

        let mut train = TensorTrain::new(shape);
        let mut workspace = TtWorkspace::new(shape);
        let certificate = fit_tt_svd(
            &tensor,
            &mut train,
            &mut workspace,
            TtConfig {
                maximum_rank: 2,
                ..TtConfig::KERNEL_DEFAULT
            },
            7,
        )
        .unwrap();

        assert!(certificate.train_root != 0);
        assert!(certificate.relative_error_q24 < fixed::ONE / 64);
    }
}
