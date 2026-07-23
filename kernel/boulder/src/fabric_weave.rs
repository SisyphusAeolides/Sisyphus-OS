// kernel/boulder/src/fabric_weave.rs
// #![no_std] inherited
//
// FABRIC WEAVE — Causal Spacetime Execution Graph
//
// Every kernel event = a node at coordinates (logical_time, physical_tick_ns)
// Causal edge A→B: event A must complete before event B begins
// Light cone: event A can causally influence event B iff
//   logical_time(B) > logical_time(A)   AND
//   physical_tick(B) >= physical_tick(A) + MIN_CAUSAL_GAP_NS
//
// Causal violation: edge A→B exists but B already completed before A
//   → paradox → kernel raises SIGCAUSAL (new signal class) to offending process
//
// Topological sort: Kahn's algorithm on the live graph
//   Used to determine safe execution order for kernel subsystems
//   If cycle detected → deadlock predicted before it occurs
//
// Event types modeled:
//   Syscall, IpcSend, IpcRecv, PageFault, IrqFire, MemAlloc, MemFree,
//   ScheduleIn, ScheduleOut, EpochAdvance
//
// Cone analysis:
//   PAST cone of event E:  all events that could have caused E
//   FUTURE cone of event E: all events E could cause
//   ELSEWHERE:             spacelike-separated events (cannot causally interact)
//   Elsewhere events are safe to execute in parallel → parallelism oracle

#![allow(dead_code)]
extern crate alloc;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

// ─────────────────────────────────────────────
// CONSTANTS
// ─────────────────────────────────────────────

pub const MAX_EVENTS: usize = 4096;
pub const MAX_EDGES: usize = 16384;
pub const MIN_CAUSAL_GAP_NS: u64 = 100; // minimum physical time for causality
pub const CAUSAL_CONE_DEPTH: usize = 16; // max depth to trace causal cone
pub const TOPO_SORT_MAX_ITERS: usize = MAX_EVENTS * 2;
pub const EVENT_RETAIN_TICKS: u64 = 1024; // evict events older than this

// ─────────────────────────────────────────────
// EVENT TYPES
// ─────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum EventKind {
    Syscall = 0,
    IpcSend = 1,
    IpcRecv = 2,
    PageFault = 3,
    IrqFire = 4,
    MemAlloc = 5,
    MemFree = 6,
    ScheduleIn = 7,
    ScheduleOut = 8,
    EpochAdvance = 9,
    CausalViolation = 10,
}

// ─────────────────────────────────────────────
// SPACETIME EVENT NODE
// ─────────────────────────────────────────────

#[derive(Clone, Copy)]
pub struct SpacetimeEvent {
    pub id: u64, // unique event ID (monotonic)
    pub kind: EventKind,
    pub pid: u32,
    pub logical_time: u64,  // Lamport / vector clock
    pub physical_ns: u64,   // wall-clock nanoseconds
    pub semantic_hash: u64, // content fingerprint
    pub completed: bool,
    pub in_degree: u32,     // for topological sort
    pub cone_visited: bool, // scratch flag for cone traversal
    pub is_violation: bool,
    pub core_id: u8,
    pub numa_node: u8,
}

impl SpacetimeEvent {
    pub fn new(
        id: u64,
        kind: EventKind,
        pid: u32,
        logical: u64,
        physical_ns: u64,
        core: u8,
    ) -> Self {
        Self {
            id,
            kind,
            pid,
            logical_time: logical,
            physical_ns,
            semantic_hash: id.wrapping_mul(0x9e3779b97f4a7c15) ^ (kind as u64),
            completed: false,
            in_degree: 0,
            cone_visited: false,
            is_violation: false,
            core_id: core,
            numa_node: core / 64,
        }
    }

    /// Can this event causally influence `other`?
    pub fn can_cause(&self, other: &SpacetimeEvent) -> bool {
        other.logical_time > self.logical_time
            && other.physical_ns >= self.physical_ns + MIN_CAUSAL_GAP_NS
    }

    /// Is this event spacelike-separated from `other` (safe to parallelize)?
    pub fn is_elsewhere(&self, other: &SpacetimeEvent) -> bool {
        !self.can_cause(other) && !other.can_cause(self)
    }
}

// ─────────────────────────────────────────────
// CAUSAL EDGE
// ─────────────────────────────────────────────

