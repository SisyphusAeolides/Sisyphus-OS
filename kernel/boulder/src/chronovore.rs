// kernel/boulder/src/chronovore.rs
// #![no_std] inherited
//
// CHRONOVORE — Time-Eating Entropy Engine
//
// Jitter harvesting:  Δt_raw = RDTSC[i] − RDTSC[i-1] (pipeline variance)
// Entropy extraction: LSBs of Δt carry ~1–2 bits of true entropy each sample
// Distillation:       Rule 30 Cellular Automaton (Wolfram, 1983)
//                     State is 256-bit wide; each step: s[i] = s[i-1] XOR (s[i] OR s[i+1])
//                     After 64 steps, output 64 bits of CSPRNG output
// Time crystal:       FFT-free autocorrelation of jitter sequence → find dominant period
//                     via sliding window sum of products (integer autocorrelation)
//                     If AC(τ) > CRYSTAL_THRESHOLD for any τ ∈ [τ_min, τ_max]:
//                       a time crystal is detected at period τ
// Predictive window:  use crystal period to predict next low-jitter scheduling moment
//                     (CPU is in a "quiet" microarch state → optimal preemption point)

extern crate alloc;
use core::sync::atomic::{AtomicU64, Ordering};

// ─────────────────────────────────────────────
// CONSTANTS
// ─────────────────────────────────────────────

pub const JITTER_RING_LEN: usize = 1024; // jitter samples ring buffer
pub const CA_WIDTH_BITS: usize = 256; // Rule 30 CA width in bits
pub const CA_WIDTH_U64: usize = CA_WIDTH_BITS / 64; // = 4 u64 words
pub const CA_STEPS_PER_OUTPUT: usize = 64; // CA steps before outputting entropy
pub const CRYSTAL_TAU_MIN: usize = 8; // min period to search (samples)
pub const CRYSTAL_TAU_MAX: usize = 256; // max period
pub const CRYSTAL_THRESHOLD_FP: u64 = 0x0001_0000 * 3 / 4; // 0.75 correlation in 16.16
pub const PREDICT_LOOKAHEAD: usize = 4; // predict N windows ahead
pub const ENTROPY_POOL_WORDS: usize = 64; // 512-byte entropy pool
pub const RESEED_THRESHOLD: u64 = 256; // reseed CSPRNG every N jitter samples

// ─────────────────────────────────────────────
// RULE 30 CELLULAR AUTOMATON
// ─────────────────────────────────────────────
// Rule 30: output[i] = input[i-1] XOR (input[i] OR input[i+1])
// Applied to 256-bit wide state stored as 4 × u64

pub struct Rule30 {
    pub state: [u64; CA_WIDTH_U64],
    pub steps: u64,
}

impl Rule30 {
    pub const fn new() -> Self {
        Self {
            state: [
                0x9e3779b97f4a7c15,
                0x6c62272e07bb0142,
                0x62b821756295c58d,
                0x0u64,
            ],
            steps: 0,
        }
    }

    pub fn seed(&mut self, entropy: &[u64]) {
        for (i, &e) in entropy.iter().enumerate() {
            self.state[i % CA_WIDTH_U64] ^= e;
        }
    }

    /// One step of Rule 30 across the 256-bit state (bit-parallel)
    pub fn step(&mut self) {
        let mut next = [0u64; CA_WIDTH_U64];
        for w in 0..CA_WIDTH_U64 {
            let left = if w == 0 {
                (self.state[CA_WIDTH_U64 - 1] << 63) | (self.state[w] >> 1)
            } else {
                (self.state[w - 1] << 63) | (self.state[w] >> 1)
            };
            let right = if w == CA_WIDTH_U64 - 1 {
                (self.state[w] << 1) | (self.state[0] >> 63)
            } else {
                (self.state[w] << 1) | (self.state[w + 1] >> 63)
            };
            let center = self.state[w];
            // Rule 30: new[i] = left[i] XOR (center[i] OR right[i])
            next[w] = left ^ (center | right);
        }
        self.state = next;
        self.steps += 1;
    }

    /// Run CA_STEPS_PER_OUTPUT steps and return center 64 bits as entropy
    pub fn generate(&mut self) -> u64 {
        for _ in 0..CA_STEPS_PER_OUTPUT {
            self.step();
        }
        self.state[CA_WIDTH_U64 / 2] // center column = highest entropy
    }
}

// ─────────────────────────────────────────────
// JITTER HARVESTER
// ─────────────────────────────────────────────

