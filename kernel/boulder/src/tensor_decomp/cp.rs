//! CANDECOMP/PARAFAC decomposition by bounded alternating least squares.
//!
//! For tensor X and rank R:
//! ```text
//!     X[i0,...,iN] ~= sum_r lambda[r] * product_n A_n[in,r]
//! ```
//!
//! Every mode update solves the regularized normal equations formed from an
//! MTTKRP and a Hadamard product of Gram matrices.  A sweep is committed only
//! when the reconstruction error remains monotone within configured slack.

use super::fixed;
use super::linalg::{MAX_MATRIX_DIMENSION, SmallMatrix, gram, hadamard_assign, solve_spd};
use super::tensor::{DenseTensor, MAX_ORDER, TensorError, TensorShape, mix, squared_error_q48};

pub const MAX_CP_RANK: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpConfig {
    pub rank: u8,
    pub maximum_sweeps: u16,
    pub relative_tolerance_q24: i64,
    pub ridge_q24: i64,
    pub diagonal_floor_q24: i64,
    pub monotonic_slack_q24: i64,
    pub factor_limit_q24: i64,
}

impl CpConfig {
    pub const KERNEL_DEFAULT: Self = Self {
        rank: 3,
        maximum_sweeps: 24,
        relative_tolerance_q24: fixed::ONE / 4096,
        ridge_q24: fixed::ONE / 4096,
        diagonal_floor_q24: fixed::ONE / 65_536,
        monotonic_slack_q24: fixed::ONE / 2048,
        factor_limit_q24: 64 * fixed::ONE,
    };

