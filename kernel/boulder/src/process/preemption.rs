//! Lock-free bridge from the periodic timer IRQ to a scheduler safe point.
//!
//! Boulder does not yet save per-process XSAVE state or FS/GS bases, so an
//! interrupt-time cross-process IRET would corrupt architectural state. This
//! mailbox implements deferred preemption instead: IRQ context publishes one
//! generation-and-epoch-bound request, and the next syscall boundary consumes
//! it through the lifecycle scheduler. The IRQ path has a fixed instruction
//! and memory bound, allocates nothing, and never acquires a lock.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use super::lifecycle::{ExecutionAuthority, current_execution_authority_from_irq};

const EMPTY: u32 = 0;
const CLAIMED: u32 = 1;
const PENDING: u32 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PreemptionTicket {
    pub authority: ExecutionAuthority,
    pub requested_tick: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestDisposition {
    Published,
    Coalesced,
    NoUserAuthority,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PreemptionStatistics {
    pub irq_requests: u64,
    pub published: u64,
    pub coalesced: u64,
    pub no_user_authority: u64,
    pub serviced: u64,
    pub stale: u64,
    pub superseded: u64,
}

struct PreemptionMailbox {
    state: AtomicU32,
    identity: AtomicU64,
    scheduler_epoch: AtomicU64,
    requested_tick: AtomicU64,
}

impl PreemptionMailbox {
    const fn new() -> Self {
        Self {
            state: AtomicU32::new(EMPTY),
            identity: AtomicU64::new(0),
            scheduler_epoch: AtomicU64::new(0),
            requested_tick: AtomicU64::new(0),
        }
    }

    /// Publishes only into an empty slot. A request already pending or being
    /// consumed represents the same future scheduler boundary and is
    /// deliberately coalesced instead of spinning in IRQ context.
    fn publish(&self, ticket: PreemptionTicket) -> bool {
        if self
            .state
            .compare_exchange(EMPTY, CLAIMED, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        self.identity
            .store(ticket.authority.encode_identity(), Ordering::Relaxed);
        self.scheduler_epoch
            .store(ticket.authority.scheduler_epoch, Ordering::Relaxed);
        self.requested_tick
            .store(ticket.requested_tick, Ordering::Relaxed);
        self.state.store(PENDING, Ordering::Release);
        true
    }

    fn take(&self) -> Option<PreemptionTicket> {
        self.state
            .compare_exchange(PENDING, CLAIMED, Ordering::Acquire, Ordering::Relaxed)
            .ok()?;
        let identity = self.identity.load(Ordering::Relaxed);
        let scheduler_epoch = self.scheduler_epoch.load(Ordering::Relaxed);
        let requested_tick = self.requested_tick.load(Ordering::Relaxed);
        self.state.store(EMPTY, Ordering::Release);

        let authority = ExecutionAuthority::decode(identity, scheduler_epoch)?;
        Some(PreemptionTicket {
            authority,
            requested_tick,
        })
    }
}

static MAILBOX: PreemptionMailbox = PreemptionMailbox::new();
static IRQ_REQUESTS: AtomicU64 = AtomicU64::new(0);
static PUBLISHED: AtomicU64 = AtomicU64::new(0);
static COALESCED: AtomicU64 = AtomicU64::new(0);
static NO_USER_AUTHORITY: AtomicU64 = AtomicU64::new(0);
static SERVICED: AtomicU64 = AtomicU64::new(0);
static STALE: AtomicU64 = AtomicU64::new(0);
static SUPERSEDED: AtomicU64 = AtomicU64::new(0);
static FIRST_SERVICE: AtomicBool = AtomicBool::new(false);

/// Requests a preemption from timer IRQ context without locks or allocation.
#[inline(always)]
pub fn request_from_timer_irq(wall_tick: u64) -> RequestDisposition {
    IRQ_REQUESTS.fetch_add(1, Ordering::Relaxed);
    let Some(authority) = current_execution_authority_from_irq() else {
        NO_USER_AUTHORITY.fetch_add(1, Ordering::Relaxed);
        return RequestDisposition::NoUserAuthority;
    };
    if MAILBOX.publish(PreemptionTicket {
        authority,
        requested_tick: wall_tick,
    }) {
        PUBLISHED.fetch_add(1, Ordering::Relaxed);
        RequestDisposition::Published
    } else {
        COALESCED.fetch_add(1, Ordering::Relaxed);
        RequestDisposition::Coalesced
    }
}

/// Takes at most one request at a syscall or PID0 scheduling boundary.
pub fn take_at_safe_point() -> Option<PreemptionTicket> {
    MAILBOX.take()
}

/// Records a lifecycle-validated preemption and reports whether this is the
/// first runtime proof marker for the current boot.
pub fn record_serviced() -> bool {
    SERVICED.fetch_add(1, Ordering::Relaxed);
    FIRST_SERVICE
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

pub fn record_stale() {
    STALE.fetch_add(1, Ordering::Relaxed);
}

/// Explicit yield/exit already supplies a stronger scheduling boundary, so a
/// pending timer request can be retired without carrying it into a new epoch.
pub fn retire_superseded() {
    if MAILBOX.take().is_some() {
        SUPERSEDED.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn statistics() -> PreemptionStatistics {
    PreemptionStatistics {
        irq_requests: IRQ_REQUESTS.load(Ordering::Acquire),
        published: PUBLISHED.load(Ordering::Acquire),
        coalesced: COALESCED.load(Ordering::Acquire),
        no_user_authority: NO_USER_AUTHORITY.load(Ordering::Acquire),
        serviced: SERVICED.load(Ordering::Acquire),
        stale: STALE.load(Ordering::Acquire),
        superseded: SUPERSEDED.load(Ordering::Acquire),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::lifecycle::ProcessHandle;

    fn ticket(pid: u32, generation: u32, epoch: u64, tick: u64) -> PreemptionTicket {
        PreemptionTicket {
            authority: ExecutionAuthority {
                handle: ProcessHandle { pid, generation },
                scheduler_epoch: epoch,
            },
            requested_tick: tick,
        }
    }

    #[test]
    fn mailbox_is_single_slot_bounded_and_preserves_exact_authority() {
        let mailbox = PreemptionMailbox::new();
        let first = ticket(7, 11, 19, 23);
        assert!(mailbox.publish(first));
        assert!(!mailbox.publish(ticket(8, 12, 20, 24)));
        assert_eq!(mailbox.take(), Some(first));
        assert_eq!(mailbox.take(), None);
    }

    #[test]
    fn malformed_payload_fails_closed_and_releases_slot() {
        let mailbox = PreemptionMailbox::new();
        mailbox.state.store(CLAIMED, Ordering::Relaxed);
        mailbox.identity.store(0, Ordering::Relaxed);
        mailbox.scheduler_epoch.store(3, Ordering::Relaxed);
        mailbox.state.store(PENDING, Ordering::Release);
        assert_eq!(mailbox.take(), None);
        assert!(mailbox.publish(ticket(1, 1, 4, 5)));
    }
}
