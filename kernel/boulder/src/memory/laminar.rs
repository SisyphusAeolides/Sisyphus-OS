#![allow(dead_code)]
use alloc::{collections::BTreeMap, vec::Vec};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

// ─────────────────────────────────────────────
// CONSTANTS
// ─────────────────────────────────────────────

pub const PAGE_SIZE:          usize = 4096;
pub const SLAB_CLASSES:       usize = 20;   // 8B, 16B, 32B ... 4MB
pub const BUDDY_MAX_ORDER:    usize = 20;   // 2^20 pages = 4GB max contiguous
pub const RE_LAMINAR_MAX:     f64   = 2300.0;
pub const RE_TURBULENT_MIN:   f64   = 4000.0;
pub const WINDOW_SIZE:        usize = 256;  // allocation history window
pub const PRESSURE_THRESHOLD: f64   = 0.4;  // fragmentation % that triggers rerouting
pub const VISCOSITY_BASE:     f64   = 1.0;  // base allocator viscosity
pub const CACHE_COLORS:       usize = 64;   // slab cache coloring — cache line offset variants

// ─────────────────────────────────────────────
// SLAB CACHE (LAMINAR FLOW)
// ─────────────────────────────────────────────

/// A slab — one page of fixed-size objects
pub struct Slab {
    pub base_addr:   usize,
    pub object_size: usize,
    pub capacity:    usize,      // objects per slab
    pub free_count:  usize,
    pub free_bitmap: [u64; 8],   // 512 objects max per slab (8 * 64 bits)
    pub color_offset: usize,     // cache coloring offset (bytes from base)
    pub alloc_count: u64,
    pub free_time_sum: u64,      // for viscosity calculation
}

impl Slab {
    pub fn new(base: usize, obj_size: usize, color: usize) -> Self {
        let capacity = (PAGE_SIZE - color) / obj_size;
        let capacity = if capacity < 512 { capacity } else { 512 };
        let mut free_bitmap = [0u64; 8];
        // Mark all capacity objects as free
        for i in 0..capacity {
            let word = i / 64;
            let bit = i % 64;
            free_bitmap[word] |= 1u64 << bit;
        }
        Self {
            base_addr: base + color,
            object_size: obj_size,
            capacity,
            free_count: capacity,
            free_bitmap,
            color_offset: color,
            alloc_count: 0,
            free_time_sum: 0,
        }
    }

    /// Allocate one object — O(1) via bitscanning
    pub fn alloc(&mut self) -> Option<usize> {
        if self.free_count == 0 { return None; }
        for (word_idx, &word) in self.free_bitmap.iter().enumerate() {
            if word == 0 { continue; }
            let bit = word.trailing_zeros() as usize;
            let obj_idx = word_idx * 64 + bit;
            if obj_idx >= self.capacity { continue; }
            self.free_bitmap[word_idx] &= !(1u64 << bit);
            self.free_count -= 1;
            self.alloc_count += 1;
            return Some(self.base_addr + obj_idx * self.object_size);
        }
        None
    }

    /// Free one object — validate address and set bit
    pub fn free(&mut self, addr: usize, now_ns: u64) -> bool {
        if addr < self.base_addr { return false; }
        let offset = addr - self.base_addr;
        if offset % self.object_size != 0 { return false; }
        let obj_idx = offset / self.object_size;
        if obj_idx >= self.capacity { return false; }
        let word = obj_idx / 64;
        let bit  = obj_idx % 64;
        if self.free_bitmap[word] & (1u64 << bit) != 0 { return false; } // double-free!
        self.free_bitmap[word] |= 1u64 << bit;
        self.free_count += 1;
        self.free_time_sum += now_ns;
        true
    }

    pub fn is_full(&self) -> bool { self.free_count == 0 }
    pub fn is_empty(&self) -> bool { self.free_count == self.capacity }
    pub fn utilization(&self) -> f64 {
        1.0 - self.free_count as f64 / self.capacity as f64
    }
}

/// Per-size-class slab cache
pub struct SlabCache {
    pub object_size: usize,
    pub slabs_full:  Vec<Slab>,
    pub slabs_partial: Vec<Slab>,
    pub slabs_empty: Vec<Slab>,
    pub color_counter: usize,
    pub alloc_total: AtomicU64,
    pub free_total:  AtomicU64,
    pub alloc_rate_window: [u64; WINDOW_SIZE],
    pub window_head: usize,
}

impl SlabCache {
    pub fn new(object_size: usize) -> Self {
        Self {
            object_size,
            slabs_full: Vec::new(),
            slabs_partial: Vec::new(),
            slabs_empty: Vec::new(),
            color_counter: 0,
            alloc_total: AtomicU64::new(0),
            free_total: AtomicU64::new(0),
            alloc_rate_window: [0u64; WINDOW_SIZE],
            window_head: 0,
        }
    }

