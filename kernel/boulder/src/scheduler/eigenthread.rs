// kernel/boulder/src/scheduler/eigenthread.rs
// #![no_std] inherited
//
// EIGENTHREAD — Workload Hamiltonian Scheduler
//
// Model: N runnable processes = N quantum states |ψ_i⟩
// Hamiltonian H[i][j] = coupling energy between process i and process j
//   H[i][i] = self-energy (priority, deadline urgency) — diagonal
//   H[i][j] = interaction energy:
//     > 0 (repulsive): processes share I/O port, memory bus contention
//     < 0 (attractive): processes share cache lines, NUMA node, pipeline
//
// Eigenvalue equation: H|ψ⟩ = E|ψ⟩
//   Lowest eigenvalue E_0 → ground state = optimal co-schedule set
//
// Power Iteration (no_std fixed-point):
//   v_{k+1} = H * v_k / ||H * v_k||
//   Converges to dominant eigenvector of H in ~32 iterations
//   Lowest energy: use H' = λ_max * I - H, find dominant eigenvector → lowest of H
//
// Scheduling decision: processes with highest |ψ_i|² in ground state eigenvector
//   are co-scheduled on the same NUMA domain
//
// Dynamic coupling: H[i][j] updated every tick from hardware counters
//   Cache sharing detected via LLC miss correlation
//   I/O contention detected via IRQ co-arrival rate (from SynapticCortex)
//   Pipeline coupling detected via IPC correlation between processes

extern crate alloc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

// ─────────────────────────────────────────────
// CONSTANTS
// ─────────────────────────────────────────────

pub const MAX_EIGENSTATES: usize = 64; // max processes in Hamiltonian
pub const HAMILTONIAN_DIM: usize = MAX_EIGENSTATES;
pub const POWER_ITER_STEPS: usize = 32;
pub const FP_SCALE: i64 = 0x0001_0000; // 16.16 fixed point scale
pub const FP_ONE: i64 = FP_SCALE;
pub const ATTRACTIVE_CACHE: i64 = -0x0000_4000; // -0.25 in 16.16
pub const REPULSIVE_IO: i64 = 0x0000_8000; // +0.5 in 16.16
pub const SELF_ENERGY_BASE: i64 = 0x0001_0000; // 1.0
pub const COUPLING_DECAY: i64 = 63; // decay coupling by 1/64 per tick
pub const MAX_COUPLING: i64 = 0x0004_0000; // 4.0 max coupling magnitude
pub const GROUND_STATE_TOPN: usize = 8; // top N processes in ground state

// ─────────────────────────────────────────────
// PROCESS QUANTUM STATE
// ─────────────────────────────────────────────

#[derive(Clone, Copy)]
pub struct EigenProcess {
    pub pid: u32,
    pub self_energy_fp: i64, // H[i][i] diagonal (priority + deadline)
    pub ipc_rate_fp: i64,    // instructions per cycle (16.16)
    pub llc_miss_fp: i64,    // LLC miss rate (16.16)
    pub io_irq_vector: u8,   // primary IRQ vector used (0 = none)
    pub numa_node: u8,
    pub core_mask: u64,      // affinity bitmask
    pub eigenamplitude: i64, // |ψ_i| squared (16.16) — current eigenvector component
    pub energy_level: i64,   // E_i from most recent diagonalization
    pub is_ground: bool,     // in the current ground state set
    pub last_ipc_tick: u64,
}

impl EigenProcess {
    pub fn new(pid: u32, priority: u8) -> Self {
        Self {
            pid,
            self_energy_fp: SELF_ENERGY_BASE + (255 - priority as i64) * 0x0000_0100,
            ipc_rate_fp: FP_ONE,
            llc_miss_fp: 0,
            io_irq_vector: 0,
            numa_node: 0,
            core_mask: !0u64,
            eigenamplitude: 0,
            energy_level: 0,
            is_ground: false,
            last_ipc_tick: 0,
        }
    }

    pub fn update_self_energy(&mut self, deadline_urgency_fp: i64) {
        // Lower energy = more attractive = higher priority
        self.self_energy_fp = SELF_ENERGY_BASE
            - deadline_urgency_fp  // urgent deadlines lower energy (attract scheduler)
            + self.llc_miss_fp / 4; // cache misser has higher energy (less attractive)
    }
}

