// =============================================================================
// quantum_nexus.rs — Black Lab Integration Nexus
//
// Experimental. no_std. The place where every high-value kernel subsystem
// collapses into a single phase-coherent control plane.
//
// Wires:
//   blacklab       — resonance field / learning lattice
//   ouroboros      — async task ring / waker ABI
//   chronovore     — time dilation / tick devourer
//   thermogenesis  — thermal / entropy budget
//   fabric         — capability-gated IPC weave
//   tartarus_deep  — deep fault / quarantine plane
//   kairos         — critical-moment scheduler
//   aether         — ambient medium / broadcast fabric
//   capability     — typed Authority / Right sealing
//   mmio           — device window map
//   mirage         — virtual address theatre
//
// Philosophy: every runnable is a complex amplitude. Scheduling is constructive
// interference. Faults are decoherence events. Thermogenesis is the heat cost
// of collapsing a superposition into a committed state.
// =============================================================================

#![allow(dead_code)]
#![allow(clippy::too_many_arguments)]

use core::sync::atomic::{AtomicU64, AtomicU32, AtomicBool, Ordering};
use core::mem::MaybeUninit;
use core::ptr::NonNull;
use core::marker::PhantomData;

// ─── STUBS FOR EXPERIMENTAL SUBSYSTEMS ───────────────────────────────────────
pub struct Authority;
pub struct Capability<'a, T>(&'a T, PhantomData<T>);
pub struct FabricRight; pub struct ResonanceRight; pub struct SchedulerRight; pub struct LearningRight; pub struct DmaRight; pub struct DeviceMemoryRight;
pub struct FabricEndpoint;
pub struct FabricMessage { pub tag: u32, pub len: usize, pub payload: [u8; 64] }
pub struct WeaveToken;
#[derive(Clone, Copy, PartialEq, Eq)] pub struct TaskId(u64);
impl TaskId { pub const INVALID: Self = Self(0); pub fn from_raw(raw: u64) -> Self { Self(raw) } }
pub struct WakerToken;
pub struct PhaseHint { pub tag: u32 }
impl PhaseHint { pub const IDLE: Self = Self { tag: 0 }; pub fn constructive(_a: TaskId, _b: TaskId, _c: u8, _d: u32) -> Self { Self { tag: 1 } } pub fn from_global(_p: u64) -> Self { Self { tag: 2 } } }
pub trait ExecutorHook { fn priority_of(&self, _task: TaskId) -> Option<u8> { None } fn request_ephemeral_pair(&self, _a: u8, _b: u16) -> Result<(), ()> { Ok(()) } fn boost_critical_slice(&self, _a: u8) {} }
pub struct ChronoTick; pub struct DilatedDuration; pub struct TickDevourer;
impl TickDevourer { pub fn now_tick(&self) -> u64 { 0 } pub fn request_dilation(&self, _a: u8, _b: u16) {} }
pub struct ThermalBudget;
impl ThermalBudget { pub fn current_heat(&self) -> u32 { 0 } pub fn entropy_sample(&self) -> EntropySample { EntropySample { noise_floor: 0 } } pub fn charge(&self, _amount: u32) -> Result<(), ()> { Ok(()) } pub fn credit_collapse_rebate(&self, _a: u32) {} pub fn inject_spike(&self, _a: u32) {} }
pub struct EntropySample { pub noise_floor: u32 }
pub struct HeatSink;
pub struct TartarusCage;
impl TartarusCage { pub fn quarantine(&self, _ev: DecoherenceEvent, _level: QuarantineLevel) {} pub fn inject_canary(&self, _gen: u32, _level: QuarantineLevel) {} }
pub struct DecoherenceEvent { pub task_a: TaskId, pub task_b: TaskId, pub generation: u32 }
pub enum QuarantineLevel { Soft, Probe }
pub struct KairosWindow;
impl KairosWindow { pub fn offer(&self, _moment: CriticalMoment) {} }
pub struct CriticalMoment { pub task: TaskId, pub priority: MomentPriority, pub deadline_tick: u64 }
pub enum MomentPriority { Entangled }
pub struct AetherChannel; pub struct AmbientPulse;
pub struct ResonanceField;
impl ResonanceField { pub fn diagonal_cells(&self) -> core::slice::Iter<LatticeCell> { [].iter() } pub fn accumulate(&self, _tag: usize, _re: i32, _im: i32) {} pub fn eigenphase_bin(&self, _hint: usize) -> u8 { 0 } pub fn nudge_phase(&self, _a: usize, _b: u8) {} pub fn mark_stress(&self, _a: usize) {} pub fn broadcast_ambient(&self, _a: u8, _b: usize) {} pub fn bind_instrument_channels(&self, _id: WindowId, _c: usize) {} }
pub struct LatticeCell;
impl LatticeCell { pub fn weight_q16(&self) -> i32 { 0 } pub fn phase_q16(&self) -> i32 { 0 } pub fn tag(&self) -> u32 { 0 } }
pub struct LearningGradient;
pub struct MmioWindow;
impl MmioWindow { pub unsafe fn read_u32(&self, _offset: usize) -> u32 { 0 } pub fn id(&self) -> WindowId { WindowId(0) } }
pub struct WindowId(u32);
use crate::sync::SpinLock;
// ─────────────────────────────────────────────────────────────────────────────

// ─── CONSTANTS ───────────────────────────────────────────────────────────────

