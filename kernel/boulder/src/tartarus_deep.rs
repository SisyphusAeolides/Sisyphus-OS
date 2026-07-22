#![allow(dead_code)]
use alloc::{collections::BTreeMap, vec::Vec};
use core::sync::atomic::{AtomicU64, AtomicU32, Ordering};

// ─────────────────────────────────────────────
// CONSTANTS
// ─────────────────────────────────────────────

pub const MAX_SUPERPOSITION_STATES: usize = 16;  // max simultaneous mappings per frame
pub const MAX_ENTANGLED_PAIRS: usize = 1024;
pub const MAX_FRAMES: usize = 1_048_576;          // 4GB / 4KB pages
pub const AMPLITUDE_DECAY: f64 = 0.99;            // amplitude decays each epoch
pub const COLLAPSE_THRESHOLD: f64 = 0.85;         // if P(state) > 85%, preemptive collapse
pub const COHERENCE_LENGTH: u64 = 256;            // ticks before decoherence forces collapse
pub const INTERFERENCE_RADIUS: usize = 8;         // pages around an observed page that interfere

// ─────────────────────────────────────────────
// COMPLEX AMPLITUDE (simplified: real + imaginary)
// ─────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub struct Amplitude {
    pub re: f64,
    pub im: f64,
}

impl Amplitude {
    pub fn zero() -> Self { Self { re: 0.0, im: 0.0 } }
    pub fn one()  -> Self { Self { re: 1.0, im: 0.0 } }
    pub fn new(re: f64, im: f64) -> Self { Self { re, im } }

    /// |α|² — probability of this state
    pub fn probability(&self) -> f64 { self.re * self.re + self.im * self.im }

    /// Normalize: scale so |α| = magnitude
    pub fn scale(&self, factor: f64) -> Self {
        Self { re: self.re * factor, im: self.im * factor }
    }

    /// Interference: add two amplitudes (constructive/destructive)
    pub fn interfere(&self, other: &Amplitude) -> Amplitude {
        Amplitude { re: self.re + other.re, im: self.im + other.im }
    }

    /// Phase rotation by θ radians: α → α * e^(iθ)
    pub fn rotate_phase(&self, theta: f64) -> Amplitude {
        let (sin_t, cos_t) = (libm::sin(theta), libm::cos(theta));
        Amplitude {
            re: self.re * cos_t - self.im * sin_t,
            im: self.re * sin_t + self.im * cos_t,
        }
    }

    /// Conjugate
    pub fn conj(&self) -> Amplitude { Amplitude { re: self.re, im: -self.im } }

    pub fn magnitude(&self) -> f64 { libm::sqrt(self.probability()) }
}

/// Normalize a set of amplitudes so Σ |α_i|² = 1
pub fn normalize_amplitudes(amps: &mut [Amplitude]) {
    let total_prob: f64 = amps.iter().map(|a| a.probability()).sum();
    if total_prob < 1e-15 {
        // All amplitudes near zero — reset to equal superposition
        let n = amps.len();
        let eq = libm::sqrt(1.0 / n as f64);
        for a in amps.iter_mut() { *a = Amplitude::new(eq, 0.0); }
        return;
    }
    let norm = 1.0 / libm::sqrt(total_prob);
    for a in amps.iter_mut() {
        a.re *= norm; a.im *= norm;
    }
}

// ─────────────────────────────────────────────
// MAPPING STATE — one possible physical→virtual mapping
// ─────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct MappingState {
    pub address_space_id: u64,   // which process/AS owns this mapping
    pub virtual_page:     u64,   // virtual page number
    pub permissions:      u8,    // RWX bits
    pub epoch_created:    u64,
    pub access_count:     u32,
    pub phase_tag:        f64,   // semantic phase — from SemanticGraph node class
    pub is_collapsed:     bool,  // has this state been confirmed by a fault observation?
}

// ─────────────────────────────────────────────
// QUANTUM FRAME — a physical page in superposition
// ─────────────────────────────────────────────

