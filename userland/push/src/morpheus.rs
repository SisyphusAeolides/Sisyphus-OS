use alloc::{collections::BTreeMap, string::String, vec, vec::Vec};
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

// ─────────────────────────────────────────────
// SLEEP STAGES
// ─────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum SleepStage {
    Awake = 0,
    Nrem1 = 1,     // Light sleep — 5-15 min idle
    Nrem2 = 2,     // Sleep spindles — 15-60 min idle
    Nrem3 = 3,     // Slow-wave — 1-4 hr idle
    Nrem4 = 4,     // Deep slow-wave — 4+ hr idle
    Rem = 5,       // Dreaming — predictive prefetch phase
    Suspended = 6, // Cryogenic — cgroup frozen
}

impl SleepStage {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Nrem1,
            2 => Self::Nrem2,
            3 => Self::Nrem3,
            4 => Self::Nrem4,
            5 => Self::Rem,
            6 => Self::Suspended,
            _ => Self::Awake,
        }
    }

    pub fn deepen(self) -> Self {
        match self {
            Self::Awake => Self::Nrem1,
            Self::Nrem1 => Self::Nrem2,
            Self::Nrem2 => Self::Nrem3,
            Self::Nrem3 => Self::Rem, // cycle through REM before NREM4
            Self::Rem => Self::Nrem4,
            Self::Nrem4 => Self::Suspended,
            Self::Suspended => Self::Suspended,
        }
    }

    pub fn lighten(self) -> Self {
        match self {
            Self::Suspended => Self::Nrem4,
            Self::Nrem4 => Self::Rem,
            Self::Rem => Self::Nrem3,
            Self::Nrem3 => Self::Nrem2,
            Self::Nrem2 => Self::Nrem1,
            Self::Nrem1 => Self::Awake,
            Self::Awake => Self::Awake,
        }
    }

    /// Resource release multiplier — how much does this stage free?
    pub fn resource_release_pct(&self) -> f64 {
        match self {
            Self::Awake => 0.0,
            Self::Nrem1 => 0.1,      // 10% heap released
            Self::Nrem2 => 0.3,      // 30% + IPC queue compressed
            Self::Nrem3 => 0.6,      // 60% + FDs released
            Self::Nrem4 => 0.85,     // 85% + sockets suspended
            Self::Rem => 0.75,       // 75% (slightly more awake for prefetch)
            Self::Suspended => 0.99, // 99% — only kernel footprint remains
        }
    }

    pub fn wake_latency_ms(&self) -> u64 {
        match self {
            Self::Awake => 0,
            Self::Nrem1 => 5,
            Self::Nrem2 => 50,
            Self::Nrem3 => 200,
            Self::Nrem4 => 500,
            Self::Rem => 100, // REM is pre-warming — faster than NREM4
            Self::Suspended => 2000,
        }
    }
}

// ─────────────────────────────────────────────
// DREAM JOURNAL — Compressed State Log
// ─────────────────────────────────────────────

/// LZ78-inspired dictionary compressor for heap snapshots
pub struct DreamCompressor {
    dictionary: BTreeMap<Vec<u8>, u16>,
    next_code: u16,
}

impl DreamCompressor {
    pub fn new() -> Self {
        Self {
            dictionary: BTreeMap::new(),
            next_code: 256,
        }
    }

    /// Compress a heap snapshot to a dream token stream
    pub fn compress(&mut self, data: &[u8]) -> Vec<u16> {
        let mut output = Vec::new();
        let mut window: Vec<u8> = Vec::new();

        for &byte in data {
            let mut extended = window.clone();
            extended.push(byte);

            if self.dictionary.contains_key(&extended) {
                window = extended;
            } else {
                // Emit code for current window
                if window.is_empty() {
                    output.push(byte as u16);
                } else if let Some(&code) = self.dictionary.get(&window) {
                    output.push(code);
                } else {
                    output.push(byte as u16);
                }

                // Add new entry to dictionary
                if self.next_code < 65535 {
                    self.dictionary.insert(extended, self.next_code);
                    self.next_code += 1;
                }
                window.clear();
                window.push(byte);
            }
        }

        // Flush remaining window
        if !window.is_empty() {
            if let Some(&code) = self.dictionary.get(&window) {
                output.push(code);
            } else if window.len() == 1 {
                output.push(window[0] as u16);
            }
        }
        output
    }