/// Maximum entangled task pairs tracked simultaneously.
pub const MAX_ENTANGLED_PAIRS: usize = 256;

/// Amplitude ring depth (power-of-two for mask indexing).
pub const AMPLITUDE_RING_DEPTH: usize = 512;
const AMPLITUDE_MASK: usize = AMPLITUDE_RING_DEPTH - 1;

/// Phase bins for constructive-interference scoring (0..TAU mapped to bins).
pub const PHASE_BINS: usize = 64;

/// Lorentz γ table resolution for priority → mass mapping.
pub const GAMMA_TABLE_LEN: usize = 16;

/// Black-lab experiment slot count.
pub const EXPERIMENT_SLOTS: usize = 32;

/// Nexus magic — written to serial on successful ignition.
const NEXUS_MAGIC: u64 = 0x51_5F_4E_45_58_55_53_21; // "Q_NEXUS!"

// ─── FIXED-POINT COMPLEX AMPLITUDE ───────────────────────────────────────────
//
// Q16.16 fixed-point complex number. No f32/f64 in the kernel hot path.
// Real and imag are i32; magnitude² fits in i64.

#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct Amplitude {
    /// Real part, Q16.16
    pub re: i32,
    /// Imaginary part, Q16.16
    pub im: i32,
}

impl Amplitude {
    pub const ZERO: Self = Self { re: 0, im: 0 };
    /// |1⟩  (unit real)
    pub const ONE:  Self = Self { re: 1 << 16, im: 0 };
    /// i|1⟩ (unit imag)
    pub const I:    Self = Self { re: 0, im: 1 << 16 };

    #[inline(always)]
    pub const fn new(re: i32, im: i32) -> Self {
        Self { re, im }
    }

    /// Magnitude squared in Q32.32-ish (product of two Q16.16 → Q32.32, we keep high).
    #[inline(always)]
    pub fn mag_sq(self) -> u64 {
        let re = self.re as i64;
        let im = self.im as i64;
        ((re * re + im * im) as u64) >> 16
    }

    /// Multiply two amplitudes (Q16.16 × Q16.16 → Q16.16).
    #[inline(always)]
    pub fn mul(self, other: Self) -> Self {
        let ar = self.re as i64;
        let ai = self.im as i64;
        let br = other.re as i64;
        let bi = other.im as i64;
        let re = (ar * br - ai * bi) >> 16;
        let im = (ar * bi + ai * br) >> 16;
        Self {
            re: re.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
            im: im.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        }
    }

    /// Add two amplitudes with saturating arithmetic.
    #[inline(always)]
    pub fn add(self, other: Self) -> Self {
        Self {
            re: self.re.saturating_add(other.re),
            im: self.im.saturating_add(other.im),
        }
    }

    /// Rotate by a phase bin index (0..PHASE_BINS).
    /// Uses a baked cos/sin LUT in Q16.16.
    #[inline]
    pub fn rotate_bin(self, bin: u8) -> Self {
        let (c, s) = PHASE_LUT[(bin as usize) & (PHASE_BINS - 1)];
        self.mul(Amplitude { re: c, im: s })
    }

    /// Collapse: if mag_sq below threshold, snap to ZERO (decoherence).
    #[inline]
    pub fn collapse_if_weak(self, threshold: u64) -> Self {
        if self.mag_sq() < threshold {
            Self::ZERO
        } else {
            self
        }
    }
}

// Cos/sin LUT for 64 phase bins. Generated offline: angle = 2π·k/64.
// Values are Q16.16 (cos(0)=65536, cos(π/2)=0, …).
static PHASE_LUT: [(i32, i32); PHASE_BINS] = [
    ( 65536,      0), ( 64277,   6393), ( 60547,  12540), ( 54491,  18205),
    ( 46341,  23170), ( 36410,  27246), ( 25080,  30274), ( 12785,  32138),
    (     0,  32768), (-12785,  32138), (-25080,  30274), (-36410,  27246),
    (-46341,  23170), (-54491,  18205), (-60547,  12540), (-64277,   6393),
    (-65536,      0), (-64277,  -6393), (-60547, -12540), (-54491, -18205),
    (-46341, -23170), (-36410, -27246), (-25080, -30274), (-12785, -32138),
    (     0, -32768), ( 12785, -32138), ( 25080, -30274), ( 36410, -27246),
    ( 46341, -23170), ( 54491, -18205), ( 60547, -12540), ( 64277,  -6393),
    // mirrored second half (bins 32..63) — same as 0..31 with sign flip on imag
    ( 65536,      0), ( 64277,   6393), ( 60547,  12540), ( 54491,  18205),
    ( 46341,  23170), ( 36410,  27246), ( 25080,  30274), ( 12785,  32138),
    (     0,  32768), (-12785,  32138), (-25080,  30274), (-36410,  27246),
    (-46341,  23170), (-54491,  18205), (-60547,  12540), (-64277,   6393),
    (-65536,      0), (-64277,  -6393), (-60547, -12540), (-54491, -18205),
    (-46341, -23170), (-36410, -27246), (-25080, -30274), (-12785, -32138),
    (     0, -32768), ( 12785, -32138), ( 25080, -30274), ( 36410, -27246),
    ( 46341, -23170), ( 54491, -18205), ( 60547, -12540), ( 64277,  -6393),
];

