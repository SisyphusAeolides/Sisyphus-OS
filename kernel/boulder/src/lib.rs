#![no_std]
extern crate alloc;

pub mod aether;
pub mod arch;
pub mod argus_sentinel;
pub mod axiom_manifold;
pub mod blacklab;
pub mod blacklab_bootstrap;
pub mod blacklab_control_plane;
pub mod boot;
pub mod capability;
pub mod cassandra_reactor;
pub mod causal_lattice;
pub mod charybdis_dma_firewall;
pub mod chronovore;
pub mod commit_reactor;
pub mod continuity_vault;
pub mod counterfactual;
pub mod cpu;
pub mod divergence_vault;
pub mod drivers;
pub mod fabric;
pub mod fabric_weave;
pub mod fs;
pub mod ghost_chronicle;
pub mod hw;
pub mod ignition;
pub mod interrupts;
pub mod ipc;
pub mod kairos;
pub mod lab_capsule;
pub mod lease_lattice;
pub mod many_worlds;
pub mod memory;
pub mod mirage;
pub mod mmio;
pub mod mnemosyne_ledger;
pub mod module;
pub mod nexus_commit;
pub mod nexus_deferred;
pub mod nexus_gateway;
pub mod nexus_matrix;
pub mod nexus_plane;
pub mod nexus_runtime;
pub mod oracular_mesh;
pub mod ouroboros;
pub mod paradox;
pub mod phase_rotor;
pub mod policy_chamber;
pub mod process;
pub mod quantum_crest_gateway;
pub mod quantum_desktop_recovery;
pub mod quantum_nexus;
pub mod reality_forge;
pub mod scheduler;
pub mod serial;
pub mod shim;
pub mod singularity;
pub mod sync;
pub mod syscalls;
pub mod tartarus_deep;
pub mod temporal_echo;
pub mod thermogenesis;

use crate::axiom_manifold::{
    AxiomManifold, AxiomPolicy, CELL_READ_ONLY, CommitCertificate, DraftError, DriveOutcome,
    ManifoldError, Mutation, MutationOp, ReadConstraint, RejectReason, StateCell, TransactionDraft,
    TransactionId,
};

pub const CELL_NUMA_ZERO_CREDITS: usize = 0;
pub const CELL_NUMA_ONE_CREDITS: usize = 1;
pub const CELL_THERMAL_BUDGET: usize = 2;
pub const CELL_DMA_RESERVE: usize = 3;
pub const CELL_COMMIT_EPOCH_FLOOR: usize = 4;

pub const CLASS_COMPUTE_CREDITS: u16 = 0x0101;
pub const CLASS_THERMAL_BUDGET: u16 = 0x0201;
pub const CLASS_DMA_RESERVE: u16 = 0x0301;
pub const CLASS_EPOCH_FLOOR: u16 = 0x0401;

// 256 state cells
// 128 in-flight or retained transactions
// 32 causal origins and witnesses
// 16 optimistic reads per transaction
// 16 reversible mutations per transaction
// 8 explicit dependencies per transaction
pub type KernelAxiomManifold = AxiomManifold<256, 128, 32, 16, 16, 8>;

pub static KERNEL_AXIOM_MANIFOLD: KernelAxiomManifold = KernelAxiomManifold::new(0);

pub fn seed_kernel_axioms() -> Result<(), ManifoldError> {
    KERNEL_AXIOM_MANIFOLD.seed_cell(CELL_NUMA_ZERO_CREDITS, 500_000, CLASS_COMPUTE_CREDITS, 0)?;

    KERNEL_AXIOM_MANIFOLD.seed_cell(CELL_NUMA_ONE_CREDITS, 500_000, CLASS_COMPUTE_CREDITS, 0)?;

    KERNEL_AXIOM_MANIFOLD.seed_cell(CELL_THERMAL_BUDGET, 48 << 16, CLASS_THERMAL_BUDGET, 0)?;

    KERNEL_AXIOM_MANIFOLD.seed_cell(CELL_DMA_RESERVE, 65_536, CLASS_DMA_RESERVE, 0)?;

    KERNEL_AXIOM_MANIFOLD.seed_cell(
        CELL_COMMIT_EPOCH_FLOOR,
        0,
        CLASS_EPOCH_FLOOR,
        CELL_READ_ONLY,
    )?;

    Ok(())
}

pub fn read_axiom_cell(index: usize) -> Option<StateCell> {
    KERNEL_AXIOM_MANIFOLD.cell(index)
}

