use alloc::{collections::BTreeMap, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};

// ─────────────────────────────────────────────
// CONSTANTS
// ─────────────────────────────────────────────

/// Speed of light in our spacetime — max CPU load = 1.0 (100%)
const C: f64 = 1.0;
/// Number of hierarchical wheel levels
const WHEEL_LEVELS: usize = 4;
/// Slots per level: level 0=1ms, 1=256ms, 2=65s, 3=4.6hr
const SLOTS_PER_LEVEL: usize = 256;

// ─────────────────────────────────────────────
// SPACETIME EVENT
// ─────────────────────────────────────────────

#[derive(Clone)]
pub struct SpacetimeEvent {
    pub id: u64,
    pub pid: u32,
    pub wall_deadline_ns: u64,   // coordinate time (what the kernel sees)
    pub proper_deadline_ns: u64, // proper time (what the service experiences)
    pub lorentz_factor: f64,     // γ = 1/sqrt(1 - v²/c²) at time of registration
    pub event_type: EventType,
    pub payload: EventPayload,
    pub worldline_x: f64,   // spatial coordinate (cpu_load at registration)
    pub worldline_t: u64,   // temporal coordinate (wall time at registration)
    pub is_lightlike: bool, // true if this is a signal/IPC (propagates at c)
}

#[derive(Clone, Copy, PartialEq)]
pub enum EventType {
    Timeout,      // service requested a sleep/wait
    Heartbeat,    // periodic liveness check
    WatchdogBark, // watchdog firing
    ServiceStart, // deferred service launch
    ServiceStop,  // deferred service termination
    ProperSync,   // synchronize a group's proper time
    LightCone,    // causal signal (IPC, signal delivery)
}

#[derive(Clone)]
pub enum EventPayload {
    None,
    Signal(i32),
    Message(Vec<u8>),
    ProperTimeSync {
        group_id: u32,
        target_proper_ns: u64,
    },
}

// ─────────────────────────────────────────────
// WORLDLINE TRACKER
// ─────────────────────────────────────────────

/// Tracks a service's path through (load, time) spacetime
/// Worldline is a sequence of (wall_time, cpu_load) events
#[derive(Clone)]
pub struct Worldline {
    pub pid: u32,
    /// Spacetime points: (wall_time_ns, cpu_load)
    pub points: Vec<(u64, f64)>,
    /// Accumulated proper time (elapsed time in service's rest frame)
    pub proper_time_ns: u64,
    /// Current Lorentz factor
    pub gamma: f64,
    /// Velocity (cpu load analog) — 0=rest, 1=max (near-c)
    pub velocity: f64,
}

impl Worldline {
    pub fn new(pid: u32) -> Self {
        Self {
            pid,
            points: Vec::new(),
            proper_time_ns: 0,
            gamma: 1.0,
            velocity: 0.0,
        }
    }

    /// Record a new spacetime point and advance proper time
    pub fn advance(&mut self, wall_time_ns: u64, cpu_load: f64) {
        let v = if cpu_load < 0.0 {
            0.0
        } else if cpu_load > 0.9999 {
            0.9999
        } else {
            cpu_load
        };
        let gamma = 1.0 / libm::sqrt(1.0 - v * v / (C * C));
        self.gamma = gamma;
        self.velocity = v;

        // Proper time elapsed = coordinate time / gamma (time dilation)
        if let Some(&(last_t, _)) = self.points.last() {
            let dt_wall = wall_time_ns.saturating_sub(last_t);
            let dt_proper = (dt_wall as f64 / gamma) as u64;
            self.proper_time_ns += dt_proper;
        }
        self.points.push((wall_time_ns, cpu_load));
        // Keep only last 256 points
        if self.points.len() > 256 {
            self.points.remove(0);
        }
    }

    /// Compute proper time from wall time for this service (inverse: wall from proper)
    pub fn wall_from_proper(&self, proper_ns: u64) -> u64 {
        // wall_time = proper_time * gamma
        (proper_ns as f64 * self.gamma) as u64
    }

    /// Check if two events on this worldline are causally connected
    /// (inside each other's light cone)
    pub fn causally_connected(&self, t1_ns: u64, x1: f64, t2_ns: u64, x2: f64) -> bool {
        // Minkowski interval: s² = -(c*Δt)² + Δx²
        let dt = (t2_ns as f64 - t1_ns as f64) * 1e-9; // ns to seconds
        let dx = x2 - x1;
        let interval_sq = -(C * dt) * (C * dt) + dx * dx;
        // Timelike or lightlike interval → causally connected
        interval_sq <= 0.0
    }

