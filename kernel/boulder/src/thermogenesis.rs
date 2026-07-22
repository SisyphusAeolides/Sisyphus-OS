#![allow(dead_code)]
use alloc::{collections::BTreeMap, vec, vec::Vec};
use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

#[repr(C, align(64))]
pub struct ThermalPage {
    pub temperature_zone:    AtomicU8,
    pub throttle_hint:       AtomicU8,
    pub kernel_epoch:        AtomicU8,
    _pad:                    u8,
    pub tsc_frequency_mhz:   AtomicU32,
    pub cpu_budget_ticks:    AtomicU64,
    pub cpu_used_ticks:      AtomicU64,
    pub thermal_ticks:       AtomicU64,
    _reserved:               [u8; 32],
}

impl ThermalPage {
    pub const fn zeroed() -> Self {
        Self {
            temperature_zone:  AtomicU8::new(0),
            throttle_hint:     AtomicU8::new(0),
            kernel_epoch:      AtomicU8::new(0),
            _pad:              0,
            tsc_frequency_mhz: AtomicU32::new(0),
            cpu_budget_ticks:  AtomicU64::new(0),
            cpu_used_ticks:    AtomicU64::new(0),
            thermal_ticks:     AtomicU64::new(0),
            _reserved:         [0; 32],
        }
    }
}

// ─────────────────────────────────────────────
// NO_STD MATH HELPERS
// ─────────────────────────────────────────────

fn f64_max(a: f64, b: f64) -> f64 {
    if a > b { a } else { b }
}

fn f64_clamp(val: f64, min: f64, max: f64) -> f64 {
    if val < min { min } else if val > max { max } else { val }
}

// ─────────────────────────────────────────────
// BIOLOGICAL CONSTANTS
// ─────────────────────────────────────────────

pub const HAYFLICK_LIMIT:        u64   = 50;     // cell divisions before senescence
pub const ATP_INITIAL:           f64   = 1000.0; // initial ATP per cell
pub const ATP_RESTING_RATE:      f64   = 0.1;    // ATP consumed per tick at rest
pub const ATP_ACTIVE_RATE:       f64   = 2.0;    // ATP consumed per access
pub const ATP_REGENERATION_RATE: f64   = 0.5;    // ATP regenerated per tick (oxidative phosphorylation)
pub const TEMP_AMBIENT:          f64   = 37.0;   // °C — normal operating temperature
pub const TEMP_HEAT_SHOCK:       f64   = 42.0;   // °C — heat shock trigger
pub const TEMP_NECROSIS:         f64   = 50.0;   // °C — catastrophic failure
pub const TEMP_ACCESS_HEAT:      f64   = 0.5;    // °C per access
pub const THERMAL_CONDUCTIVITY:  f64   = 0.05;   // heat dissipation per tick
pub const MAX_CELLS:             usize = 65536;
pub const IMMUNE_PATROL_RATE:    usize = 64;     // macrophages check N cells per tick
pub const MITOSIS_SIZE_THRESHOLD: usize = 65536; // bytes — divide if larger
pub const APOPTOSIS_ATP_THRESHOLD: f64  = 50.0;  // ATP below this → apoptosis

// ─────────────────────────────────────────────
// CELL LIFECYCLE STATES
// ─────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum CellState {
    Embryonic,     // just allocated — forming
    Healthy,       // normal operation
    Stressed,      // high temperature / low ATP
    HeatShock,     // fever response — fragmenting
    Senescent,     // Hayflick limit reached — read-only quarantine
    Apoptotic,     // programmed death in progress
    Necrotic,      // catastrophic failure — corrupt
    Recycled,      // dead, waiting for macrophage cleanup
}

// ─────────────────────────────────────────────
// MEMORY CELL — A Living Allocation
// ─────────────────────────────────────────────

pub struct MemoryCell {
    pub addr:            usize,
    pub size:            usize,
    pub state:           CellState,
    pub temperature:     f64,      // °C
    pub atp:             f64,      // energy units
    pub age:             f64,      // biological age (Hayflick counter)
    pub division_count:  u64,      // how many times this cell has divided (mitosis)
    pub access_count:    AtomicU64,
    pub last_access_tick: u64,
    pub metabolic_rate:  f64,      // individual cell metabolism speed
    pub hsp_proteins:    u32,      // heat shock protein count (protective)
    pub telomere_length: f64,      // 1.0 = full, 0.0 = senescent (Hayflick analog)
    pub parent_addr:     Option<usize>, // birth lineage
    pub daughters:       Vec<usize>,    // mitosis offspring
    pub immune_flags:    u8,       // macrophage targeting flags
    pub owner_pid:       u32,
    pub semantic_class:  u8,       // from blacklab SemanticGraph
}

