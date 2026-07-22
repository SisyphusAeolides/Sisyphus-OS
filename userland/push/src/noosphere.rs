#![allow(dead_code)]
use alloc::{
    collections::BTreeMap,
    string::String,
    vec,
    vec::Vec,
};


// ─────────────────────────────────────────────
// CONSTANTS
// ─────────────────────────────────────────────

/// Dimensionality of each service's mind-state vector
pub const MIND_DIM: usize = 32;
/// Maximum services in the noosphere
pub const MAX_NEURONS: usize = 128;
/// Hebbian learning rate
const ETA: f64 = 0.01;
/// Weight decay (L2 regularization to prevent weight explosion)
const DECAY: f64 = 0.001;
/// Hopfield threshold per neuron
const THETA: f64 = 0.5;
/// SOM grid dimensions (8x8 = 64 cluster nodes)
const SOM_W: usize = 8;
const SOM_H: usize = 8;
const SOM_NODES: usize = SOM_W * SOM_H;

// ─────────────────────────────────────────────
// MIND-STATE VECTOR
// ─────────────────────────────────────────────

/// A service's real-time behavioral fingerprint
/// Encoded as a MIND_DIM-dimensional float vector
#[derive(Clone, Copy)]
pub struct MindState {
    pub v: [f64; MIND_DIM],
    pub pid: u32,
    pub active: bool,
}

impl MindState {
    pub fn zero(pid: u32) -> Self {
        Self { v: [0.0; MIND_DIM], pid, active: false }
    }

    /// Encode service vitals into mind-state vector
    pub fn encode(
        pid: u32,
        cpu_pct: f64, mem_mb: f64, ipc_send: f64, ipc_recv: f64,
        fd_count: f64, syscall_freq: f64, page_faults: f64,
        ctx_switches: f64, net_tx: f64, net_rx: f64,
    ) -> Self {
        let mut v = [0.0f64; MIND_DIM];
        let clamp = |val: f64| -> f64 { if val < 0.0 { 0.0 } else if val > 1.0 { 1.0 } else { val } };

        // Raw features (normalized to [0,1])
        v[0]  = clamp(cpu_pct / 100.0);
        v[1]  = clamp(mem_mb / 32768.0);
        v[2]  = clamp(ipc_send / 10000.0);
        v[3]  = clamp(ipc_recv / 10000.0);
        v[4]  = clamp(fd_count / 1024.0);
        v[5]  = clamp(syscall_freq / 100000.0);
        v[6]  = clamp(page_faults / 1000.0);
        v[7]  = clamp(ctx_switches / 10000.0);
        v[8]  = clamp(net_tx / 1_000_000.0);
        v[9]  = clamp(net_rx / 1_000_000.0);

        // Cross-product features — capture nonlinear relationships
        v[10] = v[0] * v[1];               // cpu × mem (compute-bound signature)
        v[11] = v[2] * v[3];               // ipc_send × ipc_recv (messaging service)
        v[12] = v[8] * v[9];               // net_tx × net_rx (network service)
        v[13] = v[5] * v[6];               // syscall × page_fault (I/O bound)
        v[14] = v[0] * v[5];               // cpu × syscall (kernel intensive)
        v[15] = (v[0] + v[1] + v[5]) / 3.0; // general load average

        // Fourier-like frequency features on cpu (detect periodic behavior)
        v[16] = libm::sin(core::f64::consts::TAU * v[0]) * 0.5 + 0.5;
        v[17] = libm::cos(core::f64::consts::TAU * v[0]) * 0.5 + 0.5;
        v[18] = libm::sin(core::f64::consts::TAU * v[1]) * 0.5 + 0.5;
        v[19] = libm::sin(core::f64::consts::TAU * v[5]) * 0.5 + 0.5;

        // Sigmoid-squashed high-order features
        for i in 20..MIND_DIM {
            let x = v[i - 20] * v[i - 10];
            v[i] = 1.0 / (1.0 + libm::exp(-10.0 * (x - 0.5))); // sigmoid
        }

        Self { v, pid, active: true }
    }