    /// Geodesic deviation: how much is this worldline curving?
    /// High curvature = rapid load changes = turbulent service
    pub fn curvature(&self) -> f64 {
        if self.points.len() < 3 {
            return 0.0;
        }
        let n = self.points.len();
        let p0 = &self.points[n - 3];
        let p1 = &self.points[n - 2];
        let p2 = &self.points[n - 1];
        // Second derivative of cpu_load w.r.t. time
        let dt1 = (p1.0 - p0.0) as f64;
        let dt2 = (p2.0 - p1.0) as f64;
        if dt1 < 1.0 || dt2 < 1.0 {
            return 0.0;
        }
        let d_load1 = (p1.1 - p0.1) / dt1;
        let d_load2 = (p2.1 - p1.1) / dt2;
        let diff = (d_load2 - d_load1) / ((dt1 + dt2) / 2.0);
        if diff < 0.0 { -diff } else { diff }
    }
}

// ─────────────────────────────────────────────
// HIERARCHICAL TIMER WHEEL
// ─────────────────────────────────────────────

/// A slot in the timer wheel containing pending events
pub struct WheelSlot {
    pub events: Vec<SpacetimeEvent>,
}

impl WheelSlot {
    pub fn new() -> Self {
        Self { events: Vec::new() }
    }
}

/// 4-level hierarchical timer wheel
/// Level 0: 1ms resolution, 256ms range
/// Level 1: 256ms resolution, 65.5s range
/// Level 2: 65.5s resolution, 4.6hr range
/// Level 3: 4.6hr resolution, 49 days range
pub struct TimerWheel {
    pub levels: [[WheelSlot; SLOTS_PER_LEVEL]; WHEEL_LEVELS],
    pub cursors: [usize; WHEEL_LEVELS], // current position in each level
    pub wall_ns: AtomicU64,
    pub resolution_ns: [u64; WHEEL_LEVELS],
    pub event_count: u64,
}

impl TimerWheel {
    pub fn new() -> Self {
        Self {
            levels: core::array::from_fn(|_| core::array::from_fn(|_| WheelSlot::new())),
            cursors: [0; WHEEL_LEVELS],
            wall_ns: AtomicU64::new(0),
            // Resolutions: 1ms, 256ms, 65536ms, 16777216ms
            resolution_ns: [1_000_000, 256_000_000, 65_536_000_000, 16_777_216_000_000],
            event_count: 0,
        }
    }

    /// Insert a spacetime event into the appropriate wheel level
    pub fn insert(&mut self, event: SpacetimeEvent) {
        let now = self.wall_ns.load(Ordering::Relaxed);
        let delta_ns = event.wall_deadline_ns.saturating_sub(now);

        // Find appropriate level based on delta
        let level = if delta_ns < self.resolution_ns[1] {
            0
        } else if delta_ns < self.resolution_ns[2] {
            1
        } else if delta_ns < self.resolution_ns[3] {
            2
        } else {
            3
        };

        let slot_idx = (self.cursors[level] + (delta_ns / self.resolution_ns[level]) as usize)
            % SLOTS_PER_LEVEL;

        self.levels[level][slot_idx].events.push(event);
        self.event_count += 1;
    }

    /// Advance the wheel by one tick (1ms resolution)
    /// Returns fired events
    pub fn tick(&mut self) -> Vec<SpacetimeEvent> {
        let _new_time = self.wall_ns.fetch_add(1_000_000, Ordering::SeqCst) + 1_000_000;
        let mut fired = Vec::new();

        // Advance level 0
        let slot0 = self.cursors[0];
        let ready: Vec<SpacetimeEvent> = self.levels[0][slot0].events.drain(..).collect();
        fired.extend(ready);
        self.cursors[0] = (self.cursors[0] + 1) % SLOTS_PER_LEVEL;

        // Cascade higher levels when they overflow
        if self.cursors[0] == 0 {
            self.cascade_level(1);
        }

        fired
    }

    fn cascade_level(&mut self, level: usize) {
        if level >= WHEEL_LEVELS {
            return;
        }
        let slot = self.cursors[level];
        let now = self.wall_ns.load(Ordering::Relaxed);

        // Pull events from this level slot and re-insert into lower levels
        let events: Vec<SpacetimeEvent> = self.levels[level][slot].events.drain(..).collect();
        for event in events {
            if event.wall_deadline_ns <= now + self.resolution_ns[0] * SLOTS_PER_LEVEL as u64 {
                // Fits in level 0 — insert precisely
                let delta = event.wall_deadline_ns.saturating_sub(now);
                let slot0 =
                    (self.cursors[0] + (delta / self.resolution_ns[0]) as usize) % SLOTS_PER_LEVEL;
                self.levels[0][slot0].events.push(event);
            } else {
                // Re-insert into same or lower level
                self.insert(event);
            }
        }

        self.cursors[level] = (self.cursors[level] + 1) % SLOTS_PER_LEVEL;
        if self.cursors[level] == 0 {
            self.cascade_level(level + 1);
        }
    }