    pub fn alloc(&mut self, page_provider: &mut dyn FnMut() -> Option<usize>) -> Option<usize> {
        // Try partial slabs first (best cache utilization)
        for slab in &mut self.slabs_partial {
            if let Some(addr) = slab.alloc() {
                if slab.is_full() { /* will be moved to full on next GC */ }
                self.alloc_total.fetch_add(1, Ordering::Relaxed);
                return Some(addr);
            }
        }
        // Try empty slabs (pre-allocated pages)
        if let Some(mut slab) = self.slabs_empty.pop() {
            let addr = slab.alloc();
            self.slabs_partial.push(slab);
            self.alloc_total.fetch_add(1, Ordering::Relaxed);
            return addr;
        }
        // Allocate a new page
        let page = page_provider()?;
        let color_limit = if CACHE_COLORS * 64 < PAGE_SIZE { CACHE_COLORS * 64 } else { PAGE_SIZE };
        let color = (self.color_counter * 64) % color_limit;
        self.color_counter = (self.color_counter + 1) % CACHE_COLORS;
        let mut slab = Slab::new(page, self.object_size, color);
        let addr = slab.alloc();
        self.slabs_partial.push(slab);
        self.alloc_total.fetch_add(1, Ordering::Relaxed);
        addr
    }

    pub fn free(&mut self, addr: usize, now_ns: u64) -> bool {
        for slab in &mut self.slabs_partial {
            if slab.free(addr, now_ns) {
                self.free_total.fetch_add(1, Ordering::Relaxed);
                return true;
            }
        }
        for slab in &mut self.slabs_full {
            if slab.free(addr, now_ns) {
                self.free_total.fetch_add(1, Ordering::Relaxed);
                return true;
            }
        }
        false
    }

    pub fn fragmentation(&self) -> f64 {
        let total_capacity: usize = self.slabs_partial.iter()
            .chain(self.slabs_full.iter())
            .map(|s| s.capacity)
            .sum();
        let total_used: usize = self.slabs_partial.iter()
            .chain(self.slabs_full.iter())
            .map(|s| s.capacity - s.free_count)
            .sum();
        if total_capacity == 0 { return 0.0; }
        1.0 - total_used as f64 / total_capacity as f64
    }
}

// ─────────────────────────────────────────────
// BUDDY ALLOCATOR (TURBULENT FLOW)
// ─────────────────────────────────────────────

/// Buddy system — manages 2^k page blocks
pub struct BuddyZone {
    pub base_addr:   usize,
    pub total_pages: usize,
    pub free_lists:  [Vec<usize>; BUDDY_MAX_ORDER], // free_lists[k] = list of 2^k page blocks
    pub alloc_map:   BTreeMap<usize, usize>,         // addr → order (for freeing)
    pub free_pages:  AtomicUsize,
    pub pressure:    f64,   // fragmentation pressure ∈ [0, 1]
}

impl BuddyZone {
    pub fn new(base_addr: usize, total_pages: usize) -> Self {
        let mut zone = Self {
            base_addr, total_pages,
            free_lists: core::array::from_fn(|_| Vec::new()),
            alloc_map: BTreeMap::new(),
            free_pages: AtomicUsize::new(total_pages),
            pressure: 0.0,
        };
        // Initialize: add the entire zone as one large block
        let max_order = libm::floor(libm::log2(total_pages as f64)) as usize;
        let max_order = if max_order < BUDDY_MAX_ORDER - 1 { max_order } else { BUDDY_MAX_ORDER - 1 };
        zone.free_lists[max_order].push(base_addr);
        zone
    }

    /// Allocate 2^order pages — split higher blocks if needed
    pub fn alloc_order(&mut self, order: usize) -> Option<usize> {
        if order >= BUDDY_MAX_ORDER { return None; }
        // Find the smallest free block >= requested order
        let found_order = (order..BUDDY_MAX_ORDER)
            .find(|&o| !self.free_lists[o].is_empty())?;

        let block = self.free_lists[found_order].pop()?;
        // Split down to requested order
        let mut current_order = found_order;
        while current_order > order {
            current_order -= 1;
            let buddy = block + (1 << current_order) * PAGE_SIZE;
            self.free_lists[current_order].push(buddy);
        }
        self.alloc_map.insert(block, order);
        let pages = 1 << order;
        self.free_pages.fetch_sub(pages, Ordering::Relaxed);
        self.update_pressure();
        Some(block)
    }

