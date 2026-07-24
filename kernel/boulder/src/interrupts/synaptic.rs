extern crate alloc;
use alloc::vec::Vec;

use core::sync::atomic::{AtomicU64, Ordering};

// ─────────────────────────────────────────────
// CONSTANTS
// ─────────────────────────────────────────────

pub const MAX_VECTORS: usize = 256;
pub const MAX_SYNAPSES: usize = 1024; // total weighted connections
pub const HEBBIAN_ETA_FP: u32 = 0x0000_0666; // η ≈ 0.025 in 16.16 fp
pub const LTP_WINDOW_TICKS: u64 = 200;
pub const LTD_WINDOW_TICKS: u64 = 2000;
pub const PRUNE_THRESHOLD_FP: u32 = 0x0000_0100; // weight < 0.004 → prune
pub const STDP_PRE_WINDOW_NS: u64 = 5_000_000; // 5ms STDP causal window
pub const FIRE_HISTORY_LEN: usize = 64; // ring buffer of recent spikes
pub const CORTEX_ROWS: usize = 8;
pub const CORTEX_COLS: usize = 8; // 8×8 = 64 core cortical map
pub const REFRACTORY_TICKS: u64 = 4; // ticks before neuron can re-fire

// ─────────────────────────────────────────────
// NEURON — One IRQ Vector
// ─────────────────────────────────────────────

pub struct Neuron {
    pub vector: u8,
    pub membrane_pot_fp: i32,  // membrane potential (16.16 fp, resting = 0)
    pub threshold_fp: i32,     // fire threshold (16.16 fp)
    pub resting_pot_fp: i32,   // resting potential (negative)
    pub refractory_until: u64, // tick until neuron can fire again
    pub fire_count: AtomicU64,
    pub last_fire_tick: u64,
    pub last_fire_ns: u64,
    pub fire_history: [u64; FIRE_HISTORY_LEN], // ring of recent fire timestamps (ns)
    pub fire_hist_idx: usize,
    pub cortex_row: u8, // position in cortical map
    pub cortex_col: u8,
    pub assigned_core: u8,  // which CPU core handles this IRQ
    pub is_active: bool,    // registered / unregistered
    pub decay_rate_fp: u32, // membrane decay per tick (16.16 fp)
}

impl Clone for Neuron {
    fn clone(&self) -> Self {
        Self {
            vector: self.vector,
            membrane_pot_fp: self.membrane_pot_fp,
            threshold_fp: self.threshold_fp,
            resting_pot_fp: self.resting_pot_fp,
            refractory_until: self.refractory_until,
            fire_count: AtomicU64::new(self.fire_count.load(Ordering::Relaxed)),
            last_fire_tick: self.last_fire_tick,
            last_fire_ns: self.last_fire_ns,
            fire_history: self.fire_history,
            fire_hist_idx: self.fire_hist_idx,
            cortex_row: self.cortex_row,
            cortex_col: self.cortex_col,
            assigned_core: self.assigned_core,
            is_active: self.is_active,
            decay_rate_fp: self.decay_rate_fp,
        }
    }
}

impl Neuron {
    pub fn new(vector: u8) -> Self {
        Self {
            vector,
            membrane_pot_fp: 0,
            threshold_fp: 0x0000_8000,    // 0.5 in 16.16
            resting_pot_fp: -0x0000_1000, // -0.0625
            refractory_until: 0,
            fire_count: AtomicU64::new(0),
            last_fire_tick: 0,
            last_fire_ns: 0,
            fire_history: [0u64; FIRE_HISTORY_LEN],
            fire_hist_idx: 0,
            cortex_row: (vector / CORTEX_COLS as u8) % CORTEX_ROWS as u8,
            cortex_col: vector % CORTEX_COLS as u8,
            assigned_core: 0,
            is_active: false,
            decay_rate_fp: 0x0000_0CCC, // ≈ 0.05 decay per tick
        }
    }

    /// Receive input current and potentially fire
    pub fn stimulate(&mut self, current_fp: i32, tick: u64, now_ns: u64) -> bool {
        if tick < self.refractory_until {
            return false;
        }
        self.membrane_pot_fp += current_fp;
        // Decay toward resting potential
        let delta = self.membrane_pot_fp - self.resting_pot_fp;
        let decay = ((delta.unsigned_abs() as u64 * self.decay_rate_fp as u64) >> 16) as i32;
        if self.membrane_pot_fp > self.resting_pot_fp {
            self.membrane_pot_fp -= decay;
        }
        // Check threshold
        if self.membrane_pot_fp >= self.threshold_fp {
            self.fire(tick, now_ns);
            return true;
        }
        false
    }