    /// L2 norm of the mind-state vector
    pub fn norm(&self) -> f64 {
        libm::sqrt(self.v.iter().map(|&x| x * x).sum::<f64>())
    }

    /// Cosine similarity with another mind state
    pub fn similarity(&self, other: &MindState) -> f64 {
        let dot: f64 = self.v.iter().zip(other.v.iter()).map(|(&a, &b)| a * b).sum();
        let denom = self.norm() * other.norm();
        if denom < 1e-12 { 0.0 } else { dot / denom }
    }

    /// Euclidean distance (for SOM matching)
    pub fn distance(&self, other: &MindState) -> f64 {
        libm::sqrt(self.v.iter().zip(other.v.iter())
            .map(|(&a, &b)| (a - b) * (a - b))
            .sum::<f64>())
    }

    /// Binarize to {-1, +1} for Hopfield network
    pub fn binarize(&self) -> [i8; MIND_DIM] {
        let mut b = [0i8; MIND_DIM];
        for (i, &x) in self.v.iter().enumerate() {
            b[i] = if x > 0.5 { 1 } else { -1 };
        }
        b
    }
}

// ─────────────────────────────────────────────
// HEBBIAN WEIGHT MATRIX
// ─────────────────────────────────────────────

/// Symmetric weight matrix W[i][j] — synaptic connection strength
/// between service i and service j
pub struct HebbianMatrix {
    pub w: [[f64; MAX_NEURONS]; MAX_NEURONS],
    pub tick: u64,
}

impl HebbianMatrix {
    pub fn new() -> Self {
        Self { w: [[0.0f64; MAX_NEURONS]; MAX_NEURONS], tick: 0 }
    }

    /// Hebbian update: Δw_ij = η*x_i*x_j - λ*w_ij (with weight decay)
    /// Called every tick for all active service pairs
    pub fn update(&mut self, states: &[MindState]) {
        self.tick += 1;
        let n = if states.len() < MAX_NEURONS { states.len() } else { MAX_NEURONS };
        for i in 0..n {
            if !states[i].active { continue; }
            for j in (i+1)..n {
                if !states[j].active { continue; }

                // Oja's rule: normalized Hebbian — prevents runaway weights
                // Δw_ij = η * (x_i · x_j - (x_j²) * w_ij)
                let dot: f64 = states[i].v.iter().zip(states[j].v.iter())
                    .map(|(&a, &b)| a * b).sum();
                let norm_sq_j: f64 = states[j].v.iter().map(|&x| x*x).sum();
                let delta = ETA * (dot - norm_sq_j * self.w[i][j]) - DECAY * self.w[i][j];

                self.w[i][j] += delta;
                self.w[j][i] = self.w[i][j]; // symmetric
            }
        }
    }

    /// Anti-Hebbian suppression: services that fire out-of-phase get inhibited
    /// Detects conflicting services (e.g., two disk schedulers fighting)
    pub fn anti_hebbian_suppress(&mut self, i: usize, j: usize, states: &[MindState]) {
        if i >= states.len() || j >= states.len() { return; }
        // If phases are anti-correlated (dot product < 0), apply inhibitory delta
        let dot: f64 = states[i].v.iter().zip(states[j].v.iter())
            .map(|(&a, &b)| a * b).sum();
        if dot < -0.3 {
            let dot_abs = if dot < 0.0 { -dot } else { dot };
            self.w[i][j] -= ETA * 2.0 * dot_abs;
            self.w[j][i] = self.w[i][j];
        }
    }

    /// Return the N strongest co-activation partners for service i
    pub fn strongest_partners(&self, i: usize, n: usize) -> Vec<(usize, f64)> {
        let mut row: Vec<(usize, f64)> = (0..MAX_NEURONS)
            .filter(|&j| j != i)
            .map(|j| (j, self.w[i][j]))
            .collect();
        row.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        row.truncate(n);
        row
    }