impl Clone for MemoryCell {
    fn clone(&self) -> Self {
        Self {
            addr: self.addr,
            size: self.size,
            state: self.state,
            temperature: self.temperature,
            atp: self.atp,
            age: self.age,
            division_count: self.division_count,
            access_count: AtomicU64::new(self.access_count.load(Ordering::Relaxed)),
            last_access_tick: self.last_access_tick,
            metabolic_rate: self.metabolic_rate,
            hsp_proteins: self.hsp_proteins,
            telomere_length: self.telomere_length,
            parent_addr: self.parent_addr,
            daughters: self.daughters.clone(),
            immune_flags: self.immune_flags,
            owner_pid: self.owner_pid,
            semantic_class: self.semantic_class,
        }
    }
}

impl MemoryCell {
    pub fn new(addr: usize, size: usize, owner_pid: u32, tick: u64) -> Self {
        Self {
            addr, size,
            state: CellState::Embryonic,
            temperature: TEMP_AMBIENT,
            atp: ATP_INITIAL,
            age: 0.0,
            division_count: 0,
            access_count: AtomicU64::new(0),
            last_access_tick: tick,
            metabolic_rate: 1.0,
            hsp_proteins: 0,
            telomere_length: 1.0,
            parent_addr: None,
            daughters: Vec::new(),
            immune_flags: 0,
            owner_pid,
            semantic_class: 0,
        }
    }

    /// Metabolic tick: update temperature, ATP, age
    pub fn metabolize(&mut self, tick: u64) {
        let dt = 1.0; // per tick
        let access_freq = self.access_count.load(Ordering::Relaxed) as f64
            / (tick - self.last_access_tick + 1).max(1) as f64;

        // ATP dynamics: base consumption + access cost + regeneration
        let atp_consumed = (ATP_RESTING_RATE + ATP_ACTIVE_RATE * access_freq) * self.metabolic_rate * dt;
        let atp_regen = ATP_REGENERATION_RATE * (self.atp / ATP_INITIAL) * dt; // diminishing regen
        self.atp = f64_max(self.atp - atp_consumed + atp_regen, 0.0);

        // Temperature: heating from accesses, cooling toward ambient
        let heat_in  = TEMP_ACCESS_HEAT * access_freq;
        let heat_out = THERMAL_CONDUCTIVITY * (self.temperature - TEMP_AMBIENT);
        self.temperature += (heat_in - heat_out) * dt;
        self.temperature = f64_max(self.temperature, TEMP_AMBIENT - 5.0); // can't go below near-ambient

        // HSP proteins: produced when temperature rises (protective response)
        if self.temperature > TEMP_HEAT_SHOCK - 2.0 {
            self.hsp_proteins = (self.hsp_proteins + 1).min(1000);
        } else {
            self.hsp_proteins = self.hsp_proteins.saturating_sub(1);
        }

        // Aging: faster metabolism = faster aging (telomere shortening)
        let aging_delta = self.metabolic_rate * access_freq * dt * 0.01;
        self.age += aging_delta;
        self.telomere_length = f64_max(1.0 - self.age / HAYFLICK_LIMIT as f64, 0.0);

        // State machine transitions
        self.update_state();
    }

    fn update_state(&mut self) {
        self.state = match self.state {
            CellState::Recycled | CellState::Necrotic => self.state, // terminal
            _ => {
                if self.temperature >= TEMP_NECROSIS {
                    CellState::Necrotic
                } else if self.atp < APOPTOSIS_ATP_THRESHOLD && self.hsp_proteins < 10 {
                    CellState::Apoptotic
                } else if self.telomere_length <= 0.0 || self.age >= HAYFLICK_LIMIT as f64 {
                    CellState::Senescent
                } else if self.temperature >= TEMP_HEAT_SHOCK {
                    CellState::HeatShock
                } else if self.temperature > TEMP_AMBIENT + 3.0 || self.atp < ATP_INITIAL * 0.3 {
                    CellState::Stressed
                } else if self.state == CellState::Embryonic {
                    CellState::Healthy  // graduates from embryonic after first metabolize
                } else {
                    CellState::Healthy
                }
            }
        };
    }

    /// Heat shock response: produce more HSPs, reduce metabolic rate temporarily
    pub fn heat_shock_response(&mut self) {
        self.hsp_proteins += 50;
        self.metabolic_rate *= 0.5; // metabolic slowdown (stress response)
        // HSPs protect the cell — temporarily raise ATP recovery
        self.atp += 100.0; // emergency ATP from HSP chaperone activity
    }