    /// Free a block — coalesce with buddy if possible
    pub fn free_block(&mut self, addr: usize) -> bool {
        let order = match self.alloc_map.remove(&addr) {
            Some(o) => o, None => return false,
        };
        let pages = 1 << order;
        self.free_pages.fetch_add(pages, Ordering::Relaxed);

        let mut current_addr = addr;
        let mut current_order = order;

        // Coalesce: repeatedly try to merge with buddy
        while current_order < BUDDY_MAX_ORDER - 1 {
            let buddy_addr = self.buddy_addr(current_addr, current_order);
            // Is the buddy free and at the same order?
            let buddy_free = self.free_lists[current_order]
                .iter().position(|&a| a == buddy_addr);
            if let Some(idx) = buddy_free {
                self.free_lists[current_order].remove(idx);
                // Merge: lower address becomes the merged block
                current_addr = if current_addr < buddy_addr { current_addr } else { buddy_addr };
                current_order += 1;
            } else { break; }
        }
        self.free_lists[current_order].push(current_addr);
        self.update_pressure();
        true
    }

    fn buddy_addr(&self, addr: usize, order: usize) -> usize {
        let block_size = (1 << order) * PAGE_SIZE;
        addr ^ block_size // XOR trick: buddy of block at addr
    }

    fn update_pressure(&mut self) {
        let free = self.free_pages.load(Ordering::Relaxed);
        // Pressure = 1 - (free_pages / total) adjusted for fragmentation
        let free_ratio = free as f64 / self.total_pages as f64;
        // Count fragmentation: many small free blocks = high pressure even if memory available
        let frag_penalty: f64 = self.free_lists[..8].iter()
            .enumerate()
            .map(|(order, list)| list.len() as f64 / (1 << order) as f64)
            .sum::<f64>() / 8.0;
        let val = 1.0 - free_ratio + frag_penalty * 0.3;
        self.pressure = if val < 0.0 { 0.0 } else if val > 1.0 { 1.0 } else { val };
    }

    pub fn utilization(&self) -> f64 {
        let free = self.free_pages.load(Ordering::Relaxed);
        1.0 - free as f64 / self.total_pages as f64
    }
}

// ─────────────────────────────────────────────
// REYNOLDS NUMBER & FLOW CLASSIFIER
// ─────────────────────────────────────────────

pub struct FlowSensor {
    history:      [usize; WINDOW_SIZE], // allocation size history
    head:         usize,
    count:        usize,
    pub reynolds: f64,
    pub regime:   FlowRegime,
    alloc_count:  u64,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum FlowRegime {
    Laminar,        // Re < 2300 — use slab cache
    Transitional,   // 2300 ≤ Re ≤ 4000 — mixed strategy
    Turbulent,      // Re > 4000 — use buddy + pressure routing
}

impl FlowSensor {
    pub fn new() -> Self {
        Self {
            history: [0; WINDOW_SIZE],
            head: 0, count: 0,
            reynolds: 0.0,
            regime: FlowRegime::Laminar,
            alloc_count: 0,
        }
    }

    pub fn record(&mut self, size: usize) {
        self.history[self.head] = size;
        self.head = (self.head + 1) % WINDOW_SIZE;
        if self.count < WINDOW_SIZE { self.count += 1; }
        self.alloc_count += 1;
        if self.alloc_count % 16 == 0 { self.recompute_reynolds(); }
    }

    fn recompute_reynolds(&mut self) {
        let n = self.count;
        if n < 4 { return; }
        let window = &self.history[..n];

        // Mean size (ρv analog)
        let mean = window.iter().map(|&s| s as f64).sum::<f64>() / n as f64;

        // Variance (μ analog — higher variance = lower viscosity)
        let variance = window.iter()
            .map(|&s| { let diff = s as f64 - mean; diff * diff })
            .sum::<f64>() / n as f64;
        let std_dev_raw = libm::sqrt(variance);
        let std_dev = if std_dev_raw > 1.0 { std_dev_raw } else { 1.0 };

        // Characteristic length = mean size
        // Velocity = alloc rate (normalized by window)
        let velocity = self.alloc_count as f64 / WINDOW_SIZE as f64;

        // Re = ρ*v*L / μ → (mean * velocity * mean) / std_dev
        self.reynolds = (mean * velocity * mean) / std_dev;

        self.regime = if self.reynolds < RE_LAMINAR_MAX {
            FlowRegime::Laminar
        } else if self.reynolds < RE_TURBULENT_MIN {
            FlowRegime::Transitional
        } else {
            FlowRegime::Turbulent
        };
    }
}

// ─────────────────────────────────────────────
// LAMINAR — The Master Allocator
// ─────────────────────────────────────────────

pub struct Laminar {
    pub slab_caches: [SlabCache; SLAB_CLASSES],
    pub buddy_zones: Vec<BuddyZone>,
    pub sensor:      FlowSensor,
    pub page_pool:   Vec<usize>,    // free pages available to slab caches
    pub wall_ns:     u64,
    pub total_allocs: AtomicU64,
    pub total_frees:  AtomicU64,
    pub large_allocs: BTreeMap<usize, usize>, // addr → size for large allocations
}

impl Laminar {
    /// Size classes: 8, 16, 32, 64, 128, 256, 512, 1K, 2K, 4K, 8K, 16K, 32K,
    ///               64K, 128K, 256K, 512K, 1M, 2M, 4M
    const SIZE_CLASSES: [usize; SLAB_CLASSES] = [
        8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096,
        8192, 16384, 32768, 65536, 131072, 262144, 524288,
        1048576, 2097152, 4194304,
    ];

