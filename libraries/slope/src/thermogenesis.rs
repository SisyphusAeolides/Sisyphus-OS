// THERMOGENESIS CLIENT — zero-syscall thermal pressure awareness
//
// The Boulder kernel maintains a per-process ThermalPage at a fixed mapping
// address. It writes thermal readings via Release stores; userland reads with
// Acquire loads. No syscall on the hot path — just an atomic read.
//
// ThermalPage layout (fits in one cache line, 64 bytes):
//   [0]    temperature_zone: u8   — 0=cold 1=warm 2=hot 3=critical
//   [1]    throttle_hint:    u8   — 0=full 1=half 2=quarter 3=minimal
//   [2]    kernel_epoch:     u8   — wrapping epoch; stale if unchanged for >N reads
//   [3]    _pad
//   [4..8] tsc_frequency_mhz: u32 — cached from CPUID, kernel writes once at boot
//   [8..16] cpu_budget_ticks:  u64 — ticks allocated to this process this epoch
//   [16..24] cpu_used_ticks:   u64 — ticks consumed this epoch (kernel writes)
//   [24..32] thermal_ticks:    u64 — absolute TSC at last thermal event
//   [32..64] _reserved
//
// ThermalGuard: a RAII throttle scope.
//   On entry: reads current throttle_hint.
//   On exit: yields retrocausally with consumed work proportional to hint.
//   Use it to wrap any hot loop so it self-throttles under pressure.
//
// ThermalPolicy: user-registered callbacks for zone transitions.
//   The event horizon executor (tachyon) calls check_transition() after
//   each task completion. If zone changed, fires the registered handler.

use crate::process::tachyon;
use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

// ─── THERMAL PAGE ──────────────────────────────────────────────────────────

#[repr(C, align(64))]
pub struct ThermalPage {
    pub temperature_zone: AtomicU8,
    pub throttle_hint: AtomicU8,
    pub kernel_epoch: AtomicU8,
    _pad: u8,
    pub tsc_frequency_mhz: AtomicU32,
    pub cpu_budget_ticks: AtomicU64,
    pub cpu_used_ticks: AtomicU64,
    pub thermal_ticks: AtomicU64,
    _reserved: [u8; 32],
}

const _: () = assert!(core::mem::size_of::<ThermalPage>() == 64);

impl ThermalPage {
    pub const fn zeroed() -> Self {
        Self {
            temperature_zone: AtomicU8::new(0),
            throttle_hint: AtomicU8::new(0),
            kernel_epoch: AtomicU8::new(0),
            _pad: 0,
            tsc_frequency_mhz: AtomicU32::new(0),
            cpu_budget_ticks: AtomicU64::new(0),
            cpu_used_ticks: AtomicU64::new(0),
            thermal_ticks: AtomicU64::new(0),
            _reserved: [0; 32],
        }
    }

    pub fn zone(&self) -> ThermalZone {
        ThermalZone::from_raw(self.temperature_zone.load(Ordering::Acquire))
    }

    pub fn hint(&self) -> ThrottleHint {
        ThrottleHint::from_raw(self.throttle_hint.load(Ordering::Acquire))
    }

    pub fn budget_remaining(&self) -> u64 {
        let budget = self.cpu_budget_ticks.load(Ordering::Acquire);
        let used = self.cpu_used_ticks.load(Ordering::Acquire);
        budget.saturating_sub(used)
    }

    pub fn epoch(&self) -> u8 {
        self.kernel_epoch.load(Ordering::Acquire)
    }

    pub fn tsc_mhz(&self) -> u32 {
        self.tsc_frequency_mhz.load(Ordering::Relaxed)
    }
}

// ─── THERMAL ZONE ──────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum ThermalZone {
    Cold = 0,
    Warm = 1,
    Hot = 2,
    Critical = 3,
}

impl ThermalZone {
    pub const fn from_raw(n: u8) -> Self {
        match n {
            1 => Self::Warm,
            2 => Self::Hot,
            3 => Self::Critical,
            _ => Self::Cold,
        }
    }
    pub const fn is_throttled(self) -> bool {
        self as u8 >= 2
    }
}

// ─── THROTTLE HINT ─────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThrottleHint {
    Full = 0,
    Half = 1,
    Quarter = 2,
    Minimal = 3,
}

impl ThrottleHint {
    pub const fn from_raw(n: u8) -> Self {
        match n {
            1 => Self::Half,
            2 => Self::Quarter,
            3 => Self::Minimal,
            _ => Self::Full,
        }
    }

    /// Fraction of work to perform: 1.0, 0.5, 0.25, 0.1
    pub const fn work_fraction_millipct(self) -> u32 {
        match self {
            Self::Full => 1000,
            Self::Half => 500,
            Self::Quarter => 250,
            Self::Minimal => 100,
        }
    }

    /// Yield pressure to emit when this hint is active.
    pub const fn yield_pressure(self) -> u64 {
        match self {
            Self::Full => 0,
            Self::Half => 1_000,
            Self::Quarter => 5_000,
            Self::Minimal => 20_000,
        }
    }
}