// ─── LORENTZ γ TABLE ─────────────────────────────────────────────────────────
//
// Priority mass → time-dilation factor. Higher priority = higher γ = more
// chronovore ticks consumed per wall-tick (they "move faster" through compute).
// Q8.8 fixed point (256 = 1.0).

static GAMMA_TABLE: [u16; GAMMA_TABLE_LEN] = [
    0x0100, // Idle        γ=1.00
    0x011A, // Low         γ=1.10
    0x0133, // BelowNormal γ=1.20
    0x014D, // Normal      γ=1.30
    0x0166, // AboveNormal γ=1.40
    0x019A, // High        γ=1.60
    0x01CD, // Higher      γ=1.80
    0x0200, // RealTime    γ=2.00
    0x0280, // IRQ         γ=2.50
    0x0300, // NMI         γ=3.00
    0x0400, // BlackLab    γ=4.00  ← experiment tasks
    0x0500, // Entangled   γ=5.00
    0x0600, // Superposed  γ=6.00
    0x0800, // Collapsing  γ=8.00
    0x0C00, // Singularity γ=12.0
    0x1000, // EventHorizon γ=16.0
];

#[inline(always)]
pub fn gamma_for_priority(prio: u8) -> u16 {
    GAMMA_TABLE[(prio as usize).min(GAMMA_TABLE_LEN - 1)]
}

// ─── ENTANGLED PAIR ──────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
#[repr(C)]
pub struct EntangledPair {
    pub task_a:     TaskId,
    pub task_b:     TaskId,
    pub amplitude:  Amplitude,
    pub phase_bin:  u8,
    pub generation: u32,
    pub heat_cost:  u32,
    pub flags:      u16,
}

impl EntangledPair {
    pub const FLAG_LIVE:       u16 = 1 << 0;
    pub const FLAG_COLLAPSING: u16 = 1 << 1;
    pub const FLAG_QUARANTINE: u16 = 1 << 2;
    pub const FLAG_KAIROS:     u16 = 1 << 3;

    pub const EMPTY: Self = Self {
        task_a: TaskId::INVALID,
        task_b: TaskId::INVALID,
        amplitude: Amplitude::ZERO,
        phase_bin: 0,
        generation: 0,
        heat_cost: 0,
        flags: 0,
    };

    #[inline]
    pub fn is_live(&self) -> bool {
        self.flags & Self::FLAG_LIVE != 0
    }

    /// Interference score against a peer pair (constructive if phase-aligned).
    pub fn interference(&self, other: &Self) -> i32 {
        let delta = (self.phase_bin as i16 - other.phase_bin as i16).unsigned_abs() as u8;
        let aligned = self.amplitude.rotate_bin(delta).mul(other.amplitude);
        // Project onto real axis as signed score.
        aligned.re
    }
}

// ─── AMPLITUDE RING ──────────────────────────────────────────────────────────
//
// Lock-free SPSC-ish ring of amplitudes stamped with chronovore ticks.
// Producer: blacklab learning path. Consumer: nexus scheduler tick.

#[derive(Clone, Copy)]
#[repr(C)]
struct AmplitudeSlot {
    amp:   Amplitude,
    tick:  u64,
    tag:   u32,
    _pad:  u32,
}

pub struct AmplitudeRing {
    slots:  [AmplitudeSlot; AMPLITUDE_RING_DEPTH],
    head:   AtomicU64,
    tail:   AtomicU64,
}

impl AmplitudeRing {
    pub const fn new() -> Self {
        const EMPTY: AmplitudeSlot = AmplitudeSlot {
            amp: Amplitude::ZERO, tick: 0, tag: 0, _pad: 0,
        };
        Self {
            slots: [EMPTY; AMPLITUDE_RING_DEPTH],
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
        }
    }

    pub fn push(&mut self, amp: Amplitude, tick: u64, tag: u32) -> bool {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) as usize >= AMPLITUDE_RING_DEPTH {
            return false; // full
        }
        let idx = (head as usize) & AMPLITUDE_MASK;
        self.slots[idx] = AmplitudeSlot { amp, tick, tag, _pad: 0 };
        self.head.store(head.wrapping_add(1), Ordering::Release);
        true
    }

    pub fn pop(&mut self) -> Option<(Amplitude, u64, u32)> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail == head {
            return None;
        }
        let idx = (tail as usize) & AMPLITUDE_MASK;
        let s = self.slots[idx];
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Some((s.amp, s.tick, s.tag))
    }
}

// ─── EXPERIMENT SLOT ─────────────────────────────────────────────────────────
//
// Black-lab experiment descriptor. Each slot is a controlled mutation of
// scheduler / fabric / thermal policy under a ResonanceRight capability.

#[derive(Clone, Copy)]
#[repr(u8)]
pub enum ExperimentKind {
    PhaseDrift       = 0,
    EntangleBurst    = 1,
    ThermalSpike     = 2,
    FabricFlood      = 3,
    ChronoDilate     = 4,
    TartarusInject   = 5,
    KairosFocus      = 6,
    AetherBroadcast  = 7,
    Superposition    = 8,
    CollapseStress   = 9,
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct Experiment {
    pub kind:        ExperimentKind,
    pub active:      bool,
    pub priority:    u8,
    pub phase_seed:  u8,
    pub budget_heat: u32,
    pub budget_ticks: u64,
    pub spent_heat:  u32,
    pub spent_ticks: u64,
    pub result_code: i32,
    pub lattice_idx: u16,
    pub generation:  u32,
}

impl Experiment {
    pub const EMPTY: Self = Self {
        kind: ExperimentKind::PhaseDrift,
        active: false,
        priority: 0,
        phase_seed: 0,
        budget_heat: 0,
        budget_ticks: 0,
        spent_heat: 0,
        spent_ticks: 0,
        result_code: 0,
        lattice_idx: 0,
        generation: 0,
    };
}

// ─── NEXUS STATE ─────────────────────────────────────────────────────────────

pub struct QuantumNexus {
    /// Entanglement table
    pairs:       [EntangledPair; MAX_ENTANGLED_PAIRS],
    pair_count:  AtomicU32,

