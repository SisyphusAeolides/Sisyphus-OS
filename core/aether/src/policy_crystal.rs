use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::resonance_policy::{PolicyError, ResonancePolicy};

const POLICY_WORDS: usize = 5;
const BANKS: usize = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PolicySnapshot {
    pub policy: ResonancePolicy,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PolicyCrystalError {
    WriterBusy,
    NoMajority,
    InvalidPolicy(PolicyError),
}

#[repr(C, align(128))]
struct PolicyBank {
    guard: AtomicU64,
    generation: AtomicU64,
    digest: AtomicU64,
    words: [AtomicU64; POLICY_WORDS],
}

impl PolicyBank {
    const fn new() -> Self {
        Self {
            guard: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            digest: AtomicU64::new(0),
            words: [const { AtomicU64::new(0) }; POLICY_WORDS],
        }
    }

    fn publish(&self, generation: u64, words: [u64; POLICY_WORDS]) {
        let odd = self.guard.fetch_add(1, Ordering::AcqRel).wrapping_add(1);

        debug_assert!(odd & 1 == 1);

        for (target, value) in self.words.iter().zip(words) {
            target.store(value, Ordering::Relaxed);
        }

        self.digest
            .store(policy_digest(generation, words), Ordering::Relaxed);

        self.generation.store(generation, Ordering::Relaxed);

        self.guard.store(odd.wrapping_add(1), Ordering::Release);
    }

    fn snapshot(&self) -> Option<PolicySnapshot> {
        for _ in 0..8 {
            let before = self.guard.load(Ordering::Acquire);

            if before & 1 != 0 {
                core::hint::spin_loop();
                continue;
            }

            let generation = self.generation.load(Ordering::Relaxed);

            if generation == 0 {
                return None;
            }

            let mut words = [0_u64; POLICY_WORDS];

            for (target, source) in words.iter_mut().zip(self.words.iter()) {
                *target = source.load(Ordering::Relaxed);
            }

            let observed_digest = self.digest.load(Ordering::Relaxed);

            let after = self.guard.load(Ordering::Acquire);

            if before != after {
                core::hint::spin_loop();
                continue;
            }

            if observed_digest != policy_digest(generation, words) {
                return None;
            }

            let policy = decode_policy(words).ok()?;

            return Some(PolicySnapshot { policy, generation });
        }

        None
    }
}

pub struct PolicyCrystal {
    writer: AtomicBool,
    generation: AtomicU64,
    banks: [PolicyBank; BANKS],
}

impl PolicyCrystal {
    pub const fn new() -> Self {
        Self {
            writer: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            banks: [const { PolicyBank::new() }; BANKS],
        }
    }

    pub fn publish(&self, policy: ResonancePolicy) -> Result<u64, PolicyCrystalError> {
        let policy = policy
            .validate()
            .map_err(PolicyCrystalError::InvalidPolicy)?;

        if self
            .writer
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(PolicyCrystalError::WriterBusy);
        }

        let generation = self
            .generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1)
            .max(1);

        let words = encode_policy(policy);

        // The publication order intentionally permits old-old-new and
        // old-new-new states. Either state has a valid majority.
        for bank in &self.banks {
            bank.publish(generation, words);
        }

        self.writer.store(false, Ordering::Release);

        Ok(generation)
    }

    pub fn snapshot(&self) -> Result<PolicySnapshot, PolicyCrystalError> {
        let a = self.banks[0].snapshot();
        let b = self.banks[1].snapshot();
        let c = self.banks[2].snapshot();

        if let (Some(left), Some(right)) = (a, b) {
            if same_snapshot(left, right) {
                return Ok(left);
            }
        }

        if let (Some(left), Some(right)) = (a, c) {
            if same_snapshot(left, right) {
                return Ok(left);
            }
        }

        if let (Some(left), Some(right)) = (b, c) {
            if same_snapshot(left, right) {
                return Ok(left);
            }
        }

        Err(PolicyCrystalError::NoMajority)
    }

    pub fn scrub(&self) -> Result<u64, PolicyCrystalError> {
        let majority = self.snapshot()?;
        self.publish(majority.policy)
    }
}

impl Default for PolicyCrystal {
    fn default() -> Self {
        Self::new()
    }
}

fn same_snapshot(left: PolicySnapshot, right: PolicySnapshot) -> bool {
    left.generation == right.generation && left.policy == right.policy
}

fn encode_policy(policy: ResonancePolicy) -> [u64; POLICY_WORDS] {
    [
        policy.collapse_threshold,
        policy.heat_ceiling,
        policy.quarantine_ticks,
        u64::from(policy.priority_mass)
            | (u64::from(policy.target_phase) << 16)
            | (u64::from(policy.maximum_pairs) << 32)
            | (u64::from(policy.reserved) << 48),
        u64::from(policy.flags),
    ]
}

fn decode_policy(words: [u64; POLICY_WORDS]) -> Result<ResonancePolicy, PolicyError> {
    ResonancePolicy {
        collapse_threshold: words[0],
        heat_ceiling: words[1],
        quarantine_ticks: words[2],
        priority_mass: words[3] as u16,
        target_phase: (words[3] >> 16) as u16,
        maximum_pairs: (words[3] >> 32) as u16,
        reserved: (words[3] >> 48) as u16,
        flags: words[4] as u32,
    }
    .validate()
}

fn policy_digest(generation: u64, words: [u64; POLICY_WORDS]) -> u64 {
    words
        .into_iter()
        .fold(mix(0x6a09_e667_f3bc_c909, generation), mix)
}

// Deterministic corruption detector, not an authentication primitive.
fn mix(mut state: u64, value: u64) -> u64 {
    state ^= value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    state = state.rotate_left(29);
    state = state.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    state ^ (state >> 31)
}
