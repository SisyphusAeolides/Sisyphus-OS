#![allow(dead_code)]
use alloc::{collections::BTreeMap, vec::Vec};
use core::sync::atomic::{AtomicU64, AtomicU32, Ordering};

// ─────────────────────────────────────────────
// CONSTANTS
// ─────────────────────────────────────────────

pub const MAX_CORES:    usize = 256;
pub const MAX_PROCS:    usize = 65536;
pub const QUANTUM_NS:   u64   = 4_000_000;  // 4ms default quantum
pub const MIN_QUANTUM:  u64   = 500_000;     // 500µs (real-time)
pub const MAX_QUANTUM:  u64   = 16_000_000;  // 16ms (background)
pub const ALPHA_INIT:   f64   = 1.0;         // Beta prior α (uniform)
pub const BETA_INIT:    f64   = 1.0;         // Beta prior β (uniform)
pub const CACHE_WARM_THRESHOLD_NS: u64 = 50_000_000; // 50ms cache warm window
pub const EXPLORATION_BOOST: f64 = 0.15;    // UCB-style exploration addend
pub const GOSSIP_INTERVAL_TICKS: u64 = 64;  // share model every 64 ticks

// ─────────────────────────────────────────────
// PROCESS SCHEDULER METADATA
// ─────────────────────────────────────────────

#[derive(Clone)]
pub struct ProcessArm {
    pub pid:              u32,
    pub alpha:            f64,      // Beta dist α — successful rewards
    pub beta:             f64,      // Beta dist β — failed/missed rewards
    pub last_sample:      f64,      // last Thompson sample θ
    pub last_run_ns:      u64,      // wall time of last context switch-in
    pub last_core:        u8,       // last CPU core this ran on
    pub total_quanta:     u64,      // total scheduled quanta
    pub total_reward:     f64,      // cumulative reward
    pub ipc_rate:         f64,      // instructions per cycle (from perf counters)
    pub cache_miss_rate:  f64,      // LLC miss rate
    pub voluntary_yields: u64,      // times process yielded before quantum expiry
    pub preemptions:      u64,      // times process was forcibly preempted
    pub soft_deadline_ns: Option<u64>, // EDF deadline if real-time
    pub numa_node:        u8,
    pub affinity_mask:    u64,      // bitmask of preferred cores
    pub weight:           f64,      // static weight (nice-level analog)
    pub is_pinned:        bool,     // cannot migrate
    pub credit:           f64,      // credit scheduler smoothing term
}

impl ProcessArm {
    pub fn new(pid: u32) -> Self {
        Self {
            pid,
            alpha: ALPHA_INIT, beta: BETA_INIT,
            last_sample: 0.5,
            last_run_ns: 0, last_core: 0,
            total_quanta: 0, total_reward: 0.0,
            ipc_rate: 1.0, cache_miss_rate: 0.0,
            voluntary_yields: 0, preemptions: 0,
            soft_deadline_ns: None,
            numa_node: 0, affinity_mask: !0u64,
            weight: 1.0, is_pinned: false, credit: 0.0,
        }
    }

    /// Thompson sample from Beta(α, β) using Johnk's method
    /// Pure no_std implementation — no RNG dependency
    pub fn thompson_sample(&mut self, rng_state: &mut u64) -> f64 {
        // Generate Beta(α, β) via ratio of Gamma samples
        // Gamma(k) approximated via Marsaglia-Tsang for k≥1, else squeeze
        let x = self.sample_gamma(self.alpha, rng_state);
        let y = self.sample_gamma(self.beta, rng_state);
        let sum = x + y;
        if sum < 1e-12 { return 0.5; }
        let val = x / sum;
        self.last_sample = if val < 0.0 { 0.0 } else if val > 1.0 { 1.0 } else { val };
        self.last_sample
    }