    /// Amplitude history ring
    ring:        AmplitudeRing,

    /// Active black-lab experiments
    experiments: [Experiment; EXPERIMENT_SLOTS],

    /// Global phase accumulator (advances every chronovore tick)
    global_phase: AtomicU64,

    /// Collapse threshold (mag_sq below this → decoherence)
    collapse_threshold: AtomicU64,

    /// Nexus armed?
    armed: AtomicBool,

    /// Generation counter (bumped on every structural mutation)
    generation: AtomicU32,

    /// Cached thermal budget snapshot
    last_heat: AtomicU32,

    /// Stats
    stats_collapses:   AtomicU64,
    stats_entangles:   AtomicU64,
    stats_experiments: AtomicU64,
    stats_interf_pos:  AtomicU64,
    stats_interf_neg:  AtomicU64,
}

impl QuantumNexus {
    pub const fn new() -> Self {
        Self {
            pairs: [EntangledPair::EMPTY; MAX_ENTANGLED_PAIRS],
            pair_count: AtomicU32::new(0),
            ring: AmplitudeRing::new(),
            experiments: [Experiment::EMPTY; EXPERIMENT_SLOTS],
            global_phase: AtomicU64::new(0),
            collapse_threshold: AtomicU64::new(64), // Q-units
            armed: AtomicBool::new(false),
            generation: AtomicU32::new(0),
            last_heat: AtomicU32::new(0),
            stats_collapses: AtomicU64::new(0),
            stats_entangles: AtomicU64::new(0),
            stats_experiments: AtomicU64::new(0),
            stats_interf_pos: AtomicU64::new(0),
            stats_interf_neg: AtomicU64::new(0),
        }
    }

    // ── IGNITION ─────────────────────────────────────────────────────────────

    /// Arm the nexus. Requires ResonanceRight + SchedulerRight + LearningRight.
    pub fn ignite(
        &mut self,
        _auth: &Authority,
        _res:  &Capability<'_, ResonanceRight>,
        _sch:  &Capability<'_, SchedulerRight>,
        _lrn:  &Capability<'_, LearningRight>,
        field: &mut ResonanceField,
        chrono: &mut TickDevourer,
        thermal: &mut ThermalBudget,
    ) -> Result<(), NexusError> {
        if self.armed.load(Ordering::Acquire) {
            return Err(NexusError::AlreadyArmed);
        }

        // Seed amplitude ring from blacklab lattice diagonal.
        for (i, cell) in field.diagonal_cells().enumerate() {
            if i >= AMPLITUDE_RING_DEPTH {
                break;
            }
            let amp = Amplitude::new(cell.weight_q16(), cell.phase_q16());
            let tick = chrono.now_tick();
            let _ = self.ring.push(amp, tick, cell.tag());
        }

        // Snapshot thermal floor.
        self.last_heat.store(thermal.current_heat(), Ordering::Release);

        // Baseline collapse threshold from entropy sample.
        let entropy = thermal.entropy_sample();
        let thr = 64u64.saturating_add(entropy.noise_floor as u64);
        self.collapse_threshold.store(thr, Ordering::Release);

        self.armed.store(true, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);

        // Serial breadcrumb for the mad scientist watching the UART.
        // crate::serial::write_hex_u64(NEXUS_MAGIC);
        // crate::serial::write_str(" quantum_nexus ARMED\n");

        Ok(())
    }

    // ── ENTANGLE ─────────────────────────────────────────────────────────────

    /// Entangle two ouroboros tasks. Returns pair index.
    pub fn entangle(
        &mut self,
        a: TaskId,
        b: TaskId,
        phase_bin: u8,
        initial: Amplitude,
        thermal: &mut ThermalBudget,
    ) -> Result<usize, NexusError> {
        self.require_armed()?;

        let heat = estimate_entangle_heat(initial);
        thermal.charge(heat).map_err(|_| NexusError::ThermalThrottle)?;

        let count = self.pair_count.load(Ordering::Acquire) as usize;
        let slot = self.pairs.iter_mut()
            .position(|p| !p.is_live())
            .ok_or(NexusError::TableFull)?;

        let g = self.generation.fetch_add(1, Ordering::AcqRel);
        self.pairs[slot] = EntangledPair {
            task_a: a,
            task_b: b,
            amplitude: initial,
            phase_bin: phase_bin & (PHASE_BINS as u8 - 1),
            generation: g,
            heat_cost: heat,
            flags: EntangledPair::FLAG_LIVE,
        };

        if slot >= count {
            self.pair_count.store((slot + 1) as u32, Ordering::Release);
        }
        self.stats_entangles.fetch_add(1, Ordering::Relaxed);
        self.last_heat.store(thermal.current_heat(), Ordering::Release);
        Ok(slot)
    }