    /// Apoptosis initiation: cell commits programmed death
    /// Returns list of addresses that should be zeroed (safe cleanup)
    pub fn begin_apoptosis(&mut self) -> Vec<usize> {
        self.state = CellState::Apoptotic;
        self.immune_flags |= 0x01; // flag for macrophage collection
        vec![self.addr]
    }

    /// Mitosis: divide large cell into two daughters
    /// Returns the two daughter addresses (kernel must allocate second one)
    pub fn can_divide(&self) -> bool {
        self.size >= MITOSIS_SIZE_THRESHOLD
            && self.state == CellState::Stressed
            && self.division_count < 5
            && self.telomere_length > 0.2
    }

    /// Mark as recycled after macrophage collection
    pub fn recycle(&mut self) {
        self.state = CellState::Recycled;
        self.atp = 0.0;
        self.hsp_proteins = 0;
        self.immune_flags = 0;
    }

    pub fn is_alive(&self) -> bool {
        !matches!(self.state, CellState::Recycled | CellState::Necrotic | CellState::Apoptotic)
    }

    pub fn health_score(&self) -> f64 {
        let temp_factor = 1.0 - f64_clamp((self.temperature - TEMP_AMBIENT) / 20.0, 0.0, 1.0);
        let atp_factor = f64_clamp(self.atp / ATP_INITIAL, 0.0, 1.0);
        let age_factor = self.telomere_length;
        (temp_factor + atp_factor + age_factor) / 3.0
    }
}

// ─────────────────────────────────────────────
// MACROPHAGE — Immune Cell That Patrols Memory
// ─────────────────────────────────────────────

pub struct Macrophage {
    pub id:           u32,
    pub patrol_idx:   usize,   // current scan position
    pub cells_eaten:  u64,
    pub atp_reclaimed: f64,
    pub necrotic_cleared: u64,
}

impl Macrophage {
    pub fn new(id: u32) -> Self {
        Self { id, patrol_idx: 0, cells_eaten: 0, atp_reclaimed: 0.0, necrotic_cleared: 0 }
    }

    /// Patrol: scan N cells, engulf dying/dead ones
    /// Returns list of addresses to free back to the allocator
    pub fn patrol(&mut self, cells: &mut BTreeMap<usize, MemoryCell>, budget: usize) -> Vec<usize> {
        let mut freed = Vec::new();
        let addrs: Vec<usize> = cells.keys().cloned().collect();
        let start = self.patrol_idx % addrs.len().max(1);

        for i in 0..budget.min(addrs.len()) {
            let addr = addrs[(start + i) % addrs.len()];
            if let Some(cell) = cells.get_mut(&addr) {
                match cell.state {
                    CellState::Apoptotic | CellState::Necrotic | CellState::Recycled => {
                        self.atp_reclaimed += cell.atp;
                        if matches!(cell.state, CellState::Necrotic) {
                            self.necrotic_cleared += 1;
                        }
                        self.cells_eaten += 1;
                        freed.push(addr);
                    },
                    CellState::Senescent => {
                        // Flag senescent cells but don't immediately kill —
                        // they can still serve read-only requests
                        cell.immune_flags |= 0x02;
                    },
                    _ => {}
                }
            }
        }

        self.patrol_idx = (self.patrol_idx + budget) % addrs.len().max(1);
        freed
    }
}

// ─────────────────────────────────────────────
// THERMOGENESIS ENGINE
// ─────────────────────────────────────────────

pub struct Thermogenesis {
    pub cells:           BTreeMap<usize, MemoryCell>,
    pub macrophages:     Vec<Macrophage>,
    pub graveyard:       Vec<usize>,      // recycled addresses waiting for rebirth
    pub tick:            u64,
    pub system_temp:     f64,             // aggregate system temperature
    pub total_born:      AtomicU64,
    pub total_died:      AtomicU64,
    pub total_divided:   AtomicU64,
    pub total_necrotic:  AtomicU64,
    pub heat_events:     AtomicU64,
}

impl Thermogenesis {
    pub fn new(num_macrophages: usize) -> Self {
        let macrophages = (0..num_macrophages)
            .map(|i| Macrophage::new(i as u32))
            .collect();
        Self {
            cells: BTreeMap::new(),
            macrophages,
            graveyard: Vec::new(),
            tick: 0,
            system_temp: TEMP_AMBIENT,
            total_born: AtomicU64::new(0),
            total_died: AtomicU64::new(0),
            total_divided: AtomicU64::new(0),
            total_necrotic: AtomicU64::new(0),
            heat_events: AtomicU64::new(0),
        }
    }