#[derive(Clone, Copy)]
pub struct CausalEdge {
    pub from: u64, // event ID
    pub to: u64,
    pub weight: u32, // causal strength (1 = hard dependency, lower = soft)
    pub kind: EdgeKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    HardDependency,  // must-happen-before
    DataFlow,        // data produced by `from` consumed by `to`
    MemoryOrder,     // memory fence / ordering constraint
    IpcChannel,      // message delivery causality
    SpeculativeHint, // soft — can be violated with rollback cost
}

// ─────────────────────────────────────────────
// CAUSAL FABRIC WEAVER
// ─────────────────────────────────────────────

pub struct FabricWeave {
    pub events: BTreeMap<u64, SpacetimeEvent>,
    pub edges: Vec<CausalEdge>,
    pub next_event_id: AtomicU64,
    pub logical_clock: AtomicU64,
    pub topo_order: Vec<u64>, // result of last topological sort
    pub topo_valid: bool,
    pub violations: AtomicU32,
    pub deadlocks_predicted: AtomicU32,
    pub parallel_pairs: AtomicU64, // spacelike-separated event pairs found
    pub tick: u64,
    pub eviction_watermark: u64,
}

impl FabricWeave {
    pub fn new() -> Self {
        Self {
            events: BTreeMap::new(),
            edges: Vec::new(),
            next_event_id: AtomicU64::new(1),
            logical_clock: AtomicU64::new(0),
            topo_order: Vec::new(),
            topo_valid: false,
            violations: AtomicU32::new(0),
            deadlocks_predicted: AtomicU32::new(0),
            parallel_pairs: AtomicU64::new(0),
            tick: 0,
            eviction_watermark: 0,
        }
    }

    /// Record a new kernel event into the fabric
    pub fn weave(&mut self, kind: EventKind, pid: u32, physical_ns: u64, core: u8) -> u64 {
        let id = self.next_event_id.fetch_add(1, Ordering::AcqRel);
        let logical = self.logical_clock.fetch_add(1, Ordering::AcqRel);
        let ev = SpacetimeEvent::new(id, kind, pid, logical, physical_ns, core);
        if self.events.len() < MAX_EVENTS {
            self.events.insert(id, ev);
        }
        self.topo_valid = false;
        id
    }

    /// Add causal edge between two events
    pub fn add_edge(&mut self, from: u64, to: u64, kind: EdgeKind) -> bool {
        if self.edges.len() >= MAX_EDGES {
            return false;
        }
        // Causal violation check: if `to` already completed before `from`
        let violation = match (self.events.get(&from), self.events.get(&to)) {
            (Some(f), Some(t)) => {
                t.completed && !f.completed && kind == EdgeKind::HardDependency
                    || (t.physical_ns + MIN_CAUSAL_GAP_NS < f.physical_ns
                        && kind != EdgeKind::SpeculativeHint)
            }
            _ => false,
        };

        if violation {
            self.violations.fetch_add(1, Ordering::Relaxed);
            if let Some(ev) = self.events.get_mut(&to) {
                ev.is_violation = true;
            }
        }

        if let Some(ev) = self.events.get_mut(&to) {
            ev.in_degree += 1;
        }
        self.edges.push(CausalEdge {
            from,
            to,
            weight: 1,
            kind,
        });
        self.topo_valid = false;
        !violation
    }

    /// Kahn's topological sort — detects cycles (deadlock prediction)
    pub fn topological_sort(&mut self) -> TopoResult {
        let n = self.events.len();
        if n == 0 {
            return TopoResult::Empty;
        }

        // Build in-degree map from scratch
        let mut in_deg: BTreeMap<u64, u32> = self.events.keys().map(|&id| (id, 0)).collect();
        for edge in &self.edges {
            if let Some(d) = in_deg.get_mut(&edge.to) {
                *d += 1;
            }
        }

        // Queue of zero-in-degree nodes
        let mut queue: Vec<u64> = in_deg
            .iter()
            .filter(|&(_, &d)| d == 0)
            .map(|(&id, _)| id)
            .collect();

        self.topo_order.clear();
        let mut processed = 0usize;

        while !queue.is_empty() && processed < TOPO_SORT_MAX_ITERS {
            // Pick lowest logical_time event from queue (stable ordering)
            let idx = queue
                .iter()
                .enumerate()
                .min_by_key(|&(_, &id)| {
                    self.events
                        .get(&id)
                        .map(|e| e.logical_time)
                        .unwrap_or(u64::MAX)
                })
                .map(|(i, _)| i)
                .unwrap_or(0);
            let id = queue.remove(idx);
            self.topo_order.push(id);
            processed += 1;

            // Reduce in-degree of successors
            let successors: Vec<u64> = self
                .edges
                .iter()
                .filter(|e| e.from == id)
                .map(|e| e.to)
                .collect();
            for succ in successors {
                if let Some(d) = in_deg.get_mut(&succ) {
                    *d = d.saturating_sub(1);
                    if *d == 0 {
                        queue.push(succ);
                    }
                }
            }
        }

        if processed < n {
            // Cycle detected — deadlock predicted!
            self.deadlocks_predicted.fetch_add(1, Ordering::Relaxed);
            self.topo_valid = false;
            TopoResult::CycleDetected {
                remaining: (n - processed) as u32,
            }
        } else {
            self.topo_valid = true;
            TopoResult::Sorted {
                count: processed as u32,
            }
        }
    }

