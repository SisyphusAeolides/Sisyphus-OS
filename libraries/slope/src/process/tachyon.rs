use crate::{SyscallError, process, time};

pub const DEFAULT_EVENT_HORIZON_TICKS: u64 = 50_000;

/// Result of a measured operation and any scheduling hint it emitted.
pub struct EventHorizonOutcome<T> {
    pub value: T,
    pub elapsed_ticks: u64,
    pub yield_result: Option<Result<(), SyscallError>>,
}

/// Yields cooperatively and supplies unfinished-work pressure to the kernel.
#[inline(always)]
pub fn yield_retrocausally(unfinished_work: u64) -> Result<(), SyscallError> {
    process::yield_with_hint(unfinished_work)
}

/// Measures a closure and emits a scheduler hint when it exceeds `limit`.
///
/// This does not reverse time. It preserves the useful policy signal from the
/// Tachyon model without entering through an unregistered software interrupt.
pub fn execute_within_event_horizon<F, T>(limit: u64, operation: F) -> EventHorizonOutcome<T>
where
    F: FnOnce() -> T,
{
    let start = time::read_counter();
    let value = operation();
    let elapsed_ticks = time::read_counter().saturating_sub(start);
    let yield_result = (elapsed_ticks > limit).then(|| yield_retrocausally(elapsed_ticks));
    EventHorizonOutcome {
        value,
        elapsed_ticks,
        yield_result,
    }
}

pub fn execute_with_default_horizon<F, T>(operation: F) -> EventHorizonOutcome<T>
where
    F: FnOnce() -> T,
{
    execute_within_event_horizon(DEFAULT_EVENT_HORIZON_TICKS, operation)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_the_closure_result() {
        let outcome = execute_within_event_horizon(u64::MAX, || 42);
        assert_eq!(outcome.value, 42);
        assert!(outcome.yield_result.is_none());
    }
}
