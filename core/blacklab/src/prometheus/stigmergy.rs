use alloc::{collections::BTreeMap, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};

const PHEROMONE_DECAY: f64 = 0.95; // per tick evaporation rate
const MAX_SERVICES: usize = 256;

/// Pheromone trail — a concentration left by a running service
pub struct PheromoneTrail {
    pub concentration: f64,
    pub depositor: u32,    // PID of depositing service
    pub trail_type: TrailType,
}

#[derive(Clone, Copy, PartialEq)]
pub enum TrailType {
    Alive,       // "I am running and healthy"
    Dependency,  // "I need service X to function"
    Critical,    // "If I die, restart me IMMEDIATELY"
    Poison,      // "Service X is thrashing, avoid it"
}

/// The pheromone field — a 2D map of (service_id, resource_id) → trail
pub struct PheromoneField {
    trails: BTreeMap<(u32, u32), PheromoneTrail>, // (depositor, target) → trail
    tick: AtomicU64,
}

impl PheromoneField {
    pub fn new() -> Self {
        Self { trails: BTreeMap::new(), tick: AtomicU64::new(0) }
    }

    /// Deposit a pheromone — called by each service's heartbeat
    pub fn deposit(&mut self, from: u32, to: u32, trail_type: TrailType, amount: f64) {
        let entry = self.trails.entry((from, to)).or_insert(PheromoneTrail {
            concentration: 0.0,
            depositor: from,
            trail_type,
        });
        let mut val = entry.concentration + amount;
        if val > 100.0 { val = 100.0; }
        entry.concentration = val;
        entry.trail_type = trail_type;
    }

    /// Evaporate all trails — run every PID 1 tick
    pub fn evaporate(&mut self) {
        self.tick.fetch_add(1, Ordering::Relaxed);
        let mut dead_keys = Vec::new();
        for (key, trail) in &mut self.trails {
            trail.concentration *= PHEROMONE_DECAY;
            if trail.concentration < 0.01 {
                dead_keys.push(*key);
            }
        }
        for k in dead_keys { self.trails.remove(&k); }
    }

    /// Compute restart urgency for a dead service — ant colony consensus
    /// urgency = Σ(dependency trails pointing to it) + critical_bonus
    pub fn restart_urgency(&self, dead_svc: u32) -> f64 {
        self.trails.iter()
            .filter(|((_, target), _)| *target == dead_svc)
            .map(|(_, trail)| {
                match trail.trail_type {
                    TrailType::Dependency => trail.concentration * 1.0,
                    TrailType::Critical   => trail.concentration * 10.0,
                    TrailType::Poison     => trail.concentration * -5.0, // inhibit restart
                    TrailType::Alive      => 0.0,
                }
            })
            .sum()
    }

    /// Return services ranked by restart urgency (highest first)
    pub fn restart_priority_queue(&self, dead_services: &[u32]) -> Vec<(u32, f64)> {
        let mut ranked: Vec<(u32, f64)> = dead_services.iter()
            .map(|&svc| (svc, self.restart_urgency(svc)))
            .collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        ranked
    }
}