// ─────────────────────────────────────────────
// HAMILTONIAN MATRIX (sparse fixed-point)
// ─────────────────────────────────────────────

pub struct Hamiltonian {
    // Dense storage for MAX_EIGENSTATES × MAX_EIGENSTATES
    // Stored as i64 16.16 fixed-point
    // Upper triangular only (symmetric H[i][j] = H[j][i])
    pub h: [[i64; MAX_EIGENSTATES]; MAX_EIGENSTATES],
    pub dim: usize, // actual number of active processes
    pub eigenvalues: [i64; MAX_EIGENSTATES],
    pub lambda_max: i64, // dominant eigenvalue estimate (for shift)
}

impl Hamiltonian {
    pub const fn new() -> Self {
        Self {
            h: [[0i64; MAX_EIGENSTATES]; MAX_EIGENSTATES],
            dim: 0,
            eigenvalues: [0i64; MAX_EIGENSTATES],
            lambda_max: FP_ONE,
        }
    }

    pub fn reset(&mut self, dim: usize) {
        self.dim = dim.min(MAX_EIGENSTATES);
        for row in &mut self.h {
            row.fill(0);
        }
    }

    /// Set diagonal self-energy for process at index i
    pub fn set_self_energy(&mut self, i: usize, energy_fp: i64) {
        if i < self.dim {
            self.h[i][i] = energy_fp;
        }
    }

    /// Set coupling between processes i and j (symmetric)
    pub fn set_coupling(&mut self, i: usize, j: usize, coupling_fp: i64) {
        if i < self.dim && j < self.dim && i != j {
            let c = coupling_fp.clamp(-MAX_COUPLING, MAX_COUPLING);
            self.h[i][j] = c;
            self.h[j][i] = c;
        }
    }

    /// Add coupling delta (used for incremental updates from hw counters)
    pub fn add_coupling(&mut self, i: usize, j: usize, delta_fp: i64) {
        if i < self.dim && j < self.dim && i != j {
            let new = (self.h[i][j] + delta_fp).clamp(-MAX_COUPLING, MAX_COUPLING);
            self.h[i][j] = new;
            self.h[j][i] = new;
        }
    }

    /// Decay all off-diagonal couplings toward zero (decoherence)
    pub fn decay_couplings(&mut self) {
        for i in 0..self.dim {
            for j in 0..self.dim {
                if i != j {
                    self.h[i][j] = self.h[i][j] * COUPLING_DECAY / 64;
                }
            }
        }
    }

    /// Matrix-vector multiply: result = H * vec (fixed-point i64)
    pub fn matvec(&self, vec: &[i64; MAX_EIGENSTATES], result: &mut [i64; MAX_EIGENSTATES]) {
        for i in 0..self.dim {
            let mut sum: i64 = 0;
            for j in 0..self.dim {
                // (H[i][j] * vec[j]) in 16.16 fp → shift right 16
                sum = sum.saturating_add((self.h[i][j].saturating_mul(vec[j])) >> 16);
            }
            result[i] = sum;
        }
    }

    /// L2 norm of vector (fixed-point, returns 16.16 fp)
    pub fn norm_fp(vec: &[i64; MAX_EIGENSTATES], dim: usize) -> i64 {
        let sum_sq: i64 = vec[..dim]
            .iter()
            .map(|&v| (v.saturating_mul(v)) >> 16)
            .fold(0i64, |a, b| a.saturating_add(b));
        // Integer sqrt of 16.16 fixed-point sum → result in 16.16
        isqrt_fp(sum_sq)
    }

