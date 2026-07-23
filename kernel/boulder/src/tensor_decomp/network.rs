//! Bounded real tensor-network algorithms.
//!
//! `TensorTrain` is used as a real matrix-product state (MPS). Exact transfer
//! contractions provide norms, local expectations, and two-point correlations
//! without reconstructing the full state.
//!
//! `Peps2x2` is an exact four-site projected-entangled-pair-state patch. It is
//! intended for bounded spatial correlation among four neighboring kernel
//! subsystems, not for unbounded lattice simulation.

use super::fixed;
use super::tensor::{TensorError, mix};
use super::tt::{MAX_TT_DIMENSION, MAX_TT_RANK, TensorTrain};

const MAX_TRANSFER_ENTRIES: usize = MAX_TT_RANK * MAX_TT_RANK;
pub const MAX_PEPS_PHYSICAL: usize = 4;
pub const MAX_PEPS_BOND: usize = 4;
const MAX_PEPS_SITE_ENTRIES: usize = MAX_PEPS_PHYSICAL * MAX_PEPS_BOND * MAX_PEPS_BOND;
const MAX_PEPS_STORAGE: usize = 4 * MAX_PEPS_SITE_ENTRIES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MpsCertificate {
    pub order: u8,
    pub norm_squared_q24: i64,
    pub observable_q24: i64,
    pub normalized_expectation_q24: i64,
    pub first_site: u8,
    pub second_site: u8,
    pub train_root: u64,
    pub root: u64,
}

impl MpsCertificate {
    pub const EMPTY: Self = Self {
        order: 0,
        norm_squared_q24: 0,
        observable_q24: 0,
        normalized_expectation_q24: 0,
        first_site: u8::MAX,
        second_site: u8::MAX,
        train_root: 0,
        root: 0,
    };

    pub fn verify(&self, secret: u64) -> bool {
        self.order >= 2
            && self.norm_squared_q24 > 0
            && self.root == mps_certificate_root(secret, self)
    }
}

pub struct MpsWorkspace {
    environment: [i64; MAX_TRANSFER_ENTRIES],
    next: [i64; MAX_TRANSFER_ENTRIES],
}

impl MpsWorkspace {
    pub const fn new() -> Self {
        Self {
            environment: [0; MAX_TRANSFER_ENTRIES],
            next: [0; MAX_TRANSFER_ENTRIES],
        }
    }
}

impl Default for MpsWorkspace {
    fn default() -> Self {
        Self::new()
    }
}

pub fn mps_norm(
    train: &TensorTrain,
    workspace: &mut MpsWorkspace,
    secret: u64,
) -> Result<MpsCertificate, TensorError> {
    let operators = [[fixed::ONE; MAX_TT_DIMENSION]; 2];
    contract_mps(train, None, None, &operators, workspace, secret)
}

pub fn mps_local_diagonal_expectation(
    train: &TensorTrain,
    site: usize,
    diagonal_q24: &[i64; MAX_TT_DIMENSION],
    workspace: &mut MpsWorkspace,
    secret: u64,
) -> Result<MpsCertificate, TensorError> {
    let mut operators = [[fixed::ONE; MAX_TT_DIMENSION]; 2];
    operators[0] = *diagonal_q24;
    contract_mps(train, Some(site), None, &operators, workspace, secret)
}

pub fn mps_two_point_diagonal_expectation(
    train: &TensorTrain,
    first_site: usize,
    first_diagonal_q24: &[i64; MAX_TT_DIMENSION],
    second_site: usize,
    second_diagonal_q24: &[i64; MAX_TT_DIMENSION],
    workspace: &mut MpsWorkspace,
    secret: u64,
) -> Result<MpsCertificate, TensorError> {
    if first_site == second_site {
        return Err(TensorError::InvalidDimension);
    }

    let operators = [*first_diagonal_q24, *second_diagonal_q24];
    contract_mps(
        train,
        Some(first_site),
        Some(second_site),
        &operators,
        workspace,
        secret,
    )
}