    pub fn pending_count(&self) -> usize {
        let mut count = 0;
        for level in &self.levels {
            for slot in level {
                count += slot.events.len();
            }
        }
        count
    }
}

// ─────────────────────────────────────────────
// PROPER TIME GROUP — Shared Reference Frames
// ─────────────────────────────────────────────

/// A group of services sharing a proper time frame
/// Like a rocket ship — everyone inside agrees on elapsed time
#[derive(Clone)]
pub struct ProperTimeGroup {
    pub group_id: u32,
    pub member_pids: Vec<u32>,
    pub group_velocity: f64,  // group's collective "velocity" (avg load)
    pub proper_epoch_ns: u64, // when this group's clock started
    pub proper_now_ns: u64,   // group's current proper time
    pub gamma: f64,           // Lorentz factor for the group frame
}

impl ProperTimeGroup {
    pub fn new(group_id: u32) -> Self {
        Self {
            group_id,
            member_pids: Vec::new(),
            group_velocity: 0.0,
            proper_epoch_ns: 0,
            proper_now_ns: 0,
            gamma: 1.0,
        }
    }

    pub fn add_member(&mut self, pid: u32) {
        self.member_pids.push(pid);
    }

    /// Update group velocity from member worldlines (average cpu load)
    pub fn update_velocity(&mut self, worldlines: &BTreeMap<u32, Worldline>) {
        if self.member_pids.is_empty() {
            return;
        }
        let avg_v: f64 = self
            .member_pids
            .iter()
            .filter_map(|pid| worldlines.get(pid))
            .map(|wl| wl.velocity)
            .sum::<f64>()
            / self.member_pids.len() as f64;
        self.group_velocity = if avg_v < 0.0 {
            0.0
        } else if avg_v > 0.9999 {
            0.9999
        } else {
            avg_v
        };
        self.gamma = 1.0 / libm::sqrt(1.0 - self.group_velocity * self.group_velocity);
    }

    /// Advance group proper time by a wall-clock delta
    pub fn advance(&mut self, wall_delta_ns: u64) {
        let proper_delta = (wall_delta_ns as f64 / self.gamma) as u64;
        self.proper_now_ns += proper_delta;
    }

    /// Convert a group-proper deadline to wall time
    pub fn to_wall_time(&self, proper_deadline_ns: u64, current_wall_ns: u64) -> u64 {
        let proper_remaining = proper_deadline_ns.saturating_sub(self.proper_now_ns);
        let wall_remaining = (proper_remaining as f64 * self.gamma) as u64;
        current_wall_ns + wall_remaining
    }

    /// Simultaneity — two wall times that are simultaneous in this frame
    /// (Relativity of simultaneity: moving groups disagree on what's "now")
    pub fn simultaneous_wall_times(&self, proper_t: u64, spatial_sep: f64) -> (u64, u64) {
        // Lorentz transform: t' = γ(t - vx/c²)
        let base_wall = (proper_t as f64 * self.gamma) as u64;
        let correction = (self.group_velocity * spatial_sep / (C * C) * self.gamma * 1e9) as u64;
        (base_wall + correction, base_wall.saturating_sub(correction))
    }
}

// ─────────────────────────────────────────────
// CHRONOS — The Relativistic Time Master
// ─────────────────────────────────────────────

pub struct Chronos {
    pub wheel: TimerWheel,
    pub worldlines: BTreeMap<u32, Worldline>,
    pub groups: BTreeMap<u32, ProperTimeGroup>,
    pub event_seq: AtomicU64,
    pub wall_ns: u64,
    pub tick_count: u64,
    /// Light-cone enforcement: map of event_id → (t, x) for causal checking
    pub light_cone_map: BTreeMap<u64, (u64, f64)>,
}

impl Chronos {
    pub fn new() -> Self {
        Self {
            wheel: TimerWheel::new(),
            worldlines: BTreeMap::new(),
            groups: BTreeMap::new(),
            event_seq: AtomicU64::new(1),
            wall_ns: 0,
            tick_count: 0,
            light_cone_map: BTreeMap::new(),
        }
    }

    pub fn register_service(&mut self, pid: u32) {
        self.worldlines.insert(pid, Worldline::new(pid));
    }

    /// Report service vitals — updates worldline
    pub fn update_vitals(&mut self, pid: u32, cpu_load: f64) {
        if let Some(wl) = self.worldlines.get_mut(&pid) {
            wl.advance(self.wall_ns, cpu_load);
        }
    }