    /// Power iteration: find dominant eigenvector of (λ_max*I - H)
    /// which corresponds to LOWEST eigenvalue of H (ground state)
    pub fn power_iterate_ground(&mut self, eigenvec: &mut [i64; MAX_EIGENSTATES]) {
        // Shift: H' = λ_max * I - H
        // Dominant eigenvector of H' = lowest eigenvector of H
        let mut shifted = [[0i64; MAX_EIGENSTATES]; MAX_EIGENSTATES];
        for i in 0..self.dim {
            for j in 0..self.dim {
                shifted[i][j] = if i == j {
                    self.lambda_max - self.h[i][j]
                } else {
                    -self.h[i][j]
                };
            }
        }

        // Initialize eigenvec to uniform (warm start)
        let init_fp = FP_ONE / self.dim.max(1) as i64;
        for i in 0..self.dim {
            eigenvec[i] = init_fp;
        }

        let mut temp = [0i64; MAX_EIGENSTATES];
        let dim = self.dim;

        for _step in 0..POWER_ITER_STEPS {
            // temp = H' * eigenvec
            for i in 0..dim {
                let mut sum: i64 = 0;
                for j in 0..dim {
                    sum = sum.saturating_add((shifted[i][j].saturating_mul(eigenvec[j])) >> 16);
                }
                temp[i] = sum;
            }
            // Normalize
            let norm = Self::norm_fp(&temp, dim);
            if norm < 64 {
                break;
            } // converged / degenerate
            for i in 0..dim {
                eigenvec[i] = (temp[i].saturating_mul(FP_SCALE)) / norm.max(1);
            }
        }

        // Estimate dominant eigenvalue of H' (Rayleigh quotient): λ' = v^T H' v
        let mut lambda_shifted: i64 = 0;
        for i in 0..dim {
            let mut hv: i64 = 0;
            for j in 0..dim {
                hv = hv.saturating_add((shifted[i][j].saturating_mul(eigenvec[j])) >> 16);
            }
            lambda_shifted = lambda_shifted.saturating_add((eigenvec[i].saturating_mul(hv)) >> 16);
        }
        // Ground eigenvalue of H = λ_max - λ_shifted
        self.eigenvalues[0] = self.lambda_max - lambda_shifted;

        // Update λ_max estimate for next iteration
        let diag_max = (0..dim).map(|i| self.h[i][i]).max().unwrap_or(FP_ONE);
        self.lambda_max = (diag_max + MAX_COUPLING).max(FP_ONE);
    }
}

/// Integer square root returning 16.16 fixed-point result
fn isqrt_fp(x_fp: i64) -> i64 {
    if x_fp <= 0 {
        return 0;
    }
    // Convert from 16.16 to plain integer for sqrt, then back
    let x_plain = x_fp as u64;
    let mut s = 0u64;
    let mut bit = 1u64 << 32;
    let mut rem = x_plain;
    while bit > rem {
        bit >>= 2;
    }
    while bit != 0 {
        if rem >= s + bit {
            rem -= s + bit;
            s = (s >> 1) + bit;
        } else {
            s >>= 1;
        }
        bit >>= 2;
    }
    // s is sqrt(x_fp) in units of sqrt(16.16) = 8.8; shift to 16.16
    (s as i64) << 8
}

// ─────────────────────────────────────────────
// EIGENTHREAD SCHEDULER
// ─────────────────────────────────────────────

pub struct Eigenthread {
    pub processes: Vec<EigenProcess>,
    pub hamiltonian: Hamiltonian,
    pub eigenvec: [i64; MAX_EIGENSTATES], // current ground state eigenvector
    pub ground_set: [u32; GROUND_STATE_TOPN], // PIDs in ground state
    pub ground_size: usize,
    pub tick: u64,
    pub rediag_interval: u64, // re-solve Hamiltonian every N ticks
    pub total_rediag: AtomicU64,
    pub cache_coupling_updates: AtomicU64,
    pub io_coupling_updates: AtomicU64,
}

impl Eigenthread {
    pub fn new() -> Self {
        Self {
            processes: Vec::new(),
            hamiltonian: Hamiltonian::new(),
            eigenvec: [0i64; MAX_EIGENSTATES],
            ground_set: [0u32; GROUND_STATE_TOPN],
            ground_size: 0,
            tick: 0,
            rediag_interval: 64,
            total_rediag: AtomicU64::new(0),
            cache_coupling_updates: AtomicU64::new(0),
            io_coupling_updates: AtomicU64::new(0),
        }
    }

    pub fn admit(&mut self, pid: u32, priority: u8) -> usize {
        let idx = self.processes.len();
        if idx < MAX_EIGENSTATES {
            self.processes.push(EigenProcess::new(pid, priority));
        }
        idx
    }

    pub fn remove(&mut self, pid: u32) {
        self.processes.retain(|p| p.pid != pid);
    }

