use aether::holographic::{HolographicError, HolographicTree};
use aether::nexus_wire::NexusOpcode;
use aether::resonance_policy::{POLICY_REPHASE, ResonancePolicy};

use crate::blacklab::ResonanceField;
use crate::chronovore::{ChronoTick, TickDevourer};
use crate::kairos::{CriticalMoment, FLAG_KAIROS, KairosScheduler, KairosWindow, MomentPriority};
use crate::ouroboros::{ConstructiveRing, ExecutorHook, PhaseHint, TaskId};
use crate::nexus_amplitude::Amplitude;
use crate::tartarus_deep::{DecoherenceEvent, QuarantineLevel, TartarusCage};
use crate::thermogenesis::ThermalLedger;

pub const MATRIX_PHASE_BINS: u16 = 1024;
pub const MATRIX_HOLOGRAM_LEAVES: usize = 512;
pub const MATRIX_HOLOGRAM_NODES: usize = 1024;
pub const PAIR_FLAG_KAIROS: u32 = 1 << 0;

#[derive(Clone, Copy)]
struct MatrixPair {
    active: bool,
    task_a: TaskId,
    task_b: TaskId,
    amplitude: Amplitude,
    phase_bin: u16,
    flags: u32,
    heat_cost: u64,
    generation: u32,
}