    /// Delta encode — store only changes between snapshots
    pub fn delta_encode(prev: &[u8], curr: &[u8]) -> Vec<(usize, u8)> {
        let len = if prev.len() < curr.len() {
            prev.len()
        } else {
            curr.len()
        };
        let mut deltas = Vec::new();
        for i in 0..len {
            if prev[i] != curr[i] {
                deltas.push((i, curr[i]));
            }
        }
        // Append any new bytes beyond prev length
        for i in len..curr.len() {
            deltas.push((i, curr[i]));
        }
        deltas
    }
}

/// Dream journal entry — one checkpoint in a service's sleep cycle
#[derive(Clone)]
pub struct DreamEntry {
    pub timestamp_ns: u64,
    pub stage: SleepStage,
    pub heap_tokens: Vec<u16>,           // compressed heap snapshot
    pub delta_patches: Vec<(usize, u8)>, // delta from previous entry
    pub fd_list: Vec<i32>,               // open file descriptors at checkpoint
    pub env_hash: u64,                   // xxhash of environment
    pub markov_state: [u8; 4],           // 2nd-order Markov state at checkpoint
}

// ─────────────────────────────────────────────
// MARKOV WAKE PREDICTOR
// ─────────────────────────────────────────────

/// 2nd-order Markov chain predicting when a service will be accessed next
/// State = (last_access_type, second_last_access_type)
/// Transitions built from historical access log
pub struct MarkovWakePredictor {
    /// Transition counts: map[(s0,s1)] → map[s2 → count]
    transitions: BTreeMap<(u8, u8), [u32; 16]>, // 16 possible next states
    pub state: (u8, u8),
    pub prediction_accuracy: f64,
    correct_predictions: u64,
    total_predictions: u64,
}

impl MarkovWakePredictor {
    pub fn new() -> Self {
        Self {
            transitions: BTreeMap::new(),
            state: (0, 0),
            prediction_accuracy: 0.5,
            correct_predictions: 0,
            total_predictions: 0,
        }
    }

    /// Record an access event (0-15 event type codes)
    pub fn record_access(&mut self, event_code: u8) {
        let event_code = (event_code & 0xF) as usize;
        let entry = self.transitions.entry(self.state).or_insert([0u32; 16]);
        entry[event_code] = entry[event_code].saturating_add(1);
        self.state = (self.state.1, event_code as u8);
    }

    /// Predict most likely next access event
    pub fn predict_next(&self) -> u8 {
        if let Some(counts) = self.transitions.get(&self.state) {
            counts
                .iter()
                .enumerate()
                .max_by_key(|(_, c)| **c)
                .map(|(i, _)| i as u8)
                .unwrap_or(0)
        } else {
            // No history — fall back to 1st-order
            let s1_state = (0u8, self.state.1);
            if let Some(counts) = self.transitions.get(&s1_state) {
                counts
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, c)| **c)
                    .map(|(i, _)| i as u8)
                    .unwrap_or(0)
            } else {
                0
            }
        }
    }

    /// Predicted time until next wake (in ms) based on inter-arrival statistics
    pub fn predicted_wake_ms(&self) -> u64 {
        // Use transition entropy as proxy for regularity
        // Low entropy = predictable = we know when it'll wake
        let entropy = self.state_entropy();
        // High entropy → shorter predicted interval (more random = wake sooner)
        ((1.0 - entropy) * 10000.0 + 100.0) as u64
    }

    /// Shannon entropy of current state's transition distribution
    pub fn state_entropy(&self) -> f64 {
        if let Some(counts) = self.transitions.get(&self.state) {
            let total: u32 = counts.iter().sum();
            if total == 0 {
                return 1.0;
            }
            let n = total as f64;
            -counts
                .iter()
                .filter(|&&c| c > 0)
                .map(|&c| {
                    let p = c as f64 / n;
                    p * libm::log2(p)
                })
                .sum::<f64>()
                / 4.0 // normalize by log2(16)
        } else {
            1.0
        }
    }

    /// Update prediction accuracy metric
    pub fn validate_prediction(&mut self, predicted: u8, actual: u8) {
        self.total_predictions += 1;
        if predicted == actual {
            self.correct_predictions += 1;
        }
        if self.total_predictions > 0 {
            self.prediction_accuracy =
                self.correct_predictions as f64 / self.total_predictions as f64;
        }
    }
}

