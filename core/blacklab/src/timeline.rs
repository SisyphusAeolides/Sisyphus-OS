/// Rational conversion from a platform counter delta to logical nanoseconds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CounterScale {
    pub logical_nanoseconds: u64,
    pub counter_ticks: u64,
}

impl CounterScale {
    pub const fn new(logical_nanoseconds: u64, counter_ticks: u64) -> Self {
        Self {
            logical_nanoseconds,
            counter_ticks,
        }
    }
}

/// One active term in the logical-time workload correction.
///
/// `weighted_work` is measured in capacity-nanoseconds and `capacity` is a
/// dimensionless scheduler capacity weight. Their quotient is therefore a
/// deterministic logical-nanosecond contribution. `active` is H(t).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkloadTerm {
    pub weighted_work: u64,
    pub capacity: u64,
    pub active: bool,
}

/// Evaluates:
///
/// `delta = counter_delta * scale_num / scale_den + sum(work_i / capacity_i * active_i)`
///
/// Integer division rounds each term toward zero. Inputs are checked rather
/// than saturated so timeline corruption cannot be hidden.
pub fn logical_delta(
    counter_delta: u64,
    scale: CounterScale,
    workload: &[WorkloadTerm],
) -> Result<u64, TimelineError> {
    if scale.counter_ticks == 0 {
        return Err(TimelineError::ZeroScaleDenominator);
    }
    let scaled = u128::from(counter_delta)
        .checked_mul(u128::from(scale.logical_nanoseconds))
        .ok_or(TimelineError::Overflow)?
        / u128::from(scale.counter_ticks);
    let mut total = scaled;
    for term in workload.iter().filter(|term| term.active) {
        if term.capacity == 0 {
            return Err(TimelineError::ZeroCapacity);
        }
        total = total
            .checked_add(u128::from(term.weighted_work / term.capacity))
            .ok_or(TimelineError::Overflow)?;
    }
    u64::try_from(total).map_err(|_| TimelineError::Overflow)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimelineError {
    ZeroScaleDenominator,
    ZeroCapacity,
    Overflow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CausalBarrier {
    pub left_lane: u16,
    pub right_lane: u16,
    pub left_time: u64,
    pub right_time: u64,
}

impl CausalBarrier {
    pub fn new(
        left_lane: u16,
        right_lane: u16,
        left_time: u64,
        maximum_skew: u64,
    ) -> Result<Self, TimelineError> {
        let right_time = left_time
            .checked_add(maximum_skew)
            .ok_or(TimelineError::Overflow)?;
        Ok(Self {
            left_lane,
            right_lane,
            left_time,
            right_time,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combines_scaled_counter_time_and_active_workload_costs() {
        let terms = [
            WorkloadTerm {
                weighted_work: 400,
                capacity: 4,
                active: true,
            },
            WorkloadTerm {
                weighted_work: 900,
                capacity: 3,
                active: false,
            },
        ];
        assert_eq!(
            logical_delta(1_000, CounterScale::new(1, 2), &terms),
            Ok(600)
        );
    }

    #[test]
    fn rejects_undefined_or_overflowing_time_models() {
        assert_eq!(
            logical_delta(1, CounterScale::new(1, 0), &[]),
            Err(TimelineError::ZeroScaleDenominator)
        );
        assert_eq!(
            logical_delta(
                1,
                CounterScale::new(1, 1),
                &[WorkloadTerm {
                    weighted_work: 1,
                    capacity: 0,
                    active: true,
                }]
            ),
            Err(TimelineError::ZeroCapacity)
        );
        assert_eq!(
            logical_delta(u64::MAX, CounterScale::new(2, 1), &[]),
            Err(TimelineError::Overflow)
        );
    }
}