    /// Hopfield energy: E = -½ Σ_ij w_ij*s_i*s_j + Σ_i θ_i*s_i
    /// Lower energy = more stable configuration
    pub fn hopfield_energy(&self, states: &[MindState]) -> f64 {
        let n = if states.len() < MAX_NEURONS { states.len() } else { MAX_NEURONS };
        let binary: Vec<[i8; MIND_DIM]> = states.iter().map(|s| s.binarize()).collect();
        let mut e = 0.0f64;
        for i in 0..n {
            for j in (i+1)..n {
                // Sum over all dimensions of the state product
                let s_dot: f64 = binary[i].iter().zip(binary[j].iter())
                    .map(|(&a, &b)| a as f64 * b as f64).sum::<f64>() / MIND_DIM as f64;
                e -= 0.5 * self.w[i][j] * s_dot;
            }
            // Threshold term
            let s_sum: f64 = binary[i].iter().map(|&x| x as f64).sum::<f64>() / MIND_DIM as f64;
            e += THETA * s_sum;
        }
        e
    }

    /// Hopfield recall: given a partial/corrupted service state,
    /// recover the nearest stable attractor (the "healthy" state)
    pub fn hopfield_recall(&self, corrupted: &MindState, states: &[MindState], iters: usize) -> MindState {
        let n = if states.len() < MAX_NEURONS { states.len() } else { MAX_NEURONS };
        let mut s = corrupted.clone();
        for _ in 0..iters {
            // Asynchronous update — pick dimensions in random order (deterministic here)
            for d in 0..MIND_DIM {
                let mut h = 0.0f64;
                for j in 0..n {
                    let sj_d = states[j].v[d];
                    // Find index of this service in the weight matrix
                    let j_idx = if j < MAX_NEURONS - 1 { j } else { MAX_NEURONS - 1 };
                    let i_idx = (corrupted.pid as usize) % MAX_NEURONS;
                    h += self.w[i_idx][j_idx] * sj_d;
                }
                // Threshold activation
                s.v[d] = if h - THETA > 0.0 { 1.0 } else { 0.0 };
            }
        }
        s
    }
}

// ─────────────────────────────────────────────
// KOHONEN SELF-ORGANIZING MAP
// ─────────────────────────────────────────────

/// A node in the SOM grid — represents a "cluster archetype"
#[derive(Clone)]
pub struct SomNode {
    pub weights: [f64; MIND_DIM],
    pub cluster_label: Option<String>,
    pub member_pids: Vec<u32>,
}

impl SomNode {
    pub fn new_random(seed: u64) -> Self {
        let mut w = [0.0f64; MIND_DIM];
        let mut s = seed;
        for x in &mut w {
            s ^= s << 13; s ^= s >> 7; s ^= s << 17;
            *x = (s & 0xFFFF) as f64 / 65535.0;
        }
        Self { weights: w, cluster_label: None, member_pids: Vec::new() }
    }

    pub fn distance_to(&self, state: &MindState) -> f64 {
        libm::sqrt(self.weights.iter().zip(state.v.iter())
            .map(|(&w, &x)| (w - x) * (w - x))
            .sum::<f64>())
    }
}

pub struct KohonenSOM {
    pub nodes: [SomNode; SOM_NODES],
    pub learning_rate: f64,
    pub neighborhood_radius: f64,
    pub iteration: u64,
}

impl KohonenSOM {
    pub fn new() -> Self {
        let nodes: [SomNode; SOM_NODES] = core::array::from_fn(|i| {
            SomNode::new_random(0xdeadbeef ^ (i as u64).wrapping_mul(0x9e3779b97f4a7c15))
        });
        Self { nodes, learning_rate: 0.5, neighborhood_radius: 3.0, iteration: 0 }
    }