    /// Marsaglia-Tsang Gamma sampler (shape k, scale 1)
    fn sample_gamma(&self, k: f64, rng: &mut u64) -> f64 {
        if k < 1.0 {
            // Squeeze method for k < 1
            return self.sample_gamma(k + 1.0, rng) * libm::pow(self.xorshift_unit(rng), 1.0 / k);
        }
        let d = k - 1.0 / 3.0;
        let c = 1.0 / libm::sqrt(9.0 * d);
        loop {
            let x = self.randn(rng);
            let v_raw = 1.0 + c * x;
            if v_raw <= 0.0 { continue; }
            let v = v_raw * v_raw * v_raw;
            let u = self.xorshift_unit(rng);
            let x2 = x * x;
            // Accept-reject
            if u < 1.0 - 0.0331 * x2 * x2 { return d * v; }
            if libm::log(u) < 0.5 * x2 + d * (1.0 - v + libm::log(v)) { return d * v; }
        }
    }

    fn xorshift_unit(&self, rng: &mut u64) -> f64 {
        *rng ^= *rng << 13; *rng ^= *rng >> 7; *rng ^= *rng << 17;
        (*rng & 0x000FFFFFFFFFFFFF) as f64 / (0x000FFFFFFFFFFFFFu64 as f64)
    }

    fn randn(&self, rng: &mut u64) -> f64 {
        // Box-Muller (one sample)
        let u1_raw = self.xorshift_unit(rng);
        let u1 = if u1_raw < 1e-300 { 1e-300 } else { u1_raw };
        let u2 = self.xorshift_unit(rng);
        libm::sqrt(-2.0 * libm::log(u1)) * libm::cos(core::f64::consts::TAU * u2)
    }

    /// Update Beta posterior from observed reward
    /// reward ∈ [0, 1] — normalized scheduling efficiency
    pub fn update(&mut self, reward: f64) {
        let r = if reward < 0.0 { 0.0 } else if reward > 1.0 { 1.0 } else { reward };
        self.alpha += r;
        self.beta  += 1.0 - r;
        self.total_reward += r;
        self.total_quanta += 1;
        // Bayesian forgetting — exponential decay of old evidence
        // Prevents arms from becoming overconfident on stale history
        let decay = libm::pow(0.9995, self.total_quanta as f64);
        self.alpha = 1.0 + (self.alpha - 1.0) * decay;
        self.beta  = 1.0 + (self.beta  - 1.0) * decay;
    }

    /// Compute contextual reward signal from hardware counters
    /// r = ipc_weight * ipc + cache_weight * (1-miss) - preemption_penalty
    pub fn compute_reward(
        &self,
        quantum_used_ns: u64,
        quantum_granted_ns: u64,
        instructions: u64,
        llc_misses: u64,
        llc_refs: u64,
    ) -> f64 {
        // Throughput: fraction of quantum used productively
        let q_granted = if quantum_granted_ns == 0 { 1 } else { quantum_granted_ns };
        let util_val = quantum_used_ns as f64 / q_granted as f64;
        let utilization = if util_val > 1.0 { 1.0 } else { util_val };

        // IPC quality: higher is better (compute-bound is efficient)
        let cycles = (quantum_used_ns as f64 / 0.3) as u64; // ~3GHz approximation
        let ipc = if cycles > 0 { 
            let ipc_val = instructions as f64 / cycles as f64;
            (if ipc_val > 4.0 { 4.0 } else { ipc_val }) / 4.0 
        } else { 0.5 };

        // Cache efficiency
        let cache_eff = if llc_refs > 0 {
            let miss_val = llc_misses as f64 / llc_refs as f64;
            1.0 - (if miss_val > 1.0 { 1.0 } else { miss_val })
        } else { 0.8 };

        // Voluntarily yielded = well-behaved = bonus
        let yield_bonus = if quantum_used_ns < quantum_granted_ns / 2 { 0.1 } else { 0.0 };

        // Weighted sum
        0.4 * utilization + 0.3 * ipc + 0.2 * cache_eff + 0.1 * yield_bonus
    }

    /// Mean of Beta posterior — expected reward
    pub fn posterior_mean(&self) -> f64 {
        self.alpha / (self.alpha + self.beta)
    }

    /// Variance of Beta posterior — uncertainty
    pub fn posterior_variance(&self) -> f64 {
        let sum = self.alpha + self.beta;
        (self.alpha * self.beta) / (sum * sum * (sum + 1.0))
    }

    /// UCB-augmented Thompson score (adds exploration bonus for uncertain arms)
    pub fn ucb_score(&self) -> f64 {
        self.last_sample + EXPLORATION_BOOST * libm::sqrt(self.posterior_variance())
    }