// ─────────────────────────────────────────────
// SLEEPING SERVICE RECORD
// ─────────────────────────────────────────────

pub struct SleepingService {
    pub pid: u32,
    pub name: String,
    pub stage: AtomicU8,
    pub idle_since_ns: AtomicU64,
    pub last_wake_ns: AtomicU64,
    pub dream_journal: Vec<DreamEntry>,
    pub markov: MarkovWakePredictor,
    pub compressor: DreamCompressor,
    pub prefetch_ready: bool, // true if REM pre-warming completed
    pub checkpoint_count: u64,
    pub total_sleep_ns: u64,
    pub sleep_efficiency: f64, // sleep time / idle time — how well we're hibernating
    pub heap_snapshot: Vec<u8>, // last full heap snapshot
}

impl SleepingService {
    pub fn new(pid: u32, name: &str) -> Self {
        Self {
            pid,
            name: String::from(name),
            stage: AtomicU8::new(SleepStage::Awake as u8),
            idle_since_ns: AtomicU64::new(0),
            last_wake_ns: AtomicU64::new(0),
            dream_journal: Vec::new(),
            markov: MarkovWakePredictor::new(),
            compressor: DreamCompressor::new(),
            prefetch_ready: false,
            checkpoint_count: 0,
            total_sleep_ns: 0,
            sleep_efficiency: 0.0,
            heap_snapshot: Vec::new(),
        }
    }

    pub fn current_stage(&self) -> SleepStage {
        SleepStage::from_u8(self.stage.load(Ordering::Relaxed))
    }

    /// Deepen sleep — called when idle timer fires
    pub fn deepen_sleep(&self) {
        let current = self.current_stage();
        let next = current.deepen();
        self.stage.store(next as u8, Ordering::Relaxed);
    }

    /// Wake up — return to awake state, record wake event
    pub fn wake(&self, now_ns: u64, _wake_event: u8) {
        let _prev_stage = self.current_stage();
        self.stage.store(SleepStage::Awake as u8, Ordering::Relaxed);
        self.last_wake_ns.store(now_ns, Ordering::Relaxed);
    }

    /// Create a dream journal checkpoint at current sleep stage
    pub fn checkpoint(&mut self, now_ns: u64, heap_data: &[u8]) {
        let stage = self.current_stage();
        let delta = DreamCompressor::delta_encode(&self.heap_snapshot, heap_data);
        let tokens = self.compressor.compress(heap_data);
        let markov_state = [self.markov.state.0, self.markov.state.1, 0, 0];
        let env_hash = heap_data
            .iter()
            .enumerate()
            .fold(0u64, |h, (i, &b)| h ^ (b as u64).wrapping_mul(i as u64 + 1));

        let entry = DreamEntry {
            timestamp_ns: now_ns,
            stage,
            heap_tokens: tokens,
            delta_patches: delta,
            fd_list: Vec::new(), // populated by caller
            env_hash,
            markov_state,
        };
        self.dream_journal.push(entry);
        if self.dream_journal.len() > 32 {
            self.dream_journal.remove(0);
        }
        self.heap_snapshot = heap_data.to_vec();
        self.checkpoint_count += 1;
    }