    /// Find Best Matching Unit (BMU) — the node closest to input state
    pub fn bmu(&self, state: &MindState) -> usize {
        self.nodes.iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                a.distance_to(state).partial_cmp(&b.distance_to(state)).unwrap()
            })
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    /// SOM training step for one input
    /// Updates BMU and its topological neighbors
    pub fn train_step(&mut self, state: &MindState) {
        self.iteration += 1;

        // Decay learning rate and radius over time
        let lr = self.learning_rate * libm::exp(-(self.iteration as f64) / 10000.0);
        let radius = self.neighborhood_radius * libm::exp(-(self.iteration as f64) / 5000.0);
        let radius_sq = radius * radius;

        let bmu_idx = self.bmu(state);
        let bmu_row = (bmu_idx / SOM_W) as f64;
        let bmu_col = (bmu_idx % SOM_W) as f64;

        for (idx, node) in self.nodes.iter_mut().enumerate() {
            let row = (idx / SOM_W) as f64;
            let col = (idx % SOM_W) as f64;
            let grid_dist_sq = (row - bmu_row) * (row - bmu_row) + (col - bmu_col) * (col - bmu_col);

            // Gaussian neighborhood function
            let neighborhood = libm::exp(-grid_dist_sq / (2.0 * radius_sq));
            if neighborhood < 0.001 { continue; }

            // Update node weights toward input
            for d in 0..MIND_DIM {
                node.weights[d] += lr * neighborhood * (state.v[d] - node.weights[d]);
            }
        }
    }

    /// Classify a service into a cluster by finding its BMU
    pub fn classify(&self, state: &MindState) -> (usize, String) {
        let bmu = self.bmu(state);
        let row = bmu / SOM_W;
        let col = bmu % SOM_W;
        // Auto-generate cluster label from grid position + BMU dominant feature
        let dominant_dim = self.nodes[bmu].weights.iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
            .map(|(i, _)| i)
            .unwrap_or(0);
        let label_base = match dominant_dim {
            0 => "cpu_bound",
            1 => "mem_heavy",
            2..=3 => "ipc_mesh",
            4..=5 => "syscall_driver",
            8..=9 => "network_svc",
            _ => "general",
        };
        (bmu, alloc::format!("cluster_{label_base}_{row}x{col}"))
    }

    /// Return quantization error — overall SOM fit quality
    pub fn quantization_error(&self, states: &[MindState]) -> f64 {
        if states.is_empty() { return 0.0; }
        states.iter()
            .map(|s| self.nodes[self.bmu(s)].distance_to(s))
            .sum::<f64>() / states.len() as f64
    }
}

// ─────────────────────────────────────────────
// RESONANCE DETECTOR — Gamma Oscillation Sync
// ─────────────────────────────────────────────

/// Models service activity as a neural oscillator
/// Services that phase-lock are in resonance — they form tight functional groups
/// Based on Kuramoto model of coupled oscillators
pub struct KuramotoOscillator {
    pub pid: u32,
    pub phase: f64,       // θ_i ∈ [0, 2π)
    pub frequency: f64,   // ω_i — natural frequency (from service type)
    pub coupling: f64,    // K — coupling strength to peers
}

impl KuramotoOscillator {
    pub fn new(pid: u32, frequency: f64) -> Self {
        Self { pid, phase: 0.0, frequency, coupling: 2.0 }
    }

    /// Kuramoto update: dθ_i/dt = ω_i + (K/N) Σ_j sin(θ_j - θ_i)
    pub fn step(&mut self, peers: &[KuramotoOscillator], dt: f64) {
        let n = peers.len() as f64;
        if n < 1.0 { return; }
        let coupling_sum: f64 = peers.iter()
            .map(|p| libm::sin(p.phase - self.phase))
            .sum();
        let dphi = self.frequency + (self.coupling / n) * coupling_sum;
        self.phase = (self.phase + dphi * dt) % core::f64::consts::TAU;
    }