impl MatrixPair {
    const EMPTY: Self = Self {
        active: false,
        task_a: TaskId::INVALID,
        task_b: TaskId::INVALID,
        amplitude: Amplitude::ZERO,
        phase_bin: 0,
        flags: 0,
        heat_cost: 0,
        generation: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MatrixError {
    Capacity,
    InvalidTask,
    InvalidPair,
    InvalidArgument,
    ThermalThrottle,
    Scheduler,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MatrixStats {
    pub logical_tick: u64,
    pub global_phase: u16,
    pub pairs_live: u32,
    pub generation: u32,
    pub collapses: u64,
    pub heat: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MatrixPulse {
    pub logical_tick: u64,
    pub eigenphase: u16,
    pub next_task: Option<TaskId>,
    pub pairs_live: u32,
    pub collapses: u64,
}

#[derive(Clone)]
pub struct NexusMatrix<
    const TASKS: usize,
    const PAIRS: usize,
    const CAGES: usize,
    const MOMENTS: usize,
    const BINS: usize,
> {
    scheduler: ConstructiveRing<TASKS>,
    chrono: TickDevourer,
    field: ResonanceField<BINS>,
    cage: TartarusCage<CAGES>,
    kairos: KairosScheduler<MOMENTS>,

    pairs: [MatrixPair; PAIRS],

    global_phase: u16,
    collapse_threshold: u64,
    generation: u32,
    collapses: u64,
    last_logical_tick: u64,
    last_heat: u64,
}

impl<
    const TASKS: usize,
    const PAIRS: usize,
    const CAGES: usize,
    const MOMENTS: usize,
    const BINS: usize,
> NexusMatrix<TASKS, PAIRS, CAGES, MOMENTS, BINS>
{
    pub const fn new(wall_origin: u64) -> Self {
        Self {
            scheduler: ConstructiveRing::new(),
            chrono: TickDevourer::new(ChronoTick(wall_origin), ChronoTick(wall_origin)),
            field: ResonanceField::new(),
            cage: TartarusCage::new(),
            kairos: KairosScheduler::new(),
            pairs: [MatrixPair::EMPTY; PAIRS],
            global_phase: 0,
            collapse_threshold: 64,
            generation: 1,
            collapses: 0,
            last_logical_tick: wall_origin,
            last_heat: 0,
        }
    }

    pub fn execute<T: ThermalLedger + ?Sized>(
        &mut self,
        opcode: NexusOpcode,
        arguments: [u64; 4],
        wall_tick: u64,
        thermal: &T,
    ) -> Result<[u64; 3], MatrixError> {
        match opcode {
            NexusOpcode::QueryStats | NexusOpcode::QueryTelemetry => {
                let stats = self.stats();

                Ok([
                    u64::from(stats.pairs_live) | (u64::from(stats.generation) << 32),
                    stats.logical_tick,
                    u64::from(stats.global_phase) | (stats.collapses << 16),
                ])
            }

            NexusOpcode::AttachTask => {
                let task = task_from_raw(arguments[0])?;
                let hint = PhaseHint::from_packed(arguments[1]);

                self.scheduler
                    .offer(task, hint, wall_tick)
                    .map_err(|_| MatrixError::Scheduler)?;

                Ok([task_to_raw(task), hint.packed(), 0])
            }

            NexusOpcode::Entangle => {
                let task_a = task_from_raw(arguments[0])?;
                let task_b = task_from_raw(arguments[1])?;

                let phase_bin = (arguments[2] as u16) & (MATRIX_PHASE_BINS - 1);
                let flags = (arguments[2] >> 32) as u32;

                let amplitude_word = arguments[3];
                let re = amplitude_word as u32 as i32;
                let im = (amplitude_word >> 32) as u32 as i32;

                let pair = self.entangle(
                    task_a,
                    task_b,
                    phase_bin,
                    flags,
                    Amplitude::new(re, im),
                    thermal,
                )?;

                Ok([pair as u64, self.generation as u64, 0])
            }

            NexusOpcode::SetCollapseThreshold => {
                let threshold = arguments[0];

                if !(1..=(1_u64 << 48)).contains(&threshold) {
                    return Err(MatrixError::InvalidArgument);
                }

                let previous = self.collapse_threshold;
                self.collapse_threshold = threshold;
                self.generation = self.generation.wrapping_add(1).max(1);

                Ok([previous, threshold, 0])
            }

            NexusOpcode::SetPriorityMass => {
                let mass = arguments[0].min(u16::MAX as u64) as u16;
                let wall = ChronoTick(wall_tick);

                self.chrono.set_priority_mass(mass, wall);

                Ok([
                    u64::from(self.chrono.priority_mass()),
                    self.chrono.now_tick(wall).0,
                    0,
                ])
            }

            NexusOpcode::OfferKairos => {
                let pair_index =
                    usize::try_from(arguments[0]).map_err(|_| MatrixError::InvalidPair)?;

                let pair = self
                    .pairs
                    .get_mut(pair_index)
                    .ok_or(MatrixError::InvalidPair)?;

                if !pair.active {
                    return Err(MatrixError::InvalidPair);
                }

                pair.flags |= PAIR_FLAG_KAIROS;
                self.generation = self.generation.wrapping_add(1).max(1);

                Ok([
                    pair_index as u64,
                    u64::from(pair.phase_bin),
                    pair.flags as u64,
                ])
            }
        }
    }

    pub fn snapshot_telemetry(&self) -> aether::nexus_wire::NexusTelemetry {
        let stats = self.stats();
        aether::nexus_wire::NexusTelemetry::new(
            0,
            stats.logical_tick,
            u64::from(stats.global_phase),
            stats.pairs_live,
            self.generation,
            stats.heat,
            stats.collapses,
        )
    }

    pub fn apply_policy(&mut self, policy: ResonancePolicy, wall_tick: u64) {
        self.collapse_threshold = policy.collapse_threshold;

        self.chrono
            .set_priority_mass(policy.priority_mass, ChronoTick(wall_tick));

        if policy.flags & POLICY_REPHASE != 0 {
            self.global_phase = policy.target_phase & (MATRIX_PHASE_BINS - 1);
        }

        self.generation = self.generation.wrapping_add(1).max(1);
    }

    pub fn heartbeat<T: ThermalLedger + ?Sized>(
        &mut self,
        wall_tick: u64,
        thermal: &T,
    ) -> MatrixPulse {
        let logical_tick = self.chrono.now_tick(ChronoTick(wall_tick)).0;
        self.last_logical_tick = logical_tick;
        self.last_heat = thermal.current_heat();

        self.global_phase = self.global_phase.wrapping_add(1) & (MATRIX_PHASE_BINS - 1);

        self.kairos
            .retire_expired(logical_tick, &mut self.scheduler);

        let eigenphase = self
            .field
            .eigenphase_bin()
            .map(|phase| (phase >> 6) as u16)
            .unwrap_or(self.global_phase);

        for index in 0..PAIRS {
            let mut pair = self.pairs[index];

            if !pair.active {
                continue;
            }

            let delta = wrapped_phase_delta(pair.phase_bin, eigenphase as u16);
            let rotor = rotor_from_delta(delta);

            pair.amplitude = pair.amplitude.rotate_bin(rotor);

            // Gentle amplitude leakage toward zero.
            pair.amplitude.re = pair.amplitude.re.saturating_sub(pair.amplitude.re >> 12);
            pair.amplitude.im = pair.amplitude.im.saturating_sub(pair.amplitude.im >> 12);

            pair.phase_bin = pair.phase_bin.wrapping_add(1) & (MATRIX_PHASE_BINS - 1);

            self.field.accumulate(
                pair.phase_bin << 6,
                q16_to_q31(pair.amplitude.re),
                q16_to_q31(pair.amplitude.im),
                0xffff,
            );

            if pair.amplitude.mag_sq() < self.collapse_threshold {
                pair.active = false;
                pair.amplitude = Amplitude::ZERO;

                let pair_id = (u64::from(pair.generation) << 32) | index as u64;

                let event = DecoherenceEvent {
                    pair_id,
                    task: pair.task_a,
                    amplitude_q31: 0,
                    tick: logical_tick,
                    phase_bin: pair.phase_bin,
                };

                let decision = self.cage.observe(event);

                if decision.level != QuarantineLevel::None {
                    self.collapses = self.collapses.saturating_add(1);
                }

                thermal.credit_collapse_rebate(pair.heat_cost / 4);
                self.pairs[index] = pair;
                continue;
            }

            if pair.flags & PAIR_FLAG_KAIROS != 0 {
                let pair_id = (u64::from(pair.generation) << 32) | index as u64;

                let _ = self.kairos.offer(
                    CriticalMoment {
                        task: pair.task_a,
                        pair_id,
                        window: KairosWindow {
                            opens_at: logical_tick,
                            closes_at: logical_tick.saturating_add(1024),
                        },
                        priority: MomentPriority::Critical,
                        phase_bin: pair.phase_bin,
                        coherence: 900,
                        entanglement_q15: entanglement_q15(pair.amplitude),
                        flags: FLAG_KAIROS,
                    },
                    logical_tick,
                    &mut self.scheduler,
                );
            }

            self.pairs[index] = pair;
        }

        if logical_tick & 0xff == 0 {
            self.field.decay(1);
        }

        let next_task = self.scheduler.select(
            PhaseHint {
                phase_bin: eigenphase,
                coherence: 768,
                priority_mass: self.chrono.priority_mass(),
                flags: 0,
            },
            logical_tick,
        );

        MatrixPulse {
            logical_tick,
            eigenphase,
            next_task,
            pairs_live: self.live_pairs(),
            collapses: self.collapses,
        }
    }

    pub fn stats(&self) -> MatrixStats {
        MatrixStats {
            logical_tick: self.last_logical_tick,
            global_phase: self.global_phase,
            pairs_live: self.live_pairs(),
            generation: self.generation,
            collapses: self.collapses,
            heat: self.last_heat,
        }
    }

    pub fn refresh_hologram(
        &self,
        tree: &mut HolographicTree<MATRIX_HOLOGRAM_LEAVES, MATRIX_HOLOGRAM_NODES>,
    ) -> Result<u64, HolographicError> {
        tree.clear()?;

        tree.write_leaf(0, self.last_logical_tick)?;
        tree.write_leaf(1, u64::from(self.global_phase))?;

        tree.write_leaf(
            2,
            u64::from(self.live_pairs()) | (u64::from(self.generation) << 32),
        )?;

        tree.write_leaf(3, self.collapses)?;
        tree.write_leaf(4, self.last_heat)?;
        tree.write_leaf(5, self.collapse_threshold)?;

        tree.write_leaf(6, u64::from(self.chrono.priority_mass()))?;

        tree.write_leaf(7, u64::from(self.field.eigenphase_bin().unwrap_or(0)))?;

        for (index, pair) in self.pairs.iter().enumerate() {
            let value = if pair.active {
                let mut digest =
                    mix_matrix_state(task_to_raw(pair.task_a), task_to_raw(pair.task_b));

                digest = mix_matrix_state(
                    digest,
                    pair.amplitude.re as u32 as u64 | ((pair.amplitude.im as u32 as u64) << 32),
                );

                digest = mix_matrix_state(
                    digest,
                    u64::from(pair.phase_bin) | (u64::from(pair.flags) << 16),
                );

                mix_matrix_state(digest, u64::from(pair.generation))
            } else {
                0
            };

            tree.write_leaf(16 + index, value)?;
        }

        tree.rebuild()
    }

    fn entangle<T: ThermalLedger + ?Sized>(
        &mut self,
        task_a: TaskId,
        task_b: TaskId,
        phase_bin: u16,
        flags: u32,
        amplitude: Amplitude,
        thermal: &T,
    ) -> Result<usize, MatrixError> {
        if task_a == task_b || task_a == TaskId::INVALID || task_b == TaskId::INVALID {
            return Err(MatrixError::InvalidTask);
        }

        let slot = self
            .pairs
            .iter()
            .position(|pair| !pair.active)
            .ok_or(MatrixError::Capacity)?;

        let heat_cost = 8_u64.saturating_add(amplitude.mag_sq() >> 20);

        thermal
            .charge(heat_cost)
            .map_err(|_| MatrixError::ThermalThrottle)?;

        self.generation = self.generation.wrapping_add(1).max(1);

        self.pairs[slot] = MatrixPair {
            active: true,
            task_a,
            task_b,
            amplitude,
            phase_bin,
            flags,
            heat_cost,
            generation: self.generation,
        };

        Ok(slot)
    }

    pub fn rephase(&mut self, target_phase: u16) -> Result<u16, MatrixError> {
        if target_phase >= MATRIX_PHASE_BINS {
            return Err(MatrixError::InvalidArgument);
        }

        let previous = self.global_phase;
        self.global_phase = target_phase;

        self.generation = self.generation.wrapping_add(1).max(1);

        Ok(previous)
    }

    fn live_pairs(&self) -> u32 {
        self.pairs
            .iter()
            .filter(|pair| pair.active)
            .count()
            .min(u32::MAX as usize) as u32
    }
}

fn task_from_raw(raw: u64) -> Result<TaskId, MatrixError> {
    let slot = raw as u16;
    let generation = (raw >> 16) as u16;

    if slot == u16::MAX || generation == 0 {
        return Err(MatrixError::InvalidTask);
    }

    Ok(TaskId::new(slot, generation))
}

pub const fn task_to_raw(task: TaskId) -> u64 {
    (task.slot as u64) | ((task.generation as u64) << 16)
}

fn q16_to_q31(value: i32) -> i32 {
    ((i64::from(value)) << 15).clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

fn entanglement_q15(amplitude: Amplitude) -> i16 {
    (amplitude.mag_sq() >> 17).min(i16::MAX as u64) as i16
}

fn wrapped_phase_delta(from: u16, to: u16) -> i16 {
    let raw = i32::from(to) - i32::from(from);

    let wrapped = if raw > 511 {
        raw - 1024
    } else if raw < -512 {
        raw + 1024
    } else {
        raw
    };

    wrapped as i16
}

fn rotor_from_delta(delta: i16) -> u8 {
    let step = (delta / 16).clamp(-4, 4);

    if step >= 0 {
        step as u8
    } else {
        (64_i16 + step) as u8
    }
}

fn mix_matrix_state(mut state: u64, value: u64) -> u64 {
    state ^= value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    state = state.rotate_left(27);
    state = state.wrapping_mul(0x94d0_49bb_1331_11eb);
    state ^ (state >> 31)
}
