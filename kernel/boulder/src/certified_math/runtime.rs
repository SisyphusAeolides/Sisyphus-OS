//! End-to-end adapter for the current ManifoldOrchestrator.
//!
//! The runtime converts one manifold observation into independently verified
//! Hodge, persistence, spectral, tropical, and convex-optimization artifacts,
//! then combines them with externally supplied sheaf, stabilizer, and density
//! proofs.

use super::bridge::{
    default_hodge_tau_q32, graph_from_hodge, hodge_state_from_actuation,
    populate_filtered_complex_from_hodge, pressure_from_actuation,
    quadratic_program_from_actuation, tropical_from_resource_quiver,
};
use super::certificate::{
    CertificationError, CertificationPolicy, CertifiedActuation, DensityProof, MathDomainSecrets,
    ProofArtifacts, ProofCarryingController,
};
use super::exact_ntt::{ExactNttError, ExactSpectralFairQueue, SpectralDecision};
use super::hodge_implicit::{HodgeSolveError, HodgeStepCertificate, MAX_VERTICES};
use super::persistent::{
    FilteredComplex, PersistenceDigest, PersistenceError, PersistenceReport, PersistenceWorkspace,
};
use super::primal_dual::{
    MAX_VARIABLES, OptimizationError, OptimizationResult, PrimalDualSolver, Q32_ONE,
};
use super::sheaf::GlueCertificate;
use super::symplectic::SyndromeCertificate;
use super::tropical::{TropicalCluster, TropicalError, TropicalMutationCertificate};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimePolicy {
    pub hodge_tau_q32: i64,
    pub hodge_iterations: u16,
    pub hodge_tolerance_q32: u64,
    pub queue_service: u64,
    pub tropical_pressure_threshold: u64,
    pub optimization_primal_step_q32: i64,
    pub optimization_dual_step_q32: i64,
    pub optimization_iterations: u16,
}

impl RuntimePolicy {
    pub const STRICT: Self = Self {
        hodge_tau_q32: Q32_ONE / 8,
        hodge_iterations: 64,
        hodge_tolerance_q32: 1 << 12,
        queue_service: 1,
        tropical_pressure_threshold: 0,
        optimization_primal_step_q32: Q32_ONE / 32,
        optimization_dual_step_q32: Q32_ONE / 32,
        optimization_iterations: 256,
    };

    pub const fn with_default_tau(mut self) -> Self {
        self.hodge_tau_q32 = default_hodge_tau_q32();
        self
    }
}

pub struct ExternalProofs<'a> {
    pub sheaf: &'a GlueCertificate,
    pub stabilizer: &'a SyndromeCertificate,
    pub density: DensityProof<'a>,
}

#[derive(Debug, Eq, PartialEq)]
pub struct CertifiedRuntimeStep {
    pub actuation: CertifiedActuation,
    pub hodge_state_q32: [i64; MAX_VERTICES],
    pub hodge: HodgeStepCertificate,
    pub persistence: PersistenceDigest,
    pub optimization: OptimizationResult,
    pub spectral: SpectralDecision,
    pub tropical: TropicalMutationCertificate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeError {
    Hodge(HodgeSolveError),
    Persistence(PersistenceError),
    Optimization(OptimizationError),
    Spectral(ExactNttError),
    Tropical(TropicalError),
    Certification(CertificationError),
    InvalidManifold,
}

impl From<HodgeSolveError> for RuntimeError {
    fn from(error: HodgeSolveError) -> Self {
        Self::Hodge(error)
    }
}

impl From<PersistenceError> for RuntimeError {
    fn from(error: PersistenceError) -> Self {
        Self::Persistence(error)
    }
}

impl From<OptimizationError> for RuntimeError {
    fn from(error: OptimizationError) -> Self {
        Self::Optimization(error)
    }
}

impl From<ExactNttError> for RuntimeError {
    fn from(error: ExactNttError) -> Self {
        Self::Spectral(error)
    }
}

impl From<TropicalError> for RuntimeError {
    fn from(error: TropicalError) -> Self {
        Self::Tropical(error)
    }
}

impl From<CertificationError> for RuntimeError {
    fn from(error: CertificationError) -> Self {
        Self::Certification(error)
    }
}

pub struct CertifiedMathRuntime {
    secrets: MathDomainSecrets,
    policy: RuntimePolicy,
    queue: ExactSpectralFairQueue,
    tropical: TropicalCluster,
    optimizer: PrimalDualSolver,
    controller: ProofCarryingController,
    source_mutation_count: u32,
    persistence_complex: FilteredComplex,
    persistence_workspace: PersistenceWorkspace,
    last_persistence: PersistenceReport,
}

impl CertifiedMathRuntime {
    pub fn from_manifold(
        manifold: &crate::manifold_orchestrator::ManifoldOrchestrator,
        classes: u8,
        secrets: MathDomainSecrets,
        runtime_policy: RuntimePolicy,
        certification_policy: CertificationPolicy,
    ) -> Result<Self, RuntimeError> {
        let tropical = tropical_from_resource_quiver(manifold.quiver(), secrets.tropical)?;
        let queue = ExactSpectralFairQueue::new(classes, secrets.spectral)?;
        let optimizer = PrimalDualSolver::new(
            runtime_policy.optimization_primal_step_q32,
            runtime_policy.optimization_dual_step_q32,
            runtime_policy.optimization_iterations,
            secrets.optimization,
        )?;
        let controller = ProofCarryingController::new(secrets, certification_policy)?;

        Ok(Self {
            secrets,
            policy: runtime_policy,
            queue,
            tropical,
            optimizer,
            controller,
            source_mutation_count: manifold.quiver().mutation_count,
            persistence_complex: FilteredComplex::new(),
            persistence_workspace: PersistenceWorkspace::new(),
            last_persistence: PersistenceReport::EMPTY,
        })
    }

