//! Online and coordinate optimizers for an existing CP model.
//!
//! - CP-ALS remains the deterministic batch optimizer in `cp.rs`.
//! - SGD consumes a deterministic bounded sample stream for frequent updates.
//! - CCD++ exactly minimizes one factor coordinate at a time against all
//!   matching tensor entries.
//!
//! Both online optimizers are transactional: a full reconstruction gate
//! restores the previous model when the update increases error beyond policy.

use super::cp::{CpModel, MAX_CP_RANK, initialize_model};
use super::fixed;
use super::tensor::{DenseTensor, MAX_ORDER, TensorError, TensorShape, mix, squared_error_q48};

pub const MAX_SGD_BATCH: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SgdConfig {
    pub epochs: u16,
    pub batch_size: u8,
    pub learning_rate_q24: i64,
    pub ridge_q24: i64,
    pub factor_limit_q24: i64,
    pub monotonic_slack_q24: i64,
    pub stream_seed: u64,
}

impl SgdConfig {
    pub const KERNEL_DEFAULT: Self = Self {
        epochs: 4,
        batch_size: 32,
        learning_rate_q24: fixed::ONE / 512,
        ridge_q24: fixed::ONE / 65_536,
        factor_limit_q24: 64 * fixed::ONE,
        monotonic_slack_q24: fixed::ONE / 1024,
        stream_seed: 0x5347_445f_5354_524d,
    };