    // ── TICK (called from chronovore / scheduler heartbeat) ───────────────────

    /// Advance the nexus by one dilated tick.
    /// Returns a PhaseHint for ouroboros (constructive interference schedule).
    pub fn tick(
        &mut self,
        chrono:  &mut TickDevourer,
        thermal: &mut ThermalBudget,
        field:   &mut ResonanceField,
        tartarus: &mut TartarusCage,
        kairos:  &mut KairosWindow,
        ouro:    &mut dyn ExecutorHook,
    ) -> PhaseHint {
        if !self.armed.load(Ordering::Acquire) {
            return PhaseHint::IDLE;
        }

        let tick_now = chrono.now_tick();
        let phase = self.global_phase.fetch_add(1, Ordering::AcqRel);

        // 1. Drain amplitude ring into lattice.
        while let Some((amp, _t, tag)) = self.ring.pop() {
            field.accumulate(tag as usize, amp.re, amp.im);
        }

        // 2. Evolve entangled pairs.
        let thr = self.collapse_threshold.load(Ordering::Acquire);
        let mut best_score: i32 = i32::MIN;
        let mut best_pair: usize = usize::MAX;
        let live = self.pair_count.load(Ordering::Acquire) as usize;

        for i in 0..live.min(MAX_ENTANGLED_PAIRS) {
            if !self.pairs[i].is_live() {
                continue;
            }

            // Phase drift under chronovore dilation.
            let prio = ouro.priority_of(self.pairs[i].task_a).unwrap_or(0);
            let gamma = gamma_for_priority(prio);
            let drift = ((phase.wrapping_mul(gamma as u64)) >> 8) as u8;
            self.pairs[i].phase_bin = self.pairs[i].phase_bin.wrapping_add(drift)
                & (PHASE_BINS as u8 - 1);

            // Rotate amplitude slightly toward lattice eigenphase.
            let eigen = field.eigenphase_bin(self.pairs[i].lattice_hint());
            self.pairs[i].amplitude = self.pairs[i].amplitude
                .rotate_bin(eigen)
                .collapse_if_weak(thr);

            // Decoherence → tartarus.
            if self.pairs[i].amplitude.mag_sq() == 0 {
                self.pairs[i].flags |= EntangledPair::FLAG_COLLAPSING;
                let ev = DecoherenceEvent {
                    task_a: self.pairs[i].task_a,
                    task_b: self.pairs[i].task_b,
                    generation: self.pairs[i].generation,
                };
                tartarus.quarantine(ev, QuarantineLevel::Soft);
                self.pairs[i].flags &= !EntangledPair::FLAG_LIVE;
                self.pairs[i].flags |= EntangledPair::FLAG_QUARANTINE;
                self.stats_collapses.fetch_add(1, Ordering::Relaxed);
                thermal.credit_collapse_rebate(self.pairs[i].heat_cost / 4);
                continue;
            }

            // Interference tournament against neighbours.
            for j in (i + 1)..live.min(MAX_ENTANGLED_PAIRS) {
                if !self.pairs[j].is_live() {
                    continue;
                }
                let score = self.pairs[i].interference(&self.pairs[j]);
                if score > 0 {
                    self.stats_interf_pos.fetch_add(1, Ordering::Relaxed);
                } else {
                    self.stats_interf_neg.fetch_add(1, Ordering::Relaxed);
                }
                if score > best_score {
                    best_score = score;
                    best_pair = i;
                }
            }

            // Kairos: if pair carries FLAG_KAIROS and score is peak, open window.
            if self.pairs[i].flags & EntangledPair::FLAG_KAIROS != 0 {
                kairos.offer(CriticalMoment {
                    task: self.pairs[i].task_a,
                    priority: MomentPriority::Entangled,
                    deadline_tick: tick_now.wrapping_add(1024),
                });
            }
        }

        // 3. Step active experiments.
        self.step_experiments(chrono, thermal, field, tartarus, ouro);

        // 4. Emit phase hint for ouroboros scheduler.
        if best_pair != usize::MAX {
            let p = &self.pairs[best_pair];
            PhaseHint::constructive(p.task_a, p.task_b, p.phase_bin, best_score as u32)
        } else {
            PhaseHint::from_global(phase)
        }
    }

    // ── EXPERIMENTS ──────────────────────────────────────────────────────────

    pub fn start_experiment(
        &mut self,
        kind: ExperimentKind,
        priority: u8,
        phase_seed: u8,
        budget_heat: u32,
        budget_ticks: u64,
        lattice_idx: u16,
        thermal: &mut ThermalBudget,
    ) -> Result<usize, NexusError> {
        self.require_armed()?;
        thermal.charge(budget_heat / 8).map_err(|_| NexusError::ThermalThrottle)?;

        let slot = self.experiments.iter_mut()
            .position(|e| !e.active)
            .ok_or(NexusError::TableFull)?;

        let g = self.generation.fetch_add(1, Ordering::AcqRel);
        self.experiments[slot] = Experiment {
            kind,
            active: true,
            priority,
            phase_seed,
            budget_heat,
            budget_ticks,
            spent_heat: budget_heat / 8,
            spent_ticks: 0,
            result_code: 0,
            lattice_idx,
            generation: g,
        };
        self.stats_experiments.fetch_add(1, Ordering::Relaxed);
        Ok(slot)
    }