    /// REM dreaming: simulate next-access pre-computation
    /// Returns list of memory addresses to prefetch
    pub fn rem_prefetch_plan(&self) -> Vec<u64> {
        let next_event = self.markov.predict_next();
        // Map predicted event code to likely memory regions (heuristic)
        let base: u64 = self.pid as u64 * 0x1000;
        match next_event {
            0 => vec![base, base + 0x1000, base + 0x2000], // sequential read
            1 => vec![base + 0x8000, base + 0x9000],       // heap access
            2 => vec![base + 0x4000, base + 0x5000, base + 0x6000], // stack
            3 => vec![base + 0x10000],                     // mmap region
            _ => vec![base],
        }
    }

    pub fn compression_ratio(&self) -> f64 {
        if self.heap_snapshot.is_empty() {
            return 1.0;
        }
        if let Some(last) = self.dream_journal.last() {
            let compressed_size = last.heap_tokens.len() * 2 + last.delta_patches.len() * 9;
            let csize = if compressed_size == 0 {
                1
            } else {
                compressed_size
            };
            self.heap_snapshot.len() as f64 / csize as f64
        } else {
            1.0
        }
    }
}

// ─────────────────────────────────────────────
// MORPHEUS — The Dream Master
// ─────────────────────────────────────────────

pub struct Morpheus {
    pub services: BTreeMap<u32, SleepingService>,
    pub wall_ns: u64,
    /// Idle thresholds per stage (nanoseconds of idle time to trigger deepening)
    pub stage_thresholds: [u64; 6],
    /// Pre-wake lead time: how early to start REM prefetch before predicted wake
    pub prefetch_lead_ns: u64,
    pub total_bytes_freed: u64,
    pub total_checkpoints: u64,
    pub rem_sessions: u64,
}

impl Morpheus {
    pub fn new() -> Self {
        Self {
            services: BTreeMap::new(),
            wall_ns: 0,
            stage_thresholds: [
                5_000_000_000,      // NREM1:  5s idle
                60_000_000_000,     // NREM2: 60s idle
                600_000_000_000,    // NREM3: 10m idle
                3_600_000_000_000,  // REM:   60m idle (REM before deep NREM4)
                7_200_000_000_000,  // NREM4: 2hr idle
                86_400_000_000_000, // SUSPENDED: 24hr idle
            ],
            prefetch_lead_ns: 500_000_000, // 500ms lead time for REM prefetch
            total_bytes_freed: 0,
            total_checkpoints: 0,
            rem_sessions: 0,
        }
    }

    pub fn register(&mut self, pid: u32, name: &str) {
        self.services.insert(pid, SleepingService::new(pid, name));
    }

    /// Mark a service as active — resets idle timer
    pub fn mark_active(&mut self, pid: u32) {
        if let Some(svc) = self.services.get_mut(&pid) {
            let prev_stage = svc.current_stage();
            svc.idle_since_ns.store(0, Ordering::Relaxed);
            svc.wake(self.wall_ns, 0);
            svc.markov.record_access(0); // wake event = type 0
            if prev_stage != SleepStage::Awake {
                svc.prefetch_ready = false;
            }
        }
    }

    /// Tick: advance time, check idle timers, deepen/enter REM as appropriate
    pub fn tick(&mut self, wall_delta_ns: u64) -> Vec<(u32, SleepStage, SleepAction)> {
        self.wall_ns += wall_delta_ns;
        let mut actions = Vec::new();

        let pids: Vec<u32> = self.services.keys().cloned().collect();
        for pid in pids {
            let action = self.evaluate_service(pid, wall_delta_ns);
            if let Some((stage, act)) = action {
                actions.push((pid, stage, act));
            }
        }
        actions
    }

