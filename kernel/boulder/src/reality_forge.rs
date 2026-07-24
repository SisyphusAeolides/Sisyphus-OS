use aether::effect_program::PreparedEffects;
use aether::invariant_mesh::{InvariantReport, evaluate};
use aether::temporal_contract::{TemporalContract, TemporalObservation};

use crate::counterfactual::{CounterfactualError, CounterfactualReceipt, CounterfactualUniverse};
use crate::divergence_vault::DivergenceVault;
use crate::nexus_matrix::NexusMatrix;
use crate::sync::SpinLock;
use crate::thermogenesis::Thermogenesis;

pub const LANE_ALPHA: u8 = 1 << 0;
pub const LANE_BETA: u8 = 1 << 1;
pub const LANE_GAMMA: u8 = 1 << 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForgeError {
    Counterfactual(CounterfactualError),
    NoRealityMajority,
    InvariantFailure(InvariantReport),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ForgeReceipt {
    pub transition: CounterfactualReceipt,
    pub reality_mask: u8,
    pub invariants: InvariantReport,
}

pub struct RealityForge<const DIVERGENCES: usize> {
    divergences: SpinLock<DivergenceVault<DIVERGENCES>>,
}

impl<const DIVERGENCES: usize> RealityForge<DIVERGENCES> {
    pub const fn new(seed: u64) -> Self {
        Self {
            divergences: SpinLock::new(DivergenceVault::new(seed)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn forge_and_commit<
        const TASKS: usize,
        const PAIRS: usize,
        const CAGES: usize,
        const MOMENTS: usize,
        const BINS: usize,
        const N: usize,
    >(
        &self,
        live_matrix: &mut NexusMatrix<TASKS, PAIRS, CAGES, MOMENTS, BINS>,
        live_thermal: &Thermogenesis,
        prepared: &PreparedEffects<N>,
        contract: TemporalContract,
        wall_tick: u64,
        live_root: u64,
    ) -> Result<ForgeReceipt, ForgeError> {
        let effect_digest = prepared.digest();

        let alpha = CounterfactualUniverse::simulate(
            live_matrix,
            live_thermal,
            prepared,
            contract,
            wall_tick,
            live_root,
        );

        match alpha {
            Ok(alpha) => self.forge_with_alpha(
                alpha,
                live_matrix,
                live_thermal,
                prepared,
                contract,
                wall_tick,
                live_root,
                effect_digest,
            ),

            Err(_) => self.forge_without_alpha(
                live_matrix,
                live_thermal,
                prepared,
                contract,
                wall_tick,
                live_root,
                effect_digest,
            ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn forge_with_alpha<
        const TASKS: usize,
        const PAIRS: usize,
        const CAGES: usize,
        const MOMENTS: usize,
        const BINS: usize,
        const N: usize,
    >(
        &self,
        alpha: CounterfactualUniverse<TASKS, PAIRS, CAGES, MOMENTS, BINS>,
        live_matrix: &mut NexusMatrix<TASKS, PAIRS, CAGES, MOMENTS, BINS>,
        live_thermal: &Thermogenesis,
        prepared: &PreparedEffects<N>,
        contract: TemporalContract,
        wall_tick: u64,
        live_root: u64,
        effect_digest: u64,
    ) -> Result<ForgeReceipt, ForgeError> {
        let alpha_after = alpha.after();

        // Lane β is reduced to a compact observation before lane γ exists.
        let beta_result = CounterfactualUniverse::simulate(
            live_matrix,
            live_thermal,
            prepared,
            contract,
            wall_tick,
            live_root,
        );

        let (beta_after, beta_failed) = match beta_result {
            Ok(beta) => {
                let observation = beta.after();
                drop(beta);
                (Some(observation), false)
            }

            Err(_) => (None, true),
        };

        let gamma_result = CounterfactualUniverse::simulate(
            live_matrix,
            live_thermal,
            prepared,
            contract,
            wall_tick,
            live_root,
        );

        let mut gamma = match gamma_result {
            Ok(universe) => Some(universe),
            Err(_) => None,
        };

        let gamma_after = gamma.as_ref().map(|universe| universe.after());

        let failure_mask =
            if beta_failed { LANE_BETA } else { 0 } | if gamma.is_none() { LANE_GAMMA } else { 0 };

        let alpha_beta = beta_after.is_some_and(|beta| equivalent(alpha_after, beta));

        let alpha_gamma = gamma_after.is_some_and(|gamma| equivalent(alpha_after, gamma));

        let beta_gamma = match (beta_after, gamma_after) {
            (Some(beta), Some(gamma)) => equivalent(beta, gamma),
            _ => false,
        };

        let (selected, reality_mask) = if alpha_beta {
            let mut mask = LANE_ALPHA | LANE_BETA;

            if alpha_gamma {
                mask |= LANE_GAMMA;
            }

            (alpha, mask)
        } else if alpha_gamma {
            (alpha, LANE_ALPHA | LANE_GAMMA)
        } else if beta_gamma {
            (
                gamma.take().expect("gamma observation without universe"),
                LANE_BETA | LANE_GAMMA,
            )
        } else {
            self.record_divergence(
                effect_digest,
                Some(alpha_after),
                beta_after,
                gamma_after,
                failure_mask,
                0,
            );

            return Err(ForgeError::NoRealityMajority);
        };

        self.commit_selected(
            selected,
            live_matrix,
            live_thermal,
            contract,
            live_root,
            effect_digest,
            reality_mask,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn forge_without_alpha<
        const TASKS: usize,
        const PAIRS: usize,
        const CAGES: usize,
        const MOMENTS: usize,
        const BINS: usize,
        const N: usize,
    >(
        &self,
        live_matrix: &mut NexusMatrix<TASKS, PAIRS, CAGES, MOMENTS, BINS>,
        live_thermal: &Thermogenesis,
        prepared: &PreparedEffects<N>,
        contract: TemporalContract,
        wall_tick: u64,
        live_root: u64,
        effect_digest: u64,
    ) -> Result<ForgeReceipt, ForgeError> {
        let beta = CounterfactualUniverse::simulate(
            live_matrix,
            live_thermal,
            prepared,
            contract,
            wall_tick,
            live_root,
        );

        let gamma = CounterfactualUniverse::simulate(
            live_matrix,
            live_thermal,
            prepared,
            contract,
            wall_tick,
            live_root,
        );

        match (beta, gamma) {
            (Ok(beta), Ok(gamma)) if equivalent(beta.after(), gamma.after()) => {
                drop(gamma);

                self.commit_selected(
                    beta,
                    live_matrix,
                    live_thermal,
                    contract,
                    live_root,
                    effect_digest,
                    LANE_BETA | LANE_GAMMA,
                )
            }

            (beta, gamma) => {
                let beta_after = beta.as_ref().ok().map(|value| value.after());

                let gamma_after = gamma.as_ref().ok().map(|value| value.after());

                let failure_mask = LANE_ALPHA
                    | if beta.is_err() { LANE_BETA } else { 0 }
                    | if gamma.is_err() { LANE_GAMMA } else { 0 };

                self.record_divergence(
                    effect_digest,
                    None,
                    beta_after,
                    gamma_after,
                    failure_mask,
                    0,
                );

                Err(ForgeError::NoRealityMajority)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_selected<
        const TASKS: usize,
        const PAIRS: usize,
        const CAGES: usize,
        const MOMENTS: usize,
        const BINS: usize,
    >(
        &self,
        selected: CounterfactualUniverse<TASKS, PAIRS, CAGES, MOMENTS, BINS>,
        live_matrix: &mut NexusMatrix<TASKS, PAIRS, CAGES, MOMENTS, BINS>,
        live_thermal: &Thermogenesis,
        contract: TemporalContract,
        live_root: u64,
        effect_digest: u64,
        reality_mask: u8,
    ) -> Result<ForgeReceipt, ForgeError> {
        let before = selected.before();
        let after = selected.after();

        let invariants = evaluate(before, after, contract, effect_digest, reality_mask);

        if !invariants.is_clear() {
            self.record_divergence(
                effect_digest,
                Some(before),
                Some(after),
                None,
                0,
                reality_mask,
            );

            return Err(ForgeError::InvariantFailure(invariants));
        }

        let transition = selected
            .commit(live_matrix, live_thermal, live_root)
            .map_err(ForgeError::Counterfactual)?;

        Ok(ForgeReceipt {
            transition,
            reality_mask,
            invariants,
        })
    }

    fn record_divergence(
        &self,
        effect_digest: u64,
        alpha: Option<TemporalObservation>,
        beta: Option<TemporalObservation>,
        gamma: Option<TemporalObservation>,
        failure_mask: u8,
        majority_mask: u8,
    ) {
        let _ = self.divergences.lock().record(
            effect_digest,
            alpha,
            beta,
            gamma,
            failure_mask,
            majority_mask,
        );
    }

    pub fn divergence_root(&self) -> u64 {
        self.divergences.lock().root()
    }

    pub fn divergence_count(&self) -> usize {
        self.divergences.lock().retained()
    }
}

fn equivalent(left: TemporalObservation, right: TemporalObservation) -> bool {
    left.generation == right.generation
        && left.pairs_live == right.pairs_live
        && left.state_root == right.state_root
        && left.collapses == right.collapses
        && left.heat == right.heat
        && left.phase_bin == right.phase_bin
}