fn contract_mps(
    train: &TensorTrain,
    first_site: Option<usize>,
    second_site: Option<usize>,
    operators: &[[i64; MAX_TT_DIMENSION]; 2],
    workspace: &mut MpsWorkspace,
    secret: u64,
) -> Result<MpsCertificate, TensorError> {
    if secret == 0 {
        return Err(TensorError::ZeroSecret);
    }
    if first_site
        .map(|site| site >= train.shape().order())
        .unwrap_or(false)
        || second_site
            .map(|site| site >= train.shape().order())
            .unwrap_or(false)
    {
        return Err(TensorError::Coordinate);
    }

    workspace.environment.fill(0);
    workspace.environment[0] = fixed::ONE;
    let mut left_rank = 1_usize;

    for mode in 0..train.shape().order() {
        let right_rank = train.ranks()[mode + 1] as usize;
        workspace.next.fill(0);

        for left_bra in 0..left_rank {
            for left_ket in 0..left_rank {
                let environment = workspace.environment[left_bra * MAX_TT_RANK + left_ket];
                if environment == 0 {
                    continue;
                }

                for physical in 0..train.shape().dimension(mode) {
                    let operator = if first_site == Some(mode) {
                        operators[0][physical]
                    } else if second_site == Some(mode) {
                        operators[1][physical]
                    } else {
                        fixed::ONE
                    };

                    for right_bra in 0..right_rank {
                        let bra = train.core_value(mode, left_bra, physical, right_bra)?;
                        for right_ket in 0..right_rank {
                            let ket = train.core_value(mode, left_ket, physical, right_ket)?;
                            let local = fixed::mul(fixed::mul(bra, operator)?, ket)?;
                            let contribution = fixed::mul(environment, local)?;
                            let index = right_bra * MAX_TT_RANK + right_ket;
                            workspace.next[index] = workspace.next[index]
                                .checked_add(contribution)
                                .ok_or(TensorError::Arithmetic)?;
                        }
                    }
                }
            }
        }

        workspace.environment = workspace.next;
        left_rank = right_rank;
    }

    let observable = workspace.environment[0];
    let norm = if first_site.is_none() && second_site.is_none() {
        observable
    } else {
        let norm_certificate = mps_norm_only(train, workspace)?.max(1);
        norm_certificate
    };
    let normalized = if first_site.is_none() && second_site.is_none() {
        fixed::ONE
    } else {
        fixed::div(observable, norm)?
    };

    let mut certificate = MpsCertificate {
        order: train.shape().order() as u8,
        norm_squared_q24: norm,
        observable_q24: observable,
        normalized_expectation_q24: normalized,
        first_site: first_site.map(|site| site as u8).unwrap_or(u8::MAX),
        second_site: second_site.map(|site| site as u8).unwrap_or(u8::MAX),
        train_root: train.root(),
        root: 0,
    };
    certificate.root = mps_certificate_root(secret, &certificate);
    Ok(certificate)
}

fn mps_norm_only(train: &TensorTrain, workspace: &mut MpsWorkspace) -> Result<i64, TensorError> {
    workspace.environment.fill(0);
    workspace.environment[0] = fixed::ONE;
    let mut left_rank = 1_usize;

    for mode in 0..train.shape().order() {
        let right_rank = train.ranks()[mode + 1] as usize;
        workspace.next.fill(0);

        for left_bra in 0..left_rank {
            for left_ket in 0..left_rank {
                let environment = workspace.environment[left_bra * MAX_TT_RANK + left_ket];
                for physical in 0..train.shape().dimension(mode) {
                    for right_bra in 0..right_rank {
                        let bra = train.core_value(mode, left_bra, physical, right_bra)?;
                        for right_ket in 0..right_rank {
                            let ket = train.core_value(mode, left_ket, physical, right_ket)?;
                            let local = fixed::mul(bra, ket)?;
                            let contribution = fixed::mul(environment, local)?;
                            let index = right_bra * MAX_TT_RANK + right_ket;
                            workspace.next[index] = workspace.next[index]
                                .checked_add(contribution)
                                .ok_or(TensorError::Arithmetic)?;
                        }
                    }
                }
            }
        }

        workspace.environment = workspace.next;
        left_rank = right_rank;
    }

    Ok(workspace.environment[0])
}