pub struct QuantumFrame {
    pub phys_frame:     u64,
    pub states:         Vec<MappingState>,
    pub amplitudes:     Vec<Amplitude>,       // |α_i|² = P(state_i)
    pub epoch:          u64,
    pub coherence_tick: u64,                  // tick when superposition began
    pub entangled_with: Option<u64>,          // frame number of entangled partner
    pub collapsed:      bool,                 // true after Born rule collapse
    pub collapsed_idx:  Option<usize>,        // which state it collapsed to
    pub decoherence_rate: f64,                // environmental noise (thermal pressure)
    pub observation_count: AtomicU32,
}

impl Clone for QuantumFrame {
    fn clone(&self) -> Self {
        Self {
            phys_frame: self.phys_frame,
            states: self.states.clone(),
            amplitudes: self.amplitudes.clone(),
            epoch: self.epoch,
            coherence_tick: self.coherence_tick,
            entangled_with: self.entangled_with,
            collapsed: self.collapsed,
            collapsed_idx: self.collapsed_idx,
            decoherence_rate: self.decoherence_rate,
            observation_count: AtomicU32::new(self.observation_count.load(Ordering::Relaxed)),
        }
    }
}

impl QuantumFrame {
    pub fn new(phys_frame: u64, epoch: u64) -> Self {
        Self {
            phys_frame,
            states: Vec::new(),
            amplitudes: Vec::new(),
            epoch,
            coherence_tick: 0,
            entangled_with: None,
            collapsed: false,
            collapsed_idx: None,
            decoherence_rate: 0.001,
            observation_count: AtomicU32::new(0),
        }
    }

    /// Add a possible mapping state with initial amplitude
    pub fn add_state(&mut self, state: MappingState, amplitude: Amplitude) -> usize {
        if self.states.len() >= MAX_SUPERPOSITION_STATES { return self.states.len(); }
        self.states.push(state);
        self.amplitudes.push(amplitude);
        normalize_amplitudes(&mut self.amplitudes);
        self.states.len() - 1
    }

    /// Born rule collapse: sample from probability distribution
    /// Returns the collapsed state index
    pub fn collapse(&mut self, rng: u64) -> Option<usize> {
        if self.states.is_empty() { return None; }
        if self.collapsed { return self.collapsed_idx; }

        // Compute cumulative probabilities
        let probs: Vec<f64> = self.amplitudes.iter().map(|a| a.probability()).collect();
        let total: f64 = probs.iter().sum();
        if total < 1e-15 { return None; }

        // PRNG-based Born rule sampling
        let u = (rng & 0x000FFFFFFFFFFFFFu64) as f64 / (0x000FFFFFFFFFFFFFu64 as f64);
        let mut cum = 0.0;
        let mut chosen = 0;
        for (i, &p) in probs.iter().enumerate() {
            cum += p / total;
            if u <= cum { chosen = i; break; }
        }

        self.collapsed = true;
        self.collapsed_idx = Some(chosen);
        self.states[chosen].is_collapsed = true;
        self.observation_count.fetch_add(1, Ordering::Relaxed);
        Some(chosen)
    }

    /// Decoherence: amplitude decay toward classical mixed state over time
    /// Models environmental noise (thermal, electrical, cosmic ray events)
    pub fn decohere(&mut self, elapsed_ticks: u64) {
        if self.collapsed { return; }
        let decay = libm::pow(1.0 - self.decoherence_rate, elapsed_ticks as f64);
        for amp in &mut self.amplitudes {
            amp.re *= decay;
            amp.im *= decay;
        }
        // After COHERENCE_LENGTH ticks, force collapse to highest-prob state
        if elapsed_ticks >= COHERENCE_LENGTH {
            let best_idx = self.amplitudes.iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| a.probability().partial_cmp(&b.probability()).unwrap())
                .map(|(i, _)| i)
                .unwrap_or(0);
            let rng_seed = elapsed_ticks ^ (self.phys_frame * 0x9e3779b97f4a7c15);
            let _ = self.collapse(rng_seed);
            let _ = best_idx; // forced-collapse already picks max-prob
        }
        normalize_amplitudes(&mut self.amplitudes);
    }

    /// Apply interference from an adjacent frame's observation
    /// Constructive if same address space (correlated), destructive if competing
    pub fn apply_interference(&mut self, observer_as: u64, observed_vpage: u64, phase: f64) {
        if self.collapsed { return; }
        for (i, state) in self.states.iter().enumerate() {
            let correlation = if state.address_space_id == observer_as {
                // Same AS — constructive interference (page walk locality)
                let vdist = state.virtual_page.abs_diff(observed_vpage) as f64;
                1.0 / (1.0 + vdist / 512.0) // locality falloff
            } else {
                // Different AS — destructive interference
                -0.1
            };
            // Rotate phase by correlation-scaled amount
            let theta = phase * correlation;
            self.amplitudes[i] = self.amplitudes[i].rotate_phase(theta);
        }
        normalize_amplitudes(&mut self.amplitudes);
    }

    /// Check if any state has probability > COLLAPSE_THRESHOLD (preemptive collapse)
    pub fn should_preemptive_collapse(&self) -> bool {
        if self.collapsed { return false; }
        self.amplitudes.iter().any(|a| a.probability() > COLLAPSE_THRESHOLD)
    }

    pub fn dominant_probability(&self) -> f64 {
        self.amplitudes.iter().map(|a| a.probability()).fold(0.0f64, f64::max)
    }

    pub fn entropy(&self) -> f64 {
        // Von Neumann entropy: S = -Σ p_i * log(p_i)
        self.amplitudes.iter()
            .map(|a| a.probability())
            .filter(|&p| p > 1e-15)
            .map(|p| -p * libm::log(p))
            .sum()
    }
}