pub struct JitterHarvester {
    pub ring: [u64; JITTER_RING_LEN], // raw Δt samples (ns)
    pub ring_idx: usize,
    pub count: u64,
    pub last_tsc: u64,
    pub min_jitter: u64,
    pub max_jitter: u64,
    pub sum_jitter: u64,
    pub entropy_bits: u64, // estimated bits harvested
}

impl JitterHarvester {
    pub const fn new() -> Self {
        Self {
            ring: [0u64; JITTER_RING_LEN],
            ring_idx: 0,
            count: 0,
            last_tsc: 0,
            min_jitter: u64::MAX,
            max_jitter: 0,
            sum_jitter: 0,
            entropy_bits: 0,
        }
    }

    /// Ingest a raw TSC reading and extract jitter
    pub fn ingest(&mut self, tsc: u64) -> u64 {
        let delta = tsc.saturating_sub(self.last_tsc);
        self.last_tsc = tsc;
        if delta == 0 {
            return 0;
        }
        let idx = self.ring_idx % JITTER_RING_LEN;
        self.ring[idx] = delta;
        self.ring_idx = self.ring_idx.wrapping_add(1);
        self.count += 1;
        // Stats
        if delta < self.min_jitter {
            self.min_jitter = delta;
        }
        if delta > self.max_jitter {
            self.max_jitter = delta;
        }
        self.sum_jitter += delta;
        // Entropy estimate: 1 bit per sample from LSBs (conservative)
        self.entropy_bits += 1;
        delta
    }

    /// Extract raw entropy from jitter LSBs (von Neumann debiasing)
    /// Returns a u64 composed of debiased LSBs from recent samples
    pub fn extract_entropy(&self) -> u64 {
        let mut bits = 0u64;
        let mut bit_count = 0;
        let start = self.ring_idx.wrapping_sub(128) % JITTER_RING_LEN;
        let mut i = 0usize;
        while bit_count < 64 && i + 1 < 128 {
            let a = self.ring[(start + i) % JITTER_RING_LEN];
            let b = self.ring[(start + i + 1) % JITTER_RING_LEN];
            // Von Neumann: if bits differ, output the first
            let bit_a = a & 1;
            let bit_b = b & 1;
            if bit_a != bit_b && bit_count < 64 {
                bits |= bit_a << bit_count;
                bit_count += 1;
            }
            i += 2;
        }
        bits
    }

    pub fn mean_jitter(&self) -> u64 {
        if self.count == 0 {
            return 0;
        }
        self.sum_jitter / self.count
    }
}

// ─────────────────────────────────────────────
// TIME CRYSTAL DETECTOR
// ─────────────────────────────────────────────
// Autocorrelation: AC(τ) = Σ_{i} jitter[i] * jitter[i+τ]
// Normalized:      r(τ) = AC(τ) / AC(0)
// Crystal period:  first τ where r(τ) > CRYSTAL_THRESHOLD

pub struct TimeCrystal {
    pub detected: bool,
    pub period: usize,                          // dominant period in samples
    pub strength_fp: u64,                       // normalized correlation (16.16 fp)
    pub phase: usize,                           // current phase within period
    pub predict_next: [u64; PREDICT_LOOKAHEAD], // predicted quiet-window TSC values
}

impl TimeCrystal {
    pub const fn new() -> Self {
        Self {
            detected: false,
            period: 0,
            strength_fp: 0,
            phase: 0,
            predict_next: [0u64; PREDICT_LOOKAHEAD],
        }
    }

    /// Scan jitter ring for autocorrelation peaks — integer only
    pub fn scan(&mut self, ring: &[u64; JITTER_RING_LEN], ring_idx: usize, count: u64) {
        if count < CRYSTAL_TAU_MAX as u64 * 2 {
            return;
        }

        // AC(0): sum of squares (normalization factor)
        let mut ac0: u64 = 0;
        for i in 0..CRYSTAL_TAU_MAX {
            let v = ring[(ring_idx.wrapping_sub(i).wrapping_sub(1)) % JITTER_RING_LEN];
            ac0 = ac0.saturating_add(v.saturating_mul(v) >> 16);
        }
        if ac0 == 0 {
            return;
        }

        let mut best_tau = 0usize;
        let mut best_ac = 0u64;

        for tau in CRYSTAL_TAU_MIN..CRYSTAL_TAU_MAX {
            let mut ac: u64 = 0;
            for i in 0..CRYSTAL_TAU_MAX {
                let j = ring_idx.wrapping_sub(i).wrapping_sub(1) % JITTER_RING_LEN;
                let k = ring_idx.wrapping_sub(i + tau).wrapping_sub(1) % JITTER_RING_LEN;
                let vj = ring[j];
                let vk = ring[k];
                ac = ac.saturating_add(vj.saturating_mul(vk) >> 16);
            }
            // Normalized correlation in 16.16 fp
            let r_fp = (ac << 16) / ac0.max(1);
            if r_fp > CRYSTAL_THRESHOLD_FP && ac > best_ac {
                best_ac = ac;
                best_tau = tau;
                self.strength_fp = r_fp;
            }
        }

        if best_tau > 0 {
            self.detected = true;
            self.period = best_tau;
        }
    }

