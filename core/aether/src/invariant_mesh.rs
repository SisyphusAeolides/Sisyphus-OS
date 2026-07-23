use crate::temporal_contract::{
    CONTRACT_REQUIRE_GENERATION_CHANGE, CONTRACT_REQUIRE_ROOT_CHANGE, TemporalContract,
    TemporalObservation,
};

pub const INVARIANT_ROOT_NONZERO: u32 = 1 << 0;
pub const INVARIANT_ROOT_TRANSITION: u32 = 1 << 1;
pub const INVARIANT_GENERATION_MONOTONIC: u32 = 1 << 2;
pub const INVARIANT_GENERATION_BOUND: u32 = 1 << 3;
pub const INVARIANT_HEAT_BOUND: u32 = 1 << 4;
pub const INVARIANT_PAIR_BOUND: u32 = 1 << 5;
pub const INVARIANT_COLLAPSE_BOUND: u32 = 1 << 6;
pub const INVARIANT_PHASE_BOUND: u32 = 1 << 7;
pub const INVARIANT_EFFECT_DIGEST: u32 = 1 << 8;
pub const INVARIANT_REALITY_MAJORITY: u32 = 1 << 9;

pub const ALL_INVARIANTS: u32 = INVARIANT_ROOT_NONZERO
    | INVARIANT_ROOT_TRANSITION
    | INVARIANT_GENERATION_MONOTONIC
    | INVARIANT_GENERATION_BOUND
    | INVARIANT_HEAT_BOUND
    | INVARIANT_PAIR_BOUND
    | INVARIANT_COLLAPSE_BOUND
    | INVARIANT_PHASE_BOUND
    | INVARIANT_EFFECT_DIGEST
    | INVARIANT_REALITY_MAJORITY;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct InvariantReport {
    pub passed: u32,
    pub failed: u32,

    /// Failed-invariant fraction in Q16.16.
    pub severity_q16: u32,

    pub digest: u64,
}

impl InvariantReport {
    pub const CLEAR: Self = Self {
        passed: 0,
        failed: 0,
        severity_q16: 0,
        digest: 0,
    };

    pub const fn is_clear(self) -> bool {
        self.failed == 0
    }
}

pub fn evaluate(
    before: TemporalObservation,
    after: TemporalObservation,
    contract: TemporalContract,
    effect_digest: u64,
    reality_mask: u8,
) -> InvariantReport {
    let mut passed = 0_u32;
    let mut failed = 0_u32;

    record(
        after.state_root != 0,
        INVARIANT_ROOT_NONZERO,
        &mut passed,
        &mut failed,
    );

    let root_transition_valid =
        contract.flags & CONTRACT_REQUIRE_ROOT_CHANGE == 0 || after.state_root != before.state_root;

    record(
        root_transition_valid,
        INVARIANT_ROOT_TRANSITION,
        &mut passed,
        &mut failed,
    );

    let generation_delta = after.generation.wrapping_sub(before.generation);

    let generation_monotonic = after.generation >= before.generation;

    if contract.flags & CONTRACT_REQUIRE_GENERATION_CHANGE != 0 {
        record(
            generation_monotonic && after.generation != before.generation,
            INVARIANT_GENERATION_MONOTONIC,
            &mut passed,
            &mut failed,
        );
    } else {
        record(
            generation_monotonic,
            INVARIANT_GENERATION_MONOTONIC,
            &mut passed,
            &mut failed,
        );
    }

    record(
        generation_delta <= contract.maximum_generation_delta,
        INVARIANT_GENERATION_BOUND,
        &mut passed,
        &mut failed,
    );

    record(
        after.heat <= contract.maximum_heat,
        INVARIANT_HEAT_BOUND,
        &mut passed,
        &mut failed,
    );

    record(
        after.pairs_live.saturating_sub(before.pairs_live) <= contract.maximum_pair_growth,
        INVARIANT_PAIR_BOUND,
        &mut passed,
        &mut failed,
    );

    record(
        after.collapses.saturating_sub(before.collapses) <= contract.maximum_collapse_growth,
        INVARIANT_COLLAPSE_BOUND,
        &mut passed,
        &mut failed,
    );

    record(
        wrapped_phase_distance(before.phase_bin, after.phase_bin)
            <= contract.maximum_phase_distance,
        INVARIANT_PHASE_BOUND,
        &mut passed,
        &mut failed,
    );

    record(
        effect_digest != 0,
        INVARIANT_EFFECT_DIGEST,
        &mut passed,
        &mut failed,
    );

    record(
        reality_mask.count_ones() >= 2,
        INVARIANT_REALITY_MAJORITY,
        &mut passed,
        &mut failed,
    );

    let total = ALL_INVARIANTS.count_ones().max(1);
    let failed_count = failed.count_ones();

    let severity_q16 = (((failed_count as u64) << 16) / total as u64).min(u32::MAX as u64) as u32;

    let mut digest = mix(0x494e_5641_5249_414e, u64::from(passed));

    digest = mix(digest, u64::from(failed));
    digest = mix(digest, u64::from(severity_q16));
    digest = mix(digest, before.state_root);
    digest = mix(digest, after.state_root);
    digest = mix(digest, effect_digest);
    digest = mix(digest, u64::from(reality_mask));

    InvariantReport {
        passed,
        failed,
        severity_q16,
        digest,
    }
}

fn record(condition: bool, invariant: u32, passed: &mut u32, failed: &mut u32) {
    if condition {
        *passed |= invariant;
    } else {
        *failed |= invariant;
    }
}

fn wrapped_phase_distance(left: u16, right: u16) -> u16 {
    let left = left & 1023;
    let right = right & 1023;

    let direct = left.abs_diff(right);
    direct.min(1024 - direct)
}

fn mix(mut state: u64, value: u64) -> u64 {
    state ^= value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    state = state.rotate_left(29);
    state = state.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    state ^ (state >> 31)
}