    fn step_experiments(
        &mut self,
        chrono:   &mut TickDevourer,
        thermal: &mut ThermalBudget,
        field:   &mut ResonanceField,
        tartarus: &mut TartarusCage,
        ouro:    &mut dyn ExecutorHook,
    ) {
        let tick_now = chrono.now_tick();
        for exp in self.experiments.iter_mut() {
            if !exp.active {
                continue;
            }
            exp.spent_ticks = exp.spent_ticks.wrapping_add(1);

            let heat_step = match exp.kind {
                ExperimentKind::PhaseDrift      => 1,
                ExperimentKind::EntangleBurst   => 8,
                ExperimentKind::ThermalSpike    => 32,
                ExperimentKind::FabricFlood     => 16,
                ExperimentKind::ChronoDilate    => 4,
                ExperimentKind::TartarusInject  => 24,
                ExperimentKind::KairosFocus     => 6,
                ExperimentKind::AetherBroadcast => 12,
                ExperimentKind::Superposition   => 20,
                ExperimentKind::CollapseStress  => 28,
            };

            if thermal.charge(heat_step).is_err() {
                exp.result_code = -1; // thermal abort
                exp.active = false;
                continue;
            }
            exp.spent_heat = exp.spent_heat.saturating_add(heat_step);

            match exp.kind {
                ExperimentKind::PhaseDrift => {
                    field.nudge_phase(exp.lattice_idx as usize, exp.phase_seed);
                    let amp = Amplitude::ONE.rotate_bin(exp.phase_seed);
                    let _ = self.ring.push(amp, tick_now, exp.lattice_idx as u32);
                }
                ExperimentKind::EntangleBurst => {
                    // Request ouroboros to spawn a transient entangled helper.
                    let _ = ouro.request_ephemeral_pair(exp.phase_seed, gamma_for_priority(exp.priority));
                }
                ExperimentKind::ThermalSpike => {
                    thermal.inject_spike(heat_step.saturating_mul(4));
                }
                ExperimentKind::FabricFlood => {
                    // Signal fabric weave to stress-test endpoint queues.
                    // Actual send is capability-gated at the call site.
                    field.mark_stress(exp.lattice_idx as usize);
                }
                ExperimentKind::ChronoDilate => {
                    chrono.request_dilation(exp.priority, gamma_for_priority(exp.priority));
                }
                ExperimentKind::TartarusInject => {
                    tartarus.inject_canary(exp.generation, QuarantineLevel::Probe);
                }
                ExperimentKind::KairosFocus => {
                    // Widen kairos acceptance window for one tick burst.
                    ouro.boost_critical_slice(exp.priority);
                }
                ExperimentKind::AetherBroadcast => {
                    field.broadcast_ambient(exp.phase_seed, exp.lattice_idx as usize);
                }
                ExperimentKind::Superposition => {
                    // Push a balanced superposition into the ring.
                    let a = Amplitude::ONE.rotate_bin(exp.phase_seed);
                    let b = Amplitude::ONE.rotate_bin(exp.phase_seed.wrapping_add(PHASE_BINS as u8 / 4));
                    let superpos = a.add(b);
                    let _ = self.ring.push(superpos, tick_now, exp.lattice_idx as u32);
                }
                ExperimentKind::CollapseStress => {
                    // Temporarily lower collapse threshold to force decoherence.
                    let thr = self.collapse_threshold.load(Ordering::Relaxed);
                    self.collapse_threshold.store(thr.saturating_add(128), Ordering::Relaxed);
                }
            }

            if exp.spent_ticks >= exp.budget_ticks || exp.spent_heat >= exp.budget_heat {
                exp.result_code = 1; // completed
                exp.active = false;
                // Restore collapse threshold if we stressed it.
                if matches!(exp.kind, ExperimentKind::CollapseStress) {
                    let thr = self.collapse_threshold.load(Ordering::Relaxed);
                    self.collapse_threshold.store(thr.saturating_sub(128).max(64), Ordering::Relaxed);
                }
            }
        }
    }

    // ── FABRIC BRIDGE ────────────────────────────────────────────────────────

    /// Encode nexus telemetry into a fabric message for crest/cerebral.
    pub fn telemetry_frame(&self) -> FabricMessage {
        let mut payload = [0u8; 64];
        payload[0..8].copy_from_slice(
            &self.global_phase.load(Ordering::Acquire).to_le_bytes()
        );
        payload[8..12].copy_from_slice(
            &self.pair_count.load(Ordering::Acquire).to_le_bytes()
        );
        payload[12..16].copy_from_slice(
            &self.last_heat.load(Ordering::Acquire).to_le_bytes()
        );
        payload[16..24].copy_from_slice(
            &self.stats_collapses.load(Ordering::Relaxed).to_le_bytes()
        );
        payload[24..32].copy_from_slice(
            &self.stats_entangles.load(Ordering::Relaxed).to_le_bytes()
        );
        payload[32..40].copy_from_slice(
            &self.stats_experiments.load(Ordering::Relaxed).to_le_bytes()
        );
        payload[40..48].copy_from_slice(
            &self.stats_interf_pos.load(Ordering::Relaxed).to_le_bytes()
        );
        payload[48..56].copy_from_slice(
            &self.stats_interf_neg.load(Ordering::Relaxed).to_le_bytes()
        );
        payload[56..60].copy_from_slice(
            &self.generation.load(Ordering::Acquire).to_le_bytes()
        );
        payload[60] = if self.armed.load(Ordering::Acquire) { 1 } else { 0 };
        payload[61] = (self.collapse_threshold.load(Ordering::Acquire) & 0xFF) as u8;

        FabricMessage {
            tag: 0x4E_58_54_4C, // 'NXTL' nexus telemetry
            len: 64,
            payload,
        }
    }