    /// Predict the next N quiet scheduling windows based on crystal period
    /// quiet_tsc = last_min_jitter_tsc + k * period_tsc
    pub fn predict_windows(&mut self, last_tsc: u64, tsc_per_sample: u64) {
        if !self.detected || self.period == 0 {
            return;
        }
        let period_tsc = self.period as u64 * tsc_per_sample;
        for k in 0..PREDICT_LOOKAHEAD {
            self.predict_next[k] = last_tsc + (k as u64 + 1) * period_tsc;
        }
    }
}

// ─────────────────────────────────────────────
// ENTROPY POOL
// ─────────────────────────────────────────────

pub struct EntropyPool {
    pub pool: [u64; ENTROPY_POOL_WORDS],
    pub write_idx: usize,
    pub read_idx: usize,
    pub available: AtomicU64,
}

impl EntropyPool {
    pub const fn new() -> Self {
        Self {
            pool: [0u64; ENTROPY_POOL_WORDS],
            write_idx: 0,
            read_idx: 0,
            available: AtomicU64::new(0),
        }
    }

    pub fn add(&mut self, word: u64) {
        self.pool[self.write_idx % ENTROPY_POOL_WORDS] ^= word;
        self.write_idx = self.write_idx.wrapping_add(1);
        self.available.fetch_add(64, Ordering::Relaxed);
    }

    pub fn take(&mut self) -> Option<u64> {
        if self.available.load(Ordering::Relaxed) < 64 {
            return None;
        }
        let word = self.pool[self.read_idx % ENTROPY_POOL_WORDS];
        self.read_idx = self.read_idx.wrapping_add(1);
        self.available.fetch_sub(64, Ordering::Relaxed);
        Some(word)
    }
}

// ─────────────────────────────────────────────
// CHRONOVORE ENGINE
// ─────────────────────────────────────────────

pub struct Chronovore {
    pub harvester: JitterHarvester,
    pub ca: Rule30,
    pub crystal: TimeCrystal,
    pub pool: EntropyPool,
    pub tsc_per_ns: u64, // calibrated TSC frequency (ticks per nanosecond)
    pub total_ingested: AtomicU64,
    pub total_generated: AtomicU64,
    pub reseed_count: AtomicU64,
    pub initialized: bool,
}

impl Chronovore {
    pub const fn new() -> Self {
        Self {
            harvester: JitterHarvester::new(),
            ca: Rule30::new(),
            crystal: TimeCrystal::new(),
            pool: EntropyPool::new(),
            tsc_per_ns: 3, // default: 3 GHz
            total_ingested: AtomicU64::new(0),
            total_generated: AtomicU64::new(0),
            reseed_count: AtomicU64::new(0),
            initialized: false,
        }
    }

    pub fn init(&mut self, tsc_per_ns: u64) {
        self.tsc_per_ns = tsc_per_ns.max(1);
        self.initialized = true;
    }

    /// Feed a raw TSC sample into the engine
    pub fn feed(&mut self, tsc: u64) {
        let jitter = self.harvester.ingest(tsc);
        self.total_ingested.fetch_add(1, Ordering::Relaxed);

        // Reseed CA with new entropy periodically
        if self.harvester.count % RESEED_THRESHOLD == 0 {
            let raw_entropy = self.harvester.extract_entropy();
            self.ca
                .seed(&[raw_entropy, tsc ^ jitter, self.harvester.count]);
            self.reseed_count.fetch_add(1, Ordering::Relaxed);
        }

        // Generate entropy word and add to pool
        if self.harvester.count % 16 == 0 {
            let entropy_word = self.ca.generate();
            self.pool.add(entropy_word);
            self.total_generated.fetch_add(64, Ordering::Relaxed);
        }

        // Crystal detection every 512 samples
        if self.harvester.count % 512 == 0 {
            self.crystal.scan(
                &self.harvester.ring,
                self.harvester.ring_idx,
                self.harvester.count,
            );
            if self.crystal.detected {
                self.crystal.predict_windows(tsc, self.tsc_per_ns);
            }
        }
    }

