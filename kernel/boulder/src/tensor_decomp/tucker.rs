//! Tucker decomposition by a bounded higher-order singular-value transform.
//!
//! Each factor matrix contains the dominant eigenspace of a mode covariance.
//! The core is the tensor projected through every transposed factor:
//! ```text
//!     G = X ×0 U0^T ×1 U1^T ... ×N UN^T
//! ```
//!
//! Reconstruction is:
//!
//! ```text
//!     X_hat = G ×0 U0 ×1 U1 ... ×N UN.
//! ```
//!
//! The certificate records orthogonality, eigen residuals, retained energy,
//! reconstruction error, and exact parameter count.

use super::fixed;
use super::linalg::{EigenspaceCertificate, SmallMatrix, dominant_eigenspace};
use super::tensor::{DenseTensor, MAX_ORDER, TensorError, TensorShape, mix, squared_error_q48};

pub const MAX_TUCKER_RANK: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TuckerConfig {
    pub ranks: [u8; MAX_ORDER],
    pub eigenspace_iterations: u16,
    pub eigenspace_tolerance_q24: i64,
}

impl TuckerConfig {
    pub const KERNEL_DEFAULT: Self = Self {
        ranks: [3, 3, 4, 0],
        eigenspace_iterations: 64,
        eigenspace_tolerance_q24: fixed::ONE / 4096,
    };