#[derive(Debug, Eq, PartialEq)]
pub struct Peps2x2 {
    physical_dimension: u8,
    bond_dimension: u8,
    sites_q24: [i64; MAX_PEPS_STORAGE],
    root: u64,
}

impl Peps2x2 {
    pub fn new(physical_dimension: usize, bond_dimension: usize) -> Result<Self, TensorError> {
        if physical_dimension == 0
            || physical_dimension > MAX_PEPS_PHYSICAL
            || bond_dimension == 0
            || bond_dimension > MAX_PEPS_BOND
        {
            return Err(TensorError::InvalidDimension);
        }

        Ok(Self {
            physical_dimension: physical_dimension as u8,
            bond_dimension: bond_dimension as u8,
            sites_q24: [0; MAX_PEPS_STORAGE],
            root: 0,
        })
    }

    pub const fn physical_dimension(&self) -> usize {
        self.physical_dimension as usize
    }

    pub const fn bond_dimension(&self) -> usize {
        self.bond_dimension as usize
    }

    pub const fn root(&self) -> u64 {
        self.root
    }

    pub fn set(
        &mut self,
        site: usize,
        physical: usize,
        horizontal_bond: usize,
        vertical_bond: usize,
        value_q24: i64,
    ) -> Result<(), TensorError> {
        let index = self.site_index(site, physical, horizontal_bond, vertical_bond)?;
        self.sites_q24[index] = value_q24;
        Ok(())
    }

    pub fn get(
        &self,
        site: usize,
        physical: usize,
        horizontal_bond: usize,
        vertical_bond: usize,
    ) -> Result<i64, TensorError> {
        let index = self.site_index(site, physical, horizontal_bond, vertical_bond)?;
        Ok(self.sites_q24[index])
    }

    pub fn amplitude(&self, physical: [usize; 4]) -> Result<i64, TensorError> {
        if physical
            .iter()
            .any(|value| *value >= self.physical_dimension())
        {
            return Err(TensorError::Coordinate);
        }

        let bond = self.bond_dimension();
        let mut amplitude = 0_i64;

        for top in 0..bond {
            for left in 0..bond {
                for right in 0..bond {
                    for bottom in 0..bond {
                        let mut term = self.get(0, physical[0], top, left)?;
                        term = fixed::mul(term, self.get(1, physical[1], top, right)?)?;
                        term = fixed::mul(term, self.get(2, physical[2], bottom, left)?)?;
                        term = fixed::mul(term, self.get(3, physical[3], bottom, right)?)?;
                        amplitude = amplitude.checked_add(term).ok_or(TensorError::Arithmetic)?;
                    }
                }
            }
        }

        Ok(amplitude)
    }

    pub fn seal(&mut self, secret: u64) -> Result<(), TensorError> {
        if secret == 0 {
            return Err(TensorError::ZeroSecret);
        }

        let mut state = mix(
            secret,
            self.physical_dimension as u64 | ((self.bond_dimension as u64) << 8),
        );
        let site_length = self.site_length();
        for site in 0..4 {
            let offset = site * MAX_PEPS_SITE_ENTRIES;
            for value in &self.sites_q24[offset..offset + site_length] {
                state = mix(state, *value as u64);
            }
        }
        self.root = state;
        Ok(())
    }

    fn site_length(&self) -> usize {
        self.physical_dimension() * self.bond_dimension() * self.bond_dimension()
    }

    fn site_index(
        &self,
        site: usize,
        physical: usize,
        horizontal_bond: usize,
        vertical_bond: usize,
    ) -> Result<usize, TensorError> {
        if site >= 4
            || physical >= self.physical_dimension()
            || horizontal_bond >= self.bond_dimension()
            || vertical_bond >= self.bond_dimension()
        {
            return Err(TensorError::Coordinate);
        }

        Ok(site * MAX_PEPS_SITE_ENTRIES
            + (physical * self.bond_dimension() + horizontal_bond) * self.bond_dimension()
            + vertical_bond)
    }
}

