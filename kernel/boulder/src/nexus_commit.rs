use aether::causal_quorum::{CausalQuorum, QuorumError, QuorumTicket};
use aether::effect_program::{EffectError, EffectKind, PreparedEffects};
use aether::nexus_wire::NexusOpcode;
use aether::witness_chain::{CommitOutcome, CommitWitness, WitnessChain};

use crate::nexus_matrix::{MatrixError, NexusMatrix};
use crate::sync::SpinLock;
use crate::thermogenesis::ThermalLedger;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitError {
    Quorum(QuorumError),
    Effects(EffectError),
    Matrix(MatrixError),
    Thermal,
    WitnessCapacity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommitReceipt {
    pub epoch: u64,
    pub witness_root: u64,
    pub participants: u32,
}

pub struct NexusCommitEngine<const CPUS: usize, const WITNESSES: usize> {
    quorum: CausalQuorum<CPUS>,
    witnesses: SpinLock<WitnessChain<WITNESSES>>,
}

impl<const CPUS: usize, const WITNESSES: usize> NexusCommitEngine<CPUS, WITNESSES> {
    pub const fn new(seed: u64) -> Self {
        Self {
            quorum: CausalQuorum::new(),
            witnesses: SpinLock::new(WitnessChain::new(seed)),
        }
    }

    pub fn begin<const N: usize>(
        &self,
        prepared: &PreparedEffects<N>,
        required_cpus: usize,
    ) -> Result<QuorumTicket, CommitError> {
        self.quorum
            .begin(prepared.digest(), required_cpus)
            .map_err(CommitError::Quorum)
    }

    pub fn proposal(&self) -> Option<QuorumTicket> {
        self.quorum.proposal()
    }

    pub fn acknowledge(&self, cpu: usize, ticket: QuorumTicket) -> Result<(), CommitError> {
        self.quorum
            .acknowledge(cpu, ticket, ticket.digest)
            .map_err(CommitError::Quorum)
    }

    pub fn ready(&self, ticket: QuorumTicket) -> Result<bool, CommitError> {
        self.quorum.ready(ticket).map_err(CommitError::Quorum)
    }

    pub fn finalize_success<const N: usize>(
        &self,
        ticket: QuorumTicket,
        prepared: &PreparedEffects<N>,
        before_root: u64,
        after_root: u64,
        generation_before: u32,
        generation_after: u32,
        wall_tick: u64,
    ) -> Result<CommitReceipt, CommitError> {
        let participants = self.quorum.commit(ticket).map_err(CommitError::Quorum)?;

        let witness = CommitWitness {
            epoch: ticket.epoch,
            effect_digest: prepared.digest(),

            before_root,
            after_root,

            wall_tick,

            generation_before,
            generation_after,

            participants: participants.min(u16::MAX as u32) as u16,

            effect_count: prepared.effects().len().min(u16::MAX as usize) as u16,

            outcome: CommitOutcome::Committed,
            reserved: [0; 7],
        };

        let witness_root = self
            .witnesses
            .lock()
            .append(witness)
            .ok_or(CommitError::WitnessCapacity)?;

        Ok(CommitReceipt {
            epoch: ticket.epoch,
            witness_root,
            participants,
        })
    }

    pub fn finalize_abort<const N: usize>(
        &self,
        ticket: QuorumTicket,
        prepared: &PreparedEffects<N>,
        state_root: u64,
        generation: u32,
        wall_tick: u64,
        rolled_back: bool,
    ) -> Result<CommitReceipt, CommitError> {
        let participants = self.quorum.prepared_count(ticket).unwrap_or(0);

        self.quorum.abort(ticket).map_err(CommitError::Quorum)?;

        let outcome = if rolled_back {
            CommitOutcome::RolledBack
        } else {
            CommitOutcome::Aborted
        };

        let witness = CommitWitness {
            epoch: ticket.epoch,
            effect_digest: prepared.digest(),

            before_root: state_root,
            after_root: state_root,

            wall_tick,

            generation_before: generation,
            generation_after: generation,

            participants: participants.min(u16::MAX as u32) as u16,

            effect_count: prepared.effects().len().min(u16::MAX as usize) as u16,

            outcome,
            reserved: [0; 7],
        };

        let witness_root = self
            .witnesses
            .lock()
            .append(witness)
            .ok_or(CommitError::WitnessCapacity)?;

        Ok(CommitReceipt {
            epoch: ticket.epoch,
            witness_root,
            participants,
        })
    }

    pub fn witness_root(&self) -> u64 {
        self.witnesses.lock().root()
    }

    pub fn witness_chain_is_valid(&self) -> bool {
        self.witnesses.lock().verify()
    }
}

pub fn apply_prepared<
    const TASKS: usize,
    const PAIRS: usize,
    const CAGES: usize,
    const MOMENTS: usize,
    const BINS: usize,
    const N: usize,
    T: ThermalLedger + ?Sized,
>(
    matrix: &mut NexusMatrix<TASKS, PAIRS, CAGES, MOMENTS, BINS>,
    thermal: &T,
    prepared: &PreparedEffects<N>,
    wall_tick: u64,
) -> Result<(), CommitError> {
    for effect in prepared.effects() {
        let kind = effect.effect_kind().map_err(CommitError::Effects)?;

        match kind {
            EffectKind::AttachTask => {
                matrix
                    .execute(
                        NexusOpcode::AttachTask,
                        effect.arguments,
                        wall_tick,
                        thermal,
                    )
                    .map_err(CommitError::Matrix)?;
            }

            EffectKind::Entangle => {
                matrix
                    .execute(NexusOpcode::Entangle, effect.arguments, wall_tick, thermal)
                    .map_err(CommitError::Matrix)?;
            }

            EffectKind::SetCollapseThreshold => {
                matrix
                    .execute(
                        NexusOpcode::SetCollapseThreshold,
                        effect.arguments,
                        wall_tick,
                        thermal,
                    )
                    .map_err(CommitError::Matrix)?;
            }

            EffectKind::SetPriorityMass => {
                matrix
                    .execute(
                        NexusOpcode::SetPriorityMass,
                        effect.arguments,
                        wall_tick,
                        thermal,
                    )
                    .map_err(CommitError::Matrix)?;
            }

            EffectKind::OfferKairos => {
                matrix
                    .execute(
                        NexusOpcode::OfferKairos,
                        effect.arguments,
                        wall_tick,
                        thermal,
                    )
                    .map_err(CommitError::Matrix)?;
            }

            EffectKind::ThermalCharge => {
                thermal
                    .charge(effect.arguments[0])
                    .map_err(|_| CommitError::Thermal)?;
            }

            EffectKind::ThermalCredit => {
                thermal.credit_collapse_rebate(effect.arguments[0]);
            }

            EffectKind::Rephase => {
                matrix
                    .rephase(effect.arguments[0] as u16)
                    .map_err(CommitError::Matrix)?;
            }
        }
    }

    Ok(())
}
