pub const CONTRACT_REQUIRE_ROOT_CHANGE: u32 = 1 << 0;
pub const CONTRACT_REQUIRE_GENERATION_CHANGE: u32 = 1 << 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct TemporalObservation {
    pub generation: u32,
    pub pairs_live: u32,

    pub state_root: u64,
    pub collapses: u64,
    pub heat: u64,

    pub phase_bin: u16,
    pub reserved: u16,
}

impl TemporalObservation {
    pub const ZERO: Self = Self {
        generation: 0,
        pairs_live: 0,
        state_root: 0,
        collapses: 0,
        heat: 0,
        phase_bin: 0,
        reserved: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct TemporalContract {
    pub expected_generation: u32,
    pub flags: u32,

    pub expected_state_root: u64,
    pub deadline_tick: u64,

    pub maximum_heat: u64,
    pub maximum_generation_delta: u32,
    pub maximum_pair_growth: u32,

    pub maximum_collapse_growth: u64,
    pub maximum_phase_distance: u16,
    pub reserved: u16,

    /// Bit N permits effect kind N.
    pub allowed_effects: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContractError {
    Expired,
    GenerationConflict { expected: u32, observed: u32 },
    StateRootConflict { expected: u64, observed: u64 },
    EffectDenied(u8),
    HeatExceeded { observed: u64, maximum: u64 },
    GenerationGrowth { observed: u32, maximum: u32 },
    PairGrowth { observed: u32, maximum: u32 },
    CollapseGrowth { observed: u64, maximum: u64 },
    PhaseDistance { observed: u16, maximum: u16 },
    RootDidNotChange,
    GenerationDidNotChange,
}

impl TemporalContract {
    pub fn verify_before(
        &self,
        observation: TemporalObservation,
        now_tick: u64,
    ) -> Result<(), ContractError> {
        if now_tick > self.deadline_tick {
            return Err(ContractError::Expired);
        }

        if observation.generation != self.expected_generation {
            return Err(ContractError::GenerationConflict {
                expected: self.expected_generation,
                observed: observation.generation,
            });
        }

        if observation.state_root != self.expected_state_root {
            return Err(ContractError::StateRootConflict {
                expected: self.expected_state_root,
                observed: observation.state_root,
            });
        }

        Ok(())
    }

    pub fn verify_effect(&self, effect_kind: u8) -> Result<(), ContractError> {
        let bit = effect_bit(effect_kind);

        if bit == 0 || self.allowed_effects & bit == 0 {
            return Err(ContractError::EffectDenied(effect_kind));
        }

        Ok(())
    }

    pub fn verify_after(
        &self,
        before: TemporalObservation,
        after: TemporalObservation,
    ) -> Result<(), ContractError> {
        if after.heat > self.maximum_heat {
            return Err(ContractError::HeatExceeded {
                observed: after.heat,
                maximum: self.maximum_heat,
            });
        }

        let generation_delta = after.generation.wrapping_sub(before.generation);

        if generation_delta > self.maximum_generation_delta {
            return Err(ContractError::GenerationGrowth {
                observed: generation_delta,
                maximum: self.maximum_generation_delta,
            });
        }

        let pair_growth = after.pairs_live.saturating_sub(before.pairs_live);

        if pair_growth > self.maximum_pair_growth {
            return Err(ContractError::PairGrowth {
                observed: pair_growth,
                maximum: self.maximum_pair_growth,
            });
        }

        let collapse_growth = after.collapses.saturating_sub(before.collapses);

        if collapse_growth > self.maximum_collapse_growth {
            return Err(ContractError::CollapseGrowth {
                observed: collapse_growth,
                maximum: self.maximum_collapse_growth,
            });
        }

        let phase_distance = wrapped_phase_distance(before.phase_bin, after.phase_bin);

        if phase_distance > self.maximum_phase_distance {
            return Err(ContractError::PhaseDistance {
                observed: phase_distance,
                maximum: self.maximum_phase_distance,
            });
        }

        if self.flags & CONTRACT_REQUIRE_ROOT_CHANGE != 0 && after.state_root == before.state_root {
            return Err(ContractError::RootDidNotChange);
        }

        if self.flags & CONTRACT_REQUIRE_GENERATION_CHANGE != 0
            && after.generation == before.generation
        {
            return Err(ContractError::GenerationDidNotChange);
        }

        Ok(())
    }
}

#[inline(always)]
pub const fn effect_bit(effect_kind: u8) -> u64 {
    if effect_kind < 64 {
        1_u64 << effect_kind
    } else {
        0
    }
}

fn wrapped_phase_distance(left: u16, right: u16) -> u16 {
    let left = left & 1023;
    let right = right & 1023;
    let direct = left.abs_diff(right);

    direct.min(1024 - direct)
}

impl TemporalContract {
    pub fn digest(&self) -> u64 {
        let mut digest = fold(0x434f_4e54_5241_4354, u64::from(self.expected_generation));

        digest = fold(digest, u64::from(self.flags));
        digest = fold(digest, self.expected_state_root);
        digest = fold(digest, self.deadline_tick);
        digest = fold(digest, self.maximum_heat);

        digest = fold(
            digest,
            u64::from(self.maximum_generation_delta) | (u64::from(self.maximum_pair_growth) << 32),
        );

        digest = fold(digest, self.maximum_collapse_growth);

        digest = fold(digest, u64::from(self.maximum_phase_distance));

        fold(digest, self.allowed_effects)
    }
}

fn fold(mut state: u64, value: u64) -> u64 {
    state ^= value.wrapping_add(0x517c_c1b7_2722_0a95);
    state = state.rotate_left(31);
    state = state.wrapping_mul(0x9e37_79b1_85eb_ca87);
    state ^ (state >> 28)
}