pub fn build_binary_peps_patch(
    probabilities_q24: [i64; 4],
    horizontal_correlation_q24: [i64; 2],
    vertical_correlation_q24: [i64; 2],
    patch: &mut Peps2x2,
    secret: u64,
) -> Result<(), TensorError> {
    if patch.physical_dimension() != 2 || patch.bond_dimension() != 2 {
        return Err(TensorError::ShapeMismatch);
    }

    for probability in probabilities_q24 {
        if !(0..=fixed::ONE).contains(&probability) {
            return Err(TensorError::InvalidDimension);
        }
    }
    for correlation in horizontal_correlation_q24
        .into_iter()
        .chain(vertical_correlation_q24)
    {
        if correlation.checked_abs().ok_or(TensorError::Arithmetic)? > fixed::ONE {
            return Err(TensorError::InvalidDimension);
        }
    }

    for site in 0..4 {
        let probability = probabilities_q24[site];
        let amplitudes = [
            fixed::sqrt(
                fixed::ONE
                    .checked_sub(probability)
                    .ok_or(TensorError::Arithmetic)?,
            )?,
            fixed::sqrt(probability)?,
        ];

        let horizontal = if site < 2 {
            horizontal_correlation_q24[0]
        } else {
            horizontal_correlation_q24[1]
        };
        let vertical = if site % 2 == 0 {
            vertical_correlation_q24[0]
        } else {
            vertical_correlation_q24[1]
        };

        for physical in 0..2 {
            for horizontal_bond in 0..2 {
                for vertical_bond in 0..2 {
                    let horizontal_weight = bond_weight(physical, horizontal_bond, horizontal)?;
                    let vertical_weight = bond_weight(physical, vertical_bond, vertical)?;
                    let value = fixed::mul(
                        fixed::mul(amplitudes[physical], horizontal_weight)?,
                        vertical_weight,
                    )?;
                    patch.set(site, physical, horizontal_bond, vertical_bond, value)?;
                }
            }
        }
    }

    patch.seal(secret)
}

