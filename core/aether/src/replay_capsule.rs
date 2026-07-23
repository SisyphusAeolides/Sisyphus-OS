use crate::effect_program::PreparedEffects;
use crate::temporal_contract::TemporalContract;
use crate::transition_certificate::{CertificateError, CertificateOutcome, TransitionCertificate};

pub const REPLAY_CAPSULE_MAGIC: u32 = 0x5250_4c31; // RPL1
pub const REPLAY_CAPSULE_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplayCapsuleError {
    BadMagic,
    BadVersion,
    BadChecksum,
    Certificate(CertificateError),
    CertificateNotCommitted,
    EffectDigestMismatch,
    ContractDigestMismatch,
    BeforeRootMismatch,
    BeforeGenerationMismatch,
    SequenceMismatch,
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct ReplayCapsule<const N: usize> {
    pub magic: u32,
    pub version: u16,
    pub reserved: u16,

    pub sequence: u64,

    prepared: PreparedEffects<N>,
    contract: TemporalContract,
    certificate: TransitionCertificate,

    checksum: u64,
}

impl<const N: usize> ReplayCapsule<N> {
    pub fn new(
        prepared: PreparedEffects<N>,
        contract: TemporalContract,
        certificate: TransitionCertificate,
    ) -> Self {
        let mut capsule = Self {
            magic: REPLAY_CAPSULE_MAGIC,
            version: REPLAY_CAPSULE_VERSION,
            reserved: 0,
            sequence: certificate.sequence,
            prepared,
            contract,
            certificate,
            checksum: 0,
        };

        capsule.checksum = capsule.compute_checksum();
        capsule
    }

    pub fn validate(&self) -> Result<(), ReplayCapsuleError> {
        if self.magic != REPLAY_CAPSULE_MAGIC {
            return Err(ReplayCapsuleError::BadMagic);
        }

        if self.version != REPLAY_CAPSULE_VERSION {
            return Err(ReplayCapsuleError::BadVersion);
        }

        if self.checksum != self.compute_checksum() {
            return Err(ReplayCapsuleError::BadChecksum);
        }

        let outcome = self
            .certificate
            .validate()
            .map_err(ReplayCapsuleError::Certificate)?;

        if outcome != CertificateOutcome::Committed {
            return Err(ReplayCapsuleError::CertificateNotCommitted);
        }

        if self.sequence != self.certificate.sequence {
            return Err(ReplayCapsuleError::SequenceMismatch);
        }

        if self.prepared.digest() != self.certificate.effect_digest {
            return Err(ReplayCapsuleError::EffectDigestMismatch);
        }

        if self.contract.digest() != self.certificate.contract_digest {
            return Err(ReplayCapsuleError::ContractDigestMismatch);
        }

        if self.prepared.expected_state_root() != self.certificate.before_root
            || self.contract.expected_state_root != self.certificate.before_root
        {
            return Err(ReplayCapsuleError::BeforeRootMismatch);
        }

        if self.prepared.expected_generation() != self.certificate.generation_before
            || self.contract.expected_generation != self.certificate.generation_before
        {
            return Err(ReplayCapsuleError::BeforeGenerationMismatch);
        }

        Ok(())
    }

    pub const fn prepared(&self) -> PreparedEffects<N> {
        self.prepared
    }

    pub const fn contract(&self) -> TemporalContract {
        self.contract
    }

    pub const fn certificate(&self) -> TransitionCertificate {
        self.certificate
    }

    pub const fn checksum(&self) -> u64 {
        self.checksum
    }

    fn compute_checksum(&self) -> u64 {
        let header = u64::from(self.magic) | (u64::from(self.version) << 32);

        let mut digest = mix(0x5245_504c_4159_5f31, header);

        digest = mix(digest, self.sequence);
        digest = mix(digest, self.prepared.digest());
        digest = mix(digest, self.contract.digest());
        digest = mix(digest, self.certificate.checksum);
        digest = mix(digest, self.certificate.before_root);
        digest = mix(digest, self.certificate.after_root);

        mix(
            digest,
            u64::from(self.certificate.generation_before)
                | (u64::from(self.certificate.generation_after) << 32),
        )
    }
}

fn mix(mut state: u64, value: u64) -> u64 {
    state ^= value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    state = state.rotate_left(29);
    state = state.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    state ^ (state >> 31)
}