    /// Generate a random u64 from the entropy pool (CSPRNG output)
    pub fn gen_u64(&mut self) -> u64 {
        match self.pool.take() {
            Some(w) => w,
            None => {
                // Pool empty: run CA directly (still good, just uses older seed)
                self.ca.generate()
            }
        }
    }

    /// Is a time crystal currently detected?
    pub fn crystal_active(&self) -> bool {
        self.crystal.detected
    }

    /// Next predicted quiet scheduling window (TSC value)
    pub fn next_quiet_window(&self) -> Option<u64> {
        if !self.crystal.detected {
            return None;
        }
        Some(self.crystal.predict_next[0])
    }

    pub fn stats(&self) -> ChronovoreStats {
        ChronovoreStats {
            ingested: self.total_ingested.load(Ordering::Relaxed),
            generated: self.total_generated.load(Ordering::Relaxed),
            reseeds: self.reseed_count.load(Ordering::Relaxed),
            pool_available: self.pool.available.load(Ordering::Relaxed),
            crystal_detected: self.crystal.detected,
            crystal_period: self.crystal.period as u64,
            crystal_strength_fp: self.crystal.strength_fp,
            mean_jitter_tsc: self.harvester.mean_jitter(),
            ca_steps: self.ca.steps,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ChronovoreStats {
    pub ingested: u64,
    pub generated: u64,
    pub reseeds: u64,
    pub pool_available: u64,
    pub crystal_detected: bool,
    pub crystal_period: u64,
    pub crystal_strength_fp: u64,
    pub mean_jitter_tsc: u64,
    pub ca_steps: u64,
}

// ─── PRIORITY-MASS TICK DILATION ────────────────────────────────────────────

const Q16_ONE: u64 = 1 << 16;

// Lorentz-style γ values in Q16.16.
// Mass is mapped into sixteen monotonic bands.
const GAMMA_Q16: [u64; 16] = [
    65_536, 66_560, 67_584, 69_632, 71_680, 73_728, 77_824, 81_920, 86_016, 90_112, 98_304,
    106_496, 114_688, 122_880, 131_072, 147_456,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
#[repr(transparent)]
pub struct ChronoTick(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct DilatedDuration(pub u64);

impl DilatedDuration {
    pub const ZERO: Self = Self(0);

    pub const fn ticks(self) -> u64 {
        self.0
    }
}

#[derive(Clone)]
pub struct TickDevourer {
    wall_origin: ChronoTick,
    logical_origin: ChronoTick,
    priority_mass: u16,
}

impl TickDevourer {
    pub const fn new(wall_origin: ChronoTick, logical_origin: ChronoTick) -> Self {
        Self {
            wall_origin,
            logical_origin,
            priority_mass: 0,
        }
    }

    pub const fn priority_mass(&self) -> u16 {
        self.priority_mass
    }

    pub fn set_priority_mass(&mut self, mass: u16, wall_now: ChronoTick) {
        // Rebase first so changing mass never makes logical time jump backward.
        let logical_now = self.now_tick(wall_now);
        self.wall_origin = wall_now;
        self.logical_origin = logical_now;
        self.priority_mass = mass;
    }

    pub fn now_tick(&self, wall_now: ChronoTick) -> ChronoTick {
        let wall_delta = wall_now.0.saturating_sub(self.wall_origin.0);
        let logical_delta = dilate_ticks(wall_delta, self.priority_mass);
        ChronoTick(self.logical_origin.0.saturating_add(logical_delta))
    }

    pub fn dilate(&self, wall_duration: DilatedDuration) -> DilatedDuration {
        DilatedDuration(dilate_ticks(wall_duration.0, self.priority_mass))
    }
}

#[inline(always)]
pub fn gamma_q16(priority_mass: u16) -> u64 {
    GAMMA_Q16[usize::from(priority_mass >> 12)]
}

#[inline(always)]
fn dilate_ticks(wall_ticks: u64, priority_mass: u16) -> u64 {
    let gamma = gamma_q16(priority_mass).max(Q16_ONE);
    let numerator = (wall_ticks as u128) << 16;
    (numerator / gamma as u128).min(u64::MAX as u128) as u64
}