    /// Soft EDF urgency: how close is this process to its deadline?
    /// Returns [0, 1] where 1 = MUST RUN NOW or miss deadline
    pub fn deadline_urgency(&self, now_ns: u64) -> f64 {
        match self.soft_deadline_ns {
            None => 0.0,
            Some(dl) => {
                if now_ns >= dl { return 1.0; } // already late
                let remaining = (dl - now_ns) as f64;
                let urgency_window = 50_000_000.0; // 50ms urgency horizon
                let urgency_val = 1.0 - remaining / urgency_window;
                if urgency_val < 0.0 { 0.0 } else if urgency_val > 1.0 { 1.0 } else { urgency_val }
            }
        }
    }

    /// Cache warmth score for a given core
    /// Returns [0,1] where 1 = very recently ran here (hot cache)
    pub fn cache_warmth(&self, core_id: u8, now_ns: u64) -> f64 {
        if self.last_core != core_id { return 0.0; }
        let age = now_ns.saturating_sub(self.last_run_ns);
        if age >= CACHE_WARM_THRESHOLD_NS { return 0.0; }
        1.0 - (age as f64 / CACHE_WARM_THRESHOLD_NS as f64)
    }

    /// NUMA locality score for a given core
    pub fn numa_score(&self, core_numa_node: u8) -> f64 {
        if core_numa_node == self.numa_node { 1.0 } else { 0.3 }
    }
}

// ─────────────────────────────────────────────
// PER-CORE BANDIT AGENT
// ─────────────────────────────────────────────

pub struct CoreBandit {
    pub core_id:        u8,
    pub numa_node:      u8,
    pub rng_state:      u64,
    pub tick:           AtomicU64,
    pub current_pid:    AtomicU32,
    pub quantum_end_ns: AtomicU64,
    pub idle_ns:        AtomicU64,
    pub context_switches: AtomicU64,
    pub last_gossip_tick: u64,
    pub local_arms:     BTreeMap<u32, ProcessArm>, // pid → arm (local copy)
    pub run_history:    Vec<(u32, f64, u64)>,       // (pid, reward, timestamp)
}

impl CoreBandit {
    pub fn new(core_id: u8, numa_node: u8) -> Self {
        Self {
            core_id, numa_node,
            rng_state: 0xdeadbeef_cafebabe ^ ((core_id as u64) * 0x9e3779b97f4a7c15),
            tick: AtomicU64::new(0),
            current_pid: AtomicU32::new(0),
            quantum_end_ns: AtomicU64::new(0),
            idle_ns: AtomicU64::new(0),
            context_switches: AtomicU64::new(0),
            last_gossip_tick: 0,
            local_arms: BTreeMap::new(),
            run_history: Vec::new(),
        }
    }

    /// Select next process to run — the core Thompson Sampling decision
    /// Returns (pid, quantum_ns)
    pub fn select_next(
        &mut self,
        runnable: &[u32],
        now_ns: u64,
    ) -> Option<(u32, u64)> {
        if runnable.is_empty() { return None; }

        let mut best_pid = 0u32;
        let mut best_score = -1.0f64;
        let core_id = self.core_id;
        let numa = self.numa_node;

        for &pid in runnable {
            let arm = self.local_arms.entry(pid).or_insert_with(|| ProcessArm::new(pid));

            // Thompson sample
            let theta = arm.thompson_sample(&mut self.rng_state);

            // Contextual multipliers
            let cache_w  = arm.cache_warmth(core_id, now_ns);
            let numa_w   = arm.numa_score(numa);
            let deadline = arm.deadline_urgency(now_ns);
            let weight   = arm.weight;

            // Check affinity
            if arm.is_pinned && (arm.affinity_mask & (1u64 << core_id)) == 0 {
                continue; // hard affinity — skip
            }

            // Composite score: bandit + contextual + deadline override
            let score = theta * weight
                + 0.20 * cache_w       // cache warmth bonus
                + 0.10 * numa_w        // NUMA locality
                + 3.00 * deadline      // EDF urgency override
                + arm.ucb_score() * 0.05; // exploration

            if score > best_score {
                best_score = score;
                best_pid = pid;
            }
        }

        if best_pid == 0 { return None; }

        // Compute adaptive quantum
        let quantum = self.adaptive_quantum(best_pid, now_ns);

        self.current_pid.store(best_pid, Ordering::Relaxed);
        self.quantum_end_ns.store(now_ns + quantum, Ordering::Relaxed);
        self.context_switches.fetch_add(1, Ordering::Relaxed);
        self.tick.fetch_add(1, Ordering::Relaxed);

        if let Some(arm) = self.local_arms.get_mut(&best_pid) {
            arm.last_run_ns = now_ns;
            arm.last_core = core_id;
        }

        Some((best_pid, quantum))
    }

