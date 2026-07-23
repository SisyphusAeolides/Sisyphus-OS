#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CommitOutcome {
    Committed = 1,
    Aborted = 2,
    RolledBack = 3,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct CommitWitness {
    pub epoch: u64,
    pub effect_digest: u64,

    pub before_root: u64,
    pub after_root: u64,

    pub wall_tick: u64,

    pub generation_before: u32,
    pub generation_after: u32,

    pub participants: u16,
    pub effect_count: u16,

    pub outcome: CommitOutcome,
    pub reserved: [u8; 7],
}

impl CommitWitness {
    pub const EMPTY: Self = Self {
        epoch: 0,
        effect_digest: 0,
        before_root: 0,
        after_root: 0,
        wall_tick: 0,
        generation_before: 0,
        generation_after: 0,
        participants: 0,
        effect_count: 0,
        outcome: CommitOutcome::Aborted,
        reserved: [0; 7],
    };
}

#[derive(Clone, Copy)]
struct WitnessRecord {
    active: bool,
    previous_chain: u64,
    chain: u64,
    witness: CommitWitness,
}

impl WitnessRecord {
    const EMPTY: Self = Self {
        active: false,
        previous_chain: 0,
        chain: 0,
        witness: CommitWitness::EMPTY,
    };
}

pub struct WitnessChain<const N: usize> {
    records: [WitnessRecord; N],
    cursor: usize,
    length: usize,
    chain_root: u64,
}

impl<const N: usize> WitnessChain<N> {
    pub const fn new(seed: u64) -> Self {
        Self {
            records: [WitnessRecord::EMPTY; N],
            cursor: 0,
            length: 0,
            chain_root: seed,
        }
    }

    pub fn append(&mut self, witness: CommitWitness) -> Option<u64> {
        if N == 0 {
            return None;
        }

        let previous_chain = self.chain_root;
        let chain = witness_digest(previous_chain, witness);

        self.records[self.cursor] = WitnessRecord {
            active: true,
            previous_chain,
            chain,
            witness,
        };

        self.cursor = (self.cursor + 1) % N;
        self.length = (self.length + 1).min(N);
        self.chain_root = chain;

        Some(chain)
    }

    pub fn latest(&self) -> Option<CommitWitness> {
        if self.length == 0 {
            return None;
        }

        let index = (self.cursor + N - 1) % N;
        self.records[index]
            .active
            .then_some(self.records[index].witness)
    }

    pub fn verify(&self) -> bool {
        if self.length == 0 {
            return true;
        }

        let start = (self.cursor + N - self.length) % N;
        let first = self.records[start];

        if !first.active {
            return false;
        }

        let mut expected_previous = first.previous_chain;

        for offset in 0..self.length {
            let record = self.records[(start + offset) % N];

            if !record.active || record.previous_chain != expected_previous {
                return false;
            }

            let expected_chain = witness_digest(record.previous_chain, record.witness);

            if record.chain != expected_chain {
                return false;
            }

            expected_previous = record.chain;
        }

        expected_previous == self.chain_root
    }

    pub const fn root(&self) -> u64 {
        self.chain_root
    }

    pub const fn retained(&self) -> usize {
        self.length
    }
}

fn witness_digest(mut state: u64, witness: CommitWitness) -> u64 {
    state = mix(state, witness.epoch);
    state = mix(state, witness.effect_digest);
    state = mix(state, witness.before_root);
    state = mix(state, witness.after_root);
    state = mix(state, witness.wall_tick);

    state = mix(
        state,
        u64::from(witness.generation_before) | (u64::from(witness.generation_after) << 32),
    );

    state = mix(
        state,
        u64::from(witness.participants)
            | (u64::from(witness.effect_count) << 16)
            | ((witness.outcome as u64) << 32),
    );

    state
}

// Integrity and replay detector, not an authentication primitive.
fn mix(mut state: u64, value: u64) -> u64 {
    state ^= value.wrapping_add(0x517c_c1b7_2722_0a95);
    state = state.rotate_left(31);
    state = state.wrapping_mul(0x9e37_79b1_85eb_ca87);
    state ^ (state >> 28)
}