    /// Allocate a new living memory cell
    pub fn alloc(&mut self, addr: usize, size: usize, owner: u32) {
        let cell = MemoryCell::new(addr, size, owner, self.tick);
        self.cells.insert(addr, cell);
        self.total_born.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a memory access — heats the cell and burns ATP
    pub fn access(&mut self, addr: usize) {
        if let Some(cell) = self.cells.get_mut(&addr) {
            cell.access_count.fetch_add(1, Ordering::Relaxed);
            cell.last_access_tick = self.tick;
        }
    }

    /// Free a cell — triggers apoptosis sequence
    pub fn free(&mut self, addr: usize) {
        if let Some(cell) = self.cells.get_mut(&addr) {
            cell.begin_apoptosis();
        }
    }

    /// Master metabolic tick: run all cell metabolism + immune system
    pub fn tick(&mut self) {
        self.tick += 1;

        // Metabolize all cells
        let tick = self.tick;
        let mut heat_shock_cells = Vec::new();
        let mut mitosis_candidates = Vec::new();

        for (addr, cell) in &mut self.cells {
            cell.metabolize(tick);
            if cell.state == CellState::HeatShock {
                heat_shock_cells.push(*addr);
                self.heat_events.fetch_add(1, Ordering::Relaxed);
            }
            if cell.can_divide() {
                mitosis_candidates.push(*addr);
            }
        }

        // Heat shock response
        for addr in heat_shock_cells {
            if let Some(cell) = self.cells.get_mut(&addr) {
                cell.heat_shock_response();
            }
        }

        // Mitosis: divide stressed oversized cells
        for addr in mitosis_candidates {
            if let Some(cell) = self.cells.get_mut(&addr) {
                let half = cell.size / 2;
                let daughter_addr = addr + half;
                cell.size = half;
                cell.division_count += 1;
                cell.telomere_length *= 0.9; // telomere shortening from division
                cell.daughters.push(daughter_addr);

                let mut daughter = MemoryCell::new(daughter_addr, half, cell.owner_pid, tick);
                daughter.parent_addr = Some(addr);
                daughter.metabolic_rate = cell.metabolic_rate;
                daughter.semantic_class = cell.semantic_class;
                daughter.telomere_length = cell.telomere_length; // inherit shortened telomeres
                let _ = cell;
                self.cells.insert(daughter_addr, daughter);
                self.total_divided.fetch_add(1, Ordering::Relaxed);
            }
        }

        // Immune patrol: macrophages clean up dead cells
        let mut all_freed = Vec::new();
        for mac in &mut self.macrophages {
            let freed = mac.patrol(&mut self.cells, IMMUNE_PATROL_RATE);
            all_freed.extend(freed);
        }
        for addr in all_freed {
            if let Some(cell) = self.cells.remove(&addr) {
                if matches!(cell.state, CellState::Necrotic) {
                    self.total_necrotic.fetch_add(1, Ordering::Relaxed);
                }
                self.graveyard.push(addr);
                self.total_died.fetch_add(1, Ordering::Relaxed);
            }
        }

        // Update system temperature (mean of all cells)
        if !self.cells.is_empty() {
            self.system_temp = self.cells.values()
                .map(|c| c.temperature)
                .sum::<f64>() / self.cells.len() as f64;
        }
    }

    /// Rebirth: recycle graveyard addresses for new allocations
    pub fn rebirth(&mut self) -> Option<usize> {
        self.graveyard.pop()
    }

    pub fn alive_count(&self)     -> usize { self.cells.values().filter(|c| c.is_alive()).count() }
    pub fn senescent_count(&self) -> usize { self.cells.values().filter(|c| c.state == CellState::Senescent).count() }
    pub fn stressed_count(&self)  -> usize { self.cells.values().filter(|c| c.state == CellState::Stressed).count() }

    pub fn stats(&self) -> ThermogenesisStats {
        ThermogenesisStats {
            total_cells: self.cells.len() as u64,
            alive: self.alive_count() as u64,
            senescent: self.senescent_count() as u64,
            stressed: self.stressed_count() as u64,
            graveyard_size: self.graveyard.len() as u64,
            system_temp: self.system_temp,
            total_born: self.total_born.load(Ordering::Relaxed),
            total_died: self.total_died.load(Ordering::Relaxed),
            total_divided: self.total_divided.load(Ordering::Relaxed),
            heat_events: self.heat_events.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ThermogenesisStats {
    pub total_cells: u64,
    pub alive: u64,
    pub senescent: u64,
    pub stressed: u64,
    pub graveyard_size: u64,
    pub system_temp: f64,
    pub total_born: u64,
    pub total_died: u64,
    pub total_divided: u64,
    pub heat_events: u64,
}
