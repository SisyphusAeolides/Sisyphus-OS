//! Higher-order orthogonal iteration for Tucker refinement.
//!
//! Each factor is updated from the dominant left singular subspace of the
//! tensor projected through every other factor. A sweep is transactional:
//! factors are restored when the full reconstruction error increases beyond
//! fixed-point slack.

use super::fixed;
use super::linalg::{EigenspaceCertificate, SmallMatrix, dominant_eigenspace};
use super::ops::{mode_product_into, mode_product_output_shape};
use super::tensor::{DenseTensor, MAX_ORDER, TensorError, TensorShape, mix, squared_error_q48};
use super::tucker::{TuckerModel, build_mode_covariance, covariance_view, project_core};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HooiConfig {
    pub maximum_sweeps: u16,
    pub relative_tolerance_q24: i64,
    pub monotonic_slack_q24: i64,
    pub eigenspace_iterations: u16,
    pub eigenspace_tolerance_q24: i64,
}

impl HooiConfig {
    pub const KERNEL_DEFAULT: Self = Self {
        maximum_sweeps: 12,
        relative_tolerance_q24: fixed::ONE / 4096,
        monotonic_slack_q24: fixed::ONE / 2048,
        eigenspace_iterations: 48,
        eigenspace_tolerance_q24: fixed::ONE / 4096,
    };