// ─── THERMAL GUARD — RAII throttle scope ───────────────────────────────────

pub struct ThermalGuard<'page> {
    page: &'page ThermalPage,
    hint: ThrottleHint,
    start: u64,
}

impl<'page> ThermalGuard<'page> {
    pub fn enter(page: &'page ThermalPage) -> Self {
        let hint = page.hint();
        let start = crate::time::read_counter();
        Self { page, hint, start }
    }

    pub fn current_hint(&self) -> ThrottleHint {
        self.hint
    }

    /// True if the budget is exhausted this epoch — caller should yield immediately.
    pub fn budget_exhausted(&self) -> bool {
        self.page.budget_remaining() == 0
    }
}

impl Drop for ThermalGuard<'_> {
    fn drop(&mut self) {
        let elapsed = crate::time::read_counter().saturating_sub(self.start);
        let pressure = self.hint.yield_pressure();
        if pressure > 0 || elapsed > tachyon::DEFAULT_EVENT_HORIZON_TICKS {
            let _ = tachyon::yield_retrocausally(elapsed.max(pressure));
        }
    }
}

// ─── THERMAL POLICY — zone-transition dispatch ─────────────────────────────

pub type ZoneHandler = fn(old: ThermalZone, new: ThermalZone);

fn default_zone_handler(_old: ThermalZone, new: ThermalZone) {
    if new == ThermalZone::Critical {
        // Default: yield hard under critical thermal load.
        let _ = tachyon::yield_retrocausally(100_000);
    }
}

pub struct ThermalPolicy<'page> {
    page: &'page ThermalPage,
    last_zone: ThermalZone,
    last_epoch: u8,
    handler: ZoneHandler,
}

impl<'page> ThermalPolicy<'page> {
    pub fn new(page: &'page ThermalPage) -> Self {
        Self {
            page,
            last_zone: page.zone(),
            last_epoch: page.epoch(),
            handler: default_zone_handler,
        }
    }

    pub fn set_handler(&mut self, handler: ZoneHandler) {
        self.handler = handler;
    }

    /// Call after each task or loop iteration to detect zone changes.
    /// Fires the handler exactly once per transition.
    pub fn check_transition(&mut self) {
        let epoch = self.page.epoch();
        if epoch == self.last_epoch {
            return;
        }
        self.last_epoch = epoch;
        let new_zone = self.page.zone();
        if new_zone != self.last_zone {
            let old = self.last_zone;
            self.last_zone = new_zone;
            (self.handler)(old, new_zone);
        }
    }

    pub fn current_zone(&self) -> ThermalZone {
        self.last_zone
    }
}

// ─── UTILITY: CPU budget check without a syscall ───────────────────────────

/// Returns true if the process is under thermal stress and should shed load.
/// Zero-cost: one atomic load on the shared thermal page.
#[inline(always)]
pub fn is_thermally_stressed(page: &ThermalPage) -> bool {
    page.zone().is_throttled()
}

/// Compute an iteration batch size adjusted for current thermal pressure.
/// At Full: returns `base`. At Minimal: returns `base / 10`.
#[inline(always)]
pub fn throttled_batch_size(page: &ThermalPage, base: usize) -> usize {
    let millipct = page.hint().work_fraction_millipct() as usize;
    (base * millipct / 1000).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thermal_zone_ordering() {
        assert!(ThermalZone::Critical > ThermalZone::Cold);
        assert!(ThermalZone::Hot.is_throttled());
        assert!(!ThermalZone::Warm.is_throttled());
    }

    #[test]
    fn throttled_batch_size_scales_correctly() {
        let page = ThermalPage::zeroed();
        page.throttle_hint
            .store(3, core::sync::atomic::Ordering::Relaxed);
        let batch = throttled_batch_size(&page, 1000);
        assert_eq!(batch, 100); // Minimal → 10%
    }

    #[test]
    fn policy_fires_handler_on_zone_change() {
        use core::sync::atomic::{AtomicU8, Ordering};
        static CALLS: AtomicU8 = AtomicU8::new(0);
        fn handler(_old: ThermalZone, _new: ThermalZone) {
            CALLS.fetch_add(1, Ordering::Relaxed);
        }
        let page = ThermalPage::zeroed();
        let mut policy = ThermalPolicy::new(&page);
        policy.set_handler(handler);

        // Simulate kernel writing a new epoch + new zone
        page.temperature_zone.store(2, Ordering::Relaxed); // Hot
        page.kernel_epoch.store(1, Ordering::Release);

        policy.check_transition();
        assert_eq!(CALLS.load(Ordering::Relaxed), 1);
        assert_eq!(policy.current_zone(), ThermalZone::Hot);

        // Same epoch — no second fire
        policy.check_transition();
        assert_eq!(CALLS.load(Ordering::Relaxed), 1);
    }
}