    /// Order parameter r = |Σ exp(iθ_j)| / N — measures synchronization
    /// r=1 → perfect sync, r=0 → total incoherence
    pub fn order_parameter(oscillators: &[KuramotoOscillator]) -> f64 {
        if oscillators.is_empty() { return 0.0; }
        let n = oscillators.len() as f64;
        let re: f64 = oscillators.iter().map(|o| libm::cos(o.phase)).sum::<f64>() / n;
        let im: f64 = oscillators.iter().map(|o| libm::sin(o.phase)).sum::<f64>() / n;
        libm::sqrt(re * re + im * im)
    }
}

// ─────────────────────────────────────────────
// THE NOOSPHERE — Master Collective Intelligence
// ─────────────────────────────────────────────

pub struct Noosphere {
    pub states:      Vec<MindState>,
    pub pid_to_idx:  BTreeMap<u32, usize>,
    pub hebbian:     HebbianMatrix,
    pub som:         KohonenSOM,
    pub oscillators: Vec<KuramotoOscillator>,
    pub tick:        u64,
    pub energy_log:  Vec<f64>,   // Hopfield energy history
    pub sync_log:    Vec<f64>,   // Kuramoto order parameter history
}

impl Noosphere {
    pub fn new() -> Self {
        Self {
            states:      Vec::new(),
            pid_to_idx:  BTreeMap::new(),
            hebbian:     HebbianMatrix::new(),
            som:         KohonenSOM::new(),
            oscillators: Vec::new(),
            tick:        0,
            energy_log:  Vec::new(),
            sync_log:    Vec::new(),
        }
    }

    /// Register a new service in the noosphere
    pub fn register_service(&mut self, pid: u32, natural_freq: f64) {
        let idx = self.states.len();
        self.states.push(MindState::zero(pid));
        self.pid_to_idx.insert(pid, idx);
        self.oscillators.push(KuramotoOscillator::new(pid, natural_freq));
    }

    /// Update a service's mind-state from its current vitals
    pub fn update_service(
        &mut self, pid: u32,
        cpu: f64, mem: f64, ipc_s: f64, ipc_r: f64,
        fd: f64, sys: f64, pf: f64, ctx: f64, ntx: f64, nrx: f64,
    ) {
        if let Some(&idx) = self.pid_to_idx.get(&pid) {
            self.states[idx] = MindState::encode(pid, cpu, mem, ipc_s, ipc_r, fd, sys, pf, ctx, ntx, nrx);
            self.som.train_step(&self.states[idx]);
        }
    }

    /// Remove a dead service from the noosphere
    pub fn deregister(&mut self, pid: u32) {
        if let Some(&idx) = self.pid_to_idx.get(&pid) {
            self.states[idx].active = false;
            if idx < self.oscillators.len() {
                self.oscillators[idx].coupling = 0.0; // decouple
            }
        }
        self.pid_to_idx.remove(&pid);
    }

    /// Master tick — advance the entire collective consciousness one step
    pub fn tick(&mut self, dt: f64) {
        self.tick += 1;

        // 1. Hebbian learning across all active pairs
        self.hebbian.update(&self.states);

        // 2. Kuramoto oscillator update — each service oscillates with peers
        let peers_snapshot: Vec<KuramotoOscillator> = self.oscillators.iter()
            .map(|o| KuramotoOscillator { pid: o.pid, phase: o.phase, frequency: o.frequency, coupling: o.coupling })
            .collect();
        for osc in &mut self.oscillators {
            osc.step(&peers_snapshot, dt);
        }

        // 3. Record energy and sync metrics every 16 ticks
        if self.tick % 16 == 0 {
            let e = self.hebbian.hopfield_energy(&self.states);
            self.energy_log.push(e);
            if self.energy_log.len() > 512 { self.energy_log.remove(0); }

            let r = KuramotoOscillator::order_parameter(&self.oscillators);
            self.sync_log.push(r);
            if self.sync_log.len() > 512 { self.sync_log.remove(0); }
        }
    }

