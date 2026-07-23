use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuorumTicket {
    pub epoch: u64,
    pub digest: u64,
    pub required: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuorumError {
    ZeroCpuCapacity,
    InvalidRequirement,
    CoordinatorBusy,
    CpuOutOfRange,
    StaleTicket,
    DigestMismatch,
    QuorumIncomplete { prepared: u32, required: u32 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuorumStatus {
    Preparing { prepared: u32, required: u32 },
    Committed,
    Aborted,
    Stale,
}

pub struct CausalQuorum<const CPUS: usize> {
    coordinator: AtomicBool,

    next_epoch: AtomicU64,
    active_epoch: AtomicU64,

    proposal_digest: AtomicU64,
    required: AtomicU64,

    prepared_epoch: [AtomicU64; CPUS],
    prepared_digest: [AtomicU64; CPUS],

    committed_epoch: AtomicU64,
    aborted_epoch: AtomicU64,
}

impl<const CPUS: usize> CausalQuorum<CPUS> {
    pub const fn new() -> Self {
        Self {
            coordinator: AtomicBool::new(false),

            next_epoch: AtomicU64::new(0),
            active_epoch: AtomicU64::new(0),

            proposal_digest: AtomicU64::new(0),
            required: AtomicU64::new(0),

            prepared_epoch: [const { AtomicU64::new(0) }; CPUS],

            prepared_digest: [const { AtomicU64::new(0) }; CPUS],

            committed_epoch: AtomicU64::new(0),
            aborted_epoch: AtomicU64::new(0),
        }
    }

    pub fn begin(&self, digest: u64, required: usize) -> Result<QuorumTicket, QuorumError> {
        if CPUS == 0 {
            return Err(QuorumError::ZeroCpuCapacity);
        }

        if required == 0 || required > CPUS || required > u32::MAX as usize {
            return Err(QuorumError::InvalidRequirement);
        }

        if self
            .coordinator
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(QuorumError::CoordinatorBusy);
        }

        let current = self.next_epoch.load(Ordering::Relaxed);
        let epoch = current.wrapping_add(1).max(1);

        self.next_epoch.store(epoch, Ordering::Relaxed);
        self.proposal_digest.store(digest, Ordering::Relaxed);
        self.required.store(required as u64, Ordering::Relaxed);

        // Publishing active_epoch makes all proposal fields visible.
        self.active_epoch.store(epoch, Ordering::Release);

        Ok(QuorumTicket {
            epoch,
            digest,
            required: required as u32,
        })
    }

    pub fn proposal(&self) -> Option<QuorumTicket> {
        let epoch = self.active_epoch.load(Ordering::Acquire);

        if epoch == 0 {
            return None;
        }

        Some(QuorumTicket {
            epoch,
            digest: self.proposal_digest.load(Ordering::Relaxed),
            required: self.required.load(Ordering::Relaxed) as u32,
        })
    }

    pub fn acknowledge(
        &self,
        cpu: usize,
        ticket: QuorumTicket,
        observed_digest: u64,
    ) -> Result<(), QuorumError> {
        if cpu >= CPUS {
            return Err(QuorumError::CpuOutOfRange);
        }

        self.require_active(ticket)?;

        if observed_digest != ticket.digest {
            return Err(QuorumError::DigestMismatch);
        }

        self.prepared_digest[cpu].store(observed_digest, Ordering::Relaxed);

        // Release publishes the digest before the acknowledgement epoch.
        self.prepared_epoch[cpu].store(ticket.epoch, Ordering::Release);

        Ok(())
    }

    pub fn prepared_count(&self, ticket: QuorumTicket) -> Result<u32, QuorumError> {
        self.require_active(ticket)?;

        let mut prepared = 0_u32;

        for cpu in 0..CPUS {
            let epoch = self.prepared_epoch[cpu].load(Ordering::Acquire);

            if epoch != ticket.epoch {
                continue;
            }

            let digest = self.prepared_digest[cpu].load(Ordering::Relaxed);

            if digest == ticket.digest {
                prepared = prepared.saturating_add(1);
            }
        }

        Ok(prepared)
    }

    pub fn ready(&self, ticket: QuorumTicket) -> Result<bool, QuorumError> {
        Ok(self.prepared_count(ticket)? >= ticket.required)
    }

    pub fn commit(&self, ticket: QuorumTicket) -> Result<u32, QuorumError> {
        let prepared = self.prepared_count(ticket)?;

        if prepared < ticket.required {
            return Err(QuorumError::QuorumIncomplete {
                prepared,
                required: ticket.required,
            });
        }

        self.committed_epoch.store(ticket.epoch, Ordering::Release);

        self.active_epoch.store(0, Ordering::Release);
        self.coordinator.store(false, Ordering::Release);

        Ok(prepared)
    }

    pub fn abort(&self, ticket: QuorumTicket) -> Result<(), QuorumError> {
        self.require_active(ticket)?;

        self.aborted_epoch.store(ticket.epoch, Ordering::Release);

        self.active_epoch.store(0, Ordering::Release);
        self.coordinator.store(false, Ordering::Release);

        Ok(())
    }

    pub fn status(&self, ticket: QuorumTicket) -> QuorumStatus {
        if self.committed_epoch.load(Ordering::Acquire) == ticket.epoch {
            return QuorumStatus::Committed;
        }

        if self.aborted_epoch.load(Ordering::Acquire) == ticket.epoch {
            return QuorumStatus::Aborted;
        }

        if self.active_epoch.load(Ordering::Acquire) != ticket.epoch {
            return QuorumStatus::Stale;
        }

        let prepared = self.prepared_count(ticket).unwrap_or(0);

        QuorumStatus::Preparing {
            prepared,
            required: ticket.required,
        }
    }

    fn require_active(&self, ticket: QuorumTicket) -> Result<(), QuorumError> {
        if self.active_epoch.load(Ordering::Acquire) != ticket.epoch {
            return Err(QuorumError::StaleTicket);
        }

        if self.proposal_digest.load(Ordering::Relaxed) != ticket.digest {
            return Err(QuorumError::DigestMismatch);
        }

        Ok(())
    }
}

impl<const CPUS: usize> Default for CausalQuorum<CPUS> {
    fn default() -> Self {
        Self::new()
    }
}