    fn validate(self, shape: TensorShape) -> Result<TensorShape, TensorError> {
        if self.eigenspace_iterations == 0 || self.eigenspace_tolerance_q24 < 0 {
            return Err(TensorError::InvalidDimension);
        }

        let mut core_dimensions = [0_u8; MAX_ORDER];
        for mode in 0..shape.order() {
            let rank = self.ranks[mode] as usize;
            if rank == 0 || rank > shape.dimension(mode) || rank > MAX_TUCKER_RANK {
                return Err(TensorError::InvalidDimension);
            }
            core_dimensions[mode] = rank as u8;
        }
        if self.ranks[shape.order()..].iter().any(|rank| *rank != 0) {
            return Err(TensorError::InvalidDimension);
        }

        TensorShape::new(shape.order(), core_dimensions)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TuckerCertificate {
    pub order: u8,
    pub ranks: [u8; MAX_ORDER],
    pub eigenspace_iterations: u16,
    pub relative_error_q24: i64,
    pub retained_energy_q24: i64,
    pub orthogonality_defect_q24: u64,
    pub maximum_eigen_residual_q24: u64,
    pub source_parameters: u16,
    pub compressed_parameters: u16,
    pub tensor_root: u64,
    pub model_root: u64,
    pub root: u64,
}

impl TuckerCertificate {
    pub const EMPTY: Self = Self {
        order: 0,
        ranks: [0; MAX_ORDER],
        eigenspace_iterations: 0,
        relative_error_q24: 0,
        retained_energy_q24: 0,
        orthogonality_defect_q24: 0,
        maximum_eigen_residual_q24: 0,
        source_parameters: 0,
        compressed_parameters: 0,
        tensor_root: 0,
        model_root: 0,
        root: 0,
    };

    pub const fn compresses(self) -> bool {
        self.compressed_parameters < self.source_parameters
    }

    pub fn verify(
        &self,
        secret: u64,
        maximum_relative_error_q24: i64,
        maximum_orthogonality_defect_q24: u64,
        maximum_eigen_residual_q24: u64,
    ) -> bool {
        self.order != 0
            && self.relative_error_q24 <= maximum_relative_error_q24
            && self.retained_energy_q24 >= 0
            && self.orthogonality_defect_q24 <= maximum_orthogonality_defect_q24
            && self.maximum_eigen_residual_q24 <= maximum_eigen_residual_q24
            && self.root == certificate_root(secret, self)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct TuckerModel {
    pub(crate) shape: TensorShape,
    pub(crate) core: DenseTensor,
    pub(crate) factors: [SmallMatrix; MAX_ORDER],
    pub(crate) ranks: [u8; MAX_ORDER],
    pub(crate) initialized: bool,
    pub(crate) model_root: u64,
}

impl TuckerModel {
    pub fn new(shape: TensorShape, ranks: [u8; MAX_ORDER]) -> Result<Self, TensorError> {
        let config = TuckerConfig {
            ranks,
            ..TuckerConfig::KERNEL_DEFAULT
        };
        let core_shape = config.validate(shape)?;
        let mut factors = [SmallMatrix::ZERO; MAX_ORDER];

        for mode in 0..shape.order() {
            factors[mode] = SmallMatrix::zeros(shape.dimension(mode), ranks[mode] as usize)?;
        }

        Ok(Self {
            shape,
            core: DenseTensor::zeros(core_shape),
            factors,
            ranks,
            initialized: false,
            model_root: 0,
        })
    }

    pub const fn shape(&self) -> TensorShape {
        self.shape
    }

    pub const fn ranks(&self) -> [u8; MAX_ORDER] {
        self.ranks
    }

    pub fn core(&self) -> &DenseTensor {
        &self.core
    }

    pub fn factor(&self, mode: usize) -> Result<SmallMatrix, TensorError> {
        if mode >= self.shape.order() {
            return Err(TensorError::Coordinate);
        }
        Ok(self.factors[mode])
    }

    pub const fn root(&self) -> u64 {
        self.model_root
    }

    pub(crate) fn set_factor(
        &mut self,
        mode: usize,
        factor: SmallMatrix,
    ) -> Result<(), TensorError> {
        if mode >= self.shape.order()
            || factor.rows() != self.shape.dimension(mode)
            || factor.columns() != self.ranks[mode] as usize
        {
            return Err(TensorError::ShapeMismatch);
        }
        self.factors[mode] = factor;
        Ok(())
    }

    pub(crate) fn install_core(&mut self, core: &DenseTensor) -> Result<(), TensorError> {
        if !self.core.shape().same_geometry(core.shape()) {
            return Err(TensorError::ShapeMismatch);
        }
        self.core.copy_from(core)?;
        Ok(())
    }

    pub(crate) fn mark_initialized(&mut self) {
        self.initialized = true;
    }

    pub fn reconstruct_into(&self, output: &mut DenseTensor) -> Result<(), TensorError> {
        if !self.initialized || !self.shape.same_geometry(output.shape()) {
            return Err(TensorError::ShapeMismatch);
        }

        for output_linear in 0..self.shape.length() {
            let output_coordinates = self.shape.unravel(output_linear);
            let mut value = 0_i64;

            for core_linear in 0..self.core.shape().length() {
                let core_coordinates = self.core.shape().unravel(core_linear);
                let mut term = self.core.get_linear(core_linear)?;

                for mode in 0..self.shape.order() {
                    term = fixed::mul(
                        term,
                        self.factors[mode].get(output_coordinates[mode], core_coordinates[mode])?,
                    )?;
                }

                value = value.checked_add(term).ok_or(TensorError::Arithmetic)?;
            }

            output.set_linear(output_linear, value)?;
        }

        Ok(())
    }

    pub fn project_coordinate(
        &self,
        source_coordinates: &[usize; MAX_ORDER],
        core_coordinates: &[usize; MAX_ORDER],
    ) -> Result<i64, TensorError> {
        self.shape.offset(source_coordinates)?;
        self.core.shape().offset(core_coordinates)?;

        let mut coefficient = fixed::ONE;
        for mode in 0..self.shape.order() {
            coefficient = fixed::mul(
                coefficient,
                self.factors[mode].get(source_coordinates[mode], core_coordinates[mode])?,
            )?;
        }
        Ok(coefficient)
    }

    pub(crate) fn seal(&mut self, secret: u64) -> Result<(), TensorError> {
        if secret == 0 {
            return Err(TensorError::ZeroSecret);
        }

        let mut state = mix(secret, self.shape.length() as u64);
        state = mix(state, self.core.root(mix(secret, 0x434f_5245))?);
        for mode in 0..self.shape.order() {
            state = mix(
                state,
                self.factors[mode].root(mix(secret, 0x4641_4354 ^ mode as u64))?,
            );
            state = mix(state, self.ranks[mode] as u64);
        }
        self.model_root = state;
        Ok(())
    }
}

pub struct TuckerWorkspace {
    covariance: SmallMatrix,
    reconstruction: DenseTensor,
    eigenspace_certificates: [EigenspaceCertificate; MAX_ORDER],
}

impl TuckerWorkspace {
    pub fn new(shape: TensorShape) -> Result<Self, TensorError> {
        Ok(Self {
            covariance: SmallMatrix::zeros(
                super::tensor::MAX_MODE_DIMENSION,
                super::tensor::MAX_MODE_DIMENSION,
            )?,
            reconstruction: DenseTensor::zeros(shape),
            eigenspace_certificates: [EigenspaceCertificate::EMPTY; MAX_ORDER],
        })
    }

    pub fn reconstruction(&self) -> &DenseTensor {
        &self.reconstruction
    }
}

pub fn fit_tucker_hosvd(
    tensor: &DenseTensor,
    model: &mut TuckerModel,
    workspace: &mut TuckerWorkspace,
    config: TuckerConfig,
    secret: u64,
) -> Result<TuckerCertificate, TensorError> {
    let expected_core_shape = config.validate(tensor.shape())?;
    if secret == 0 {
        return Err(TensorError::ZeroSecret);
    }
    if !tensor.shape().same_geometry(model.shape)
        || !model.core.shape().same_geometry(expected_core_shape)
        || !workspace
            .reconstruction
            .shape()
            .same_geometry(tensor.shape())
    {
        return Err(TensorError::ShapeMismatch);
    }

    let mut maximum_orthogonality_defect = 0_u64;
    let mut maximum_eigen_residual = 0_u64;
    let mut maximum_iterations = 0_u16;

    for mode in 0..tensor.shape().order() {
        build_mode_covariance(tensor, mode, &mut workspace.covariance)?;

        let (basis, _eigenvalues, certificate) = dominant_eigenspace(
            covariance_view(workspace.covariance, tensor.shape().dimension(mode))?,
            config.ranks[mode] as usize,
            config.eigenspace_iterations,
            config.eigenspace_tolerance_q24,
            mix(secret, mode as u64),
        )?;

        model.factors[mode] = basis;
        workspace.eigenspace_certificates[mode] = certificate;
        maximum_orthogonality_defect =
            maximum_orthogonality_defect.max(certificate.orthogonality_defect_q24);
        maximum_eigen_residual =
            maximum_eigen_residual.max(certificate.maximum_relative_residual_q24);
        maximum_iterations = maximum_iterations.max(certificate.iterations);
    }

    project_core(tensor, model)?;
    model.initialized = true;
    model.seal(secret)?;
    model.reconstruct_into(&mut workspace.reconstruction)?;

    let source_energy = tensor.frobenius_squared_q48()?.max(1);
    let error = squared_error_q48(tensor, &workspace.reconstruction)?;
    let relative_error = fixed::ratio_u128(error, source_energy)?;
    let retained_energy = fixed::ONE
        .checked_sub(relative_error)
        .ok_or(TensorError::Arithmetic)?
        .max(0);

    let source_parameters = tensor.shape().length().min(u16::MAX as usize) as u16;
    let mut compressed_parameters = model.core.shape().length();

    for mode in 0..tensor.shape().order() {
        compressed_parameters = compressed_parameters
            .checked_add(
                tensor
                    .shape()
                    .dimension(mode)
                    .checked_mul(config.ranks[mode] as usize)
                    .ok_or(TensorError::Arithmetic)?,
            )
            .ok_or(TensorError::Arithmetic)?;
    }

    let mut certificate = TuckerCertificate {
        order: tensor.shape().order() as u8,
        ranks: config.ranks,
        eigenspace_iterations: maximum_iterations,
        relative_error_q24: relative_error,
        retained_energy_q24: retained_energy,
        orthogonality_defect_q24: maximum_orthogonality_defect,
        maximum_eigen_residual_q24: maximum_eigen_residual,
        source_parameters,
        compressed_parameters: compressed_parameters.min(u16::MAX as usize) as u16,
        tensor_root: tensor.root(secret)?,
        model_root: model.model_root,
        root: 0,
    };
    certificate.root = certificate_root(secret, &certificate);
    Ok(certificate)
}

pub(crate) fn build_mode_covariance(
    tensor: &DenseTensor,
    mode: usize,
    covariance: &mut SmallMatrix,
) -> Result<(), TensorError> {
    let dimension = tensor.shape().dimension(mode);
    *covariance = SmallMatrix::zeros(
        super::tensor::MAX_MODE_DIMENSION,
        super::tensor::MAX_MODE_DIMENSION,
    )?;

    for linear in 0..tensor.shape().length() {
        let base_coordinates = tensor.shape().unravel(linear);
        if base_coordinates[mode] != 0 {
            continue;
        }

        for left in 0..dimension {
            let mut left_coordinates = base_coordinates;
            left_coordinates[mode] = left;
            let left_value = tensor.get(&left_coordinates)?;

            for right in left..dimension {
                let mut right_coordinates = base_coordinates;
                right_coordinates[mode] = right;
                let right_value = tensor.get(&right_coordinates)?;
                let contribution = fixed::mul(left_value, right_value)?;

                covariance.add(left, right, contribution)?;
                if left != right {
                    covariance.add(right, left, contribution)?;
                }
            }
        }
    }

    Ok(())
}

pub(crate) fn covariance_view(
    covariance: SmallMatrix,
    dimension: usize,
) -> Result<SmallMatrix, TensorError> {
    let mut view = SmallMatrix::zeros(dimension, dimension)?;
    for row in 0..dimension {
        for column in 0..dimension {
            view.set(row, column, covariance.get(row, column)?)?;
        }
    }
    Ok(view)
}

pub(crate) fn project_core(
    tensor: &DenseTensor,
    model: &mut TuckerModel,
) -> Result<(), TensorError> {
    model.core.clear();

    for core_linear in 0..model.core.shape().length() {
        let core_coordinates = model.core.shape().unravel(core_linear);
        let mut value = 0_i64;

        for source_linear in 0..tensor.shape().length() {
            let source_coordinates = tensor.shape().unravel(source_linear);
            let mut term = tensor.get_linear(source_linear)?;

            for mode in 0..tensor.shape().order() {
                term = fixed::mul(
                    term,
                    model.factors[mode].get(source_coordinates[mode], core_coordinates[mode])?,
                )?;
            }

            value = value.checked_add(term).ok_or(TensorError::Arithmetic)?;
        }

        model.core.set_linear(core_linear, value)?;
    }

    Ok(())
}

fn certificate_root(secret: u64, certificate: &TuckerCertificate) -> u64 {
    let mut state = mix(secret, certificate.order as u64);
    for rank in certificate.ranks {
        state = mix(state, rank as u64);
    }
    state = mix(state, certificate.eigenspace_iterations as u64);
    state = mix(state, certificate.relative_error_q24 as u64);
    state = mix(state, certificate.retained_energy_q24 as u64);
    state = mix(state, certificate.orthogonality_defect_q24);
    state = mix(state, certificate.maximum_eigen_residual_q24);
    state = mix(
        state,
        certificate.source_parameters as u64 | ((certificate.compressed_parameters as u64) << 16),
    );
    state = mix(state, certificate.tensor_root);
    mix(state, certificate.model_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tucker_compresses_a_separable_tensor() {
        let shape = TensorShape::new(3, [4, 4, 4, 0]).unwrap();
        let mut tensor = DenseTensor::zeros(shape);

        for linear in 0..shape.length() {
            let index = shape.unravel(linear);
            let value = ((index[0] as i64 + 1) * (index[1] as i64 + 2) * (index[2] as i64 + 1))
                << fixed::FRACTION_BITS;
            tensor.set_linear(linear, value).unwrap();
        }

        let ranks = [1, 1, 1, 0];
        let mut model = TuckerModel::new(shape, ranks).unwrap();
        let mut workspace = TuckerWorkspace::new(shape).unwrap();
        let config = TuckerConfig {
            ranks,
            ..TuckerConfig::KERNEL_DEFAULT
        };

        let certificate = fit_tucker_hosvd(&tensor, &mut model, &mut workspace, config, 7).unwrap();

        assert!(certificate.compressed_parameters < 64);
        assert!(certificate.retained_energy_q24 > fixed::ONE / 2);
    }
}
