use core::convert::TryFrom;

pub const TRANSITION_CERTIFICATE_MAGIC: u32 = 0x5452_4331; // TRC1

pub const TRANSITION_CERTIFICATE_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CertificateOutcome {
    Committed = 1,
    Rejected = 2,
    Diverged = 3,
}

impl TryFrom<u8> for CertificateOutcome {
    type Error = CertificateError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Committed),
            2 => Ok(Self::Rejected),
            3 => Ok(Self::Diverged),
            _ => Err(CertificateError::UnknownOutcome),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CertificateError {
    BadMagic,
    BadVersion,
    UnknownOutcome,
    BadChecksum,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C, align(128))]
pub struct TransitionCertificate {
    pub magic: u32,
    pub version: u16,
    pub outcome: u8,
    pub reality_mask: u8,

    pub sequence: u64,

    pub effect_digest: u64,
    pub contract_digest: u64,

    pub before_root: u64,
    pub after_root: u64,

    pub witness_root: u64,
    pub invariant_digest: u64,

    pub wall_tick: u64,

    pub heat_before: u64,
    pub heat_after: u64,

    pub generation_before: u32,
    pub generation_after: u32,

    pub participants: u16,
    pub effect_count: u16,

    pub phase_before: u16,
    pub phase_after: u16,

    pub passed_invariants: u32,
    pub failed_invariants: u32,

    pub reserved: [u8; 8],

    pub checksum: u64,
}

const _: () = assert!(core::mem::size_of::<TransitionCertificate>() == 128);

impl TransitionCertificate {
    pub const ZERO: Self = Self {
        magic: 0,
        version: 0,
        outcome: 0,
        reality_mask: 0,
        sequence: 0,
        effect_digest: 0,
        contract_digest: 0,
        before_root: 0,
        after_root: 0,
        witness_root: 0,
        invariant_digest: 0,
        wall_tick: 0,
        heat_before: 0,
        heat_after: 0,
        generation_before: 0,
        generation_after: 0,
        participants: 0,
        effect_count: 0,
        phase_before: 0,
        phase_after: 0,
        passed_invariants: 0,
        failed_invariants: 0,
        reserved: [0; 8],
        checksum: 0,
    };

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        outcome: CertificateOutcome,
        reality_mask: u8,
        sequence: u64,
        effect_digest: u64,
        contract_digest: u64,
        before_root: u64,
        after_root: u64,
        witness_root: u64,
        invariant_digest: u64,
        wall_tick: u64,
        heat_before: u64,
        heat_after: u64,
        generation_before: u32,
        generation_after: u32,
        participants: u16,
        effect_count: u16,
        phase_before: u16,
        phase_after: u16,
        passed_invariants: u32,
        failed_invariants: u32,
    ) -> Self {
        let mut certificate = Self {
            magic: TRANSITION_CERTIFICATE_MAGIC,
            version: TRANSITION_CERTIFICATE_VERSION,
            outcome: outcome as u8,
            reality_mask,
            sequence,
            effect_digest,
            contract_digest,
            before_root,
            after_root,
            witness_root,
            invariant_digest,
            wall_tick,
            heat_before,
            heat_after,
            generation_before,
            generation_after,
            participants,
            effect_count,
            phase_before,
            phase_after,
            passed_invariants,
            failed_invariants,
            reserved: [0; 8],
            checksum: 0,
        };

        certificate.checksum = certificate.compute_checksum();

        certificate
    }

    pub fn validate(&self) -> Result<CertificateOutcome, CertificateError> {
        if self.magic != TRANSITION_CERTIFICATE_MAGIC {
            return Err(CertificateError::BadMagic);
        }

        if self.version != TRANSITION_CERTIFICATE_VERSION {
            return Err(CertificateError::BadVersion);
        }

        if self.checksum != self.compute_checksum() {
            return Err(CertificateError::BadChecksum);
        }

        CertificateOutcome::try_from(self.outcome)
    }

    fn compute_checksum(&self) -> u64 {
        let header = u64::from(self.magic)
            | (u64::from(self.version) << 32)
            | (u64::from(self.outcome) << 48)
            | (u64::from(self.reality_mask) << 56);

        let mut digest = mix(0x5452_414e_5349_5449, header);

        digest = mix(digest, self.sequence);
        digest = mix(digest, self.effect_digest);
        digest = mix(digest, self.contract_digest);
        digest = mix(digest, self.before_root);
        digest = mix(digest, self.after_root);
        digest = mix(digest, self.witness_root);
        digest = mix(digest, self.invariant_digest);
        digest = mix(digest, self.wall_tick);
        digest = mix(digest, self.heat_before);
        digest = mix(digest, self.heat_after);

        digest = mix(
            digest,
            u64::from(self.generation_before) | (u64::from(self.generation_after) << 32),
        );

        digest = mix(
            digest,
            u64::from(self.participants)
                | (u64::from(self.effect_count) << 16)
                | (u64::from(self.phase_before) << 32)
                | (u64::from(self.phase_after) << 48),
        );

        digest = mix(
            digest,
            u64::from(self.passed_invariants) | (u64::from(self.failed_invariants) << 32),
        );

        digest
    }
}

fn mix(mut state: u64, value: u64) -> u64 {
    state ^= value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    state = state.rotate_left(27);
    state = state.wrapping_mul(0x94d0_49bb_1331_11eb);
    state ^ (state >> 31)
}