    fn validate(self, shape: TensorShape) -> Result<(), TensorError> {
        if self.rank == 0
            || self.rank as usize > MAX_CP_RANK
            || self.maximum_sweeps == 0
            || self.relative_tolerance_q24 < 0
            || self.ridge_q24 <= 0
            || self.diagonal_floor_q24 <= 0
            || self.monotonic_slack_q24 < 0
            || self.factor_limit_q24 <= fixed::ONE
            || shape.order() == 0
        {
            return Err(TensorError::InvalidDimension);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpCertificate {
    pub rank: u8,
    pub sweeps: u16,
    pub converged: bool,
    pub monotone: bool,
    pub initial_error_q48: u128,
    pub final_error_q48: u128,
    pub relative_error_q24: i64,
    pub maximum_normal_residual_q24: u64,
    pub tensor_root: u64,
    pub model_root: u64,
    pub root: u64,
}

impl CpCertificate {
    pub const EMPTY: Self = Self {
        rank: 0,
        sweeps: 0,
        converged: false,
        monotone: false,
        initial_error_q48: 0,
        final_error_q48: 0,
        relative_error_q24: 0,
        maximum_normal_residual_q24: 0,
        tensor_root: 0,
        model_root: 0,
        root: 0,
    };

    pub fn verify(
        &self,
        secret: u64,
        maximum_relative_error_q24: i64,
        maximum_normal_residual_q24: u64,
    ) -> bool {
        self.rank != 0
            && self.monotone
            && self.final_error_q48 <= self.initial_error_q48
            && self.relative_error_q24 <= maximum_relative_error_q24
            && self.maximum_normal_residual_q24 <= maximum_normal_residual_q24
            && self.root == certificate_root(secret, self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpModel {
    pub(crate) shape: TensorShape,
    pub(crate) rank: u8,
    pub(crate) factors: [SmallMatrix; MAX_ORDER],
    pub(crate) weights_q24: [i64; MAX_CP_RANK],
    pub(crate) initialized: bool,
    pub(crate) model_root: u64,
}

impl CpModel {
    pub fn new(shape: TensorShape, rank: usize) -> Result<Self, TensorError> {
        if rank == 0 || rank > MAX_CP_RANK {
            return Err(TensorError::InvalidDimension);
        }

        let mut factors = [SmallMatrix::ZERO; MAX_ORDER];
        for mode in 0..shape.order() {
            factors[mode] = SmallMatrix::zeros(shape.dimension(mode), rank)?;
        }

        Ok(Self {
            shape,
            rank: rank as u8,
            factors,
            weights_q24: [0; MAX_CP_RANK],
            initialized: false,
            model_root: 0,
        })
    }

    pub const fn shape(&self) -> TensorShape {
        self.shape
    }

    pub const fn rank(&self) -> usize {
        self.rank as usize
    }

    pub fn factor(&self, mode: usize) -> Result<SmallMatrix, TensorError> {
        if mode >= self.shape.order() {
            return Err(TensorError::Coordinate);
        }
        Ok(self.factors[mode])
    }

    pub fn weights(&self) -> &[i64] {
        &self.weights_q24[..self.rank()]
    }

    pub const fn root(&self) -> u64 {
        self.model_root
    }

    pub const fn initialized(&self) -> bool {
        self.initialized
    }

    pub(crate) fn factor_value(
        &self,
        mode: usize,
        row: usize,
        component: usize,
    ) -> Result<i64, TensorError> {
        if mode >= self.shape.order() || component >= self.rank() {
            return Err(TensorError::Coordinate);
        }
        self.factors[mode].get(row, component)
    }

    pub(crate) fn set_factor_value(
        &mut self,
        mode: usize,
        row: usize,
        component: usize,
        value_q24: i64,
    ) -> Result<(), TensorError> {
        if mode >= self.shape.order() || component >= self.rank() {
            return Err(TensorError::Coordinate);
        }
        self.factors[mode].set(row, component, value_q24)
    }

    pub(crate) fn weight(&self, component: usize) -> Result<i64, TensorError> {
        self.weights_q24
            .get(component)
            .copied()
            .ok_or(TensorError::Coordinate)
    }

    pub(crate) fn set_weight(
        &mut self,
        component: usize,
        value_q24: i64,
    ) -> Result<(), TensorError> {
        let destination = self
            .weights_q24
            .get_mut(component)
            .ok_or(TensorError::Coordinate)?;
        *destination = value_q24;
        Ok(())
    }

    pub fn predict(&self, coordinates: &[usize; MAX_ORDER]) -> Result<i64, TensorError> {
        self.shape.offset(coordinates)?;
        if !self.initialized {
            return Err(TensorError::Arithmetic);
        }

        let mut value = 0_i64;
        for component in 0..self.rank() {
            let mut term = self.weights_q24[component];
            for mode in 0..self.shape.order() {
                term = fixed::mul(term, self.factors[mode].get(coordinates[mode], component)?)?;
            }
            value = value.checked_add(term).ok_or(TensorError::Arithmetic)?;
        }
        Ok(value)
    }

    pub fn reconstruct_into(&self, output: &mut DenseTensor) -> Result<(), TensorError> {
        if !self.shape.same_geometry(output.shape()) {
            return Err(TensorError::ShapeMismatch);
        }

        for linear in 0..self.shape.length() {
            let coordinates = self.shape.unravel(linear);
            output.set_linear(linear, self.predict(&coordinates)?)?;
        }
        Ok(())
    }

    pub fn component_at(
        &self,
        component: usize,
        coordinates: &[usize; MAX_ORDER],
    ) -> Result<i64, TensorError> {
        if component >= self.rank() {
            return Err(TensorError::Coordinate);
        }
        self.shape.offset(coordinates)?;

        let mut value = self.weights_q24[component];
        for mode in 0..self.shape.order() {
            value = fixed::mul(value, self.factors[mode].get(coordinates[mode], component)?)?;
        }
        Ok(value)
    }

    pub(crate) fn seal(&mut self, secret: u64) -> Result<(), TensorError> {
        if secret == 0 {
            return Err(TensorError::ZeroSecret);
        }

        let mut state = mix(secret, self.rank as u64);
        state = mix(state, self.shape.length() as u64);
        for mode in 0..self.shape.order() {
            state = mix(state, self.factors[mode].root(mix(secret, mode as u64))?);
        }
        for weight in self.weights() {
            state = mix(state, *weight as u64);
        }
        self.model_root = state;
        Ok(())
    }
}

pub struct CpWorkspace {
    reconstruction: DenseTensor,
    mttkrp: SmallMatrix,
    normal: SmallMatrix,
    backup_factors: [SmallMatrix; MAX_ORDER],
    backup_weights_q24: [i64; MAX_CP_RANK],
}

impl CpWorkspace {
    pub fn new(shape: TensorShape, rank: usize) -> Result<Self, TensorError> {
        if rank == 0 || rank > MAX_CP_RANK {
            return Err(TensorError::InvalidDimension);
        }

        Ok(Self {
            reconstruction: DenseTensor::zeros(shape),
            mttkrp: SmallMatrix::zeros(super::tensor::MAX_MODE_DIMENSION, rank)?,
            normal: SmallMatrix::zeros(rank, rank)?,
            backup_factors: [SmallMatrix::ZERO; MAX_ORDER],
            backup_weights_q24: [0; MAX_CP_RANK],
        })
    }

    pub fn reconstruction(&self) -> &DenseTensor {
        &self.reconstruction
    }

    pub(crate) fn reconstruction_mut(&mut self) -> &mut DenseTensor {
        &mut self.reconstruction
    }

    fn backup(&mut self, model: &CpModel) {
        self.backup_factors = model.factors;
        self.backup_weights_q24 = model.weights_q24;
    }

    fn restore(&mut self, model: &mut CpModel) {
        model.factors = self.backup_factors;
        model.weights_q24 = self.backup_weights_q24;
    }
}

pub fn fit_cp_als(
    tensor: &DenseTensor,
    model: &mut CpModel,
    workspace: &mut CpWorkspace,
    config: CpConfig,
    secret: u64,
) -> Result<CpCertificate, TensorError> {
    config.validate(tensor.shape())?;
    if secret == 0 {
        return Err(TensorError::ZeroSecret);
    }
    if !tensor.shape().same_geometry(model.shape)
        || model.rank() != config.rank as usize
        || !tensor
            .shape()
            .same_geometry(workspace.reconstruction.shape())
    {
        return Err(TensorError::ShapeMismatch);
    }

    if !model.initialized {
        initialize_model(tensor, model, secret)?;
    }

    model.reconstruct_into(&mut workspace.reconstruction)?;
    let initial_error = squared_error_q48(tensor, &workspace.reconstruction)?;
    let tensor_energy = tensor.frobenius_squared_q48()?.max(1);
    let mut previous_error = initial_error;
    let mut final_error = initial_error;
    let mut completed_sweeps = 0_u16;
    let mut converged = false;
    let mut monotone = true;
    let mut maximum_normal_residual = 0_u64;

    for sweep in 0..config.maximum_sweeps {
        workspace.backup(model);
        let mut sweep_normal_residual = 0_u64;

        for mode in 0..tensor.shape().order() {
            let residual = update_mode(tensor, model, workspace, mode, config)?;
            sweep_normal_residual = sweep_normal_residual.max(residual);
        }

        model.reconstruct_into(&mut workspace.reconstruction)?;
        let candidate_error = squared_error_q48(tensor, &workspace.reconstruction)?;
        let slack = error_slack(previous_error, config.monotonic_slack_q24)?;

        if candidate_error > previous_error.saturating_add(slack) {
            workspace.restore(model);
            model.reconstruct_into(&mut workspace.reconstruction)?;
            final_error = previous_error;
            monotone = false;
            break;
        }

        maximum_normal_residual = maximum_normal_residual.max(sweep_normal_residual);
        final_error = candidate_error;
        completed_sweeps = sweep.saturating_add(1);

        let improvement = previous_error.saturating_sub(candidate_error);
        let relative_improvement = fixed::ratio_u128(improvement, previous_error.max(1))?;

        previous_error = candidate_error;
        if relative_improvement <= config.relative_tolerance_q24 {
            converged = true;
            break;
        }
    }

    canonicalize(model, config.diagonal_floor_q24)?;
    model.seal(secret)?;
    model.reconstruct_into(&mut workspace.reconstruction)?;
    final_error = squared_error_q48(tensor, &workspace.reconstruction)?;

    let relative_error = fixed::ratio_u128(final_error, tensor_energy)?;
    let tensor_root = tensor.root(secret)?;
    let mut certificate = CpCertificate {
        rank: model.rank,
        sweeps: completed_sweeps,
        converged,
        monotone,
        initial_error_q48: initial_error,
        final_error_q48: final_error,
        relative_error_q24: relative_error,
        maximum_normal_residual_q24: maximum_normal_residual,
        tensor_root,
        model_root: model.model_root,
        root: 0,
    };
    certificate.root = certificate_root(secret, &certificate);
    Ok(certificate)
}

pub(crate) fn initialize_model(
    tensor: &DenseTensor,
    model: &mut CpModel,
    secret: u64,
) -> Result<(), TensorError> {
    let tensor_norm_q24 = i64::try_from(fixed::integer_sqrt(tensor.frobenius_squared_q48()?))
        .map_err(|_| TensorError::Arithmetic)?;
    let component_scale = (tensor_norm_q24 / model.rank() as i64).max(fixed::ONE / 1024);

    for component in 0..model.rank() {
        model.weights_q24[component] = component_scale;
    }

    for mode in 0..model.shape.order() {
        for component in 0..model.rank() {
            let mut vector = [0_i64; MAX_MATRIX_DIMENSION];

            for row in 0..model.shape.dimension(mode) {
                let word = mix(
                    secret ^ ((mode as u64) << 48) ^ ((component as u64) << 32),
                    row as u64,
                );
                let signed = ((word >> 32) as i32) as i64;
                vector[row] = (signed >> 8).clamp(-fixed::ONE, fixed::ONE);
            }

            if super::linalg::normalize(
                &mut vector,
                model.shape.dimension(mode),
                fixed::ONE / 65_536,
            )
            .is_err()
            {
                vector.fill(0);
                vector[component % model.shape.dimension(mode)] = fixed::ONE;
            }

            model.factors[mode].set_column(component, &vector)?;
        }
    }

    model.initialized = true;
    Ok(())
}

fn update_mode(
    tensor: &DenseTensor,
    model: &mut CpModel,
    workspace: &mut CpWorkspace,
    mode: usize,
    config: CpConfig,
) -> Result<u64, TensorError> {
    build_mttkrp(tensor, model, mode, &mut workspace.mttkrp)?;
    build_normal_matrix(model, mode, config.ridge_q24, &mut workspace.normal)?;

    let dimension = model.shape.dimension(mode);
    let rank = model.rank();

    for row in 0..dimension {
        let mut rhs = [0_i64; MAX_MATRIX_DIMENSION];
        for component in 0..rank {
            rhs[component] = workspace.mttkrp.get(row, component)?;
        }

        let solution = solve_spd(workspace.normal, &rhs, rank, config.diagonal_floor_q24)?;

        for component in 0..rank {
            model.factors[mode].set(
                row,
                component,
                solution[component].clamp(-config.factor_limit_q24, config.factor_limit_q24),
            )?;
        }
    }

    let residual = normal_equation_residual(
        model.factors[mode],
        workspace.normal,
        workspace.mttkrp,
        dimension,
        rank,
    )?;

    normalize_mode_columns(model, mode, config.diagonal_floor_q24)?;

    Ok(residual)
}

fn build_mttkrp(
    tensor: &DenseTensor,
    model: &CpModel,
    mode: usize,
    output: &mut SmallMatrix,
) -> Result<(), TensorError> {
    *output = SmallMatrix::zeros(super::tensor::MAX_MODE_DIMENSION, model.rank())?;

    for linear in 0..tensor.shape().length() {
        let coordinates = tensor.shape().unravel(linear);
        let observed = tensor.get_linear(linear)?;

        for component in 0..model.rank() {
            let mut product = model.weights_q24[component];
            for other_mode in 0..tensor.shape().order() {
                if other_mode == mode {
                    continue;
                }
                product = fixed::mul(
                    product,
                    model.factors[other_mode].get(coordinates[other_mode], component)?,
                )?;
            }

            let contribution = fixed::mul(observed, product)?;
            output.add(coordinates[mode], component, contribution)?;
        }
    }

    Ok(())
}

fn build_normal_matrix(
    model: &CpModel,
    excluded_mode: usize,
    ridge_q24: i64,
    output: &mut SmallMatrix,
) -> Result<(), TensorError> {
    let rank = model.rank();
    *output = SmallMatrix::zeros(rank, rank)?;

    for row in 0..rank {
        for column in 0..rank {
            output.set(row, column, fixed::ONE)?;
        }
    }

    for mode in 0..model.shape.order() {
        if mode == excluded_mode {
            continue;
        }
        let factor_gram = gram(model.factors[mode])?;
        hadamard_assign(output, factor_gram)?;
    }

    for row in 0..rank {
        for column in 0..rank {
            let weight_product = fixed::mul(model.weights_q24[row], model.weights_q24[column])?;
            output.set(
                row,
                column,
                fixed::mul(output.get(row, column)?, weight_product)?,
            )?;
        }

        output.add(row, row, ridge_q24)?;
    }

    Ok(())
}

fn normalize_mode_columns(
    model: &mut CpModel,
    mode: usize,
    floor_q24: i64,
) -> Result<(), TensorError> {
    for component in 0..model.rank() {
        let mut column = model.factors[mode].column(component)?;
        let magnitude =
            match super::linalg::normalize(&mut column, model.shape.dimension(mode), floor_q24) {
                Ok(magnitude) => magnitude,
                Err(_) => {
                    column.fill(0);
                    column[component % model.shape.dimension(mode)] = fixed::ONE;
                    fixed::ONE
                }
            };

        model.factors[mode].set_column(component, &column)?;
        model.weights_q24[component] = fixed::mul(model.weights_q24[component], magnitude)?;
    }
    Ok(())
}

fn canonicalize(model: &mut CpModel, floor_q24: i64) -> Result<(), TensorError> {
    for component in 0..model.rank() {
        for mode in 0..model.shape.order() {
            let mut column = model.factors[mode].column(component)?;
            let magnitude = super::linalg::norm(&column, model.shape.dimension(mode))?;

            if magnitude > floor_q24 {
                for value in &mut column[..model.shape.dimension(mode)] {
                    *value = fixed::div(*value, magnitude)?;
                }
                model.weights_q24[component] = fixed::mul(model.weights_q24[component], magnitude)?;
                model.factors[mode].set_column(component, &column)?;
            }
        }

        if model.weights_q24[component] < 0 {
            model.weights_q24[component] = model.weights_q24[component]
                .checked_neg()
                .ok_or(TensorError::Arithmetic)?;
            let mut first = model.factors[0].column(component)?;
            for value in &mut first[..model.shape.dimension(0)] {
                *value = value.checked_neg().ok_or(TensorError::Arithmetic)?;
            }
            model.factors[0].set_column(component, &first)?;
        }
    }

    sort_components(model)
}

fn sort_components(model: &mut CpModel) -> Result<(), TensorError> {
    for left in 0..model.rank() {
        let mut best = left;
        for right in left + 1..model.rank() {
            if model.weights_q24[right] > model.weights_q24[best] {
                best = right;
            }
        }

        if best != left {
            model.weights_q24.swap(left, best);
            for mode in 0..model.shape.order() {
                let left_column = model.factors[mode].column(left)?;
                let best_column = model.factors[mode].column(best)?;
                model.factors[mode].set_column(left, &best_column)?;
                model.factors[mode].set_column(best, &left_column)?;
            }
        }
    }
    Ok(())
}

fn normal_equation_residual(
    factor: SmallMatrix,
    normal: SmallMatrix,
    mttkrp: SmallMatrix,
    rows: usize,
    rank: usize,
) -> Result<u64, TensorError> {
    let mut maximum_residual = 0_u64;
    let mut maximum_rhs = 0_u64;

    for row in 0..rows {
        for column in 0..rank {
            let mut image = 0_i64;
            for inner in 0..rank {
                image = image
                    .checked_add(fixed::mul(
                        factor.get(row, inner)?,
                        normal.get(inner, column)?,
                    )?)
                    .ok_or(TensorError::Arithmetic)?;
            }

            let rhs = mttkrp.get(row, column)?;
            maximum_residual = maximum_residual.max(image.abs_diff(rhs));
            maximum_rhs = maximum_rhs.max(rhs.unsigned_abs());
        }
    }

    let denominator = maximum_rhs.max((fixed::ONE / 65_536) as u64);
    let relative = (maximum_residual as u128)
        .checked_shl(fixed::FRACTION_BITS)
        .ok_or(TensorError::Arithmetic)?
        / denominator as u128;

    u64::try_from(relative).map_err(|_| TensorError::Arithmetic)
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

fn certificate_root(secret: u64, certificate: &CpCertificate) -> u64 {
    let mut state = mix(secret, certificate.rank as u64);
    state = mix(state, certificate.sweeps as u64);
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
    state = mix(state, certificate.maximum_normal_residual_q24);
    state = mix(state, certificate.tensor_root);
    mix(state, certificate.model_root)
}

#[cfg(test)]
mod tests {
    use super::super::tensor::TensorShape;
    use super::*;

    fn rank_one_tensor() -> DenseTensor {
        let shape = TensorShape::new(3, [3, 3, 3, 0]).unwrap();
        let mut tensor = DenseTensor::zeros(shape);
        let a = [fixed::ONE, 2 * fixed::ONE, 3 * fixed::ONE];
        let b = [fixed::ONE, fixed::ONE / 2, -fixed::ONE];
        let c = [2 * fixed::ONE, fixed::ONE, fixed::ONE / 4];

        for linear in 0..shape.length() {
            let index = shape.unravel(linear);
            let value =
                fixed::mul(fixed::mul(a[index[0]], b[index[1]]).unwrap(), c[index[2]]).unwrap();
            tensor.set_linear(linear, value).unwrap();
        }
        tensor
    }

    #[test]
    fn cp_als_reduces_rank_one_error() {
        let tensor = rank_one_tensor();
        let shape = tensor.shape();
        let mut model = CpModel::new(shape, 1).unwrap();
        let mut workspace = CpWorkspace::new(shape, 1).unwrap();
        let mut config = CpConfig::KERNEL_DEFAULT;
        config.rank = 1;
        config.maximum_sweeps = 32;

        let certificate = fit_cp_als(&tensor, &mut model, &mut workspace, config, 7).unwrap();

        assert!(certificate.final_error_q48 < certificate.initial_error_q48);
        assert!(certificate.model_root != 0);
    }
}