    fn fire(&mut self, tick: u64, now_ns: u64) {
        self.membrane_pot_fp = self.resting_pot_fp; // reset after spike
        self.refractory_until = tick + REFRACTORY_TICKS;
        self.last_fire_tick = tick;
        self.last_fire_ns = now_ns;
        self.fire_count.fetch_add(1, Ordering::Relaxed);
        // Record in ring buffer
        self.fire_history[self.fire_hist_idx] = now_ns;
        self.fire_hist_idx = (self.fire_hist_idx + 1) % FIRE_HISTORY_LEN;
    }

    /// Did this neuron fire recently within `window_ns`?
    pub fn fired_recently(&self, window_ns: u64, now_ns: u64) -> bool {
        now_ns.saturating_sub(self.last_fire_ns) <= window_ns
    }

    /// Inter-spike interval (mean, in ns) — measures burst vs tonic firing
    pub fn mean_isi_ns(&self) -> u64 {
        let mut intervals = 0u64;
        let mut count = 0u32;
        for i in 1..FIRE_HISTORY_LEN {
            let a = self.fire_history[(self.fire_hist_idx + i - 1) % FIRE_HISTORY_LEN];
            let b = self.fire_history[(self.fire_hist_idx + i) % FIRE_HISTORY_LEN];
            if a > 0 && b > 0 && b > a {
                intervals += b - a;
                count += 1;
            }
        }
        if count == 0 {
            u64::MAX
        } else {
            intervals / count as u64
        }
    }
}

// ─────────────────────────────────────────────
// SYNAPSE — Weighted Connection Between Two IRQ Neurons
// ─────────────────────────────────────────────

#[derive(Clone, Copy)]
pub struct Synapse {
    pub pre_vector: u8,
    pub post_vector: u8,
    pub weight_fp: u32, // 16.16 fp — synaptic strength
    pub last_ltp_tick: u64,
    pub last_ltd_tick: u64,
    pub co_fire_count: u32,
    pub is_excitatory: bool, // true = excitatory, false = inhibitory
    pub stdp_score_fp: i32,  // STDP running score (positive = potentiation)
}

impl Synapse {
    pub fn new(pre: u8, post: u8) -> Self {
        Self {
            pre_vector: pre,
            post_vector: post,
            weight_fp: 0x0000_1000, // 0.0625 initial weight
            last_ltp_tick: 0,
            last_ltd_tick: 0,
            co_fire_count: 0,
            is_excitatory: true,
            stdp_score_fp: 0,
        }
    }

    /// Hebbian update: called when both pre and post neurons are active
    pub fn hebbian_update(&mut self, pre_active: bool, post_active: bool, tick: u64) {
        if pre_active && post_active {
            // LTP: strengthen
            let delta = ((self.weight_fp as u64 * HEBBIAN_ETA_FP as u64) >> 16) as u32;
            self.weight_fp = self.weight_fp.saturating_add(delta);
            self.last_ltp_tick = tick;
            self.co_fire_count += 1;
        } else if tick - self.last_ltp_tick > LTD_WINDOW_TICKS {
            // LTD: weaken if no co-firing for a long time
            let delta = ((self.weight_fp as u64 * HEBBIAN_ETA_FP as u64) >> 16) as u32;
            self.weight_fp = self.weight_fp.saturating_sub(delta);
            self.last_ltd_tick = tick;
        }
    }

    /// STDP update: called with precise timestamps
    /// Pre fires before post → potentiate (A+)
    /// Post fires before pre → depress (A-)
    pub fn stdp_update(&mut self, pre_ns: u64, post_ns: u64) {
        if pre_ns == 0 || post_ns == 0 {
            return;
        }
        if post_ns > pre_ns {
            let dt = post_ns - pre_ns;
            if dt <= STDP_PRE_WINDOW_NS {
                // Causal: pre before post → LTP
                // A+ * exp(-dt / τ+)  — simplified: linear falloff
                let strength =
                    (0x0001_0000u64.saturating_sub(dt * 0x0001_0000 / STDP_PRE_WINDOW_NS)) as i32;
                self.stdp_score_fp = self.stdp_score_fp.saturating_add(strength);
                let ltp = ((self.weight_fp as u64 * strength.unsigned_abs() as u64) >> 20) as u32;
                self.weight_fp = self.weight_fp.saturating_add(ltp);
            }
        } else {
            let dt = pre_ns - post_ns;
            if dt <= STDP_PRE_WINDOW_NS {
                // Anti-causal: post before pre → LTD
                let strength =
                    (0x0001_0000u64.saturating_sub(dt * 0x0001_0000 / STDP_PRE_WINDOW_NS)) as i32;
                self.stdp_score_fp = self.stdp_score_fp.saturating_sub(strength);
                let ltd = ((self.weight_fp as u64 * strength.unsigned_abs() as u64) >> 20) as u32;
                self.weight_fp = self.weight_fp.saturating_sub(ltd);
            }
        }
    }

