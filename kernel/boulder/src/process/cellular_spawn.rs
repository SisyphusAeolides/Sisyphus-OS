use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

const GRID_W: usize = 16;
const GRID_H: usize = 16;
pub const MAX_CORES: usize = GRID_W * GRID_H; // supports up to 256-core machines

pub struct CoreCell {
    pub active: AtomicBool,
    pub load: AtomicU32,        // 0-100 utilization %
    pub process_count: AtomicU32,
    pub generation: AtomicU32,  // Conway generation counter
}

impl CoreCell {
    pub const fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            load: AtomicU32::new(0),
            process_count: AtomicU32::new(0),
            generation: AtomicU32::new(0),
        }
    }
}

pub struct CellularCoreGrid {
    pub cells: [CoreCell; MAX_CORES],
}

impl CellularCoreGrid {
    pub const fn new() -> Self {
        // SAFETY: CoreCell is all atomics with const constructors
        const CELL: CoreCell = CoreCell::new();
        Self { cells: [CELL; MAX_CORES] }
    }

    fn neighbors(&self, idx: usize) -> [usize; 8] {
        let row = idx / GRID_W;
        let col = idx % GRID_W;
        let mut n = [0usize; 8];
        let mut i = 0;
        for dr in [-1i32, 0, 1] {
            for dc in [-1i32, 0, 1] {
                if dr == 0 && dc == 0 { continue; }
                // Toroidal wrapping — the grid is a torus (no edges, always connected)
                let nr = ((row as i32 + dr).rem_euclid(GRID_H as i32)) as usize;
                let nc = ((col as i32 + dc).rem_euclid(GRID_W as i32)) as usize;
                n[i] = nr * GRID_W + nc;
                i += 1;
            }
        }
        n
    }

    fn active_neighbor_count(&self, idx: usize) -> u32 {
        self.neighbors(idx).iter()
            .filter(|&&n| self.cells[n].active.load(Ordering::Relaxed))
            .count() as u32
    }

    /// Conway tick — advance one generation
    /// Returns list of (core_id, spawned: bool) — true = spawn process, false = kill
    pub fn tick(&self) -> [(usize, bool); MAX_CORES] {
        let mut changes = [(0usize, false); MAX_CORES];
        
        for idx in 0..MAX_CORES {
            let alive = self.cells[idx].active.load(Ordering::Relaxed);
            let n = self.active_neighbor_count(idx);
            let load = self.cells[idx].load.load(Ordering::Relaxed);
            
            // Standard Conway + load-weighted rules:
            // Born if 3 neighbors AND local load headroom > 20%
            // Survives if 2-3 neighbors AND not overloaded
            let next_alive = if alive {
                (n == 2 || n == 3) && load < 90
            } else {
                n == 3 && load < 80
            };
            
            changes[idx] = (idx, next_alive != alive);
            if next_alive != alive {
                self.cells[idx].active.store(next_alive, Ordering::Relaxed);
                self.cells[idx].generation.fetch_add(1, Ordering::Relaxed);
            }
        }
        changes
    }

    /// Inject a "glider" pattern — a self-propagating process migration wave
    /// Gliders move diagonally, carrying process affinity data across cores
    pub fn inject_glider(&self, origin: usize) {
        // Classic Conway glider offsets from origin
        let offsets: &[(i32, i32)] = &[(0,1),(1,2),(2,0),(2,1),(2,2)];
        let row = (origin / GRID_W) as i32;
        let col = (origin % GRID_W) as i32;
        for &(dr, dc) in offsets {
            let nr = (row + dr).rem_euclid(GRID_H as i32) as usize;
            let nc = (col + dc).rem_euclid(GRID_W as i32) as usize;
            self.cells[nr * GRID_W + nc].active.store(true, Ordering::Relaxed);
        }
    }
}