    pub fn step(
        &mut self,
        manifold: &crate::manifold_orchestrator::ManifoldOrchestrator,
        observation: &crate::manifold_orchestrator::Actuation,
        external: ExternalProofs<'_>,
    ) -> Result<CertifiedRuntimeStep, RuntimeError> {
        if observation.n_ceilings == 0
            || observation.n_ceilings as usize > observation.ceilings.len()
            || observation.n_migrate as usize > observation.migrate.len()
            || manifold.hodge().n_v == 0
            || manifold.quiver().n == 0
        {
            return Err(RuntimeError::InvalidManifold);
        }

        if manifold.quiver().mutation_count != self.source_mutation_count {
            self.tropical =
                tropical_from_resource_quiver(manifold.quiver(), self.secrets.tropical)?;
            self.source_mutation_count = manifold.quiver().mutation_count;
        }

        let graph = graph_from_hodge(manifold.hodge())?;
        let initial = hodge_state_from_actuation(observation);
        let (hodge_state, hodge_certificate) = graph.solve_implicit(
            &initial,
            self.policy.hodge_tau_q32,
            self.policy.hodge_iterations,
            self.policy.hodge_tolerance_q32,
            self.secrets.hodge,
        )?;

        populate_filtered_complex_from_hodge(manifold.hodge(), &mut self.persistence_complex)?;
        self.persistence_complex.reduce_into(
            self.secrets.persistence,
            &mut self.persistence_workspace,
            &mut self.last_persistence,
        )?;

        let program = quadratic_program_from_actuation(observation)?;
        let mut initial_allocation = [0_i64; MAX_VARIABLES];
        let allocation_count = program.variables.min(MAX_VARIABLES);
        initial_allocation[..allocation_count].copy_from_slice(&hodge_state[..allocation_count]);
        let optimization = self.optimizer.solve(&program, &initial_allocation)?;

        self.charge_queue(observation)?;
        let spectral = self.queue.serve(self.policy.queue_service)?;

        let pressures = pressure_from_actuation(observation);
        let tropical = match self
            .tropical
            .mutate_max_pressure(&pressures, self.policy.tropical_pressure_threshold)?
        {
            Some(certificate) => certificate,
            None => {
                let node = spectral.class as usize % self.tropical.node_count();
                self.tropical.mutate(node)?
            }
        };

        let actuation = self.controller.certify(ProofArtifacts {
            hodge: Some(&hodge_certificate),
            optimization: Some(&optimization),
            sheaf: Some(external.sheaf),
            stabilizer: Some(external.stabilizer),
            persistence: Some(&self.last_persistence),
            spectral: Some(&spectral),
            tropical: Some(&tropical),
            density: Some(external.density),
        })?;

        let persistence = self.last_persistence.digest();

        Ok(CertifiedRuntimeStep {
            actuation,
            hodge_state_q32: hodge_state,
            hodge: hodge_certificate,
            persistence,
            optimization,
            spectral,
            tropical,
        })
    }

    pub fn last_persistence(&self) -> &PersistenceReport {
        &self.last_persistence
    }

    pub fn controller(&self) -> &ProofCarryingController {
        &self.controller
    }

    pub fn queue(&self) -> &ExactSpectralFairQueue {
        &self.queue
    }

    pub fn tropical(&self) -> &TropicalCluster {
        &self.tropical
    }

    fn charge_queue(
        &mut self,
        observation: &crate::manifold_orchestrator::Actuation,
    ) -> Result<(), ExactNttError> {
        let classes = self.queue.deficits().len();

        for index in 0..(observation.n_migrate as usize).min(observation.migrate.len()) {
            let class = index % classes;
            let amount = observation.migrate[index].unsigned_abs().max(1) as u64;
            self.queue.charge(class, amount)?;
        }

        let fair_class = observation.fair_class as usize % classes;
        self.queue.charge(fair_class, observation.energy0.max(1))?;
        Ok(())
    }
}
