pub const Q16_ONE: u64 = 1 << 16;

const EXCITED_ENERGY: u64 = 8 * Q16_ONE;
const CRITICAL_ENERGY: u64 = 20 * Q16_ONE;
const CONTAINMENT_ENERGY: u64 = 40 * Q16_ONE;
const RAPID_GROWTH: i64 = 12 * Q16_ONE as i64;
const STABLE_OBSERVATIONS: u8 = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum StabilityState {
    Nominal,
    Excited,
    Critical,
    Containment,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StabilitySample {
    pub queue_pressure_q16: u32,
    pub heat_q16: u32,
    pub fault_rate_q16: u32,
    pub phase_drift_q16: u32,
    pub replay_pressure_q16: u32,
    pub phase_bin: u16,
    pub checkpoint: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContainmentOrder {
    None,
    Rephase {
        target_phase_bin: u16,
    },
    Throttle {
        priority_mass_ceiling: u16,
    },
    Quarantine {
        duration_ticks: u64,
    },
    Rollback {
        checkpoint: u32,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StabilityDecision {
    pub state: StabilityState,
    pub energy_q16: u64,
    pub growth_q16: i64,
    pub order: ContainmentOrder,
}

pub struct SingularityGovernor<const HISTORY: usize> {
    history: [u64; HISTORY],
    cursor: usize,
    length: usize,
    state: StabilityState,
    previous_energy: u64,
    stable_runs: u8,
}

impl<const HISTORY: usize> SingularityGovernor<HISTORY> {
    pub const fn new() -> Self {
        Self {
            history: [0; HISTORY],
            cursor: 0,
            length: 0,
            state: StabilityState::Nominal,
            previous_energy: 0,
            stable_runs: 0,
        }
    }

    pub fn observe(
        &mut self,
        sample: StabilitySample,
    ) -> StabilityDecision {
        let energy = energy_q16(sample);
        let growth = signed_difference(energy, self.previous_energy);
        self.previous_energy = energy;

        if HISTORY != 0 {
            self.history[self.cursor] = energy;
            self.cursor = (self.cursor + 1) % HISTORY;
            self.length = (self.length + 1).min(HISTORY);
        }

        let order = if energy >= CONTAINMENT_ENERGY
            || growth >= RAPID_GROWTH
        {
            self.state = StabilityState::Containment;
            self.stable_runs = 0;

            ContainmentOrder::Rollback {
                checkpoint: sample.checkpoint,
            }
        } else if energy >= CRITICAL_ENERGY {
            self.state = StabilityState::Critical;
            self.stable_runs = 0;

            ContainmentOrder::Quarantine {
                duration_ticks: 4096,
            }
        } else if u64::from(sample.phase_drift_q16)
            >= (Q16_ONE * 3) / 4
        {
            self.state = StabilityState::Excited;
            self.stable_runs = 0;

            ContainmentOrder::Rephase {
                target_phase_bin:
                    sample.phase_bin.wrapping_add(512) & 1023,
            }
        } else if energy >= EXCITED_ENERGY {
            self.state = StabilityState::Excited;
            self.stable_runs = 0;

            ContainmentOrder::Throttle {
                priority_mass_ceiling: 0x8000,
            }
        } else {
            self.stable_runs = self.stable_runs.saturating_add(1);

            if self.stable_runs >= STABLE_OBSERVATIONS {
                self.state = StabilityState::Nominal;
            }

            ContainmentOrder::None
        };

        StabilityDecision {
            state: self.state,
            energy_q16: energy,
            growth_q16: growth,
            order,
        }
    }

    pub fn mean_energy_q16(&self) -> u64 {
        if self.length == 0 {
            return 0;
        }

        self.history[..self.length]
            .iter()
            .copied()
            .fold(0_u128, |sum, value| sum + value as u128)
            .checked_div(self.length as u128)
            .unwrap_or(0)
            .min(u64::MAX as u128) as u64
    }

    pub const fn state(&self) -> StabilityState {
        self.state
    }
}

impl<const HISTORY: usize> Default
    for SingularityGovernor<HISTORY>
{
    fn default() -> Self {
        Self::new()
    }
}

fn energy_q16(sample: StabilitySample) -> u64 {
    let queue = square_q16(sample.queue_pressure_q16);
    let heat = square_q16(sample.heat_q16);
    let faults = square_q16(sample.fault_rate_q16);
    let drift = square_q16(sample.phase_drift_q16);
    let replay = square_q16(sample.replay_pressure_q16);

    queue
        .saturating_mul(4)
        .saturating_add(heat.saturating_mul(6))
        .saturating_add(faults.saturating_mul(8))
        .saturating_add(drift.saturating_mul(5))
        .saturating_add(replay.saturating_mul(7))
}

fn square_q16(value: u32) -> u64 {
    (((value as u128) * (value as u128)) >> 16)
        .min(u64::MAX as u128) as u64
}

fn signed_difference(current: u64, previous: u64) -> i64 {
    if current >= previous {
        current
            .saturating_sub(previous)
            .min(i64::MAX as u64) as i64
    } else {
        -(previous
            .saturating_sub(current)
            .min(i64::MAX as u64) as i64)
    }
}
