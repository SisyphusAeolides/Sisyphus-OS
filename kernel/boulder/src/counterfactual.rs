use aether::effect_program::{EffectError, PreparedEffects};
use aether::holographic::{HolographicError, HolographicTree};
use aether::temporal_contract::{ContractError, TemporalContract, TemporalObservation};

use crate::nexus_commit::{CommitError, apply_prepared};
use crate::nexus_matrix::{MATRIX_HOLOGRAM_LEAVES, MATRIX_HOLOGRAM_NODES, NexusMatrix};
use crate::thermogenesis::{
    ThermalLedger, ThermalTransaction, ThermalTransactionError, Thermogenesis,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CounterfactualError {
    Contract(ContractError),
    Effects(EffectError),
    Execution(CommitError),
    Hologram(HolographicError),
    Thermal(ThermalTransactionError),
    LiveStateChanged,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CounterfactualReceipt {
    pub before: TemporalObservation,
    pub after: TemporalObservation,
    pub effect_digest: u64,
}

pub struct CounterfactualUniverse<
    const TASKS: usize,
    const PAIRS: usize,
    const CAGES: usize,
    const MOMENTS: usize,
    const BINS: usize,
> {
    shadow: NexusMatrix<TASKS, PAIRS, CAGES, MOMENTS, BINS>,

    thermal: ThermalTransaction,

    before: TemporalObservation,
    after: TemporalObservation,

    effect_digest: u64,
}

impl<
    const TASKS: usize,
    const PAIRS: usize,
    const CAGES: usize,
    const MOMENTS: usize,
    const BINS: usize,
> CounterfactualUniverse<TASKS, PAIRS, CAGES, MOMENTS, BINS>
{
    pub fn simulate<const N: usize>(
        live_matrix: &NexusMatrix<TASKS, PAIRS, CAGES, MOMENTS, BINS>,
        live_thermal: &Thermogenesis,
        prepared: &PreparedEffects<N>,
        contract: TemporalContract,
        wall_tick: u64,
        before_root: u64,
    ) -> Result<Self, CounterfactualError> {
        let stats = live_matrix.stats();

        let before = TemporalObservation {
            generation: stats.generation,
            pairs_live: stats.pairs_live,
            state_root: before_root,
            collapses: stats.collapses,
            heat: live_thermal.current_heat(),
            phase_bin: stats.global_phase,
            reserved: 0,
        };

        contract
            .verify_before(before, wall_tick)
            .map_err(CounterfactualError::Contract)?;

        if prepared.expected_generation() != contract.expected_generation
            || prepared.expected_state_root() != contract.expected_state_root
        {
            return Err(CounterfactualError::LiveStateChanged);
        }

        for effect in prepared.effects() {
            let kind = effect.effect_kind().map_err(CounterfactualError::Effects)?;

            contract
                .verify_effect(kind as u8)
                .map_err(CounterfactualError::Contract)?;
        }

        let mut shadow = live_matrix.clone();
        let thermal = live_thermal.begin_budget_transaction();

        apply_prepared(&mut shadow, &thermal, prepared, wall_tick)
            .map_err(CounterfactualError::Execution)?;

        let mut hologram = HolographicTree::<MATRIX_HOLOGRAM_LEAVES, MATRIX_HOLOGRAM_NODES>::new();

        let after_root = shadow
            .refresh_hologram(&mut hologram)
            .map_err(CounterfactualError::Hologram)?;

        let stats = shadow.stats();

        let after = TemporalObservation {
            generation: stats.generation,
            pairs_live: stats.pairs_live,
            state_root: after_root,
            collapses: stats.collapses,
            heat: thermal.current_heat(),
            phase_bin: stats.global_phase,
            reserved: 0,
        };

        contract
            .verify_after(before, after)
            .map_err(CounterfactualError::Contract)?;

        Ok(Self {
            shadow,
            thermal,
            before,
            after,
            effect_digest: prepared.digest(),
        })
    }

    pub const fn before(&self) -> TemporalObservation {
        self.before
    }

    pub const fn after(&self) -> TemporalObservation {
        self.after
    }

    pub const fn effect_digest(&self) -> u64 {
        self.effect_digest
    }

    pub fn commit(
        self,
        live_matrix: &mut NexusMatrix<TASKS, PAIRS, CAGES, MOMENTS, BINS>,
        live_thermal: &Thermogenesis,
        observed_live_root: u64,
    ) -> Result<CounterfactualReceipt, CounterfactualError> {
        let live_stats = live_matrix.stats();

        if live_stats.generation != self.before.generation
            || live_stats.pairs_live != self.before.pairs_live
            || live_stats.collapses != self.before.collapses
            || observed_live_root != self.before.state_root
        {
            return Err(CounterfactualError::LiveStateChanged);
        }

        // Thermal commit may fail. Matrix replacement cannot fail, so thermal
        // must be committed first while both live locks are held.
        self.thermal
            .commit(live_thermal)
            .map_err(CounterfactualError::Thermal)?;

        *live_matrix = self.shadow;

        Ok(CounterfactualReceipt {
            before: self.before,
            after: self.after,
            effect_digest: self.effect_digest,
        })
    }
}