    fn validate(self) -> Result<(), TensorError> {
        if self.maximum_sweeps == 0
            || self.relative_tolerance_q24 < 0
            || self.monotonic_slack_q24 < 0
            || self.eigenspace_iterations == 0
            || self.eigenspace_tolerance_q24 < 0
        {
            return Err(TensorError::InvalidDimension);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HooiCertificate {
    pub sweeps: u16,
    pub converged: bool,
    pub monotone: bool,
    pub initial_error_q48: u128,
    pub final_error_q48: u128,
    pub relative_error_q24: i64,
    pub maximum_orthogonality_defect_q24: u64,
    pub maximum_relative_eigen_residual_q24: u64,
    pub tensor_root: u64,
    pub model_root: u64,
    pub root: u64,
}

impl HooiCertificate {
    pub const EMPTY: Self = Self {
        sweeps: 0,
        converged: false,
        monotone: false,
        initial_error_q48: 0,
        final_error_q48: 0,
        relative_error_q24: 0,
        maximum_orthogonality_defect_q24: 0,
        maximum_relative_eigen_residual_q24: 0,
        tensor_root: 0,
        model_root: 0,
        root: 0,
    };

    pub fn verify(
        &self,
        secret: u64,
        maximum_relative_error_q24: i64,
        maximum_orthogonality_q24: u64,
        maximum_eigen_residual_q24: u64,
    ) -> bool {
        self.sweeps != 0
            && self.monotone
            && self.final_error_q48 <= self.initial_error_q48
            && self.relative_error_q24 <= maximum_relative_error_q24
            && self.maximum_orthogonality_defect_q24 <= maximum_orthogonality_q24
            && self.maximum_relative_eigen_residual_q24 <= maximum_eigen_residual_q24
            && self.root == certificate_root(secret, self)
    }
}

pub struct HooiWorkspace {
    working_a: DenseTensor,
    working_b: DenseTensor,
    reconstruction: DenseTensor,
    covariance: SmallMatrix,
    backup_factors: [SmallMatrix; MAX_ORDER],
    eigenspace_certificates: [EigenspaceCertificate; MAX_ORDER],
}

impl HooiWorkspace {
    pub fn new(shape: TensorShape) -> Result<Self, TensorError> {
        Ok(Self {
            working_a: DenseTensor::zeros(shape),
            working_b: DenseTensor::zeros(shape),
            reconstruction: DenseTensor::zeros(shape),
            covariance: SmallMatrix::zeros(
                super::tensor::MAX_MODE_DIMENSION,
                super::tensor::MAX_MODE_DIMENSION,
            )?,
            backup_factors: [SmallMatrix::ZERO; MAX_ORDER],
            eigenspace_certificates: [EigenspaceCertificate::EMPTY; MAX_ORDER],
        })
    }

    pub fn reconstruction(&self) -> &DenseTensor {
        &self.reconstruction
    }
}

pub fn refine_tucker_hooi(
    tensor: &DenseTensor,
    model: &mut TuckerModel,
    workspace: &mut HooiWorkspace,
    config: HooiConfig,
    secret: u64,
) -> Result<HooiCertificate, TensorError> {
    config.validate()?;
    if secret == 0 {
        return Err(TensorError::ZeroSecret);
    }
    if model.root() == 0
        || !tensor.shape().same_geometry(model.shape())
        || !workspace
            .reconstruction
            .shape()
            .same_geometry(tensor.shape())
    {
        return Err(TensorError::ShapeMismatch);
    }

    model.reconstruct_into(&mut workspace.reconstruction)?;
    let initial_error = squared_error_q48(tensor, &workspace.reconstruction)?;
    let source_energy = tensor.frobenius_squared_q48()?.max(1);
    let mut previous_error = initial_error;
    let mut final_error = initial_error;
    let mut completed_sweeps = 0_u16;
    let mut converged = false;
    let mut monotone = true;
    let mut maximum_orthogonality = 0_u64;
    let mut maximum_residual = 0_u64;

    for sweep in 0..config.maximum_sweeps {
        workspace.backup_factors = model.factors;

        let mut sweep_orthogonality = 0_u64;
        let mut sweep_residual = 0_u64;

        for mode in 0..tensor.shape().order() {
            workspace.working_a.reconfigure(tensor.shape());
            workspace.working_a.copy_from(tensor)?;

            for projected_mode in 0..tensor.shape().order() {
                if projected_mode == mode {
                    continue;
                }

                let factor = model.factor(projected_mode)?;
                let output_shape = mode_product_output_shape(
                    workspace.working_a.shape(),
                    projected_mode,
                    &factor,
                    true,
                )?;
                workspace.working_b.reconfigure(output_shape);
                mode_product_into(
                    &workspace.working_a,
                    projected_mode,
                    &factor,
                    true,
                    &mut workspace.working_b,
                    mix(
                        secret,
                        0x5052_4f4a ^ ((mode as u64) << 16) ^ projected_mode as u64,
                    ),
                )?;
                core::mem::swap(&mut workspace.working_a, &mut workspace.working_b);
            }

            build_mode_covariance(&workspace.working_a, mode, &mut workspace.covariance)?;
            let dimension = tensor.shape().dimension(mode);
            let rank = model.ranks()[mode] as usize;
            let covariance = covariance_view(workspace.covariance, dimension)?;
            let (basis, _eigenvalues, eigenspace_certificate) = dominant_eigenspace(
                covariance,
                rank,
                config.eigenspace_iterations,
                config.eigenspace_tolerance_q24,
                mix(secret, 0x484f_4f49 ^ mode as u64),
            )?;

            model.set_factor(mode, basis)?;
            sweep_orthogonality =
                sweep_orthogonality.max(eigenspace_certificate.orthogonality_defect_q24);
            sweep_residual =
                sweep_residual.max(eigenspace_certificate.maximum_relative_residual_q24);
            workspace.eigenspace_certificates[mode] = eigenspace_certificate;
        }

        project_core(tensor, model)?;
        model.mark_initialized();
        model.reconstruct_into(&mut workspace.reconstruction)?;
        let candidate_error = squared_error_q48(tensor, &workspace.reconstruction)?;
        let slack = error_slack(previous_error, config.monotonic_slack_q24)?;

        if candidate_error > previous_error.saturating_add(slack) {
            model.factors = workspace.backup_factors;
            project_core(tensor, model)?;
            model.reconstruct_into(&mut workspace.reconstruction)?;
            final_error = previous_error;
            monotone = false;
            break;
        }

        maximum_orthogonality = maximum_orthogonality.max(sweep_orthogonality);
        maximum_residual = maximum_residual.max(sweep_residual);
        completed_sweeps = sweep.saturating_add(1);
        final_error = candidate_error;

        let improvement = previous_error.saturating_sub(candidate_error);
        let relative_improvement = fixed::ratio_u128(improvement, previous_error.max(1))?;
        previous_error = candidate_error;

        if relative_improvement <= config.relative_tolerance_q24 {
            converged = true;
            break;
        }
    }

    model.seal(secret)?;
    let relative_error = fixed::ratio_u128(final_error, source_energy)?;

    let mut certificate = HooiCertificate {
        sweeps: completed_sweeps,
        converged,
        monotone,
        initial_error_q48: initial_error,
        final_error_q48: final_error,
        relative_error_q24: relative_error,
        maximum_orthogonality_defect_q24: maximum_orthogonality,
        maximum_relative_eigen_residual_q24: maximum_residual,
        tensor_root: tensor.root(mix(secret, 0x534f_5552))?,
        model_root: model.root(),
        root: 0,
    };
    certificate.root = certificate_root(secret, &certificate);
    Ok(certificate)
}

fn error_slack(error_q48: u128, slack_q24: i64) -> Result<u128, TensorError> {
    if slack_q24 < 0 {
        return Err(TensorError::Arithmetic);
    }
    error_q48
        .checked_mul(slack_q24 as u128)
        .map(|value| value >> fixed::FRACTION_BITS)
        .ok_or(TensorError::Arithmetic)
}

fn certificate_root(secret: u64, certificate: &HooiCertificate) -> u64 {
    let mut state = mix(secret, certificate.sweeps as u64);
    state = mix(state, u64::from(certificate.converged));
    state = mix(state, u64::from(certificate.monotone));
    state = mix(
        state,
        certificate.initial_error_q48 as u64 ^ (certificate.initial_error_q48 >> 64) as u64,
    );
    state = mix(
        state,
        certificate.final_error_q48 as u64 ^ (certificate.final_error_q48 >> 64) as u64,
    );
    state = mix(state, certificate.relative_error_q24 as u64);
    state = mix(state, certificate.maximum_orthogonality_defect_q24);
    state = mix(state, certificate.maximum_relative_eigen_residual_q24);
    state = mix(state, certificate.tensor_root);
    mix(state, certificate.model_root)
}

#[cfg(test)]
mod tests {
    use super::super::hosvd::{HosvdConfig, HosvdWorkspace, fit_st_hosvd};
    use super::*;

    #[test]
    fn hooi_does_not_increase_hosvd_error() {
        let shape = TensorShape::new(3, [4, 4, 4, 0]).unwrap();
        let mut tensor = DenseTensor::zeros(shape);

        for linear in 0..shape.length() {
            let coordinate = shape.unravel(linear);
            let value = ((coordinate[0] as i64 + 1) * (coordinate[1] as i64 + 2)
                + (coordinate[2] as i64 + 1))
                << fixed::FRACTION_BITS;
            tensor.set_linear(linear, value).unwrap();
        }

        let ranks = [2, 2, 2, 0];
        let mut model = TuckerModel::new(shape, ranks).unwrap();
        let mut hosvd_workspace = HosvdWorkspace::new(shape).unwrap();
        fit_st_hosvd(
            &tensor,
            &mut model,
            &mut hosvd_workspace,
            HosvdConfig {
                ranks,
                ..HosvdConfig::KERNEL_DEFAULT
            },
            7,
        )
        .unwrap();

        let mut workspace = HooiWorkspace::new(shape).unwrap();
        let certificate = refine_tucker_hooi(
            &tensor,
            &mut model,
            &mut workspace,
            HooiConfig::KERNEL_DEFAULT,
            9,
        )
        .unwrap();

        assert!(certificate.final_error_q48 <= certificate.initial_error_q48);
    }
}