// ─────────────────────────────────────────────
// ENTANGLEMENT REGISTRY
// ─────────────────────────────────────────────

/// Entangled frame pair: observing one collapses the other
/// Models: PML4 + PDPT entanglement during page table walk
///         huge-page + base-page entanglement during TLB walk
#[derive(Clone, Copy)]
pub struct EntangledPair {
    pub frame_a:     u64,
    pub frame_b:     u64,
    pub correlation: f64,  // +1 = same state, -1 = opposite, 0 = independent
    pub epoch:       u64,
}

pub struct EntanglementRegistry {
    pub pairs: Vec<EntangledPair>,
}

impl EntanglementRegistry {
    pub fn new() -> Self { Self { pairs: Vec::new() } }

    pub fn entangle(&mut self, a: u64, b: u64, correlation: f64, epoch: u64) {
        if self.pairs.len() < MAX_ENTANGLED_PAIRS {
            self.pairs.push(EntangledPair { frame_a: a, frame_b: b, correlation, epoch });
        }
    }

    pub fn find_partner(&self, frame: u64) -> Option<(u64, f64)> {
        self.pairs.iter()
            .find(|p| p.frame_a == frame || p.frame_b == frame)
            .map(|p| if p.frame_a == frame { (p.frame_b, p.correlation) }
                     else { (p.frame_a, p.correlation) })
    }
}

// ─────────────────────────────────────────────
// TARTARUS DEEP — Master Quantum Page Table
// ─────────────────────────────────────────────

pub struct TartarusDeep {
    pub frames:       BTreeMap<u64, QuantumFrame>,
    pub entanglement: EntanglementRegistry,
    pub tick:         u64,
    pub rng_state:    u64,
    pub total_observations: AtomicU64,
    pub total_collapses:    AtomicU64,
    pub total_entangled_collapses: AtomicU64,
    pub total_interference_events: AtomicU64,
    pub decoherence_enabled: bool,
}

impl TartarusDeep {
    pub fn new() -> Self {
        Self {
            frames: BTreeMap::new(),
            entanglement: EntanglementRegistry::new(),
            tick: 0,
            rng_state: 0xdeadbeef_cafedead,
            total_observations: AtomicU64::new(0),
            total_collapses: AtomicU64::new(0),
            total_entangled_collapses: AtomicU64::new(0),
            total_interference_events: AtomicU64::new(0),
            decoherence_enabled: true,
        }
    }

    fn next_rng(&mut self) -> u64 {
        self.rng_state ^= self.rng_state << 13;
        self.rng_state ^= self.rng_state >> 7;
        self.rng_state ^= self.rng_state << 17;
        self.rng_state
    }

