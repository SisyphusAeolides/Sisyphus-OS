use aether::causal_quorum::QuorumTicket;
use aether::effect_program::PreparedEffects;
use aether::temporal_contract::{ContractError, TemporalContract, TemporalObservation};

use crate::nexus_commit::{CommitError, CommitReceipt, NexusCommitEngine};
use crate::sync::SpinLock;

#[derive(Clone, Copy)]
pub struct PendingCommit<const N: usize> {
    pub ticket: QuorumTicket,
    pub prepared: PreparedEffects<N>,
    pub contract: TemporalContract,

    pub proposed_at: u64,
    pub deadline: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReactorError {
    AlreadyPending,
    NoPendingCommit,
    Contract(ContractError),
    Commit(CommitError),
}

#[derive(Clone, Copy)]
pub enum ReactorPoll<const N: usize> {
    Idle,

    Waiting {
        ticket: QuorumTicket,
        prepared: u32,
        required: u32,
    },

    Ready(PendingCommit<N>),
    Expired(PendingCommit<N>),
}

pub struct CausalCommitReactor<const CPUS: usize, const WITNESSES: usize, const N: usize> {
    engine: NexusCommitEngine<CPUS, WITNESSES>,
    pending: SpinLock<Option<PendingCommit<N>>>,
}

impl<const CPUS: usize, const WITNESSES: usize, const N: usize>
    CausalCommitReactor<CPUS, WITNESSES, N>
{
    pub const fn new(seed: u64) -> Self {
        Self {
            engine: NexusCommitEngine::new(seed),
            pending: SpinLock::new(None),
        }
    }

    pub fn propose(
        &self,
        prepared: PreparedEffects<N>,
        contract: TemporalContract,
        observed: TemporalObservation,
        required_cpus: usize,
        now_tick: u64,
    ) -> Result<QuorumTicket, ReactorError> {
        contract
            .verify_before(observed, now_tick)
            .map_err(ReactorError::Contract)?;

        let mut pending = self.pending.lock();

        if pending.is_some() {
            return Err(ReactorError::AlreadyPending);
        }

        let ticket = self
            .engine
            .begin(&prepared, required_cpus)
            .map_err(ReactorError::Commit)?;

        *pending = Some(PendingCommit {
            ticket,
            prepared,
            contract,
            proposed_at: now_tick,
            deadline: contract.deadline_tick,
        });

        Ok(ticket)
    }

    pub fn acknowledge(
        &self,
        cpu: usize,
        observed: TemporalObservation,
        now_tick: u64,
    ) -> Result<(), ReactorError> {
        let pending = self
            .pending
            .lock()
            .as_ref()
            .copied()
            .ok_or(ReactorError::NoPendingCommit)?;

        pending
            .contract
            .verify_before(observed, now_tick)
            .map_err(ReactorError::Contract)?;

        self.engine
            .acknowledge(cpu, pending.ticket)
            .map_err(ReactorError::Commit)
    }

    pub fn poll(&self, now_tick: u64) -> Result<ReactorPoll<N>, ReactorError> {
        let pending = self.pending.lock().as_ref().copied();

        let Some(pending) = pending else {
            return Ok(ReactorPoll::Idle);
        };

        if now_tick > pending.deadline {
            let removed = self
                .pending
                .lock()
                .take()
                .ok_or(ReactorError::NoPendingCommit)?;

            return Ok(ReactorPoll::Expired(removed));
        }

        if self
            .engine
            .ready(pending.ticket)
            .map_err(ReactorError::Commit)?
        {
            let removed = self
                .pending
                .lock()
                .take()
                .ok_or(ReactorError::NoPendingCommit)?;

            return Ok(ReactorPoll::Ready(removed));
        }

        let prepared = self
            .engine
            .prepared_count(pending.ticket)
            .map_err(ReactorError::Commit)?;

        Ok(ReactorPoll::Waiting {
            ticket: pending.ticket,
            prepared,
            required: pending.ticket.required,
        })
    }

    pub fn finalize_success(
        &self,
        pending: PendingCommit<N>,
        before_root: u64,
        after_root: u64,
        generation_before: u32,
        generation_after: u32,
        wall_tick: u64,
    ) -> Result<CommitReceipt, ReactorError> {
        self.engine
            .finalize_success(
                pending.ticket,
                &pending.prepared,
                before_root,
                after_root,
                generation_before,
                generation_after,
                wall_tick,
            )
            .map_err(ReactorError::Commit)
    }

    pub fn finalize_abort(
        &self,
        pending: PendingCommit<N>,
        state_root: u64,
        generation: u32,
        wall_tick: u64,
        rolled_back: bool,
    ) -> Result<CommitReceipt, ReactorError> {
        self.engine
            .finalize_abort(
                pending.ticket,
                &pending.prepared,
                state_root,
                generation,
                wall_tick,
                rolled_back,
            )
            .map_err(ReactorError::Commit)
    }

    pub fn witness_root(&self) -> u64 {
        self.engine.witness_root()
    }

    pub fn witness_chain_is_valid(&self) -> bool {
        self.engine.witness_chain_is_valid()
    }
}