    fn evaluate_service(&mut self, pid: u32, _dt_ns: u64) -> Option<(SleepStage, SleepAction)> {
        let svc = self.services.get_mut(&pid)?;
        let stage = svc.current_stage();

        if stage == SleepStage::Awake {
            // Accumulate idle time — caller is responsible for calling mark_active
            let idle_ns = svc.idle_since_ns.load(Ordering::Relaxed);
            if idle_ns == 0 {
                svc.idle_since_ns.store(self.wall_ns, Ordering::Relaxed);
                return None;
            }
            let elapsed = self.wall_ns.saturating_sub(idle_ns);
            if elapsed >= self.stage_thresholds[0] {
                svc.deepen_sleep();
                return Some((SleepStage::Nrem1, SleepAction::PageOutHeap(10)));
            }
            return None;
        }

        let idle_since = svc.idle_since_ns.load(Ordering::Relaxed);
        let total_idle = self.wall_ns.saturating_sub(idle_since);

        // Check if we should deepen further
        let threshold_idx = stage as usize;
        if threshold_idx < self.stage_thresholds.len() {
            if total_idle >= self.stage_thresholds[threshold_idx] {
                let next = stage.deepen();
                svc.stage.store(next as u8, Ordering::Relaxed);
                self.total_checkpoints += 1;

                let action = match next {
                    SleepStage::Nrem2 => SleepAction::CheckpointIpc,
                    SleepStage::Nrem3 => SleepAction::ReleaseFds,
                    SleepStage::Rem => {
                        self.rem_sessions += 1;
                        svc.prefetch_ready = false;
                        SleepAction::BeginRemPrefetch(svc.rem_prefetch_plan())
                    }
                    SleepStage::Nrem4 => SleepAction::SuspendSockets,
                    SleepStage::Suspended => SleepAction::CgroupFreeze,
                    _ => SleepAction::None,
                };
                return Some((next, action));
            }
        }

        // REM phase: check if predicted wake is imminent — begin pre-warming
        if stage == SleepStage::Rem && !svc.prefetch_ready {
            let predicted_wake = svc.markov.predicted_wake_ms() * 1_000_000;
            if predicted_wake <= self.prefetch_lead_ns {
                svc.prefetch_ready = true;
                let plan = svc.rem_prefetch_plan();
                return Some((SleepStage::Rem, SleepAction::PreWarm(plan)));
            }
        }

        None
    }

    /// Force-wake a service (e.g., incoming IPC wakes a sleeping service)
    pub fn force_wake(&mut self, pid: u32, reason: WakeReason) -> Option<u64> {
        let svc = self.services.get_mut(&pid)?;
        let stage = svc.current_stage();
        let latency_ms = stage.wake_latency_ms();
        svc.wake(self.wall_ns, reason as u8);
        svc.markov.record_access(reason as u8);
        Some(latency_ms)
    }

    /// Summary: bytes freed across all sleeping services
    pub fn total_resource_freed_pct(&self) -> f64 {
        if self.services.is_empty() {
            return 0.0;
        }
        self.services
            .values()
            .map(|s| s.current_stage().resource_release_pct())
            .sum::<f64>()
            / self.services.len() as f64
    }

    /// Services currently in REM — dreaming and pre-warming
    pub fn dreaming_services(&self) -> Vec<(u32, Vec<u64>)> {
        self.services
            .values()
            .filter(|s| s.current_stage() == SleepStage::Rem && s.prefetch_ready)
            .map(|s| (s.pid, s.rem_prefetch_plan()))
            .collect()
    }

    /// Return hypnogram: sleep stage history for a service
    pub fn hypnogram(&self, pid: u32) -> Vec<(u64, SleepStage)> {
        self.services
            .get(&pid)
            .map(|s| {
                s.dream_journal
                    .iter()
                    .map(|e| (e.timestamp_ns, e.stage))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Actions Morpheus sends to the service supervisor
#[derive(Clone, Debug)]
pub enum SleepAction {
    None,
    PageOutHeap(u8),            // page out N% of heap
    CheckpointIpc,              // serialize and compress IPC queues
    ReleaseFds,                 // close non-essential file descriptors
    BeginRemPrefetch(Vec<u64>), // start predictive memory prefetch
    SuspendSockets,             // TCP keepalive only, data sockets suspended
    CgroupFreeze,               // cgroup v2 freeze the entire service
    PreWarm(Vec<u64>),          // pre-load pages back into cache
}

#[derive(Clone, Copy)]
pub enum WakeReason {
    IncomingIpc = 1,
    TimerFired = 2,
    UserInput = 3,
    PeerWoke = 4,
    WatchdogKick = 5,
    SystemEvent = 6,
}
