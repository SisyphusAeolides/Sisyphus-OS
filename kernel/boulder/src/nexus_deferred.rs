use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const MAXIMUM_COALESCED_TICKS: u64 = 1 << 20;

static PENDING_TICKS: AtomicU64 = AtomicU64::new(0);
static LATEST_WALL_TICK: AtomicU64 = AtomicU64::new(0);
static RUNNING: AtomicBool = AtomicBool::new(false);

static TOTAL_REQUESTS: AtomicU64 = AtomicU64::new(0);
static TOTAL_RUNS: AtomicU64 = AtomicU64::new(0);
static TOTAL_COALESCED: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeferredReport {
    pub passes: u32,
    pub ticks_absorbed: u64,
    pub work_remains: bool,
    pub already_running: bool,
}

impl DeferredReport {
    const ALREADY_RUNNING: Self = Self {
        passes: 0,
        ticks_absorbed: 0,
        work_remains: true,
        already_running: true,
    };
}

/// IRQ-safe.
///
/// This function must never acquire a lock, allocate, execute policy code,
/// inspect user memory, or touch the NexusMatrix.
#[inline(always)]
pub fn request_from_irq(wall_tick: u64) {
    LATEST_WALL_TICK.store(wall_tick, Ordering::Release);

    let previous = PENDING_TICKS
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
            Some(pending.saturating_add(1).min(MAXIMUM_COALESCED_TICKS))
        })
        .unwrap_or(0);

    TOTAL_REQUESTS.fetch_add(1, Ordering::Relaxed);

    if previous != 0 {
        TOTAL_COALESCED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Runs only from scheduler, idle, syscall-exit, or another non-IRQ safe point.
///
/// `maximum_passes` bounds latency even when timer requests arrive faster than
/// the matrix can consume them.
pub fn run_deferred(maximum_passes: u32) -> DeferredReport {
    if maximum_passes == 0 {
        return DeferredReport {
            passes: 0,
            ticks_absorbed: 0,
            work_remains: PENDING_TICKS.load(Ordering::Acquire) != 0,
            already_running: false,
        };
    }

    if RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return DeferredReport::ALREADY_RUNNING;
    }

    let mut passes = 0_u32;
    let mut ticks_absorbed = 0_u64;

    while passes < maximum_passes {
        let pending = PENDING_TICKS.swap(0, Ordering::AcqRel);

        if pending == 0 {
            break;
        }

        let wall_tick = LATEST_WALL_TICK.load(Ordering::Acquire);

        // One matrix evolution absorbs all causally equivalent timer requests.
        crate::nexus_plane::drive_once(wall_tick, pending);

        let _ = crate::manifold_orchestrator::run_tensor_online_update_deferred();
        let _ = crate::manifold_orchestrator::run_tensor_analysis_deferred();
        let _ = crate::manifold_orchestrator::run_predictive_control_deferred();

        ticks_absorbed = ticks_absorbed.saturating_add(pending);
        passes += 1;
        TOTAL_RUNS.fetch_add(1, Ordering::Relaxed);
    }

    RUNNING.store(false, Ordering::Release);

    DeferredReport {
        passes,
        ticks_absorbed,
        work_remains: PENDING_TICKS.load(Ordering::Acquire) != 0,
        already_running: false,
    }
}

pub fn statistics() -> (u64, u64, u64) {
    (
        TOTAL_REQUESTS.load(Ordering::Acquire),
        TOTAL_RUNS.load(Ordering::Acquire),
        TOTAL_COALESCED.load(Ordering::Acquire),
    )
}