    /// Schedule a relativistic timeout — deadline is in proper time of the service
    pub fn schedule_proper_timeout(
        &mut self,
        pid: u32,
        proper_delay_ns: u64,
        event_type: EventType,
        payload: EventPayload,
    ) -> u64 {
        let id = self.event_seq.fetch_add(1, Ordering::Relaxed);
        let (gamma, velocity) = self
            .worldlines
            .get(&pid)
            .map(|wl| (wl.gamma, wl.velocity))
            .unwrap_or((1.0, 0.0));

        // Convert proper time to wall time: wall = proper * gamma
        let wall_delay_ns = (proper_delay_ns as f64 * gamma) as u64;
        let wall_deadline = self.wall_ns + wall_delay_ns;

        let event = SpacetimeEvent {
            id,
            pid,
            wall_deadline_ns: wall_deadline,
            proper_deadline_ns: self
                .worldlines
                .get(&pid)
                .map(|wl| wl.proper_time_ns + proper_delay_ns)
                .unwrap_or(proper_delay_ns),
            lorentz_factor: gamma,
            event_type,
            payload,
            worldline_x: velocity,
            worldline_t: self.wall_ns,
            is_lightlike: matches!(event_type, EventType::LightCone),
        };

        self.light_cone_map.insert(id, (self.wall_ns, velocity));
        self.wheel.insert(event);
        id
    }

    /// Schedule in group proper time — entire group wakes together
    pub fn schedule_group_sync(&mut self, group_id: u32, proper_delay_ns: u64) -> Vec<u64> {
        let mut event_ids = Vec::new();
        let pids: Vec<u32> = self
            .groups
            .get(&group_id)
            .map(|g| g.member_pids.clone())
            .unwrap_or_default();

        for pid in pids {
            let id = self.schedule_proper_timeout(
                pid,
                proper_delay_ns,
                EventType::ProperSync,
                EventPayload::ProperTimeSync {
                    group_id,
                    target_proper_ns: proper_delay_ns,
                },
            );
            event_ids.push(id);
        }
        event_ids
    }

    /// Enforce causal ordering — reject events that violate light-cone
    pub fn is_causal(&self, cause_event_id: u64, effect_wall_ns: u64, effect_x: f64) -> bool {
        if let Some(&(cause_t, cause_x)) = self.light_cone_map.get(&cause_event_id) {
            // Check if effect is inside cause's future light cone
            let dt = (effect_wall_ns as f64 - cause_t as f64) * 1e-9;
            let diff = effect_x - cause_x;
            let dx = if diff < 0.0 { -diff } else { diff };
            let dt_max = if dt > 0.0 { dt } else { 0.0 };
            // Future light cone: dx ≤ c*dt (effect cannot travel faster than light)
            dt >= 0.0 && dx <= C * dt_max
        } else {
            true // unknown cause — allow
        }
    }

    /// Master tick — advance wall time, fire expired events
    pub fn tick(&mut self, wall_delta_ns: u64) -> Vec<SpacetimeEvent> {
        self.wall_ns += wall_delta_ns;
        self.tick_count += 1;
        self.wheel.wall_ns.store(self.wall_ns, Ordering::Relaxed);

        // Advance all group proper times
        for group in self.groups.values_mut() {
            group.update_velocity(&self.worldlines);
            group.advance(wall_delta_ns);
        }

        // Fire the wheel
        self.wheel.tick()
    }

    /// Proper time remaining for a service until its next event
    pub fn proper_time_remaining(&self, pid: u32, event_id: u64) -> Option<u64> {
        let gamma = self.worldlines.get(&pid)?.gamma;
        // Scan wheel for this event (expensive — use sparingly)
        for level in &self.wheel.levels {
            for slot in level {
                for ev in &slot.events {
                    if ev.id == event_id {
                        let wall_remaining = ev.wall_deadline_ns.saturating_sub(self.wall_ns);
                        return Some((wall_remaining as f64 / gamma) as u64);
                    }
                }
            }
        }
        None
    }

    /// Find services with the most extreme time dilation (highest gamma)
    pub fn most_dilated_services(&self, n: usize) -> Vec<(u32, f64)> {
        let mut dilated: Vec<(u32, f64)> = self
            .worldlines
            .iter()
            .map(|(&pid, wl)| (pid, wl.gamma))
            .collect();
        dilated.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        dilated.truncate(n);
        dilated
    }

    /// Compute the "age" of each service in proper time
    /// Fast-moving (high-CPU) services age slower than idle ones
    pub fn proper_ages(&self) -> Vec<(u32, u64)> {
        self.worldlines
            .iter()
            .map(|(&pid, wl)| (pid, wl.proper_time_ns))
            .collect()
    }
}