    pub fn should_prune(&self) -> bool {
        self.weight_fp < PRUNE_THRESHOLD_FP
    }
}

// ─────────────────────────────────────────────
// CORTICAL MAP — 2D Topographic Grid of Cores
// ─────────────────────────────────────────────

pub struct CorticalMap {
    // cortex[row][col] = assigned CPU core
    pub grid: [[u8; CORTEX_COLS]; CORTEX_ROWS],
    pub core_load: [u32; 64], // IRQs assigned per core
    pub num_cores: usize,
}

impl CorticalMap {
    pub const fn new() -> Self {
        Self {
            grid: [[0u8; CORTEX_COLS]; CORTEX_ROWS],
            core_load: [0u32; 64],
            num_cores: 1,
        }
    }

    pub fn init(&mut self, num_cores: usize) {
        self.num_cores = num_cores.max(1);
        // Initialize: assign each cortical tile to core using space-filling
        // Adjacent tiles → adjacent cores (topographic locality)
        for row in 0..CORTEX_ROWS {
            for col in 0..CORTEX_COLS {
                self.grid[row][col] = ((row * CORTEX_COLS + col) % self.num_cores) as u8;
            }
        }
    }

    pub fn assign_core(&self, row: u8, col: u8) -> u8 {
        self.grid[row as usize % CORTEX_ROWS][col as usize % CORTEX_COLS]
    }

    /// Remap two strongly-connected neurons to adjacent cortical positions
    pub fn co_locate(&mut self, v_a: u8, v_b: u8) {
        let row_a = (v_a as usize / CORTEX_COLS) % CORTEX_ROWS;
        let col_a = v_a as usize % CORTEX_COLS;
        let row_b = (v_b as usize / CORTEX_COLS) % CORTEX_ROWS;
        let col_b = v_b as usize % CORTEX_COLS;
        // If not already adjacent, swap b to be neighbor of a
        let dist = row_a.abs_diff(row_b) + col_a.abs_diff(col_b);
        if dist > 2 {
            // Move b's core to be same as a's adjacent core
            let target_core = self.grid[row_a][col_a];
            self.grid[row_b][col_b] = target_core;
        }
    }
}

// ─────────────────────────────────────────────
// SYNAPTIC CORTEX — Master IRQ Brain
// ─────────────────────────────────────────────

pub struct SynapticCortex {
    pub neurons: [Neuron; MAX_VECTORS],
    pub synapses: Vec<Synapse>,
    pub cortex: CorticalMap,
    pub tick: u64,
    pub total_spikes: AtomicU64,
    pub total_pruned: AtomicU64,
    pub total_ltp: AtomicU64,
    pub total_colocate: AtomicU64,
}

impl SynapticCortex {
    pub fn new() -> Self {
        Self {
            neurons: core::array::from_fn(|i| Neuron::new(i as u8)),
            synapses: Vec::new(),
            cortex: CorticalMap::new(),
            tick: 0,
            total_spikes: AtomicU64::new(0),
            total_pruned: AtomicU64::new(0),
            total_ltp: AtomicU64::new(0),
            total_colocate: AtomicU64::new(0),
        }
    }

    pub fn init(&mut self, num_cores: usize) {
        self.cortex.init(num_cores);
        for n in self.neurons.iter_mut() {
            n.is_active = true;
            n.assigned_core = self.cortex.assign_core(n.cortex_row, n.cortex_col);
        }
    }