    /// Get services clustered together with pid (co-activation partners)
    pub fn cluster_mates(&self, pid: u32) -> Vec<u32> {
        let idx = match self.pid_to_idx.get(&pid) {
            Some(&i) => i, None => return Vec::new(),
        };
        let partners = self.hebbian.strongest_partners(idx, 8);
        let idx_to_pid: BTreeMap<usize, u32> = self.pid_to_idx.iter().map(|(&p, &i)| (i, p)).collect();
        partners.iter()
            .filter(|(_, w)| *w > 0.1)
            .filter_map(|(j, _)| idx_to_pid.get(j).copied())
            .collect()
    }

    /// Anomaly detection: find services whose mind-state deviates from their Hopfield attractor
    pub fn detect_anomalous_services(&self) -> Vec<(u32, f64)> {
        let mut anomalies = Vec::new();
        for (&pid, &idx) in &self.pid_to_idx {
            if !self.states[idx].active { continue; }
            let recalled = self.hebbian.hopfield_recall(&self.states[idx], &self.states, 5);
            let deviation = self.states[idx].distance(&recalled);
            if deviation > 0.3 {
                anomalies.push((pid, deviation));
            }
        }
        anomalies.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        anomalies
    }

    /// Phase-locked services — tight functional groups (sync > 0.8)
    pub fn synchronized_groups(&self) -> Vec<Vec<u32>> {
        let mut groups: Vec<Vec<u32>> = Vec::new();
        let mut assigned = vec![false; self.oscillators.len()];

        for i in 0..self.oscillators.len() {
            if assigned[i] { continue; }
            let mut group = vec![self.oscillators[i].pid];
            assigned[i] = true;
            for j in (i+1)..self.oscillators.len() {
                if assigned[j] { continue; }
                let mut phase_diff = self.oscillators[i].phase - self.oscillators[j].phase;
                if phase_diff < 0.0 { phase_diff = -phase_diff; }
                let alt_diff = core::f64::consts::TAU - phase_diff;
                let normalized_diff = if phase_diff < alt_diff { phase_diff } else { alt_diff };
                
                if normalized_diff < 0.3 { // within 0.3 rad — phase-locked
                    group.push(self.oscillators[j].pid);
                    assigned[j] = true;
                }
            }
            if group.len() > 1 { groups.push(group); }
        }
        groups
    }

    /// Identify the "conductor" — highest-coupling oscillator, the service
    /// that all others synchronize to (the heartbeat of each group)
    pub fn conductor_pid(&self) -> Option<u32> {
        self.oscillators.iter()
            .max_by(|a, b| a.coupling.partial_cmp(&b.coupling).unwrap())
            .map(|o| o.pid)
    }

    /// SOM topology map — returns (pid, cluster_label, bmu_index) for all services
    pub fn topology_map(&self) -> Vec<(u32, String, usize)> {
        self.pid_to_idx.iter()
            .filter(|&(_, &idx)| self.states[idx].active)
            .map(|(&pid, &idx)| {
                let (bmu, label) = self.som.classify(&self.states[idx]);
                (pid, label, bmu)
            })
            .collect()
    }

    /// Is the noosphere converging? (Energy decreasing monotonically)
    pub fn is_converging(&self) -> bool {
        if self.energy_log.len() < 4 { return false; }
        let last4 = &self.energy_log[self.energy_log.len()-4..];
        last4.windows(2).all(|w| w[1] <= w[0])
    }

    /// Critical transition detector — sudden energy spike = phase transition
    /// (e.g., a cascading failure about to happen)
    pub fn detect_phase_transition(&self) -> Option<f64> {
        if self.energy_log.len() < 8 { return None; }
        let n = self.energy_log.len();
        let recent_mean = self.energy_log[n-4..].iter().sum::<f64>() / 4.0;
        let older_mean  = self.energy_log[n-8..n-4].iter().sum::<f64>() / 4.0;
        let d = recent_mean - older_mean;
        let delta = if d < 0.0 { -d } else { d };
        if delta > 5.0 { Some(delta) } else { None }
    }
}