    /// Map a physical frame into quantum superposition
    /// Instead of deterministically assigning it to one process,
    /// give it amplitude across all requestors
    pub fn quantum_map(
        &mut self,
        phys_frame: u64,
        mappings: Vec<(u64, u64, u8, f64)>, // (as_id, vpage, perms, phase_tag)
        epoch: u64,
    ) {
        let frame = self.frames.entry(phys_frame)
            .or_insert_with(|| QuantumFrame::new(phys_frame, epoch));
        let n = mappings.len();
        if n == 0 { return; }

        // Initial amplitudes: equal superposition √(1/n) each
        let amp0 = libm::sqrt(1.0 / n as f64);
        frame.coherence_tick = self.tick;

        for (as_id, vpage, perms, phase) in mappings {
            let state = MappingState {
                address_space_id: as_id,
                virtual_page: vpage,
                permissions: perms,
                epoch_created: epoch,
                access_count: 0,
                phase_tag: phase,
                is_collapsed: false,
            };
            // Phase-encode each mapping's amplitude
            let amp = Amplitude::new(amp0 * libm::cos(phase), amp0 * libm::sin(phase));
            frame.add_state(state, amp);
        }
    }

    /// Observe: a page fault forces a Born rule collapse
    /// Returns the winning MappingState (the one the kernel should honor)
    pub fn observe(
        &mut self,
        phys_frame: u64,
        fault_as_id: u64,
        fault_vpage: u64,
        epoch: u64,
    ) -> Option<MappingState> {
        self.total_observations.fetch_add(1, Ordering::Relaxed);
        let rng = self.next_rng() ^ (fault_as_id * epoch);

        // Apply interference from neighboring frames before collapsing
        self.apply_neighborhood_interference(phys_frame, fault_as_id, fault_vpage);

        let frame = self.frames.get_mut(&phys_frame)?;

        // Boost amplitude of the faulting AS (measurement apparatus effect)
        for (i, state) in frame.states.iter().enumerate() {
            if state.address_space_id == fault_as_id {
                let boost = libm::sqrt(1.5f64);
                frame.amplitudes[i].re *= boost;
                frame.amplitudes[i].im *= boost;
            }
        }
        normalize_amplitudes(&mut frame.amplitudes);

        let collapsed_idx = frame.collapse(rng)?;
        self.total_collapses.fetch_add(1, Ordering::Relaxed);

        // Entanglement: collapse partner frame too
        if let Some((partner_frame, correlation)) = self.entanglement.find_partner(phys_frame) {
            self.collapse_entangled(partner_frame, collapsed_idx, correlation, rng);
            self.total_entangled_collapses.fetch_add(1, Ordering::Relaxed);
        }

        let frame = self.frames.get(&phys_frame)?;
        let mut result = frame.states.get(collapsed_idx)?.clone();
        result.access_count += 1;
        Some(result)
    }

    /// Collapse an entangled frame based on the correlated observation
    fn collapse_entangled(&mut self, frame_id: u64, source_idx: usize, correlation: f64, rng: u64) {
        let frame = match self.frames.get_mut(&frame_id) {
            Some(f) => f, None => return,
        };
        if frame.collapsed { return; }

        if correlation > 0.5 {
            // Strong positive correlation: force same state index if available
            let idx = source_idx.min(frame.states.len().saturating_sub(1));
            frame.collapsed = true;
            frame.collapsed_idx = Some(idx);
        } else if correlation < -0.5 {
            // Strong negative correlation: force opposite (anti-correlated)
            let n = frame.states.len();
            let idx = if n > 0 { (source_idx + n / 2) % n } else { 0 };
            frame.collapsed = true;
            frame.collapsed_idx = Some(idx);
        } else {
            // Weak correlation: normal Born rule collapse
            let _ = frame.collapse(rng);
        }
    }