const TOTAL_COMPUTE_CREDITS: u64 = 1_000_000;
const MAX_SINGLE_NUMA_CREDITS: u64 = 875_000;
const MAX_DMA_RESERVE: u64 = 262_144;
const THERMAL_FLOOR_Q16: u64 = 20 << 16;
const THERMAL_CEILING_Q16: u64 = 95 << 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KernelAxiomFault {
    ClassMismatch,
    ForbiddenMutation,
    ArithmeticOverflow,
    ComputeConservationBroken,
    NumaConcentrationExceeded,
    DmaReserveExceeded,
    ThermalEnvelopeBroken,
    GeometryMissing,
}

pub struct KernelAxiomPolicy;

impl AxiomPolicy for KernelAxiomPolicy {
    type Fault = KernelAxiomFault;

    fn authorize(
        &self,
        kind: u16,
        mutation: &Mutation,
        before: &StateCell,
    ) -> Result<(), Self::Fault> {
        if kind != before.class {
            return Err(KernelAxiomFault::ClassMismatch);
        }

        match before.class {
            CLASS_COMPUTE_CREDITS => {
                if !matches!(
                    mutation.op,
                    MutationOp::Set
                        | MutationOp::AddSigned
                        | MutationOp::Min
                        | MutationOp::Max
                        | MutationOp::CompareExchange
                ) {
                    return Err(KernelAxiomFault::ForbiddenMutation);
                }
            }

            CLASS_THERMAL_BUDGET => {
                if !matches!(
                    mutation.op,
                    MutationOp::Set
                        | MutationOp::AddSigned
                        | MutationOp::Min
                        | MutationOp::Max
                        | MutationOp::CompareExchange
                ) {
                    return Err(KernelAxiomFault::ForbiddenMutation);
                }
            }

            CLASS_DMA_RESERVE => {
                if !matches!(
                    mutation.op,
                    MutationOp::Set
                        | MutationOp::Min
                        | MutationOp::Max
                        | MutationOp::CompareExchange
                ) {
                    return Err(KernelAxiomFault::ForbiddenMutation);
                }
            }

            _ => return Err(KernelAxiomFault::ClassMismatch),
        }

        Ok(())
    }

    fn validate_state(&self, cells: &[StateCell]) -> Result<(), Self::Fault> {
        let numa_zero = cells
            .get(CELL_NUMA_ZERO_CREDITS)
            .ok_or(KernelAxiomFault::GeometryMissing)?
            .value;

        let numa_one = cells
            .get(CELL_NUMA_ONE_CREDITS)
            .ok_or(KernelAxiomFault::GeometryMissing)?
            .value;

        let thermal = cells
            .get(CELL_THERMAL_BUDGET)
            .ok_or(KernelAxiomFault::GeometryMissing)?
            .value;

        let dma_reserve = cells
            .get(CELL_DMA_RESERVE)
            .ok_or(KernelAxiomFault::GeometryMissing)?
            .value;

        let total = numa_zero
            .checked_add(numa_one)
            .ok_or(KernelAxiomFault::ArithmeticOverflow)?;

        if total != TOTAL_COMPUTE_CREDITS {
            return Err(KernelAxiomFault::ComputeConservationBroken);
        }

        if numa_zero > MAX_SINGLE_NUMA_CREDITS || numa_one > MAX_SINGLE_NUMA_CREDITS {
            return Err(KernelAxiomFault::NumaConcentrationExceeded);
        }

        if dma_reserve > MAX_DMA_RESERVE {
            return Err(KernelAxiomFault::DmaReserveExceeded);
        }

        if !(THERMAL_FLOOR_Q16..=THERMAL_CEILING_Q16).contains(&thermal) {
            return Err(KernelAxiomFault::ThermalEnvelopeBroken);
        }

        Ok(())
    }
}

pub type KernelAxiomDraft = TransactionDraft<16, 16, 8>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RebalanceError {
    Draft(DraftError),
    Manifold(ManifoldError),
    DeltaTooLarge,
}

impl From<DraftError> for RebalanceError {
    fn from(error: DraftError) -> Self {
        Self::Draft(error)
    }
}