    /// Handle an inbound fabric control frame from crest (experiment requests).
    pub fn handle_fabric_control(
        &mut self,
        msg: &FabricMessage,
        thermal: &mut ThermalBudget,
    ) -> Result<(), NexusError> {
        if msg.tag != 0x4E_58_43_54 {
            // 'NXCT' nexus control
            return Err(NexusError::BadMessage);
        }
        let kind_byte = msg.payload[0];
        let kind = match kind_byte {
            0 => ExperimentKind::PhaseDrift,
            1 => ExperimentKind::EntangleBurst,
            2 => ExperimentKind::ThermalSpike,
            3 => ExperimentKind::FabricFlood,
            4 => ExperimentKind::ChronoDilate,
            5 => ExperimentKind::TartarusInject,
            6 => ExperimentKind::KairosFocus,
            7 => ExperimentKind::AetherBroadcast,
            8 => ExperimentKind::Superposition,
            9 => ExperimentKind::CollapseStress,
            _ => return Err(NexusError::BadMessage),
        };
        let priority    = msg.payload[1];
        let phase_seed  = msg.payload[2];
        let budget_heat = u32::from_le_bytes(msg.payload[4..8].try_into().unwrap_or([0;4]));
        let budget_ticks = u64::from_le_bytes(msg.payload[8..16].try_into().unwrap_or([0;8]));
        let lattice_idx = u16::from_le_bytes(msg.payload[16..18].try_into().unwrap_or([0;2]));

        self.start_experiment(kind, priority, phase_seed, budget_heat, budget_ticks, lattice_idx, thermal)?;
        Ok(())
    }

    // ── MMIO / MIRAGE HOOK ───────────────────────────────────────────────────

    /// Map a black-lab instrument BAR into the nexus observation window.
    pub fn attach_instrument(
        &mut self,
        _dma: &Capability<'_, DmaRight>,
        _dev: &Capability<'_, DeviceMemoryRight>,
        window: &MmioWindow,
        field: &mut ResonanceField,
    ) -> Result<(), NexusError> {
        self.require_armed()?;
        // Read device signature at offset 0.
        let sig = unsafe { window.read_u32(0) };
        if sig & 0xFFFF_0000 != 0x000B_0000 {
            return Err(NexusError::BadInstrument);
        }
        let channels = (sig & 0xFF) as usize;
        field.bind_instrument_channels(window.id(), channels);
        Ok(())
    }

    // ── QUERY ────────────────────────────────────────────────────────────────

    pub fn stats(&self) -> NexusStats {
        NexusStats {
            armed: self.armed.load(Ordering::Acquire),
            generation: self.generation.load(Ordering::Acquire),
            pairs_live: self.pair_count.load(Ordering::Acquire),
            global_phase: self.global_phase.load(Ordering::Acquire),
            collapses: self.stats_collapses.load(Ordering::Relaxed),
            entangles: self.stats_entangles.load(Ordering::Relaxed),
            experiments: self.stats_experiments.load(Ordering::Relaxed),
            interf_pos: self.stats_interf_pos.load(Ordering::Relaxed),
            interf_neg: self.stats_interf_neg.load(Ordering::Relaxed),
            heat: self.last_heat.load(Ordering::Acquire),
            collapse_threshold: self.collapse_threshold.load(Ordering::Acquire),
        }
    }

    fn require_armed(&self) -> Result<(), NexusError> {
        if self.armed.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(NexusError::NotArmed)
        }
    }
}

// ─── HELPERS / TYPES ─────────────────────────────────────────────────────────

fn estimate_entangle_heat(amp: Amplitude) -> u32 {
    // Heat ∝ magnitude — stronger entanglement costs more thermal budget.
    let m = amp.mag_sq();
    4u32.saturating_add((m >> 12) as u32)
}

// Extension trait hooks expected on existing types (thin shims if missing).
trait LatticeHint {
    fn lattice_hint(&self) -> usize;
}
impl LatticeHint for EntangledPair {
    fn lattice_hint(&self) -> usize {
        (self.generation as usize).wrapping_mul(31) & 0xFF
    }
}

#[derive(Clone, Copy, Debug)]
pub enum NexusError {
    NotArmed,
    AlreadyArmed,
    TableFull,
    ThermalThrottle,
    BadMessage,
    BadInstrument,
    PermissionDenied,
}

#[derive(Clone, Copy, Debug)]
pub struct NexusStats {
    pub armed: bool,
    pub generation: u32,
    pub pairs_live: u32,
    pub global_phase: u64,
    pub collapses: u64,
    pub entangles: u64,
    pub experiments: u64,
    pub interf_pos: u64,
    pub interf_neg: u64,
    pub heat: u32,
    pub collapse_threshold: u64,
}

// ─── GLOBAL INSTANCE ─────────────────────────────────────────────────────────

static NEXUS: SpinLock<QuantumNexus> = SpinLock::new(QuantumNexus::new());

pub fn with_nexus<R>(f: impl FnOnce(&mut QuantumNexus) -> R) -> R {
    let mut guard = NEXUS.lock();
    f(&mut *guard)
}