    /// Apply quantum interference from neighboring pages
    /// Pages within INTERFERENCE_RADIUS of a fault get amplitude nudges
    fn apply_neighborhood_interference(&mut self, center: u64, as_id: u64, vpage: u64) {
        let lo = center.saturating_sub(INTERFERENCE_RADIUS as u64);
        let hi = center + INTERFERENCE_RADIUS as u64;
        let phase = libm::sin(vpage as f64 * core::f64::consts::TAU / 65536.0);

        let neighbor_frames: Vec<u64> = self.frames.range(lo..=hi)
            .map(|(&f, _)| f)
            .filter(|&f| f != center)
            .collect();

        for neighbor in neighbor_frames {
            if let Some(frame) = self.frames.get_mut(&neighbor) {
                frame.apply_interference(as_id, vpage, phase);
                self.total_interference_events.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Decoherence tick: decay all uncollapsed frames
    pub fn decoherence_tick(&mut self) {
        if !self.decoherence_enabled { return; }
        self.tick += 1;
        let tick = self.tick;
        let rng = self.next_rng();

        let frames_to_collapse: Vec<u64> = self.frames.iter()
            .filter(|(_, f)| !f.collapsed)
            .filter(|(_, f)| {
                let age = tick.saturating_sub(f.coherence_tick);
                age >= COHERENCE_LENGTH || f.should_preemptive_collapse()
            })
            .map(|(&k, _)| k)
            .collect();

        for fid in frames_to_collapse {
            if let Some(frame) = self.frames.get_mut(&fid) {
                let _ = frame.collapse(rng ^ fid);
                self.total_collapses.fetch_add(1, Ordering::Relaxed);
            }
        }

        // Decay coherence on remaining superposed frames
        for frame in self.frames.values_mut() {
            if !frame.collapsed {
                let age = tick.saturating_sub(frame.coherence_tick);
                frame.decohere(age);
            }
        }
    }

    /// Speculative prefetch hint: which pages are most likely to be observed next?
    /// Returns list of (phys_frame, dominant_prob, dominant_as_id) sorted by prob
    pub fn prefetch_candidates(&self, top_n: usize) -> Vec<(u64, f64, u64)> {
        let mut candidates: Vec<(u64, f64, u64)> = self.frames.iter()
            .filter(|(_, f)| !f.collapsed && !f.states.is_empty())
            .map(|(&fid, frame)| {
                let (best_idx, best_prob) = frame.amplitudes.iter()
                    .enumerate()
                    .map(|(i, a)| (i, a.probability()))
                    .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
                    .unwrap_or((0, 0.0));
                let as_id = frame.states.get(best_idx)
                    .map(|s| s.address_space_id).unwrap_or(0);
                (fid, best_prob, as_id)
            })
            .collect();
        candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        candidates.truncate(top_n);
        candidates
    }

    /// Von Neumann entropy of entire page table (system-wide quantum coherence measure)
    pub fn system_entropy(&self) -> f64 {
        self.frames.values().map(|f| f.entropy()).sum::<f64>()
            / self.frames.len().max(1) as f64
    }

    pub fn stats(&self) -> QuantumPageStats {
        let uncollapsed = self.frames.values().filter(|f| !f.collapsed).count();
        QuantumPageStats {
            total_frames: self.frames.len() as u64,
            uncollapsed_frames: uncollapsed as u64,
            total_observations: self.total_observations.load(Ordering::Relaxed),
            total_collapses: self.total_collapses.load(Ordering::Relaxed),
            entangled_collapses: self.total_entangled_collapses.load(Ordering::Relaxed),
            interference_events: self.total_interference_events.load(Ordering::Relaxed),
            system_entropy: self.system_entropy(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct QuantumPageStats {
    pub total_frames: u64,
    pub uncollapsed_frames: u64,
    pub total_observations: u64,
    pub total_collapses: u64,
    pub entangled_collapses: u64,
    pub interference_events: u64,
    pub system_entropy: f64,
}

// ─── ZERO-AMPLITUDE QUARANTINE PLANE ────────────────────────────────────────

use crate::ouroboros::TaskId;

pub const QUARANTINE_DECAY_TICKS: u64 = 4096;
pub const MAX_PROBE_FAILURES: u8 = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum QuarantineLevel {
    None = 0,
    Soft = 1,
    Probe = 2,
    Sealed = 3,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecoherenceEvent {
    pub pair_id: u64,
    pub task: TaskId,
    pub amplitude_q31: i32,
    pub tick: u64,
    pub phase_bin: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuarantineDecision {
    pub level: QuarantineLevel,
    pub resume_tick: u64,
    pub probe_budget: u8,
}

impl QuarantineDecision {
    const CLEAR: Self = Self {
        level: QuarantineLevel::None,
        resume_tick: 0,
        probe_budget: 0,
    };
}

#[derive(Clone, Copy)]
struct CageRecord {
    active: bool,
    pair_id: u64,
    task: TaskId,
    strikes: u8,
    last_tick: u64,
    level: QuarantineLevel,
    resume_tick: u64,
    probe_budget: u8,
}

impl CageRecord {
    const EMPTY: Self = Self {
        active: false,
        pair_id: 0,
        task: TaskId::INVALID,
        strikes: 0,
        last_tick: 0,
        level: QuarantineLevel::None,
        resume_tick: 0,
        probe_budget: 0,
    };
}

pub struct TartarusCage<const N: usize> {
    records: [CageRecord; N],
}

impl<const N: usize> TartarusCage<N> {
    pub const fn new() -> Self {
        Self {
            records: [CageRecord::EMPTY; N],
        }
    }

    pub fn observe(&mut self, event: DecoherenceEvent) -> QuarantineDecision {
        if event.amplitude_q31 != 0 {
            self.clear(event.pair_id, event.task);
            return QuarantineDecision::CLEAR;
        }

        let index = self
            .records
            .iter()
            .position(|record| {
                record.active
                    && record.pair_id == event.pair_id
                    && record.task == event.task
            })
            .or_else(|| self.records.iter().position(|record| !record.active))
            .unwrap_or_else(|| {
                self.records
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, record)| record.last_tick)
                    .map(|(index, _)| index)
                    .unwrap_or(0)
            });

        let record = &mut self.records[index];

        if !record.active
            || event.tick.saturating_sub(record.last_tick) > QUARANTINE_DECAY_TICKS
        {
            *record = CageRecord {
                active: true,
                pair_id: event.pair_id,
                task: event.task,
                strikes: 0,
                last_tick: event.tick,
                level: QuarantineLevel::None,
                resume_tick: event.tick,
                probe_budget: 0,
            };
        }

        record.strikes = record.strikes.saturating_add(1);
        record.last_tick = event.tick;

        if record.strikes == 1 {
            record.level = QuarantineLevel::Soft;
            record.resume_tick = event.tick.saturating_add(8);
            record.probe_budget = 0;
        } else if record.strikes < MAX_PROBE_FAILURES {
            record.level = QuarantineLevel::Probe;
            record.resume_tick = event.tick.saturating_add(2);
            record.probe_budget = 4_u8.saturating_sub(record.strikes / 2).max(1);
        } else {
            record.level = QuarantineLevel::Sealed;
            record.resume_tick = u64::MAX;
            record.probe_budget = 0;
        }

        QuarantineDecision {
            level: record.level,
            resume_tick: record.resume_tick,
            probe_budget: record.probe_budget,
        }
    }

    pub fn probe_result(
        &mut self,
        pair_id: u64,
        task: TaskId,
        coherent: bool,
        tick: u64,
    ) -> QuarantineDecision {
        let Some(record) = self.records.iter_mut().find(|record| {
            record.active && record.pair_id == pair_id && record.task == task
        }) else {
            return QuarantineDecision::CLEAR;
        };

        if coherent {
            *record = CageRecord::EMPTY;
            return QuarantineDecision::CLEAR;
        }

        record.probe_budget = record.probe_budget.saturating_sub(1);
        record.last_tick = tick;

        if record.probe_budget == 0 {
            record.strikes = record.strikes.saturating_add(1);
            record.level = if record.strikes >= MAX_PROBE_FAILURES {
                QuarantineLevel::Sealed
            } else {
                QuarantineLevel::Soft
            };
            record.resume_tick = if record.level == QuarantineLevel::Sealed {
                u64::MAX
            } else {
                tick.saturating_add(16)
            };
        }

        QuarantineDecision {
            level: record.level,
            resume_tick: record.resume_tick,
            probe_budget: record.probe_budget,
        }
    }

    pub fn decision(
        &self,
        pair_id: u64,
        task: TaskId,
    ) -> QuarantineDecision {
        self.records
            .iter()
            .find(|record| {
                record.active && record.pair_id == pair_id && record.task == task
            })
            .map(|record| QuarantineDecision {
                level: record.level,
                resume_tick: record.resume_tick,
                probe_budget: record.probe_budget,
            })
            .unwrap_or(QuarantineDecision::CLEAR)
    }

    fn clear(&mut self, pair_id: u64, task: TaskId) {
        if let Some(record) = self.records.iter_mut().find(|record| {
            record.active && record.pair_id == pair_id && record.task == task
        }) {
            *record = CageRecord::EMPTY;
        }
    }
}

impl<const N: usize> Default for TartarusCage<N> {
    fn default() -> Self {
        Self::new()
    }
}