    /// An IRQ fires: depolarize its neuron and propagate to connected neurons
    /// Returns the CPU core this IRQ should be routed to
    pub fn irq_fire(&mut self, vector: u8, now_ns: u64) -> u8 {
        self.tick += 1;
        let tick = self.tick;

        // Direct stimulation — full threshold depolarization
        let fired = {
            let n = &mut self.neurons[vector as usize];
            n.stimulate(n.threshold_fp + 1, tick, now_ns)
        };
        if fired {
            self.total_spikes.fetch_add(1, Ordering::Relaxed);
        }

        // Propagate spike through synapses (excitatory/inhibitory)
        let post_vectors: Vec<(u8, i32)> = self
            .synapses
            .iter()
            .filter(|s| s.pre_vector == vector)
            .map(|s| {
                let current = if s.is_excitatory {
                    (s.weight_fp >> 1) as i32
                } else {
                    -((s.weight_fp >> 1) as i32)
                };
                (s.post_vector, current)
            })
            .collect();

        for (post_v, current) in post_vectors {
            self.neurons[post_v as usize].stimulate(current, tick, now_ns);
        }

        // Hebbian update for all synapse pairs
        self.hebbian_tick(tick, now_ns, vector);

        // Return assigned core for this vector
        self.neurons[vector as usize].assigned_core
    }

    fn hebbian_tick(&mut self, tick: u64, now_ns: u64, fired_vector: u8) {
        // Update synapses where pre = fired_vector
        for syn in &mut self.synapses {
            if syn.pre_vector != fired_vector {
                continue;
            }
            let post_active = self.neurons[syn.post_vector as usize]
                .fired_recently(LTP_WINDOW_TICKS * 1_000_000, now_ns);
            syn.hebbian_update(true, post_active, tick);
            if post_active {
                self.total_ltp.fetch_add(1, Ordering::Relaxed);
            }
            // STDP
            let pre_ns = self.neurons[fired_vector as usize].last_fire_ns;
            let post_ns = self.neurons[syn.post_vector as usize].last_fire_ns;
            syn.stdp_update(pre_ns, post_ns);
        }

        // Prune weak synapses
        let _pruned_pairs: Vec<(u8, u8)> = self
            .synapses
            .iter()
            .filter(|s| s.should_prune())
            .map(|s| (s.pre_vector, s.post_vector))
            .collect();
        let pre_prune = self.synapses.len();
        self.synapses.retain(|s| !s.should_prune());
        let pruned = pre_prune - self.synapses.len();
        self.total_pruned
            .fetch_add(pruned as u64, Ordering::Relaxed);

        // Co-locate strongly connected pairs in cortical map
        let strong_pairs: Vec<(u8, u8)> = self
            .synapses
            .iter()
            .filter(|s| s.weight_fp > 0x0001_0000) // weight > 1.0
            .map(|s| (s.pre_vector, s.post_vector))
            .collect();
        for (a, b) in strong_pairs {
            self.cortex.co_locate(a, b);
            // Re-assign cores after co-location
            let row = self.neurons[b as usize].cortex_row;
            let col = self.neurons[b as usize].cortex_col;
            let new_core = self.cortex.assign_core(row, col);
            if self.neurons[b as usize].assigned_core != new_core {
                self.neurons[b as usize].assigned_core = new_core;
                self.total_colocate.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Register a new synaptic connection between two vectors
    pub fn connect(&mut self, pre: u8, post: u8, excitatory: bool) {
        if self.synapses.len() >= MAX_SYNAPSES {
            return;
        }
        if self
            .synapses
            .iter()
            .any(|s| s.pre_vector == pre && s.post_vector == post)
        {
            return;
        }
        let mut syn = Synapse::new(pre, post);
        syn.is_excitatory = excitatory;
        self.synapses.push(syn);
    }

    /// Get the current core assignment for a vector
    pub fn core_for(&self, vector: u8) -> u8 {
        self.neurons[vector as usize].assigned_core
    }

    pub fn stats(&self) -> CortexStats {
        CortexStats {
            active_synapses: self.synapses.len() as u32,
            total_spikes: self.total_spikes.load(Ordering::Relaxed),
            total_pruned: self.total_pruned.load(Ordering::Relaxed),
            total_ltp: self.total_ltp.load(Ordering::Relaxed),
            total_colocate: self.total_colocate.load(Ordering::Relaxed),
            tick: self.tick,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CortexStats {
    pub active_synapses: u32,
    pub total_spikes: u64,
    pub total_pruned: u64,
    pub total_ltp: u64,
    pub total_colocate: u64,
    pub tick: u64,
}