/// Called from the scheduler / chronovore heartbeat path.
pub fn nexus_heartbeat(
    chrono:   &mut TickDevourer,
    thermal:  &mut ThermalBudget,
    field:    &mut ResonanceField,
    tartarus: &mut TartarusCage,
    kairos:   &mut KairosWindow,
    ouro:     &mut dyn ExecutorHook,
) -> PhaseHint {
    with_nexus(|nx| nx.tick(chrono, thermal, field, tartarus, kairos, ouro))
}

// ─── SYSCALL SURFACE (experimental numbers — park in syscalls.rs) ────────────
//
//   SYS_NEXUS_ entangle     = 90
//   SYS_NEXUS_STATS        = 91
//   SYS_NEXUS_EXPERIMENT   = 92
//   SYS_NEXUS_TELEMETRY    = 93
//   SYS_NEXUS_CONTROL      = 94   (fabric-shaped control from userland)

pub mod sys {
    use super::*;

    pub const SYS_NEXUS_ENTANGLE:   usize = 90;
    pub const SYS_NEXUS_STATS:      usize = 91;
    pub const SYS_NEXUS_EXPERIMENT: usize = 92;
    pub const SYS_NEXUS_TELEMETRY:  usize = 93;
    pub const SYS_NEXUS_CONTROL:    usize = 94;

    /// Dispatch helper — call from syscalls.rs match arm.
    pub fn dispatch(
        id: usize,
        a0: usize, a1: usize, a2: usize, a3: usize, a4: usize, _a5: usize,
        thermal: &mut ThermalBudget,
    ) -> Result<usize, NexusError> {
        match id {
            SYS_NEXUS_ENTANGLE => {
                let task_a = TaskId::from_raw(a0 as u64);
                let task_b = TaskId::from_raw(a1 as u64);
                let phase  = a2 as u8;
                let amp    = Amplitude::new(a3 as i32, a4 as i32);
                with_nexus(|nx| nx.entangle(task_a, task_b, phase, amp, thermal)).map(|i| i)
            }
            SYS_NEXUS_STATS => {
                let stats = with_nexus(|nx| nx.stats());
                // Pack a compact status word into the return value.
                let word = (stats.pairs_live as usize)
                    | ((stats.generation as usize) << 16)
                    | ((stats.armed as usize) << 31);
                Ok(word)
            }
            SYS_NEXUS_EXPERIMENT => {
                let kind = match a0 {
                    0 => ExperimentKind::PhaseDrift,
                    1 => ExperimentKind::EntangleBurst,
                    2 => ExperimentKind::ThermalSpike,
                    3 => ExperimentKind::FabricFlood,
                    4 => ExperimentKind::ChronoDilate,
                    5 => ExperimentKind::TartarusInject,
                    6 => ExperimentKind::KairosFocus,
                    7 => ExperimentKind::AetherBroadcast,
                    8 => ExperimentKind::Superposition,
                    9 => ExperimentKind::CollapseStress,
                    _ => return Err(NexusError::BadMessage),
                };
                with_nexus(|nx| {
                    nx.start_experiment(
                        kind,
                        a1 as u8,
                        a2 as u8,
                        a3 as u32,
                        a4 as u64,
                        0,
                        thermal,
                    )
                }).map(|i| i)
            }
            SYS_NEXUS_TELEMETRY => {
                // Userland passes a pointer to a 64-byte buffer in a0.
                let frame = with_nexus(|nx| nx.telemetry_frame());
                let dst = a0 as *mut u8;
                if dst.is_null() {
                    return Err(NexusError::BadMessage);
                }
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        frame.payload.as_ptr(),
                        dst,
                        64,
                    );
                }
                Ok(64)
            }
            SYS_NEXUS_CONTROL => {
                let src = a0 as *const u8;
                if src.is_null() {
                    return Err(NexusError::BadMessage);
                }
                let mut payload = [0u8; 64];
                unsafe {
                    core::ptr::copy_nonoverlapping(src, payload.as_mut_ptr(), 64);
                }
                let msg = FabricMessage {
                    tag: 0x4E_58_43_54,
                    len: 64,
                    payload,
                };
                with_nexus(|nx| nx.handle_fabric_control(&msg, thermal))?;
                Ok(0)
            }
            _ => Err(NexusError::BadMessage),
        }
    }
}

// ─── UNIT SMOKE (host cfg only) ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amplitude_mul_unit() {
        let a = Amplitude::ONE;
        let b = Amplitude::I;
        let c = a.mul(b);
        assert_eq!(c.re, 0);
        assert!(c.im > 0);
    }

    #[test]
    fn amplitude_collapse() {
        let weak = Amplitude::new(1, 1);
        assert_eq!(weak.collapse_if_weak(1000).re, 0);
        let strong = Amplitude::ONE;
        assert!(strong.collapse_if_weak(1).re != 0);
    }

    #[test]
    fn gamma_monotonic() {
        for i in 0..GAMMA_TABLE_LEN - 1 {
            assert!(GAMMA_TABLE[i] <= GAMMA_TABLE[i + 1]);
        }
    }

    #[test]
    fn ring_push_pop() {
        let mut r = AmplitudeRing::new();
        assert!(r.push(Amplitude::ONE, 42, 7));
        let (a, t, tag) = r.pop().unwrap();
        assert_eq!(t, 42);
        assert_eq!(tag, 7);
        assert_eq!(a.re, Amplitude::ONE.re);
    }
}