    /// Adaptive quantum: process-type-aware time slice allocation
    /// High-IPC compute gets longer; high-miss I/O gets shorter
    pub fn adaptive_quantum(&self, pid: u32, now_ns: u64) -> u64 {
        let arm = match self.local_arms.get(&pid) {
            Some(a) => a, None => return QUANTUM_NS,
        };

        // EDF processes get minimum quantum (fast preemption for deadline)
        if let Some(dl) = arm.soft_deadline_ns {
            if dl.saturating_sub(now_ns) < QUANTUM_NS * 4 {
                return MIN_QUANTUM;
            }
        }

        // Base quantum scaled by IPC (compute-bound → longer slice)
        // I/O bound (low IPC) → shorter to free CPU for others sooner
        let ipc_factor = arm.ipc_rate / 2.0;
        let ipc_factor = if ipc_factor < 0.5 { 0.5 } else if ipc_factor > 2.0 { 2.0 } else { ipc_factor };
        let cache_factor = 1.0 + (1.0 - arm.cache_miss_rate) * 0.5;
        let q = (QUANTUM_NS as f64 * ipc_factor * cache_factor) as u64;
        if q < MIN_QUANTUM { MIN_QUANTUM } else if q > MAX_QUANTUM { MAX_QUANTUM } else { q }
    }

    /// Record completion of a scheduled quantum and update posterior
    pub fn record_completion(
        &mut self,
        pid: u32,
        quantum_used_ns: u64,
        quantum_granted_ns: u64,
        instructions: u64,
        llc_misses: u64,
        llc_refs: u64,
        voluntary: bool,
    ) {
        let arm = self.local_arms.entry(pid).or_insert_with(|| ProcessArm::new(pid));
        let reward = arm.compute_reward(quantum_used_ns, quantum_granted_ns, instructions, llc_misses, llc_refs);
        if voluntary { arm.voluntary_yields += 1; } else { arm.preemptions += 1; }
        arm.update(reward);
        let now = self.tick.load(Ordering::Relaxed);
        self.run_history.push((pid, reward, now));
        if self.run_history.len() > 256 { self.run_history.remove(0); }
    }

    /// Gossip: merge another core's arm knowledge into this core's model
    /// Bayesian model averaging: new_α = (α_local + α_remote) / 2 + 1
    pub fn gossip_merge(&mut self, remote_arms: &BTreeMap<u32, ProcessArm>) {
        for (&pid, remote) in remote_arms {
            let local = self.local_arms.entry(pid).or_insert_with(|| ProcessArm::new(pid));
            // Bayesian merge: average the sufficient statistics
            local.alpha = (local.alpha + remote.alpha) / 2.0 + 0.1;
            local.beta  = (local.beta  + remote.beta)  / 2.0 + 0.1;
            // Copy perf metadata from whichever core ran it more recently
            if remote.last_run_ns > local.last_run_ns {
                local.ipc_rate       = remote.ipc_rate;
                local.cache_miss_rate= remote.cache_miss_rate;
                local.soft_deadline_ns= remote.soft_deadline_ns;
            }
        }
        self.last_gossip_tick = self.tick.load(Ordering::Relaxed);
    }

    /// Should this core steal work from another?
    pub fn should_steal(&self) -> bool {
        let idle = self.idle_ns.load(Ordering::Relaxed);
        idle > 2_000_000 // steal after 2ms idle
    }