    /// Record cache sharing event between two processes → attractive coupling
    pub fn record_cache_sharing(&mut self, pid_a: u32, pid_b: u32, strength_fp: i64) {
        let (ia, ib) = match (
            self.processes.iter().position(|p| p.pid == pid_a),
            self.processes.iter().position(|p| p.pid == pid_b),
        ) {
            (Some(a), Some(b)) => (a, b),
            _ => return,
        };

        let coupling = (ATTRACTIVE_CACHE.saturating_mul(strength_fp)) >> 16;
        self.hamiltonian.add_coupling(ia, ib, coupling);
        self.cache_coupling_updates.fetch_add(1, Ordering::Relaxed);
    }

    /// Record I/O contention → repulsive coupling
    pub fn record_io_contention(&mut self, pid_a: u32, pid_b: u32, strength_fp: i64) {
        let (ia, ib) = match (
            self.processes.iter().position(|p| p.pid == pid_a),
            self.processes.iter().position(|p| p.pid == pid_b),
        ) {
            (Some(a), Some(b)) => (a, b),
            _ => return,
        };

        let coupling = (REPULSIVE_IO.saturating_mul(strength_fp)) >> 16;
        self.hamiltonian.add_coupling(ia, ib, coupling);
        self.io_coupling_updates.fetch_add(1, Ordering::Relaxed);
    }

    /// Master tick: rebuild H, solve for ground state, update assignments
    pub fn tick(&mut self, _now_ns: u64) {
        self.tick += 1;
        let dim = self.processes.len().min(MAX_EIGENSTATES);
        if dim == 0 {
            return;
        }

        // Rebuild diagonal self-energies
        self.hamiltonian.reset(dim);
        for (i, proc) in self.processes.iter().enumerate() {
            if i >= dim {
                break;
            }
            self.hamiltonian.set_self_energy(i, proc.self_energy_fp);
        }

        // Decay old couplings
        self.hamiltonian.decay_couplings();

        // Re-diagonalize periodically
        if self.tick % self.rediag_interval == 0 {
            self.hamiltonian.power_iterate_ground(&mut self.eigenvec);
            self.total_rediag.fetch_add(1, Ordering::Relaxed);

            // Update eigenamplitudes and select ground state set
            let mut indexed: [(i64, usize); MAX_EIGENSTATES] = [(0i64, 0); MAX_EIGENSTATES];
            for i in 0..dim {
                let amp = self.eigenvec[i];
                let amp_sq = (amp.saturating_mul(amp)) >> 16;
                self.processes[i].eigenamplitude = amp_sq;
                indexed[i] = (amp_sq, i);
            }
            // Sort descending by amplitude squared
            indexed[..dim].sort_unstable_by(|a, b| b.0.cmp(&a.0));

            self.ground_size = GROUND_STATE_TOPN.min(dim);
            for (k, &(_, idx)) in indexed[..self.ground_size].iter().enumerate() {
                self.ground_set[k] = self.processes[idx].pid;
                self.processes[idx].is_ground = true;
            }
            // Clear non-ground processes
            for proc in &mut self.processes {
                if !self.ground_set[..self.ground_size].contains(&proc.pid) {
                    proc.is_ground = false;
                }
            }
        }
    }

    /// Query: is this PID in the current ground state (should be scheduled now)?
    pub fn is_ground_state(&self, pid: u32) -> bool {
        self.ground_set[..self.ground_size].contains(&pid)
    }

    /// Get the eigenamplitude of a process (scheduling weight analog)
    pub fn amplitude_of(&self, pid: u32) -> i64 {
        self.processes
            .iter()
            .find(|p| p.pid == pid)
            .map(|p| p.eigenamplitude)
            .unwrap_or(0)
    }

    pub fn stats(&self) -> EigenStats {
        EigenStats {
            active_processes: self.processes.len() as u32,
            ground_set_size: self.ground_size as u32,
            hamiltonian_dim: self.hamiltonian.dim as u32,
            ground_eigenvalue: self.hamiltonian.eigenvalues[0],
            total_rediag: self.total_rediag.load(Ordering::Relaxed),
            cache_updates: self.cache_coupling_updates.load(Ordering::Relaxed),
            io_updates: self.io_coupling_updates.load(Ordering::Relaxed),
            tick: self.tick,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct EigenStats {
    pub active_processes: u32,
    pub ground_set_size: u32,
    pub hamiltonian_dim: u32,
    pub ground_eigenvalue: i64,
    pub total_rediag: u64,
    pub cache_updates: u64,
    pub io_updates: u64,
    pub tick: u64,
}
