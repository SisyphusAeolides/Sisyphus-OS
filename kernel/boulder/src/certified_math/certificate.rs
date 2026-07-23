//! Proof-carrying resource actuation.
//!
//! Numerical output is not authority.  The controller emits an actuation only
//! after every required certificate verifies under an independent domain
//! secret and policy bound.

use super::density::{DensityChannelCertificate, DensityMeasurementCertificate};
use super::exact_ntt::SpectralDecision;
use super::hodge_implicit::HodgeStepCertificate;
use super::persistent::PersistenceReport;
use super::primal_dual::{MAX_VARIABLES, OptimizationResult};
use super::sheaf::GlueCertificate;
use super::symplectic::SyndromeCertificate;
use super::tropical::TropicalMutationCertificate;

pub const PROOF_HODGE: u32 = 1 << 0;
pub const PROOF_OPTIMIZATION: u32 = 1 << 1;
pub const PROOF_SHEAF: u32 = 1 << 2;
pub const PROOF_STABILIZER: u32 = 1 << 3;
pub const PROOF_PERSISTENCE: u32 = 1 << 4;
pub const PROOF_SPECTRAL: u32 = 1 << 5;
pub const PROOF_TROPICAL: u32 = 1 << 6;
pub const PROOF_DENSITY: u32 = 1 << 7;
pub const PROOF_ALL: u32 = PROOF_HODGE
    | PROOF_OPTIMIZATION
    | PROOF_SHEAF
    | PROOF_STABILIZER
    | PROOF_PERSISTENCE
    | PROOF_SPECTRAL
    | PROOF_TROPICAL
    | PROOF_DENSITY;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CertificationError {
    InvalidSecrets,
    Rejected(ActuationRejection),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MathDomainSecrets {
    pub controller: u64,
    pub hodge: u64,
    pub optimization: u64,
    pub sheaf: u64,
    pub stabilizer: u64,
    pub persistence: u64,
    pub spectral: u64,
    pub tropical: u64,
    pub density: u64,
}

impl MathDomainSecrets {
    pub const fn valid(self) -> bool {
        let values = [
            self.controller,
            self.hodge,
            self.optimization,
            self.sheaf,
            self.stabilizer,
            self.persistence,
            self.spectral,
            self.tropical,
            self.density,
        ];

        let mut left = 0_usize;
        while left < values.len() {
            if values[left] == 0 {
                return false;
            }

            let mut right = left + 1;
            while right < values.len() {
                if values[left] == values[right] {
                    return false;
                }
                right += 1;
            }
            left += 1;
        }

        true
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CertificationPolicy {
    pub required_proofs: u32,
    pub hodge_residual_limit_q32: u64,
    pub hodge_mass_limit_q32: u64,
    pub primal_limit_q32: u64,
    pub dual_limit_q32: u64,
    pub stationarity_limit_q32: u64,
    pub complementarity_limit_q32: u64,
    pub density_tolerance_q30: u64,
    pub maximum_essential_h1: u16,
    pub require_stabilizer_membership: bool,
}

impl CertificationPolicy {
    pub const STRICT: Self = Self {
        required_proofs: PROOF_ALL,
        hodge_residual_limit_q32: 1 << 20,
        hodge_mass_limit_q32: 1 << 20,
        primal_limit_q32: 1 << 20,
        dual_limit_q32: 1 << 20,
        stationarity_limit_q32: 1 << 22,
        complementarity_limit_q32: 1 << 22,
        density_tolerance_q30: 1 << 12,
        maximum_essential_h1: 0,
        require_stabilizer_membership: true,
    };

    pub const INVARIANT_PRESERVING: Self = Self {
        require_stabilizer_membership: false,
        ..Self::STRICT
    };
}

#[derive(Clone, Copy)]
pub enum DensityProof<'a> {
    Channel(&'a DensityChannelCertificate),
    Measurement(&'a DensityMeasurementCertificate),
}

impl DensityProof<'_> {
    fn root(self) -> u64 {
        match self {
            Self::Channel(certificate) => certificate.root,
            Self::Measurement(certificate) => certificate.root,
        }
    }

    fn output_root(self) -> u64 {
        match self {
            Self::Channel(certificate) => certificate.output_root,
            Self::Measurement(certificate) => certificate.posterior_root,
        }
    }

    fn verify(self, secret: u64, tolerance_q30: u64) -> bool {
        match self {
            Self::Channel(certificate) => certificate.verify(secret, tolerance_q30),
            Self::Measurement(certificate) => certificate.verify(secret, tolerance_q30),
        }
    }
}

pub struct ProofArtifacts<'a> {
    pub hodge: Option<&'a HodgeStepCertificate>,
    pub optimization: Option<&'a OptimizationResult>,
    pub sheaf: Option<&'a GlueCertificate>,
    pub stabilizer: Option<&'a SyndromeCertificate>,
    pub persistence: Option<&'a PersistenceReport>,
    pub spectral: Option<&'a SpectralDecision>,
    pub tropical: Option<&'a TropicalMutationCertificate>,
    pub density: Option<DensityProof<'a>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CertifiedActuation {
    pub sequence: u64,
    pub queue_class: u8,
    pub allocation_q32: [i64; MAX_VARIABLES],
    pub allocation_count: u8,
    pub proof_mask: u32,
    pub hodge_state_root: u64,
    pub optimization_root: u64,
    pub sheaf_root: u64,
    pub stabilizer_root: u64,
    pub persistence_root: u64,
    pub spectral_root: u64,
    pub tropical_root: u64,
    pub density_root: u64,
    pub root: u64,
}

impl CertifiedActuation {
    pub const EMPTY: Self = Self {
        sequence: 0,
        queue_class: 0,
        allocation_q32: [0; MAX_VARIABLES],
        allocation_count: 0,
        proof_mask: 0,
        hodge_state_root: 0,
        optimization_root: 0,
        sheaf_root: 0,
        stabilizer_root: 0,
        persistence_root: 0,
        spectral_root: 0,
        tropical_root: 0,
        density_root: 0,
        root: 0,
    };

    pub fn verify(&self, secret: u64, required_proofs: u32) -> bool {
        self.sequence != 0
            && self.allocation_count as usize <= MAX_VARIABLES
            && self.proof_mask & required_proofs == required_proofs
            && self.root == actuation_root(secret, self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActuationRejection {
    pub sequence: u64,
    pub missing_proofs: u32,
    pub invalid_proofs: u32,
    pub policy_violations: u32,
    pub observed_proofs: u32,
    pub evidence_root: u64,
    pub root: u64,
}

impl ActuationRejection {
    pub const EMPTY: Self = Self {
        sequence: 0,
        missing_proofs: 0,
        invalid_proofs: 0,
        policy_violations: 0,
        observed_proofs: 0,
        evidence_root: 0,
        root: 0,
    };

    pub fn verify(&self, secret: u64) -> bool {
        self.root == rejection_root(secret, self)
    }
}

pub struct ProofCarryingController {
    secrets: MathDomainSecrets,
    policy: CertificationPolicy,
    next_sequence: u64,
}

impl ProofCarryingController {
    pub fn new(
        secrets: MathDomainSecrets,
        policy: CertificationPolicy,
    ) -> Result<Self, CertificationError> {
        if !secrets.valid() {
            return Err(CertificationError::InvalidSecrets);
        }

        Ok(Self {
            secrets,
            policy,
            next_sequence: 1,
        })
    }

    pub const fn policy(&self) -> CertificationPolicy {
        self.policy
    }

    pub fn certify(
        &mut self,
        artifacts: ProofArtifacts<'_>,
    ) -> Result<CertifiedActuation, CertificationError> {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1).max(1);

        let mut observed = 0_u32;
        let mut invalid = 0_u32;
        let mut policy_violations = 0_u32;
        let mut evidence_root = mix(self.secrets.controller, sequence);

        let mut actuation = CertifiedActuation {
            sequence,
            ..CertifiedActuation::EMPTY
        };

        match artifacts.hodge {
            Some(certificate) => {
                observed |= PROOF_HODGE;
                evidence_root = mix(evidence_root, certificate.root);
                if !certificate.verify(
                    self.secrets.hodge,
                    self.policy.hodge_residual_limit_q32,
                    self.policy.hodge_mass_limit_q32,
                ) {
                    invalid |= PROOF_HODGE;
                } else {
                    actuation.hodge_state_root = certificate.state_root;
                }
            }
            None => {}
        }

        match artifacts.optimization {
            Some(result) => {
                observed |= PROOF_OPTIMIZATION;
                evidence_root = mix(evidence_root, result.certificate.root);
                if !result.certificate.verify(
                    self.secrets.optimization,
                    self.policy.primal_limit_q32,
                    self.policy.dual_limit_q32,
                    self.policy.stationarity_limit_q32,
                    self.policy.complementarity_limit_q32,
                ) {
                    invalid |= PROOF_OPTIMIZATION;
                } else {
                    actuation.allocation_q32 = result.primal_q32;
                    actuation.allocation_count = result.certificate.variables;
                    actuation.optimization_root = result.certificate.root;
                }
            }
            None => {}
        }

        match artifacts.sheaf {
            Some(certificate) => {
                observed |= PROOF_SHEAF;
                evidence_root = mix(evidence_root, certificate.root);
                if !certificate.verify(self.secrets.sheaf) {
                    invalid |= PROOF_SHEAF;
                } else if !certificate.glued() {
                    policy_violations |= PROOF_SHEAF;
                } else {
                    actuation.sheaf_root = certificate.root;
                }
            }
            None => {}
        }

        match artifacts.stabilizer {
            Some(certificate) => {
                observed |= PROOF_STABILIZER;
                evidence_root = mix(evidence_root, certificate.root);
                if !certificate.verify(self.secrets.stabilizer) {
                    invalid |= PROOF_STABILIZER;
                } else if certificate.syndrome != 0
                    || (self.policy.require_stabilizer_membership && !certificate.stabilized)
                {
                    policy_violations |= PROOF_STABILIZER;
                } else {
                    actuation.stabilizer_root = certificate.root;
                }
            }
            None => {}
        }

        match artifacts.persistence {
            Some(report) => {
                observed |= PROOF_PERSISTENCE;
                evidence_root = mix(evidence_root, report.barcode_root);
                if !report.verify(self.secrets.persistence) {
                    invalid |= PROOF_PERSISTENCE;
                } else if report.essential_count[1] > self.policy.maximum_essential_h1 {
                    policy_violations |= PROOF_PERSISTENCE;
                } else {
                    actuation.persistence_root = report.barcode_root;
                }
            }
            None => {}
        }

        match artifacts.spectral {
            Some(decision) => {
                observed |= PROOF_SPECTRAL;
                evidence_root = mix(evidence_root, decision.root);
                if !decision.verify(self.secrets.spectral) {
                    invalid |= PROOF_SPECTRAL;
                } else {
                    actuation.queue_class = decision.class;
                    actuation.spectral_root = decision.root;
                }
            }
            None => {}
        }

        match artifacts.tropical {
            Some(certificate) => {
                observed |= PROOF_TROPICAL;
                evidence_root = mix(evidence_root, certificate.root);
                if !certificate.verify(self.secrets.tropical) {
                    invalid |= PROOF_TROPICAL;
                } else {
                    actuation.tropical_root = certificate.root;
                }
            }
            None => {}
        }

        match artifacts.density {
            Some(proof) => {
                observed |= PROOF_DENSITY;
                evidence_root = mix(evidence_root, proof.root());
                if !proof.verify(self.secrets.density, self.policy.density_tolerance_q30) {
                    invalid |= PROOF_DENSITY;
                } else {
                    actuation.density_root = proof.output_root();
                }
            }
            None => {}
        }

        let missing = self.policy.required_proofs & !observed;
        if missing != 0 || invalid != 0 || policy_violations != 0 {
            let mut rejection = ActuationRejection {
                sequence,
                missing_proofs: missing,
                invalid_proofs: invalid,
                policy_violations,
                observed_proofs: observed,
                evidence_root,
                root: 0,
            };
            rejection.root = rejection_root(self.secrets.controller, &rejection);
            return Err(CertificationError::Rejected(rejection));
        }

        actuation.proof_mask = observed;
        actuation.root = actuation_root(self.secrets.controller, &actuation);
        Ok(actuation)
    }
}

fn actuation_root(secret: u64, actuation: &CertifiedActuation) -> u64 {
    let mut state = mix(secret, actuation.sequence);
    state = mix(state, actuation.queue_class as u64);
    state = mix(state, actuation.allocation_count as u64);
    state = mix(state, actuation.proof_mask as u64);
    for allocation in &actuation.allocation_q32[..actuation.allocation_count as usize] {
        state = mix(state, *allocation as u64);
    }
    state = mix(state, actuation.hodge_state_root);
    state = mix(state, actuation.optimization_root);
    state = mix(state, actuation.sheaf_root);
    state = mix(state, actuation.stabilizer_root);
    state = mix(state, actuation.persistence_root);
    state = mix(state, actuation.spectral_root);
    state = mix(state, actuation.tropical_root);
    mix(state, actuation.density_root)
}

fn rejection_root(secret: u64, rejection: &ActuationRejection) -> u64 {
    let mut state = mix(secret, rejection.sequence);
    state = mix(state, rejection.missing_proofs as u64);
    state = mix(state, rejection.invalid_proofs as u64);
    state = mix(state, rejection.policy_violations as u64);
    state = mix(state, rejection.observed_proofs as u64);
    mix(state, rejection.evidence_root)
}

fn mix(mut state: u64, word: u64) -> u64 {
    state ^= word.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    state ^= state >> 30;
    state = state.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    state ^= state >> 27;
    state = state.wrapping_mul(0x94d0_49bb_1331_11eb);
    state ^ (state >> 31)
}