    /// Trace the PAST causal cone of event `id` — all ancestors
    pub fn past_cone(&self, id: u64, depth: usize) -> Vec<u64> {
        if depth == 0 {
            return Vec::new();
        }
        let parents: Vec<u64> = self
            .edges
            .iter()
            .filter(|e| e.to == id)
            .map(|e| e.from)
            .collect();
        let mut cone = parents.clone();
        for parent in parents {
            let sub = self.past_cone(parent, depth - 1);
            for s in sub {
                if !cone.contains(&s) {
                    cone.push(s);
                }
            }
        }
        cone
    }

    /// Trace the FUTURE causal cone of event `id` — all descendants
    pub fn future_cone(&self, id: u64, depth: usize) -> Vec<u64> {
        if depth == 0 {
            return Vec::new();
        }
        let children: Vec<u64> = self
            .edges
            .iter()
            .filter(|e| e.from == id)
            .map(|e| e.to)
            .collect();
        let mut cone = children.clone();
        for child in children {
            let sub = self.future_cone(child, depth - 1);
            for s in sub {
                if !cone.contains(&s) {
                    cone.push(s);
                }
            }
        }
        cone
    }

    /// Find all spacelike-separated (parallelizable) event pairs
    /// Returns count of safe parallel pairs found
    pub fn find_parallel_pairs(&mut self) -> u64 {
        let ids: Vec<u64> = self.events.keys().cloned().collect();
        let mut count = 0u64;
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                if let (Some(a), Some(b)) = (self.events.get(&ids[i]), self.events.get(&ids[j])) {
                    if a.is_elsewhere(b) {
                        count += 1;
                    }
                }
            }
        }
        self.parallel_pairs.fetch_add(count, Ordering::Relaxed);
        count
    }

    /// Mark an event as completed — advances the physical causal frontier
    pub fn complete(&mut self, id: u64) {
        if let Some(ev) = self.events.get_mut(&id) {
            ev.completed = true;
        }
    }

    /// Evict old completed events to bound memory usage
    pub fn evict_old(&mut self, current_tick: u64) {
        self.tick = current_tick;
        let cutoff_logical = self
            .logical_clock
            .load(Ordering::Relaxed)
            .saturating_sub(EVENT_RETAIN_TICKS);
        let evict_ids: Vec<u64> = self
            .events
            .iter()
            .filter(|(_, e)| e.completed && e.logical_time < cutoff_logical)
            .map(|(&id, _)| id)
            .collect();
        for id in &evict_ids {
            self.events.remove(id);
            self.edges.retain(|e| e.from != *id && e.to != *id);
        }
        self.eviction_watermark = evict_ids.len() as u64;
        self.topo_valid = false;
    }

    pub fn stats(&self) -> FabricStats {
        FabricStats {
            events: self.events.len() as u32,
            edges: self.edges.len() as u32,
            violations: self.violations.load(Ordering::Relaxed),
            deadlocks: self.deadlocks_predicted.load(Ordering::Relaxed),
            parallel: self.parallel_pairs.load(Ordering::Relaxed),
            topo_valid: self.topo_valid,
            logical_clock: self.logical_clock.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum TopoResult {
    Sorted { count: u32 },
    CycleDetected { remaining: u32 },
    Empty,
}

#[derive(Clone, Copy, Debug)]
pub struct FabricStats {
    pub events: u32,
    pub edges: u32,
    pub violations: u32,
    pub deadlocks: u32,
    pub parallel: u64,
    pub topo_valid: bool,
    pub logical_clock: u64,
}