    /// Regret: difference between best possible and actual reward
    pub fn cumulative_regret(&self) -> f64 {
        if self.run_history.is_empty() { return 0.0; }
        let best = self.local_arms.values()
            .map(|a| a.posterior_mean())
            .fold(0.0f64, |acc, x| if x > acc { x } else { acc });
        self.run_history.iter()
            .map(|(_, r, _)| { let diff = best - r; if diff > 0.0 { diff } else { 0.0 } })
            .sum()
    }
}

// ─────────────────────────────────────────────
// GLOBAL SCHEDULER — HYDRA
// ─────────────────────────────────────────────

pub struct Hydra {
    pub cores:         Vec<CoreBandit>,
    pub global_arms:   BTreeMap<u32, ProcessArm>,
    pub runqueues:     Vec<Vec<u32>>,  // per-core runqueue
    pub global_queue:  Vec<u32>,
    pub wall_ns:       AtomicU64,
    pub total_switches: AtomicU64,
    pub migration_count: AtomicU64,
    pub tick:          u64,
}

impl Hydra {
    pub fn new(num_cores: usize) -> Self {
        let mut cores = Vec::new();
        let mut runqueues = Vec::new();
        // Simple NUMA layout: cores 0-63 on node 0, 64-127 on node 1, etc.
        let num_cores = if num_cores < MAX_CORES { num_cores } else { MAX_CORES };
        for i in 0..num_cores {
            cores.push(CoreBandit::new(i as u8, (i / 64) as u8));
            runqueues.push(Vec::new());
        }
        Self {
            cores, global_arms: BTreeMap::new(),
            runqueues, global_queue: Vec::new(),
            wall_ns: AtomicU64::new(0),
            total_switches: AtomicU64::new(0),
            migration_count: AtomicU64::new(0),
            tick: 0,
        }
    }

    /// Admit a new process into the scheduler
    pub fn admit(&mut self, pid: u32, affinity: u64, numa_pref: u8, weight: f64, deadline: Option<u64>) {
        let mut arm = ProcessArm::new(pid);
        arm.affinity_mask = affinity;
        arm.numa_node = numa_pref;
        arm.weight = weight;
        arm.soft_deadline_ns = deadline;
        self.global_arms.insert(pid, arm.clone());
        // Place on best core's runqueue
        let best_core = self.best_core_for(pid);
        if let Some(rq) = self.runqueues.get_mut(best_core) {
            rq.push(pid);
        }
        // Propagate to all core local models (sparse — only when gossiping)
        if let Some(core) = self.cores.get_mut(best_core) {
            core.local_arms.insert(pid, arm);
        }
    }

    /// Find the best core to initially place a process on
    fn best_core_for(&self, pid: u32) -> usize {
        let arm = match self.global_arms.get(&pid) { Some(a) => a, None => return 0 };
        let mut best = 0;
        let mut best_load = u64::MAX;
        for (i, rq) in self.runqueues.iter().enumerate() {
            // Check affinity
            if (arm.affinity_mask & (1u64 << (i % 64))) == 0 { continue; }
            // Prefer matching NUMA node
            let numa_penalty = if self.cores[i].numa_node != arm.numa_node { 1000 } else { 0 };
            let load = rq.len() as u64 + numa_penalty;
            if load < best_load { best_load = load; best = i; }
        }
        best
    }

    /// Remove a terminated process
    pub fn terminate(&mut self, pid: u32) {
        self.global_arms.remove(&pid);
        for rq in &mut self.runqueues { rq.retain(|&p| p != pid); }
        self.global_queue.retain(|&p| p != pid);
        for core in &mut self.cores { core.local_arms.remove(&pid); }
    }

    /// Schedule tick: each core selects its next process
    pub fn tick(&mut self, now_ns: u64) -> Vec<(u8, u32, u64)> {
        self.tick += 1;
        self.wall_ns.store(now_ns, Ordering::Relaxed);
        let mut decisions = Vec::new();

        for i in 0..self.cores.len() {
            let runnable: Vec<u32> = self.runqueues[i].clone();
            if runnable.is_empty() {
                self.cores[i].idle_ns.fetch_add(QUANTUM_NS, Ordering::Relaxed);
                continue;
            }
            if let Some((pid, quantum)) = self.cores[i].select_next(&runnable, now_ns) {
                self.total_switches.fetch_add(1, Ordering::Relaxed);
                decisions.push((i as u8, pid, quantum));
            }
        }

        // Work stealing: idle cores steal from busiest
        self.work_steal(now_ns);

        // Gossip: periodically share model across cores
        if self.tick % GOSSIP_INTERVAL_TICKS == 0 {
            self.gossip_round();
        }

        decisions
    }

