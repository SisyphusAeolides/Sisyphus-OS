use core::sync::atomic::{
    AtomicU64, Ordering,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuVector {
    pub epoch: u64,
    pub logical_tick: u64,
    pub heat: u64,
    pub queue_pressure: u64,
    pub collapses: u64,
    pub replay_rejections: u64,
    pub phase_bin: u16,
    pub coherence: u16,
}

impl CpuVector {
    pub const ZERO: Self = Self {
        epoch: 0,
        logical_tick: 0,
        heat: 0,
        queue_pressure: 0,
        collapses: 0,
        replay_rejections: 0,
        phase_bin: 0,
        coherence: 0,
    };
}

#[repr(C, align(128))]
struct CpuShard {
    guard: AtomicU64,
    epoch: AtomicU64,
    logical_tick: AtomicU64,
    heat: AtomicU64,
    queue_pressure: AtomicU64,
    collapses: AtomicU64,
    replay_rejections: AtomicU64,
    phase_and_coherence: AtomicU64,
}

impl CpuShard {
    const fn new() -> Self {
        Self {
            guard: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
            logical_tick: AtomicU64::new(0),
            heat: AtomicU64::new(0),
            queue_pressure: AtomicU64::new(0),
            collapses: AtomicU64::new(0),
            replay_rejections: AtomicU64::new(0),
            phase_and_coherence: AtomicU64::new(0),
        }
    }

    /// Exactly one writer per shard.
    fn publish(&self, vector: CpuVector) {
        let odd = self
            .guard
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);

        self.epoch.store(vector.epoch, Ordering::Relaxed);
        self.logical_tick
            .store(vector.logical_tick, Ordering::Relaxed);
        self.heat.store(vector.heat, Ordering::Relaxed);
        self.queue_pressure
            .store(vector.queue_pressure, Ordering::Relaxed);
        self.collapses
            .store(vector.collapses, Ordering::Relaxed);
        self.replay_rejections
            .store(vector.replay_rejections, Ordering::Relaxed);

        self.phase_and_coherence.store(
            u64::from(vector.phase_bin)
                | (u64::from(vector.coherence) << 16),
            Ordering::Relaxed,
        );

        self.guard.store(odd.wrapping_add(1), Ordering::Release);
    }

    fn snapshot(&self, attempts: usize) -> Option<CpuVector> {
        for _ in 0..attempts.max(1) {
            let before = self.guard.load(Ordering::Acquire);

            if before & 1 != 0 {
                core::hint::spin_loop();
                continue;
            }

            let epoch = self.epoch.load(Ordering::Relaxed);
            let logical_tick =
                self.logical_tick.load(Ordering::Relaxed);
            let heat = self.heat.load(Ordering::Relaxed);
            let queue_pressure =
                self.queue_pressure.load(Ordering::Relaxed);
            let collapses =
                self.collapses.load(Ordering::Relaxed);
            let replay_rejections =
                self.replay_rejections.load(Ordering::Relaxed);
            let phase =
                self.phase_and_coherence.load(Ordering::Relaxed);

            let after = self.guard.load(Ordering::Acquire);

            if before == after {
                return Some(CpuVector {
                    epoch,
                    logical_tick,
                    heat,
                    queue_pressure,
                    collapses,
                    replay_rejections,
                    phase_bin: phase as u16,
                    coherence: (phase >> 16) as u16,
                });
            }
        }

        None
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CrystalError {
    CpuOutOfRange,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SystemVector {
    pub active_cpus: u16,
    pub newest_epoch: u64,
    pub newest_logical_tick: u64,
    pub maximum_heat: u64,
    pub total_queue_pressure: u64,
    pub total_collapses: u64,
    pub total_replay_rejections: u64,
    pub dominant_phase_bin: u16,
    pub dominant_coherence: u16,
}

pub struct EpochCrystal<const CPUS: usize> {
    shards: [CpuShard; CPUS],
}

impl<const CPUS: usize> EpochCrystal<CPUS> {
    pub const fn new() -> Self {
        Self {
            shards: [const { CpuShard::new() }; CPUS],
        }
    }

    pub fn publish(
        &self,
        cpu: usize,
        vector: CpuVector,
    ) -> Result<(), CrystalError> {
        let shard = self
            .shards
            .get(cpu)
            .ok_or(CrystalError::CpuOutOfRange)?;

        shard.publish(vector);
        Ok(())
    }

    pub fn aggregate(&self) -> SystemVector {
        let mut result = SystemVector {
            active_cpus: 0,
            newest_epoch: 0,
            newest_logical_tick: 0,
            maximum_heat: 0,
            total_queue_pressure: 0,
            total_collapses: 0,
            total_replay_rejections: 0,
            dominant_phase_bin: 0,
            dominant_coherence: 0,
        };

        for shard in &self.shards {
            let Some(vector) = shard.snapshot(8) else {
                continue;
            };

            if vector.epoch == 0 {
                continue;
            }

            result.active_cpus =
                result.active_cpus.saturating_add(1);

            result.newest_epoch =
                result.newest_epoch.max(vector.epoch);

            result.newest_logical_tick =
                result.newest_logical_tick.max(vector.logical_tick);

            result.maximum_heat =
                result.maximum_heat.max(vector.heat);

            result.total_queue_pressure = result
                .total_queue_pressure
                .saturating_add(vector.queue_pressure);

            result.total_collapses =
                result.total_collapses.saturating_add(vector.collapses);

            result.total_replay_rejections = result
                .total_replay_rejections
                .saturating_add(vector.replay_rejections);

            if vector.coherence > result.dominant_coherence {
                result.dominant_coherence = vector.coherence;
                result.dominant_phase_bin = vector.phase_bin;
            }
        }

        result
    }
}

impl<const CPUS: usize> Default for EpochCrystal<CPUS> {
    fn default() -> Self {
        Self::new()
    }
}
