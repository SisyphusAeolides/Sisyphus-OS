#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchedulerPolicy {
    // Signed Q8.8 weights.
    pub latency_weight: i16,
    pub throughput_weight: i16,
    pub heat_weight: i16,
    pub fairness_weight: i16,

    pub quantum_ticks: u16,
    pub priority_mass_ceiling: u16,
}

impl SchedulerPolicy {
    pub const ZERO: Self = Self {
        latency_weight: 0,
        throughput_weight: 0,
        heat_weight: 0,
        fairness_weight: 0,
        quantum_ticks: 0,
        priority_mass_ceiling: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorldOutcome {
    // Each metric uses Q16.16.
    pub latency_q16: u32,
    pub throughput_q16: u32,
    pub heat_q16: u32,
    pub fairness_q16: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorldToken {
    index: u16,
    epoch: u32,
}

impl WorldToken {
    pub const fn index(self) -> usize {
        self.index as usize
    }

    pub const fn epoch(self) -> u32 {
        self.epoch
    }
}

#[derive(Clone, Copy)]
struct WorldStat {
    samples: u64,
    reward_sum: i128,
    last_observed_epoch: u32,
}

impl WorldStat {
    const ZERO: Self = Self {
        samples: 0,
        reward_sum: 0,
        last_observed_epoch: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorldError {
    NoPolicies,
    InvalidToken,
    DuplicateObservation,
}

pub struct ManyWorlds<const N: usize> {
    policies: [SchedulerPolicy; N],
    stats: [WorldStat; N],
    epoch: u32,
    total_samples: u64,
}

impl<const N: usize> ManyWorlds<N> {
    pub const fn new(policies: [SchedulerPolicy; N]) -> Self {
        Self {
            policies,
            stats: [WorldStat::ZERO; N],
            epoch: 0,
            total_samples: 0,
        }
    }

    pub fn fork(&mut self) -> Result<WorldToken, WorldError> {
        if N == 0 {
            return Err(WorldError::NoPolicies);
        }

        let selected = self
            .stats
            .iter()
            .position(|stat| stat.samples == 0)
            .unwrap_or_else(|| self.select_exploratory());

        self.epoch = self.epoch.wrapping_add(1).max(1);

        Ok(WorldToken {
            index: selected as u16,
            epoch: self.epoch,
        })
    }

    pub fn policy(
        &self,
        token: WorldToken,
    ) -> Result<SchedulerPolicy, WorldError> {
        self.policies
            .get(token.index())
            .copied()
            .ok_or(WorldError::InvalidToken)
    }

    pub fn observe(
        &mut self,
        token: WorldToken,
        outcome: WorldOutcome,
    ) -> Result<i64, WorldError> {
        let index = token.index();

        let policy = self
            .policies
            .get(index)
            .copied()
            .ok_or(WorldError::InvalidToken)?;

        let stat = self
            .stats
            .get_mut(index)
            .ok_or(WorldError::InvalidToken)?;

        if token.epoch <= stat.last_observed_epoch {
            return Err(WorldError::DuplicateObservation);
        }

        let reward = reward(policy, outcome);

        stat.samples = stat.samples.saturating_add(1);
        stat.reward_sum =
            stat.reward_sum.saturating_add(i128::from(reward));
        stat.last_observed_epoch = token.epoch;

        self.total_samples = self.total_samples.saturating_add(1);

        Ok(reward)
    }

    pub fn best_policy(&self) -> Option<(usize, SchedulerPolicy)> {
        self.stats
            .iter()
            .enumerate()
            .filter(|(_, stat)| stat.samples != 0)
            .max_by_key(|(_, stat)| mean_reward(stat))
            .map(|(index, _)| (index, self.policies[index]))
    }

    fn select_exploratory(&self) -> usize {
        self.stats
            .iter()
            .enumerate()
            .max_by_key(|(_, stat)| {
                let mean = mean_reward(stat);
                let exploration = exploration_bonus(
                    self.total_samples,
                    stat.samples,
                );

                mean.saturating_add(exploration)
            })
            .map(|(index, _)| index)
            .unwrap_or(0)
    }
}

fn reward(
    policy: SchedulerPolicy,
    outcome: WorldOutcome,
) -> i64 {
    let latency =
        weighted(policy.latency_weight, outcome.latency_q16);
    let throughput =
        weighted(policy.throughput_weight, outcome.throughput_q16);
    let heat =
        weighted(policy.heat_weight, outcome.heat_q16);
    let fairness =
        weighted(policy.fairness_weight, outcome.fairness_q16);

    throughput
        .saturating_add(fairness)
        .saturating_sub(latency)
        .saturating_sub(heat)
}

fn weighted(weight_q8: i16, metric_q16: u32) -> i64 {
    let product = i128::from(weight_q8) * i128::from(metric_q16);
    (product >> 8)
        .clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

fn mean_reward(stat: &WorldStat) -> i64 {
    if stat.samples == 0 {
        return i64::MIN / 2;
    }

    (stat.reward_sum / stat.samples as i128)
        .clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

fn exploration_bonus(total: u64, samples: u64) -> i64 {
    if samples == 0 {
        return i64::MAX / 4;
    }

    let numerator = (u128::from(total.saturating_add(1))) << 32;
    integer_sqrt(numerator / u128::from(samples))
        .min(i64::MAX as u128) as i64
}

fn integer_sqrt(value: u128) -> u128 {
    if value < 2 {
        return value;
    }

    let mut x = value;
    let mut next = (x + value / x) / 2;

    while next < x {
        x = next;
        next = (x + value / x) / 2;
    }

    x
}