    /// Work stealing: move processes from overloaded cores to idle ones
    pub fn work_steal(&mut self, _now_ns: u64) {
        let loads: Vec<usize> = self.runqueues.iter().map(|rq| rq.len()).collect();
        let total_load: usize = loads.iter().sum();
        if total_load == 0 { return; }
        let num_cores = if self.cores.is_empty() { 1 } else { self.cores.len() };
        let avg_load = total_load / num_cores;

        let idle_cores: Vec<usize> = loads.iter().enumerate()
            .filter(|(i, l)| **l == 0 && self.cores[*i].should_steal())
            .map(|(i, _)| i)
            .collect();
        let busy_cores: Vec<usize> = loads.iter().enumerate()
            .filter(|(_, l)| **l > avg_load + 1)
            .map(|(i, _)| i)
            .collect();

        for idle in &idle_cores {
            if let Some(&busy) = busy_cores.first() {
                // Steal the process with LOWEST affinity for the busy core
                // (the one that won't lose much cache warmth)
                let steal_pid = self.runqueues[busy].iter()
                    .filter(|&&pid| {
                        let arm = self.global_arms.get(&pid);
                        arm.map(|a| !a.is_pinned &&
                            (a.affinity_mask & (1u64 << (*idle % 64))) != 0)
                            .unwrap_or(false)
                    })
                    .cloned()
                    .next();

                if let Some(pid) = steal_pid {
                    self.runqueues[busy].retain(|&p| p != pid);
                    self.runqueues[*idle].push(pid);
                    // Penalize stolen process (migration cost = cache miss)
                    if let Some(core) = self.cores.get_mut(*idle) {
                        let arm = core.local_arms.entry(pid)
                            .or_insert_with(|| ProcessArm::new(pid));
                        arm.last_core = busy as u8; // invalidate cache warmth
                        arm.beta += 0.5; // migration cost — mild negative reward
                    }
                    self.migration_count.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// Gossip round: ring-topology model sharing between adjacent cores
    pub fn gossip_round(&mut self) {
        let n = self.cores.len();
        if n < 2 { return; }
        // Ring gossip: core i shares with core (i+1) % n
        let snapshots: Vec<BTreeMap<u32, ProcessArm>> = self.cores.iter()
            .map(|c| c.local_arms.clone())
            .collect();
        for i in 0..n {
            let neighbor = (i + 1) % n;
            let neighbor_arms = snapshots[neighbor].clone();
            self.cores[i].gossip_merge(&neighbor_arms);
        }
    }

    /// Global scheduler health metrics
    pub fn metrics(&self) -> SchedulerMetrics {
        let total_regret: f64 = self.cores.iter().map(|c| c.cumulative_regret()).sum();
        let avg_posterior: f64 = if self.global_arms.is_empty() { 0.5 } else {
            self.global_arms.values().map(|a| a.posterior_mean()).sum::<f64>()
                / self.global_arms.len() as f64
        };
        let load_variance: f64 = {
            let loads: Vec<f64> = self.runqueues.iter().map(|rq| rq.len() as f64).collect();
            let mut total = 0.0;
            for &l in &loads { total += l; }
            let len = if loads.is_empty() { 1.0 } else { loads.len() as f64 };
            let mean = total / len;
            let mut sum_sq = 0.0;
            for &l in &loads { sum_sq += (l - mean) * (l - mean); }
            sum_sq / len
        };
        SchedulerMetrics {
            total_context_switches: self.total_switches.load(Ordering::Relaxed),
            total_migrations: self.migration_count.load(Ordering::Relaxed),
            cumulative_regret: total_regret,
            avg_posterior_reward: avg_posterior,
            load_variance,
            num_processes: self.global_arms.len() as u32,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SchedulerMetrics {
    pub total_context_switches: u64,
    pub total_migrations:       u64,
    pub cumulative_regret:      f64,
    pub avg_posterior_reward:   f64,
    pub load_variance:          f64,
    pub num_processes:          u32,
}
