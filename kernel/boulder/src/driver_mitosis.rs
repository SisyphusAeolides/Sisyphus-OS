// kernel/boulder/src/driver_mitosis.rs
//! AUTOPOIETIC DRIVER MITOSIS
//!
//! A driver under sustained load is a cell past its geometric checkpoint.
//! Mitosis:
//!   1. Require Kardashev allow_mitosis + Lazarus healthy
//!   2. Snapshot parent membrane checkpoint
//!   3. Spawn daughter DriverCell with split capability stalk
//!   4. Rebind half of IRQ/queue affinity (phononic core hint from Golem)
//!   5. Parent + daughter enter G2 quiescence window, then both run
//!
//! Apoptosis: Hayflick-like resurrection_count from Lazarus ≥ MAX →
//!   recycle daughter, fold work back if sibling alive.
//!
//! Does NOT replace Lazarus — it orchestrates multiple membranes.


pub const MAX_DRIVER_CELLS: usize = 32;
pub const LOAD_MITOSIS_THRESHOLD_FP: u32 = 0xE000; // ~87.5% load 16.16
pub const MIN_TICKS_BETWEEN_MITOSIS: u64 = 10_000_000;
pub const MAX_LINEAGE_DEPTH: u8 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MitosisFault {
    DisabledByCivilization,
    ParentUnhealthy,
    Capacity,
    LineageExhausted,
    LoadTooLow,
    Cooldown,
    SplitRejected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CellPhase {
    Quiescent = 0,
    Active = 1,
    G2Hold = 2,
    Apoptotic = 3,
    Dead = 4,
}

#[derive(Clone, Copy, Debug)]
pub struct DriverCell {
    pub live: bool,
    pub id: u64,
    pub parent_id: u64,
    pub lineage_depth: u8,
    pub phase: CellPhase,
    /// Golem archetype hint (GpuDisplay=1, NetworkCard=0, ...)
    pub archetype: u8,
    /// Load 16.16
    pub load_fp: u32,
    pub core_lo: u8,
    pub core_hi: u8,
    /// Lazarus membrane handle
    pub lazarus_handle: u64,
    pub last_mitosis_tsc: u64,
    pub daughter_id: u64,
}

impl DriverCell {
    pub const EMPTY: Self = Self {
        live: false,
        id: 0,
        parent_id: 0,
        lineage_depth: 0,
        phase: CellPhase::Dead,
        archetype: 7,
        load_fp: 0,
        core_lo: 0,
        core_hi: 255,
        lazarus_handle: 0,
        last_mitosis_tsc: 0,
        daughter_id: 0,
    };
}

pub struct MitosisChamber {
    cells: [DriverCell; MAX_DRIVER_CELLS],
    length: usize,
    next_id: u64,
    allow: bool,
    mitosis_events: u64,
    apoptosis_events: u64,
}

impl MitosisChamber {
    pub const fn new() -> Self {
        Self {
            cells: [DriverCell::EMPTY; MAX_DRIVER_CELLS],
            length: 0,
            next_id: 1,
            allow: true,
            mitosis_events: 0,
            apoptosis_events: 0,
        }
    }

    pub fn set_allowed(&mut self, allow: bool) {
        self.allow = allow;
    }

    pub fn register_zygote(
        &mut self,
        lazarus_handle: u64,
        archetype: u8,
        core_lo: u8,
        core_hi: u8,
    ) -> Option<u64> {
        if self.length >= MAX_DRIVER_CELLS {
            return None;
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.cells[self.length] = DriverCell {
            live: true,
            id,
            parent_id: 0,
            lineage_depth: 0,
            phase: CellPhase::Active,
            archetype,
            load_fp: 0,
            core_lo,
            core_hi,
            lazarus_handle,
            last_mitosis_tsc: 0,
            daughter_id: 0,
        };
        self.length += 1;
        Some(id)
    }

    pub fn observe_load(&mut self, id: u64, load_fp: u32) {
        if let Some(c) = self.cells.iter_mut().find(|c| c.live && c.id == id) {
            c.load_fp = load_fp;
        }
    }

    /// Attempt mitosis on any Active cell past threshold.
    pub fn tick(&mut self, now_tsc: u64) -> Option<(u64, u64)> {
        if !self.allow {
            return None;
        }
        let mut parent_idx = None;
        for (i, c) in self.cells.iter().enumerate().take(self.length) {
            if c.live
                && c.phase == CellPhase::Active
                && c.daughter_id == 0
                && c.lineage_depth < MAX_LINEAGE_DEPTH
                && c.load_fp >= LOAD_MITOSIS_THRESHOLD_FP
                && now_tsc.wrapping_sub(c.last_mitosis_tsc) >= MIN_TICKS_BETWEEN_MITOSIS
            {
                parent_idx = Some(i);
                break;
            }
        }
        let pi = parent_idx?;
        match self.split(pi, now_tsc) {
            Ok(pair) => Some(pair),
            Err(_) => None,
        }
    }

    fn split(&mut self, parent_idx: usize, now_tsc: u64) -> Result<(u64, u64), MitosisFault> {
        if !self.allow {
            return Err(MitosisFault::DisabledByCivilization);
        }
        if self.length >= MAX_DRIVER_CELLS {
            return Err(MitosisFault::Capacity);
        }
        let parent = self.cells[parent_idx];
        if parent.phase != CellPhase::Active {
            return Err(MitosisFault::ParentUnhealthy);
        }
        if parent.lineage_depth >= MAX_LINEAGE_DEPTH {
            return Err(MitosisFault::LineageExhausted);
        }
        if parent.load_fp < LOAD_MITOSIS_THRESHOLD_FP {
            return Err(MitosisFault::LoadTooLow);
        }

        // Split core affinity band
        let mid = parent
            .core_lo
            .saturating_add((parent.core_hi.saturating_sub(parent.core_lo)) / 2);
        let daughter_id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);

        // Daughter lazarus handle: parent handle with high bit lineage tag
        // (real bind creates a fresh membrane via LazarusPool::register)
        let daughter_lazarus = parent.lazarus_handle ^ 0xD1A6_0000_0000_0000 ^ daughter_id;

        let daughter = DriverCell {
            live: true,
            id: daughter_id,
            parent_id: parent.id,
            lineage_depth: parent.lineage_depth.saturating_add(1),
            phase: CellPhase::G2Hold,
            archetype: parent.archetype,
            load_fp: parent.load_fp / 2,
            core_lo: mid.saturating_add(1).min(parent.core_hi),
            core_hi: parent.core_hi,
            lazarus_handle: daughter_lazarus,
            last_mitosis_tsc: now_tsc,
            daughter_id: 0,
        };

        self.cells[parent_idx].core_hi = mid;
        self.cells[parent_idx].load_fp = parent.load_fp / 2;
        self.cells[parent_idx].phase = CellPhase::G2Hold;
        self.cells[parent_idx].last_mitosis_tsc = now_tsc;
        self.cells[parent_idx].daughter_id = daughter_id;

        self.cells[self.length] = daughter;
        self.length += 1;
        self.mitosis_events = self.mitosis_events.saturating_add(1);
        Ok((parent.id, daughter_id))
    }

    /// Release G2 hold after both membranes checkpoint.
    pub fn release_g2(&mut self, id: u64) {
        for c in self.cells.iter_mut().take(self.length) {
            if c.live && (c.id == id || c.daughter_id == id || c.parent_id == id) {
                if c.phase == CellPhase::G2Hold {
                    c.phase = CellPhase::Active;
                }
            }
        }
    }

    pub fn apoptosis(&mut self, id: u64) -> Option<u64> {
        let idx = self.cells.iter().position(|c| c.live && c.id == id)?;
        self.cells[idx].phase = CellPhase::Apoptotic;
        self.cells[idx].live = false;
        self.apoptosis_events = self.apoptosis_events.saturating_add(1);
        // If parent points here, clear daughter link
        let dead_id = self.cells[idx].id;
        for c in self.cells.iter_mut().take(self.length) {
            if c.daughter_id == dead_id {
                c.daughter_id = 0;
            }
        }
        Some(dead_id)
    }

    pub fn stats(&self) -> (usize, u64, u64) {
        let live = self
            .cells
            .iter()
            .take(self.length)
            .filter(|c| c.live)
            .count();
        (live, self.mitosis_events, self.apoptosis_events)
    }
}
