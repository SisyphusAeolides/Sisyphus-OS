use aether::temporal_contract::TemporalObservation;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct DivergenceRecord {
    pub sequence: u64,
    pub effect_digest: u64,

    pub alpha: TemporalObservation,
    pub beta: TemporalObservation,
    pub gamma: TemporalObservation,

    pub present_mask: u8,
    pub failure_mask: u8,
    pub majority_mask: u8,
    pub reserved: u8,

    pub chain_before: u64,
    pub chain_after: u64,
}

impl DivergenceRecord {
    pub const EMPTY: Self = Self {
        sequence: 0,
        effect_digest: 0,
        alpha: TemporalObservation::ZERO,
        beta: TemporalObservation::ZERO,
        gamma: TemporalObservation::ZERO,
        present_mask: 0,
        failure_mask: 0,
        majority_mask: 0,
        reserved: 0,
        chain_before: 0,
        chain_after: 0,
    };
}

pub struct DivergenceVault<const N: usize> {
    records: [DivergenceRecord; N],
    cursor: usize,
    length: usize,
    next_sequence: u64,
    chain_root: u64,
}

impl<const N: usize> DivergenceVault<N> {
    pub const fn new(seed: u64) -> Self {
        Self {
            records: [DivergenceRecord::EMPTY; N],
            cursor: 0,
            length: 0,
            next_sequence: 1,
            chain_root: seed,
        }
    }

    pub fn record(
        &mut self,
        effect_digest: u64,
        alpha: Option<TemporalObservation>,
        beta: Option<TemporalObservation>,
        gamma: Option<TemporalObservation>,
        failure_mask: u8,
        majority_mask: u8,
    ) -> Option<u64> {
        if N == 0 {
            return None;
        }

        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1).max(1);

        let present_mask = u8::from(alpha.is_some())
            | (u8::from(beta.is_some()) << 1)
            | (u8::from(gamma.is_some()) << 2);

        let chain_before = self.chain_root;

        let mut record = DivergenceRecord {
            sequence,
            effect_digest,
            alpha: alpha.unwrap_or(TemporalObservation::ZERO),
            beta: beta.unwrap_or(TemporalObservation::ZERO),
            gamma: gamma.unwrap_or(TemporalObservation::ZERO),
            present_mask,
            failure_mask,
            majority_mask,
            reserved: 0,
            chain_before,
            chain_after: 0,
        };

        record.chain_after = record_digest(record);

        self.records[self.cursor] = record;
        self.cursor = (self.cursor + 1) % N;
        self.length = (self.length + 1).min(N);
        self.chain_root = record.chain_after;

        Some(sequence)
    }

    pub fn latest(&self) -> Option<DivergenceRecord> {
        if self.length == 0 {
            return None;
        }

        let index = (self.cursor + N - 1) % N;
        Some(self.records[index])
    }

    pub const fn root(&self) -> u64 {
        self.chain_root
    }

    pub const fn retained(&self) -> usize {
        self.length
    }
}

fn record_digest(record: DivergenceRecord) -> u64 {
    let mut digest = mix(record.chain_before, record.sequence);

    digest = mix(digest, record.effect_digest);
    digest = mix(digest, observation_digest(record.alpha));
    digest = mix(digest, observation_digest(record.beta));
    digest = mix(digest, observation_digest(record.gamma));

    mix(
        digest,
        u64::from(record.present_mask)
            | (u64::from(record.failure_mask) << 8)
            | (u64::from(record.majority_mask) << 16),
    )
}

fn observation_digest(observation: TemporalObservation) -> u64 {
    let mut digest = mix(
        u64::from(observation.generation),
        u64::from(observation.pairs_live),
    );

    digest = mix(digest, observation.state_root);
    digest = mix(digest, observation.collapses);
    digest = mix(digest, observation.heat);
    mix(digest, u64::from(observation.phase_bin))
}

fn mix(mut state: u64, value: u64) -> u64 {
    state ^= value.wrapping_add(0x517c_c1b7_2722_0a95);
    state = state.rotate_left(31);
    state = state.wrapping_mul(0x9e37_79b1_85eb_ca87);
    state ^ (state >> 28)
}