fn bond_weight(physical: usize, bond: usize, correlation_q24: i64) -> Result<i64, TensorError> {
    let signed = if physical == bond {
        correlation_q24
    } else {
        correlation_q24
            .checked_neg()
            .ok_or(TensorError::Arithmetic)?
    };
    let probability = fixed::HALF
        .checked_add(signed / 2)
        .ok_or(TensorError::Arithmetic)?
        .clamp(0, fixed::ONE);
    fixed::sqrt(probability).map_err(Into::into)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PepsCertificate {
    pub physical_dimension: u8,
    pub bond_dimension: u8,
    pub norm_squared_q24: i64,
    pub observable_q24: i64,
    pub normalized_expectation_q24: i64,
    pub observed_site: u8,
    pub patch_root: u64,
    pub root: u64,
}

impl PepsCertificate {
    pub fn verify(&self, secret: u64) -> bool {
        self.norm_squared_q24 > 0 && self.root == peps_certificate_root(secret, self)
    }
}

pub fn peps_norm(patch: &Peps2x2, secret: u64) -> Result<PepsCertificate, TensorError> {
    contract_peps(patch, None, None, secret)
}

pub fn peps_local_diagonal_expectation(
    patch: &Peps2x2,
    site: usize,
    diagonal_q24: &[i64; MAX_PEPS_PHYSICAL],
    secret: u64,
) -> Result<PepsCertificate, TensorError> {
    if site >= 4 {
        return Err(TensorError::Coordinate);
    }
    contract_peps(patch, Some(site), Some(diagonal_q24), secret)
}

fn contract_peps(
    patch: &Peps2x2,
    observed_site: Option<usize>,
    diagonal: Option<&[i64; MAX_PEPS_PHYSICAL]>,
    secret: u64,
) -> Result<PepsCertificate, TensorError> {
    if secret == 0 || patch.root() == 0 {
        return Err(TensorError::ZeroSecret);
    }

    let physical_dimension = patch.physical_dimension();
    let configurations = physical_dimension
        .checked_pow(4)
        .ok_or(TensorError::Arithmetic)?;
    let mut norm = 0_i64;
    let mut observable = 0_i64;

    for linear in 0..configurations {
        let mut remainder = linear;
        let mut physical = [0_usize; 4];
        for site in (0..4).rev() {
            physical[site] = remainder % physical_dimension;
            remainder /= physical_dimension;
        }

        let amplitude = patch.amplitude(physical)?;
        let probability = fixed::mul(amplitude, amplitude)?;
        norm = norm
            .checked_add(probability)
            .ok_or(TensorError::Arithmetic)?;

        let weight = match (observed_site, diagonal) {
            (Some(site), Some(values)) => values[physical[site]],
            _ => fixed::ONE,
        };
        observable = observable
            .checked_add(fixed::mul(probability, weight)?)
            .ok_or(TensorError::Arithmetic)?;
    }

    if norm <= 0 {
        return Err(TensorError::Arithmetic);
    }
    let normalized = fixed::div(observable, norm)?;

    let mut certificate = PepsCertificate {
        physical_dimension: patch.physical_dimension as u8,
        bond_dimension: patch.bond_dimension as u8,
        norm_squared_q24: norm,
        observable_q24: observable,
        normalized_expectation_q24: normalized,
        observed_site: observed_site.map(|site| site as u8).unwrap_or(u8::MAX),
        patch_root: patch.root(),
        root: 0,
    };
    certificate.root = peps_certificate_root(secret, &certificate);
    Ok(certificate)
}

fn mps_certificate_root(secret: u64, certificate: &MpsCertificate) -> u64 {
    let mut state = mix(
        secret,
        certificate.order as u64
            | ((certificate.first_site as u64) << 8)
            | ((certificate.second_site as u64) << 16),
    );
    state = mix(state, certificate.norm_squared_q24 as u64);
    state = mix(state, certificate.observable_q24 as u64);
    state = mix(state, certificate.normalized_expectation_q24 as u64);
    mix(state, certificate.train_root)
}

fn peps_certificate_root(secret: u64, certificate: &PepsCertificate) -> u64 {
    let mut state = mix(
        secret,
        certificate.physical_dimension as u64
            | ((certificate.bond_dimension as u64) << 8)
            | ((certificate.observed_site as u64) << 16),
    );
    state = mix(state, certificate.norm_squared_q24 as u64);
    state = mix(state, certificate.observable_q24 as u64);
    state = mix(state, certificate.normalized_expectation_q24 as u64);
    mix(state, certificate.patch_root)
}

#[cfg(test)]
mod tests {
    use super::super::tt::{TtConfig, TtDense, TtShape, TtWorkspace, fit_tt_svd};
    use super::*;

    #[test]
    fn mps_transfer_contraction_matches_positive_norm() {
        let shape = TtShape::new(4, [2, 2, 2, 2, 0, 0, 0, 0]).unwrap();
        let mut dense = TtDense::zeros(shape);
        dense.values_mut().fill(fixed::ONE / 4);

        let mut train = TensorTrain::new(shape);
        let mut tt_workspace = TtWorkspace::new(shape);
        fit_tt_svd(
            &dense,
            &mut train,
            &mut tt_workspace,
            TtConfig {
                maximum_rank: 2,
                ..TtConfig::KERNEL_DEFAULT
            },
            7,
        )
        .unwrap();

        let mut workspace = MpsWorkspace::new();
        let certificate = mps_norm(&train, &mut workspace, 9).unwrap();
        assert!(certificate.verify(9));
        assert!(certificate.norm_squared_q24 > 0);
    }

    #[test]
    fn product_peps_has_exact_local_expectation() {
        let mut patch = Peps2x2::new(2, 1).unwrap();
        for site in 0..4 {
            patch.set(site, 0, 0, 0, fixed::ONE).unwrap();
            patch.set(site, 1, 0, 0, 0).unwrap();
        }
        patch.seal(7).unwrap();

        let diagonal = [0, fixed::ONE, 0, 0];
        let certificate = peps_local_diagonal_expectation(&patch, 2, &diagonal, 9).unwrap();

        assert_eq!(certificate.normalized_expectation_q24, 0);
        assert!(certificate.verify(9));
    }
}