    pub fn new() -> Self {
        Self {
            slab_caches: core::array::from_fn(|i| SlabCache::new(Self::SIZE_CLASSES[i])),
            buddy_zones: Vec::new(),
            sensor: FlowSensor::new(),
            page_pool: Vec::new(),
            wall_ns: 0,
            total_allocs: AtomicU64::new(0),
            total_frees: AtomicU64::new(0),
            large_allocs: BTreeMap::new(),
        }
    }

    /// Add a physical memory zone to the buddy allocator
    pub fn add_zone(&mut self, base: usize, pages: usize) {
        self.buddy_zones.push(BuddyZone::new(base, pages));
        // Pre-populate page pool from first zone
        let pages_to_add = if pages < 64 { pages } else { 64 };
        for i in 0..pages_to_add {
            self.page_pool.push(base + i * PAGE_SIZE);
        }
    }

    /// Core allocation — Reynolds-adaptive routing
    pub fn alloc(&mut self, size: usize) -> Option<usize> {
        self.sensor.record(size);
        self.total_allocs.fetch_add(1, Ordering::Relaxed);

        match self.sensor.regime {
            FlowRegime::Laminar => self.alloc_laminar(size),
            FlowRegime::Turbulent => self.alloc_turbulent(size),
            FlowRegime::Transitional => {
                // Mixed: small sizes → slab, medium/large → buddy
                if size <= 4096 { self.alloc_laminar(size) }
                else { self.alloc_turbulent(size) }
            }
        }
    }

    fn alloc_laminar(&mut self, size: usize) -> Option<usize> {
        let class = Self::size_class_for(size)?;
        let mut pool = core::mem::take(&mut self.page_pool);
        let addr = {
            let mut zone = self.buddy_zones.first_mut();
            self.slab_caches[class].alloc(&mut || {
                if let Some(pg) = pool.pop() { return Some(pg); }
                if let Some(z) = zone.as_deref_mut() {
                    z.alloc_order(0)
                } else {
                    None
                }
            })
        };
        self.page_pool = pool;
        addr
    }

    fn alloc_turbulent(&mut self, size: usize) -> Option<usize> {
        let pages_needed = (size + PAGE_SIZE - 1) / PAGE_SIZE;
        let order = libm::ceil(libm::log2(pages_needed as f64)) as usize;
        let order = if order < BUDDY_MAX_ORDER - 1 { order } else { BUDDY_MAX_ORDER - 1 };

        // Route to lowest-pressure zone (Navier-Stokes pressure gradient)
        let mut min_pressure = 2.0;
        let mut best_zone_idx = None;
        for (i, z) in self.buddy_zones.iter().enumerate() {
            if z.free_pages.load(Ordering::Relaxed) >= pages_needed {
                if z.pressure < min_pressure {
                    min_pressure = z.pressure;
                    best_zone_idx = Some(i);
                }
            }
        }
        let zone_idx = best_zone_idx?;
        let addr = self.buddy_zones[zone_idx].alloc_order(order)?;
        self.large_allocs.insert(addr, size);
        Some(addr)
    }

    pub fn free(&mut self, addr: usize) -> bool {
        self.total_frees.fetch_add(1, Ordering::Relaxed);

        // Try slab caches first
        for cache in &mut self.slab_caches {
            if cache.free(addr, self.wall_ns) { return true; }
        }

        // Try buddy zones
        if self.large_allocs.remove(&addr).is_some() {
            for zone in &mut self.buddy_zones {
                if addr >= zone.base_addr && addr < zone.base_addr + zone.total_pages * PAGE_SIZE {
                    return zone.free_block(addr);
                }
            }
        }
        false
    }

    /// Find the slab size class for a given allocation size
    fn size_class_for(size: usize) -> Option<usize> {
        Self::SIZE_CLASSES.iter().position(|&s| s >= size)
    }

    pub fn tick(&mut self, wall_ns: u64) { self.wall_ns = wall_ns; }

    pub fn global_pressure(&self) -> f64 {
        if self.buddy_zones.is_empty() { return 0.0; }
        self.buddy_zones.iter().map(|z| z.pressure).sum::<f64>() / self.buddy_zones.len() as f64
    }

    pub fn flow_regime(&self) -> FlowRegime { self.sensor.regime }
    pub fn reynolds_number(&self) -> f64 { self.sensor.reynolds }
}
