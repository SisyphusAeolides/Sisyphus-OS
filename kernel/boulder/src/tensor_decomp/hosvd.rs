//! Sequentially truncated higher-order singular-value decomposition.
//!
//! This is ST-HOSVD: each mode is unfolded implicitly through its covariance,
//! its complete left singular system is estimated, the requested leading
//! subspace is retained, and the working tensor is projected before the next
//! mode. The final working tensor is the Tucker core.

use super::fixed;
use super::linalg::{
    EigenspaceCertificate, MAX_MATRIX_DIMENSION, SmallMatrix, dominant_eigenspace,
};
use super::ops::{mode_product_into, mode_product_output_shape};
use super::tensor::{DenseTensor, MAX_ORDER, TensorError, TensorShape, mix, squared_error_q48};
use super::tucker::{TuckerModel, build_mode_covariance, covariance_view};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HosvdConfig {
    pub ranks: [u8; MAX_ORDER],
    pub eigenspace_iterations: u16,
    pub eigenspace_tolerance_q24: i64,
}

impl HosvdConfig {
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
            if rank == 0 || rank > shape.dimension(mode) {
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
pub struct HosvdCertificate {
    pub order: u8,
    pub ranks: [u8; MAX_ORDER],
    pub maximum_iterations: u16,
    pub relative_error_q24: i64,
    pub retained_energy_q24: i64,
    pub maximum_orthogonality_defect_q24: u64,
    pub maximum_relative_eigen_residual_q24: u64,
    pub discarded_mode_energy_q24: [i64; MAX_ORDER],
    pub singular_values_q24: [[i64; MAX_MATRIX_DIMENSION]; MAX_ORDER],
    pub source_parameters: u16,
    pub compressed_parameters: u16,
    pub tensor_root: u64,
    pub core_root: u64,
    pub model_root: u64,
    pub root: u64,
}

impl HosvdCertificate {
    pub const EMPTY: Self = Self {
        order: 0,
        ranks: [0; MAX_ORDER],
        maximum_iterations: 0,
        relative_error_q24: 0,
        retained_energy_q24: 0,
        maximum_orthogonality_defect_q24: 0,
        maximum_relative_eigen_residual_q24: 0,
        discarded_mode_energy_q24: [0; MAX_ORDER],
        singular_values_q24: [[0; MAX_MATRIX_DIMENSION]; MAX_ORDER],
        source_parameters: 0,
        compressed_parameters: 0,
        tensor_root: 0,
        core_root: 0,
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
        maximum_relative_eigen_residual_q24: u64,
    ) -> bool {
        self.order != 0
            && self.relative_error_q24 <= maximum_relative_error_q24
            && self.retained_energy_q24 >= 0
            && self.maximum_orthogonality_defect_q24 <= maximum_orthogonality_defect_q24
            && self.maximum_relative_eigen_residual_q24 <= maximum_relative_eigen_residual_q24
            && self.root == certificate_root(secret, self)
    }
}

pub struct HosvdWorkspace {
    working_a: DenseTensor,
    working_b: DenseTensor,
    reconstruction: DenseTensor,
    covariance: SmallMatrix,
    eigenspace_certificates: [EigenspaceCertificate; MAX_ORDER],
}

impl HosvdWorkspace {
    pub fn new(shape: TensorShape) -> Result<Self, TensorError> {
        Ok(Self {
            working_a: DenseTensor::zeros(shape),
            working_b: DenseTensor::zeros(shape),
            reconstruction: DenseTensor::zeros(shape),
            covariance: SmallMatrix::zeros(
                super::tensor::MAX_MODE_DIMENSION,
                super::tensor::MAX_MODE_DIMENSION,
            )?,
            eigenspace_certificates: [EigenspaceCertificate::EMPTY; MAX_ORDER],
        })
    }

    pub fn reconstruction(&self) -> &DenseTensor {
        &self.reconstruction
    }
}

pub fn fit_st_hosvd(
    tensor: &DenseTensor,
    model: &mut TuckerModel,
    workspace: &mut HosvdWorkspace,
    config: HosvdConfig,
    secret: u64,
) -> Result<HosvdCertificate, TensorError> {
    if secret == 0 {
        return Err(TensorError::ZeroSecret);
    }

    let core_shape = config.validate(tensor.shape())?;
    if !tensor.shape().same_geometry(model.shape())
        || model.ranks() != config.ranks
        || !model.core().shape().same_geometry(core_shape)
        || !workspace
            .reconstruction
            .shape()
            .same_geometry(tensor.shape())
    {
        return Err(TensorError::ShapeMismatch);
    }

    workspace.working_a.reconfigure(tensor.shape());
    workspace.working_a.copy_from(tensor)?;

    let mut singular_values = [[0_i64; MAX_MATRIX_DIMENSION]; MAX_ORDER];
    let mut discarded_mode_energy = [0_i64; MAX_ORDER];
    let mut maximum_orthogonality = 0_u64;
    let mut maximum_residual = 0_u64;
    let mut maximum_iterations = 0_u16;

    for mode in 0..tensor.shape().order() {
        build_mode_covariance(&workspace.working_a, mode, &mut workspace.covariance)?;

        let dimension = workspace.working_a.shape().dimension(mode);
        let covariance = covariance_view(workspace.covariance, dimension)?;
        let rank = config.ranks[mode] as usize;
        let (factor, eigenvalues, eigenspace_certificate) = dominant_eigenspace(
            covariance,
            rank,
            config.eigenspace_iterations,
            config.eigenspace_tolerance_q24,
            mix(secret, 0x484f_5356 ^ mode as u64),
        )?;
        model.set_factor(mode, factor)?;

        let mut total_energy = 0_i64;
        for diagonal in 0..dimension {
            total_energy = total_energy
                .checked_add(workspace.covariance.get(diagonal, diagonal)?.max(0))
                .ok_or(TensorError::Arithmetic)?;
        }

        let mut retained = 0_i64;
        for index in 0..rank {
            let eigenvalue = eigenvalues[index].max(0);
            singular_values[mode][index] = fixed::sqrt(eigenvalue)?;
            retained = retained
                .checked_add(eigenvalue)
                .ok_or(TensorError::Arithmetic)?;
        }
        let discarded = total_energy
            .checked_sub(retained)
            .ok_or(TensorError::Arithmetic)?
            .max(0);
        discarded_mode_energy[mode] = if total_energy > 0 {
            fixed::div(discarded, total_energy)?
        } else {
            0
        };

        maximum_orthogonality =
            maximum_orthogonality.max(eigenspace_certificate.orthogonality_defect_q24);
        maximum_residual =
            maximum_residual.max(eigenspace_certificate.maximum_relative_residual_q24);
        maximum_iterations = maximum_iterations.max(eigenspace_certificate.iterations);
        workspace.eigenspace_certificates[mode] = eigenspace_certificate;

        let output_shape =
            mode_product_output_shape(workspace.working_a.shape(), mode, &factor, true)?;
        workspace.working_b.reconfigure(output_shape);
        mode_product_into(
            &workspace.working_a,
            mode,
            &factor,
            true,
            &mut workspace.working_b,
            mix(secret, 0x5052_4f4a ^ mode as u64),
        )?;
        core::mem::swap(&mut workspace.working_a, &mut workspace.working_b);
    }

    if !workspace
        .working_a
        .shape()
        .same_geometry(model.core().shape())
    {
        return Err(TensorError::ShapeMismatch);
    }

    model.install_core(&workspace.working_a)?;
    model.mark_initialized();
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
    let mut compressed_parameters = model.core().shape().length();
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

    let mut certificate = HosvdCertificate {
        order: tensor.shape().order() as u8,
        ranks: config.ranks,
        maximum_iterations,
        relative_error_q24: relative_error,
        retained_energy_q24: retained_energy,
        maximum_orthogonality_defect_q24: maximum_orthogonality,
        maximum_relative_eigen_residual_q24: maximum_residual,
        discarded_mode_energy_q24: discarded_mode_energy,
        singular_values_q24: singular_values,
        source_parameters,
        compressed_parameters: compressed_parameters.min(u16::MAX as usize) as u16,
        tensor_root: tensor.root(mix(secret, 0x534f_5552))?,
        core_root: model.core().root(mix(secret, 0x434f_5245))?,
        model_root: model.root(),
        root: 0,
    };
    certificate.root = certificate_root(secret, &certificate);
    Ok(certificate)
}

fn certificate_root(secret: u64, certificate: &HosvdCertificate) -> u64 {
    let mut state = mix(
        secret,
        certificate.order as u64 | ((certificate.maximum_iterations as u64) << 8),
    );
    for rank in certificate.ranks {
        state = mix(state, rank as u64);
    }
    state = mix(state, certificate.relative_error_q24 as u64);
    state = mix(state, certificate.retained_energy_q24 as u64);
    state = mix(state, certificate.maximum_orthogonality_defect_q24);
    state = mix(state, certificate.maximum_relative_eigen_residual_q24);
    for mode in 0..MAX_ORDER {
        state = mix(state, certificate.discarded_mode_energy_q24[mode] as u64);
        for singular in certificate.singular_values_q24[mode] {
            state = mix(state, singular as u64);
        }
    }
    state = mix(
        state,
        certificate.source_parameters as u64 | ((certificate.compressed_parameters as u64) << 16),
    );
    state = mix(state, certificate.tensor_root);
    state = mix(state, certificate.core_root);
    mix(state, certificate.model_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequential_hosvd_builds_a_tucker_core() {
        let shape = TensorShape::new(3, [4, 4, 4, 0]).unwrap();
        let mut tensor = DenseTensor::zeros(shape);

        for linear in 0..shape.length() {
            let coordinate = shape.unravel(linear);
            let value = ((coordinate[0] as i64 + 1)
                * (coordinate[1] as i64 + 2)
                * (coordinate[2] as i64 + 3))
                << fixed::FRACTION_BITS;
            tensor.set_linear(linear, value).unwrap();
        }

        let ranks = [1, 1, 1, 0];
        let mut model = TuckerModel::new(shape, ranks).unwrap();
        let mut workspace = HosvdWorkspace::new(shape).unwrap();
        let config = HosvdConfig {
            ranks,
            ..HosvdConfig::KERNEL_DEFAULT
        };

        let certificate = fit_st_hosvd(&tensor, &mut model, &mut workspace, config, 7).unwrap();

        assert!(certificate.retained_energy_q24 > fixed::ONE / 2);
        assert!(certificate.model_root != 0);
    }
}