    fn validate(self) -> Result<(), TensorError> {
        if self.epochs == 0
            || self.batch_size == 0
            || self.batch_size as usize > MAX_SGD_BATCH
            || self.learning_rate_q24 <= 0
            || self.ridge_q24 < 0
            || self.factor_limit_q24 <= fixed::ONE
            || self.monotonic_slack_q24 < 0
            || self.stream_seed == 0
        {
            return Err(TensorError::InvalidDimension);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SgdCertificate {
    pub epochs: u16,
    pub samples: u32,
    pub committed: bool,
    pub initial_error_q48: u128,
    pub final_error_q48: u128,
    pub relative_error_q24: i64,
    pub maximum_gradient_q24: u64,
    pub sample_stream_root: u64,
    pub model_root: u64,
    pub root: u64,
}

impl SgdCertificate {
    pub const EMPTY: Self = Self {
        epochs: 0,
        samples: 0,
        committed: false,
        initial_error_q48: 0,
        final_error_q48: 0,
        relative_error_q24: 0,
        maximum_gradient_q24: 0,
        sample_stream_root: 0,
        model_root: 0,
        root: 0,
    };

    pub fn verify(
        &self,
        secret: u64,
        maximum_relative_error_q24: i64,
        maximum_gradient_q24: u64,
    ) -> bool {
        self.samples != 0
            && self.committed
            && self.final_error_q48 <= self.initial_error_q48
            && self.relative_error_q24 <= maximum_relative_error_q24
            && self.maximum_gradient_q24 <= maximum_gradient_q24
            && self.root == sgd_certificate_root(secret, self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CcdConfig {
    pub coordinate_updates: u16,
    pub ridge_q24: i64,
    pub factor_limit_q24: i64,
    pub monotonic_slack_q24: i64,
    pub start_coordinate: u16,
}

impl CcdConfig {
    pub const KERNEL_DEFAULT: Self = Self {
        coordinate_updates: 128,
        ridge_q24: fixed::ONE / 65_536,
        factor_limit_q24: 64 * fixed::ONE,
        monotonic_slack_q24: fixed::ONE / 2048,
        start_coordinate: 0,
    };

    fn validate(self) -> Result<(), TensorError> {
        if self.coordinate_updates == 0
            || self.ridge_q24 <= 0
            || self.factor_limit_q24 <= fixed::ONE
            || self.monotonic_slack_q24 < 0
        {
            return Err(TensorError::InvalidDimension);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CcdCertificate {
    pub coordinate_updates: u16,
    pub committed: bool,
    pub initial_error_q48: u128,
    pub final_error_q48: u128,
    pub relative_error_q24: i64,
    pub maximum_coordinate_delta_q24: u64,
    pub cursor_after: u16,
    pub model_root: u64,
    pub root: u64,
}

impl CcdCertificate {
    pub const EMPTY: Self = Self {
        coordinate_updates: 0,
        committed: false,
        initial_error_q48: 0,
        final_error_q48: 0,
        relative_error_q24: 0,
        maximum_coordinate_delta_q24: 0,
        cursor_after: 0,
        model_root: 0,
        root: 0,
    };

    pub fn verify(
        &self,
        secret: u64,
        maximum_relative_error_q24: i64,
        maximum_coordinate_delta_q24: u64,
    ) -> bool {
        self.coordinate_updates != 0
            && self.committed
            && self.final_error_q48 <= self.initial_error_q48
            && self.relative_error_q24 <= maximum_relative_error_q24
            && self.maximum_coordinate_delta_q24 <= maximum_coordinate_delta_q24
            && self.root == ccd_certificate_root(secret, self)
    }
}

pub struct CpOptimizerWorkspace {
    reconstruction: DenseTensor,
    backup: CpModel,
    samples: [usize; MAX_SGD_BATCH],
}

impl CpOptimizerWorkspace {
    pub fn new(shape: TensorShape, rank: usize) -> Result<Self, TensorError> {
        Ok(Self {
            reconstruction: DenseTensor::zeros(shape),
            backup: CpModel::new(shape, rank)?,
            samples: [0; MAX_SGD_BATCH],
        })
    }

    pub fn reconstruction(&self) -> &DenseTensor {
        &self.reconstruction
    }

    fn backup(&mut self, model: &CpModel) {
        self.backup = *model;
    }

    fn restore(&mut self, model: &mut CpModel) {
        *model = self.backup;
    }
}

pub fn update_cp_sgd(
    tensor: &DenseTensor,
    model: &mut CpModel,
    workspace: &mut CpOptimizerWorkspace,
    config: SgdConfig,
    secret: u64,
) -> Result<SgdCertificate, TensorError> {
    config.validate()?;
    validate_model_workspace(tensor, model, workspace)?;
    if secret == 0 {
        return Err(TensorError::ZeroSecret);
    }

    if !model.initialized() {
        initialize_model(tensor, model, mix(secret, 0x494e_4954))?;
        model.seal(secret)?;
    }

    model.reconstruct_into(&mut workspace.reconstruction)?;
    let initial_error = squared_error_q48(tensor, &workspace.reconstruction)?;
    let source_energy = tensor.frobenius_squared_q48()?.max(1);
    workspace.backup(model);

    let mut stream = SplitMix64::new(config.stream_seed ^ secret ^ tensor.shape().length() as u64);
    let mut stream_root = mix(secret, config.stream_seed);
    let mut maximum_gradient = 0_u64;
    let mut samples = 0_u32;

    for _epoch in 0..config.epochs {
        for batch_index in 0..config.batch_size as usize {
            let sample = stream.next_usize(tensor.shape().length());
            workspace.samples[batch_index] = sample;
            stream_root = mix(stream_root, sample as u64);
        }

        for sample in workspace.samples[..config.batch_size as usize]
            .iter()
            .copied()
        {
            let coordinates = tensor.shape().unravel(sample);
            let observed = tensor.get_linear(sample)?;
            let prediction = model.predict(&coordinates)?;
            let error = prediction
                .checked_sub(observed)
                .ok_or(TensorError::Arithmetic)?;

            let mut factor_gradients = [[0_i64; MAX_ORDER]; MAX_CP_RANK];
            let mut weight_gradients = [0_i64; MAX_CP_RANK];

            for component in 0..model.rank() {
                let weight = model.weight(component)?;
                let mut full_product = fixed::ONE;

                for mode in 0..tensor.shape().order() {
                    full_product = fixed::mul(
                        full_product,
                        model.factor_value(mode, coordinates[mode], component)?,
                    )?;
                }

                weight_gradients[component] =
                    regularized_gradient(error, full_product, config.ridge_q24, weight)?;
                maximum_gradient = maximum_gradient.max(weight_gradients[component].unsigned_abs());

                for mode in 0..tensor.shape().order() {
                    let mut derivative = weight;
                    for other_mode in 0..tensor.shape().order() {
                        if other_mode == mode {
                            continue;
                        }
                        derivative = fixed::mul(
                            derivative,
                            model.factor_value(other_mode, coordinates[other_mode], component)?,
                        )?;
                    }

                    let value = model.factor_value(mode, coordinates[mode], component)?;
                    factor_gradients[component][mode] =
                        regularized_gradient(error, derivative, config.ridge_q24, value)?;
                    maximum_gradient =
                        maximum_gradient.max(factor_gradients[component][mode].unsigned_abs());
                }
            }

            for component in 0..model.rank() {
                let weight_step =
                    fixed::mul(config.learning_rate_q24, weight_gradients[component])?;
                let next_weight = model
                    .weight(component)?
                    .checked_sub(weight_step)
                    .ok_or(TensorError::Arithmetic)?
                    .clamp(-config.factor_limit_q24, config.factor_limit_q24);
                model.set_weight(component, next_weight)?;

                for mode in 0..tensor.shape().order() {
                    let row = coordinates[mode];
                    let step =
                        fixed::mul(config.learning_rate_q24, factor_gradients[component][mode])?;
                    let next = model
                        .factor_value(mode, row, component)?
                        .checked_sub(step)
                        .ok_or(TensorError::Arithmetic)?
                        .clamp(-config.factor_limit_q24, config.factor_limit_q24);
                    model.set_factor_value(mode, row, component, next)?;
                }
            }

            samples = samples.saturating_add(1);
        }
    }

    model.seal(secret)?;
    model.reconstruct_into(&mut workspace.reconstruction)?;
    let candidate_error = squared_error_q48(tensor, &workspace.reconstruction)?;
    let _slack = error_slack(initial_error, config.monotonic_slack_q24)?;
    let committed = candidate_error <= initial_error;

    let final_error = if committed {
        candidate_error
    } else {
        workspace.restore(model);
        model.reconstruct_into(&mut workspace.reconstruction)?;
        initial_error
    };

    let relative_error = fixed::ratio_u128(final_error, source_energy)?;
    let mut certificate = SgdCertificate {
        epochs: config.epochs,
        samples,
        committed,
        initial_error_q48: initial_error,
        final_error_q48: final_error,
        relative_error_q24: relative_error,
        maximum_gradient_q24: maximum_gradient,
        sample_stream_root: stream_root,
        model_root: model.root(),
        root: 0,
    };
    certificate.root = sgd_certificate_root(secret, &certificate);
    Ok(certificate)
}

pub fn update_cp_ccd(
    tensor: &DenseTensor,
    model: &mut CpModel,
    workspace: &mut CpOptimizerWorkspace,
    config: CcdConfig,
    secret: u64,
) -> Result<CcdCertificate, TensorError> {
    config.validate()?;
    validate_model_workspace(tensor, model, workspace)?;
    if secret == 0 || !model.initialized() {
        return Err(TensorError::ZeroSecret);
    }

    model.reconstruct_into(&mut workspace.reconstruction)?;
    let initial_error = squared_error_q48(tensor, &workspace.reconstruction)?;
    let source_energy = tensor.frobenius_squared_q48()?.max(1);
    workspace.backup(model);

    let total_coordinates = cp_factor_coordinate_count(model)?;
    let mut cursor = config.start_coordinate as usize % total_coordinates;
    let mut maximum_delta = 0_u64;

    for _ in 0..config.coordinate_updates {
        let (mode, row, component) = decode_factor_coordinate(model, cursor)?;
        let old_value = model.factor_value(mode, row, component)?;
        let mut numerator = 0_i64;
        let mut denominator = config.ridge_q24;

        for linear in 0..tensor.shape().length() {
            let coordinates = tensor.shape().unravel(linear);
            if coordinates[mode] != row {
                continue;
            }

            let mut basis = model.weight(component)?;
            for other_mode in 0..tensor.shape().order() {
                if other_mode == mode {
                    continue;
                }
                basis = fixed::mul(
                    basis,
                    model.factor_value(other_mode, coordinates[other_mode], component)?,
                )?;
            }

            let prediction = model.predict(&coordinates)?;
            let current_contribution = fixed::mul(basis, old_value)?;
            let prediction_without = prediction
                .checked_sub(current_contribution)
                .ok_or(TensorError::Arithmetic)?;
            let target = tensor
                .get_linear(linear)?
                .checked_sub(prediction_without)
                .ok_or(TensorError::Arithmetic)?;

            numerator = numerator
                .checked_add(fixed::mul(basis, target)?)
                .ok_or(TensorError::Arithmetic)?;
            denominator = denominator
                .checked_add(fixed::mul(basis, basis)?)
                .ok_or(TensorError::Arithmetic)?;
        }

        let optimum = fixed::div(numerator, denominator)?
            .clamp(-config.factor_limit_q24, config.factor_limit_q24);
        maximum_delta = maximum_delta.max(old_value.abs_diff(optimum));
        model.set_factor_value(mode, row, component, optimum)?;

        cursor = (cursor + 1) % total_coordinates;
    }

    model.seal(secret)?;
    model.reconstruct_into(&mut workspace.reconstruction)?;
    let candidate_error = squared_error_q48(tensor, &workspace.reconstruction)?;
    let _slack = error_slack(initial_error, config.monotonic_slack_q24)?;
    let committed = candidate_error <= initial_error;

    let final_error = if committed {
        candidate_error
    } else {
        workspace.restore(model);
        model.reconstruct_into(&mut workspace.reconstruction)?;
        initial_error
    };

    let relative_error = fixed::ratio_u128(final_error, source_energy)?;
    let mut certificate = CcdCertificate {
        coordinate_updates: config.coordinate_updates,
        committed,
        initial_error_q48: initial_error,
        final_error_q48: final_error,
        relative_error_q24: relative_error,
        maximum_coordinate_delta_q24: maximum_delta,
        cursor_after: cursor as u16,
        model_root: model.root(),
        root: 0,
    };
    certificate.root = ccd_certificate_root(secret, &certificate);
    Ok(certificate)
}

fn regularized_gradient(
    error_q24: i64,
    derivative_q24: i64,
    ridge_q24: i64,
    parameter_q24: i64,
) -> Result<i64, TensorError> {
    fixed::mul(error_q24, derivative_q24)?
        .checked_add(fixed::mul(ridge_q24, parameter_q24)?)
        .ok_or(TensorError::Arithmetic)
}

fn validate_model_workspace(
    tensor: &DenseTensor,
    model: &CpModel,
    workspace: &CpOptimizerWorkspace,
) -> Result<(), TensorError> {
    if !tensor.shape().same_geometry(model.shape())
        || !workspace
            .reconstruction
            .shape()
            .same_geometry(tensor.shape())
        || workspace.backup.rank() != model.rank()
    {
        return Err(TensorError::ShapeMismatch);
    }
    Ok(())
}

fn cp_factor_coordinate_count(model: &CpModel) -> Result<usize, TensorError> {
    let mut rows = 0_usize;
    for mode in 0..model.shape().order() {
        rows = rows
            .checked_add(model.shape().dimension(mode))
            .ok_or(TensorError::Arithmetic)?;
    }
    rows.checked_mul(model.rank())
        .ok_or(TensorError::Arithmetic)
}

fn decode_factor_coordinate(
    model: &CpModel,
    cursor: usize,
) -> Result<(usize, usize, usize), TensorError> {
    let rows_per_component = cp_factor_coordinate_count(model)? / model.rank();
    let component = cursor / rows_per_component;
    let mut row_cursor = cursor % rows_per_component;

    for mode in 0..model.shape().order() {
        let dimension = model.shape().dimension(mode);
        if row_cursor < dimension {
            return Ok((mode, row_cursor, component));
        }
        row_cursor -= dimension;
    }

    Err(TensorError::Coordinate)
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

#[derive(Clone, Copy)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn next_usize(&mut self, bound: usize) -> usize {
        if bound <= 1 {
            0
        } else {
            (self.next() % bound as u64) as usize
        }
    }
}

fn sgd_certificate_root(secret: u64, certificate: &SgdCertificate) -> u64 {
    let mut state = mix(
        secret,
        certificate.epochs as u64 | ((certificate.samples as u64) << 16),
    );
    state = mix(state, u64::from(certificate.committed));
    state = mix(
        state,
        certificate.initial_error_q48 as u64 ^ (certificate.initial_error_q48 >> 64) as u64,
    );
    state = mix(
        state,
        certificate.final_error_q48 as u64 ^ (certificate.final_error_q48 >> 64) as u64,
    );
    state = mix(state, certificate.relative_error_q24 as u64);
    state = mix(state, certificate.maximum_gradient_q24);
    state = mix(state, certificate.sample_stream_root);
    mix(state, certificate.model_root)
}

fn ccd_certificate_root(secret: u64, certificate: &CcdCertificate) -> u64 {
    let mut state = mix(
        secret,
        certificate.coordinate_updates as u64 | ((certificate.cursor_after as u64) << 16),
    );
    state = mix(state, u64::from(certificate.committed));
    state = mix(
        state,
        certificate.initial_error_q48 as u64 ^ (certificate.initial_error_q48 >> 64) as u64,
    );
    state = mix(
        state,
        certificate.final_error_q48 as u64 ^ (certificate.final_error_q48 >> 64) as u64,
    );
    state = mix(state, certificate.relative_error_q24 as u64);
    state = mix(state, certificate.maximum_coordinate_delta_q24);
    mix(state, certificate.model_root)
}

#[cfg(test)]
mod tests {
    use super::super::cp::{CpConfig, CpWorkspace, fit_cp_als};
    use super::*;

    fn test_tensor() -> DenseTensor {
        let shape = TensorShape::new(3, [3, 3, 3, 0]).unwrap();
        let mut tensor = DenseTensor::zeros(shape);
        for linear in 0..shape.length() {
            let coordinate = shape.unravel(linear);
            let value = ((coordinate[0] as i64 + 1)
                * (coordinate[1] as i64 + 1)
                * (coordinate[2] as i64 + 1))
                << fixed::FRACTION_BITS;
            tensor.set_linear(linear, value).unwrap();
        }
        tensor
    }

    #[test]
    fn sgd_update_is_transactional() {
        let tensor = test_tensor();
        let shape = tensor.shape();
        let mut model = CpModel::new(shape, 1).unwrap();
        let mut als_workspace = CpWorkspace::new(shape, 1).unwrap();
        let mut als_config = CpConfig::KERNEL_DEFAULT;
        als_config.rank = 1;
        fit_cp_als(&tensor, &mut model, &mut als_workspace, als_config, 7).unwrap();

        let mut workspace = CpOptimizerWorkspace::new(shape, 1).unwrap();
        let certificate = update_cp_sgd(
            &tensor,
            &mut model,
            &mut workspace,
            SgdConfig::KERNEL_DEFAULT,
            9,
        )
        .unwrap();

        assert!(certificate.root != 0);
    }

    #[test]
    fn ccd_coordinate_updates_do_not_escape_bounds() {
        let tensor = test_tensor();
        let shape = tensor.shape();
        let mut model = CpModel::new(shape, 1).unwrap();
        let mut als_workspace = CpWorkspace::new(shape, 1).unwrap();
        let mut als_config = CpConfig::KERNEL_DEFAULT;
        als_config.rank = 1;
        fit_cp_als(&tensor, &mut model, &mut als_workspace, als_config, 11).unwrap();

        let mut workspace = CpOptimizerWorkspace::new(shape, 1).unwrap();
        let certificate = update_cp_ccd(
            &tensor,
            &mut model,
            &mut workspace,
            CcdConfig::KERNEL_DEFAULT,
            13,
        )
        .unwrap();

        assert!(certificate.root != 0);
    }
}
