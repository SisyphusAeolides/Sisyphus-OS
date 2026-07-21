use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WatchdogToken {
    epoch: u64,
    deadline_ns: u64,
}

pub struct EnclaveWatchdog {
    epoch: AtomicU64,
    deadline_ns: AtomicU64,
    armed: AtomicBool,
}

impl EnclaveWatchdog {
    pub const fn new() -> Self {
        Self {
            epoch: AtomicU64::new(0),
            deadline_ns: AtomicU64::new(0),
            armed: AtomicBool::new(false),
        }
    }

    pub fn arm(&self, deadline_ns: u64) -> Result<WatchdogToken, WatchdogError> {
        if deadline_ns == 0
            || self
                .armed
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return Err(WatchdogError::InvalidState);
        }
        self.deadline_ns.store(deadline_ns, Ordering::Release);
        Ok(WatchdogToken {
            epoch: self.epoch.load(Ordering::Acquire),
            deadline_ns,
        })
    }

    /// Records timeout delivery from the platform's NMI/IPI handler.
    pub fn signal_timeout(&self) {
        if self.armed.swap(false, Ordering::AcqRel) {
            self.epoch.fetch_add(1, Ordering::AcqRel);
        }
    }

    pub fn disarm(&self, token: WatchdogToken) -> Result<(), WatchdogError> {
        if self.timed_out(token) {
            return Err(WatchdogError::TimedOut);
        }
        if self
            .armed
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(WatchdogError::InvalidState);
        }
        Ok(())
    }

    pub fn timed_out(&self, token: WatchdogToken) -> bool {
        self.epoch.load(Ordering::Acquire) != token.epoch
    }

    pub fn deadline_ns(&self) -> Option<u64> {
        self.armed
            .load(Ordering::Acquire)
            .then(|| self.deadline_ns.load(Ordering::Acquire))
    }
}

impl Default for EnclaveWatchdog {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WatchdogError {
    InvalidState,
    TimedOut,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_a_timeout_recorded_by_an_interrupt_handler() {
        let watchdog = EnclaveWatchdog::new();
        let token = watchdog.arm(100).unwrap();
        assert_eq!(watchdog.deadline_ns(), Some(100));
        watchdog.signal_timeout();
        assert!(watchdog.timed_out(token));
        assert_eq!(watchdog.disarm(token), Err(WatchdogError::TimedOut));
    }
}