impl From<ManifoldError> for RebalanceError {
    fn from(error: ManifoldError) -> Self {
        Self::Manifold(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RebalancePlan {
    pub credit_transfer: TransactionId,
    pub thermal_reconciliation: TransactionId,
}

#[allow(clippy::too_many_arguments)]
pub fn stage_numa_rebalance(
    origin: usize,
    wall_tick: u64,
    deadline_tick: u64,
    numa_zero_version: u32,
    numa_one_version: u32,
    credits_from_zero_to_one: u64,
    thermal_version: u32,
    thermal_before_q16: u64,
    thermal_after_q16: u64,
    witness_quorum: u8,
    nonce: u64,
) -> Result<RebalancePlan, RebalanceError> {
    let signed_delta =
        i64::try_from(credits_from_zero_to_one).map_err(|_| RebalanceError::DeltaTooLarge)?;

    let mut credit_transfer = KernelAxiomDraft::new(
        CLASS_COMPUTE_CREDITS,
        240,
        witness_quorum,
        deadline_tick,
        nonce ^ 0x4E55_4D41_5F58_4645,
    );

    credit_transfer.push_read(ReadConstraint::exact(
        CELL_NUMA_ZERO_CREDITS as u16,
        numa_zero_version,
    ))?;

    credit_transfer.push_read(ReadConstraint::exact(
        CELL_NUMA_ONE_CREDITS as u16,
        numa_one_version,
    ))?;

    credit_transfer.push_write(Mutation::add_signed(
        CELL_NUMA_ZERO_CREDITS as u16,
        -signed_delta,
    ))?;

    credit_transfer.push_write(Mutation::add_signed(
        CELL_NUMA_ONE_CREDITS as u16,
        signed_delta,
    ))?;

    let credit_transfer_id = KERNEL_AXIOM_MANIFOLD.submit(origin, wall_tick, credit_transfer)?;

    let mut thermal_reconciliation = KernelAxiomDraft::new(
        CLASS_THERMAL_BUDGET,
        224,
        witness_quorum,
        deadline_tick,
        nonce ^ 0x5448_4552_4D41_4C21,
    );

    thermal_reconciliation.push_dependency(credit_transfer_id)?;

    thermal_reconciliation.push_read(ReadConstraint::masked(
        CELL_THERMAL_BUDGET as u16,
        thermal_version,
        u64::MAX,
        thermal_before_q16,
    ))?;

    thermal_reconciliation.push_write(Mutation::compare_exchange(
        CELL_THERMAL_BUDGET as u16,
        thermal_before_q16,
        thermal_after_q16,
    ))?;

    let thermal_reconciliation_id =
        KERNEL_AXIOM_MANIFOLD.submit(origin, wall_tick, thermal_reconciliation)?;

    Ok(RebalancePlan {
        credit_transfer: credit_transfer_id,
        thermal_reconciliation: thermal_reconciliation_id,
    })
}

#[derive(Debug, Eq, PartialEq)]
pub enum AxiomReactorStep {
    Quiescent,

    Waiting {
        prepared: usize,
    },

    Rejected {
        transaction: TransactionId,
        reason: RejectReason,
        fault: Option<KernelAxiomFault>,
    },

    Committed {
        certificate: CommitCertificate,
    },
}

pub fn attest_axiom_transaction(
    transaction: TransactionId,
    witness_cpu: usize,
) -> Result<bool, ManifoldError> {
    KERNEL_AXIOM_MANIFOLD.attest(transaction, witness_cpu)
}

pub fn attest_from_cpu_mask(
    transaction: TransactionId,
    mut cpu_mask: u64,
) -> Result<u32, ManifoldError> {
    let mut accepted = 0_u32;

    while cpu_mask != 0 {
        let cpu = cpu_mask.trailing_zeros() as usize;
        cpu_mask &= cpu_mask - 1;

        if KERNEL_AXIOM_MANIFOLD.attest(transaction, cpu)? {
            accepted = accepted.saturating_add(1);
        }
    }

    Ok(accepted)
}

pub fn drive_axiom_reactor(now_tick: u64) -> AxiomReactorStep {
    match KERNEL_AXIOM_MANIFOLD.drive(now_tick, &KernelAxiomPolicy) {
        DriveOutcome::Idle => AxiomReactorStep::Quiescent,

        DriveOutcome::Blocked { prepared } => AxiomReactorStep::Waiting { prepared },

        DriveOutcome::Rejected {
            transaction,
            reason,
            fault,
        } => AxiomReactorStep::Rejected {
            transaction,
            reason,
            fault,
        },

        DriveOutcome::Committed(certificate) => AxiomReactorStep::Committed { certificate },
    }
}

pub fn drain_axiom_reactor(
    now_tick: u64,
    maximum_transitions: usize,
    mut publish: impl FnMut(CommitCertificate),
) -> usize {
    let mut committed = 0;

    while committed < maximum_transitions {
        match drive_axiom_reactor(now_tick) {
            AxiomReactorStep::Committed { certificate } => {
                publish(certificate);
                committed += 1;
            }

            AxiomReactorStep::Rejected { .. } => {
                // Continue once so dependency-failure propagation can advance.
                continue;
            }

            AxiomReactorStep::Quiescent | AxiomReactorStep::Waiting { .. } => break,
        }
    }

    committed
}
