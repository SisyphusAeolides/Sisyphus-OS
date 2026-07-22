// kernel/boulder/src/memory/mycelium.rs
// #![no_std] — inherited from crate root
//
// MYCELIUM — Fungal Network Memory Allocator
//
// Free pages = nutrients in substrate. Allocations = hyphal tips that
// grow toward free space via a diffusion nutrient gradient.
// Anastomosis: two tips from the same PID within ANASTOMOSIS_RADIUS pages
//   merge their extents into one contiguous cord.
// Allelopathy: tips from rival PIDs emit an inhibition field that repels
//   foreign growth — models NUMA/cache-domain ownership.
// Sporulation: a mat that has grown past SPORE_THRESHOLD pages caches
//   pre-split chunks ("spores") for O(1) burst allocation.
// Decomposition: on free(), a nutrient pulse diffuses outward attracting
//   the nearest living tip for reuse.
//
// Gradient:  G(p) = Σ_{free f} exp(−|p−f| / λ)  − α · Σ_{rival t} exp(−|p−t| / λ_i)
// Growth:    tip advances to neighbor page with highest G(p)
// No floats in hot path — gradients are integer-approximated via
//   a precomputed lookup table (LUT) using fixed-point 16.16 math.

#![allow(dead_code)]
extern crate alloc;
use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

// ─────────────────────────────────────────────
// CONSTANTS (all page-unit)
// ─────────────────────────────────────────────

pub const MAX_PAGES:            usize = 1 << 20;   // 4 GB / 4 KB
pub const MAX_HYPHAE:           usize = 4096;
pub const MAX_SPORES:           usize = 256;
pub const ANASTOMOSIS_RADIUS:   usize = 8;          // pages — fusion distance
pub const DIFFUSION_LAMBDA:     usize = 512;        // pages — nutrient decay half-length
pub const INHIBITION_LAMBDA:    usize = 64;         // pages — allelopathy radius
pub const INHIBITION_ALPHA_FP:  u32   = 0x0000_8000; // 0.5 in 16.16 fixed-point
pub const SPORE_THRESHOLD:      usize = 128;        // pages in mat before sporulation
pub const DECOMP_PULSE_RADIUS:  usize = 32;         // pages — nutrient pulse on free()
pub const GRADIENT_LUT_LEN:     usize = 1024;       // precomputed exp decay entries

// ─────────────────────────────────────────────
// FIXED-POINT EXP DECAY LUT
// exp_lut[d] ≈ exp(-d / λ) * 65536  (16.16 fixed point)
// Computed once at init; d is distance in pages, clamped to LUT_LEN
// ─────────────────────────────────────────────

pub struct GradientLut {
    pub nutrient:    [u32; GRADIENT_LUT_LEN], // exp(-d / DIFFUSION_LAMBDA)
    pub inhibition:  [u32; GRADIENT_LUT_LEN], // exp(-d / INHIBITION_LAMBDA)
}

impl GradientLut {
    pub const fn zeroed() -> Self {
        Self {
            nutrient:   [0u32; GRADIENT_LUT_LEN],
            inhibition: [0u32; GRADIENT_LUT_LEN],
        }
    }

    /// Build LUT using integer-only exp approximation:
    /// exp(-d/λ) ≈ product of (1 - 1/λ) applied d times, precomputed iteratively.
    /// Result stored as 16.16 fixed-point (multiply by 65536).
    pub fn build(&mut self) {
        // Nutrient LUT: exp(-d / DIFFUSION_LAMBDA) in 16.16 fp
        let mut val: u64 = 0x0001_0000; // 1.0 in 16.16
        for d in 0..GRADIENT_LUT_LEN {
            self.nutrient[d] = val as u32;
            // Multiply by (1 - 1/λ): val = val * (λ-1) / λ
            val = val * (DIFFUSION_LAMBDA as u64 - 1) / DIFFUSION_LAMBDA as u64;
            if val == 0 { break; }
        }
        // Inhibition LUT
        let mut val: u64 = 0x0001_0000;
        for d in 0..GRADIENT_LUT_LEN {
            self.inhibition[d] = val as u32;
            val = val * (INHIBITION_LAMBDA as u64 - 1) / INHIBITION_LAMBDA as u64;
            if val == 0 { break; }
        }
    }

    #[inline(always)]
    pub fn nutrient_at(&self, dist: usize) -> u32 {
        self.nutrient[dist.min(GRADIENT_LUT_LEN - 1)]
    }

    #[inline(always)]
    pub fn inhibition_at(&self, dist: usize) -> u32 {
        self.inhibition[dist.min(GRADIENT_LUT_LEN - 1)]
    }
}

// ─────────────────────────────────────────────
// PAGE SUBSTRATE
// ─────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PageState {
    Free,
    Hyphal(u32),     // occupied by PID
    Spore,           // cached for burst allocation
    Decomposing,     // recently freed — nutrient pulse active
}

/// Flat substrate array. One entry per physical page.
/// We keep this in a fixed-size array to stay no_std / no heap for the
/// substrate itself — kernel maps this into BSS or reserved memory.
pub struct Substrate {
    pub pages:          [PageState; MAX_PAGES],
    pub nutrient_pulse: [u16; MAX_PAGES],   // active nutrient pulse strength (16.16 hi-word)
    pub free_count:     AtomicUsize,
    pub total_pages:    usize,
}

impl Substrate {
    pub const fn new() -> Self {
        Self {
            pages:          [PageState::Free; MAX_PAGES],
            nutrient_pulse: [0u16; MAX_PAGES],
            free_count:     AtomicUsize::new(0),
            total_pages:    0,
        }
    }

    pub fn init(&mut self, total_pages: usize) {
        self.total_pages = total_pages.min(MAX_PAGES);
        self.free_count.store(self.total_pages, Ordering::Relaxed);
    }

    pub fn is_free(&self, page: usize) -> bool {
        page < self.total_pages && self.pages[page] == PageState::Free
    }

    /// Claim a page for a hyphal tip
    pub fn claim(&mut self, page: usize, pid: u32) -> bool {
        if !self.is_free(page) { return false; }
        self.pages[page] = PageState::Hyphal(pid);
        self.free_count.fetch_sub(1, Ordering::Relaxed);
        true
    }

    /// Release a page back to free, emit nutrient pulse
    pub fn release(&mut self, page: usize) {
        if page >= self.total_pages { return; }
        self.pages[page] = PageState::Decomposing;
        self.free_count.fetch_add(1, Ordering::Relaxed);
        // Emit nutrient pulse in radius — decays each tick
        let lo = page.saturating_sub(DECOMP_PULSE_RADIUS);
        let hi = (page + DECOMP_PULSE_RADIUS).min(self.total_pages);
        for p in lo..hi {
            let dist = p.abs_diff(page);
            let pulse = (0xFFFF_u32 >> (dist * 16 / DECOMP_PULSE_RADIUS)) as u16;
            if pulse > self.nutrient_pulse[p] {
                self.nutrient_pulse[p] = pulse;
            }
        }
    }

    /// Decay pulses each tick (nutrient diffuses away)
    pub fn tick_decay(&mut self) {
        for p in 0..self.total_pages {
            if self.nutrient_pulse[p] > 0 {
                self.nutrient_pulse[p] = self.nutrient_pulse[p].saturating_sub(128);
            }
            if self.pages[p] == PageState::Decomposing && self.nutrient_pulse[p] == 0 {
                self.pages[p] = PageState::Free;
            }
        }
    }

    /// Compute nutrient gradient score at `page` for PID `pid` (fixed-point u32)
    /// G(p) = pulse[p] + Σ nearby free pages − Σ rival tips (allelopathy)
    pub fn gradient_score(
        &self,
        page: usize,
        pid: u32,
        lut: &GradientLut,
    ) -> u32 {
        let mut score: u32 = self.nutrient_pulse[page] as u32 * 16; // pulse bonus
        let scan_lo = page.saturating_sub(DIFFUSION_LAMBDA.min(GRADIENT_LUT_LEN));
        let scan_hi = (page + DIFFUSION_LAMBDA.min(GRADIENT_LUT_LEN)).min(self.total_pages);

        for p in scan_lo..scan_hi {
            let dist = p.abs_diff(page);
            match self.pages[p] {
                PageState::Free | PageState::Decomposing => {
                    score = score.saturating_add(lut.nutrient_at(dist));
                },
                PageState::Hyphal(rival_pid) if rival_pid != pid => {
                    // Allelopathy: rival tips suppress this direction
                    let inhibit = ((lut.inhibition_at(dist) as u64
                        * INHIBITION_ALPHA_FP as u64) >> 16) as u32;
                    score = score.saturating_sub(inhibit);
                },
                _ => {}
            }
        }
        score
    }
}

// ─────────────────────────────────────────────
// HYPHAL TIP — Active Growth Front
// ─────────────────────────────────────────────

#[derive(Clone)]
pub struct HyphalTip {
    pub tip_id:      u32,
    pub pid:         u32,
    pub position:    usize,          // current page index
    pub extent_lo:   usize,          // lowest page claimed by this hypha
    pub extent_hi:   usize,          // highest page claimed
    pub page_count:  usize,          // total pages in this cord
    pub growth_ticks:u64,
    pub stalled:     bool,           // no free pages nearby
    pub anastomosed: bool,           // merged into another tip
    pub merged_into: Option<u32>,    // tip_id of dominant cord after anastomosis
}

impl HyphalTip {
    pub fn new(tip_id: u32, pid: u32, seed_page: usize) -> Self {
        Self {
            tip_id, pid, position: seed_page,
            extent_lo: seed_page, extent_hi: seed_page,
            page_count: 1, growth_ticks: 0,
            stalled: false, anastomosed: false, merged_into: None,
        }
    }

    /// Advance: scan neighbors, pick best gradient, claim page
    pub fn advance(
        &mut self,
        substrate: &mut Substrate,
        lut: &GradientLut,
    ) -> Option<usize> {
        if self.anastomosed || self.stalled { return None; }

        // Scan 3 candidate directions: left, right, and current+2 (jump)
        let candidates = [
            self.position.saturating_sub(1),
            (self.position + 1).min(substrate.total_pages.saturating_sub(1)),
            (self.position + 2).min(substrate.total_pages.saturating_sub(1)),
            self.position.saturating_sub(2),
        ];

        let mut best_page  = 0usize;
        let mut best_score = 0u32;

        for &cand in &candidates {
            if !substrate.is_free(cand) { continue; }
            let score = substrate.gradient_score(cand, self.pid, lut);
            if score > best_score {
                best_score = score;
                best_page = cand;
            }
        }

        if best_score == 0 {
            self.stalled = true;
            return None;
        }

        if substrate.claim(best_page, self.pid) {
            self.position = best_page;
            self.extent_lo = self.extent_lo.min(best_page);
            self.extent_hi = self.extent_hi.max(best_page);
            self.page_count += 1;
            self.growth_ticks += 1;
            Some(best_page)
        } else {
            None
        }
    }

    pub fn can_sporulate(&self) -> bool {
        self.page_count >= SPORE_THRESHOLD && !self.anastomosed
    }
}

// ─────────────────────────────────────────────
// SPORE CACHE — Pre-split free chunks for burst allocation
// ─────────────────────────────────────────────

#[derive(Clone, Copy)]
pub struct Spore {
    pub page_lo:  usize,
    pub page_hi:  usize,
    pub pid:      u32,
    pub age:      u64,
}

pub struct SporeCache {
    pub spores: [Option<Spore>; MAX_SPORES],
    pub count:  usize,
    pub hits:   AtomicU64,
    pub misses: AtomicU64,
}

impl SporeCache {
    pub const fn new() -> Self {
        Self {
            spores: [None; MAX_SPORES],
            count: 0,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    pub fn deposit(&mut self, spore: Spore) {
        if self.count < MAX_SPORES {
            self.spores[self.count] = Some(spore);
            self.count += 1;
        }
    }

    /// Find a spore large enough for `pages` pages
    pub fn harvest(&mut self, pages: usize) -> Option<Spore> {
        for slot in self.spores.iter_mut() {
            if let Some(s) = *slot {
                if s.page_hi - s.page_lo >= pages {
                    *slot = None;
                    self.count = self.count.saturating_sub(1);
                    self.hits.fetch_add(1, Ordering::Relaxed);
                    return Some(s);
                }
            }
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        None
    }
}

// ─────────────────────────────────────────────
// MYCELIUM — Master Allocator
// ─────────────────────────────────────────────

pub struct Mycelium {
    pub substrate:    Substrate,
    pub lut:          GradientLut,
    pub hyphae:       BTreeMap<u32, HyphalTip>,  // tip_id → tip
    pub spores:       SporeCache,
    pub next_tip_id:  u32,
    pub tick:         u64,
    pub total_alloc:  AtomicU64,
    pub total_free:   AtomicU64,
    pub anastomoses:  AtomicU64,
    pub stalls:       AtomicU64,
}

impl Mycelium {
    pub fn new() -> Self {
        let mut lut = GradientLut::zeroed();
        lut.build();
        Self {
            substrate: Substrate::new(),
            lut,
            hyphae: BTreeMap::new(),
            spores: SporeCache::new(),
            next_tip_id: 1,
            tick: 0,
            total_alloc: AtomicU64::new(0),
            total_free:  AtomicU64::new(0),
            anastomoses: AtomicU64::new(0),
            stalls:      AtomicU64::new(0),
        }
    }

    pub fn init(&mut self, total_pages: usize) {
        self.substrate.init(total_pages);
    }

    /// Alloc: spawn a hyphal tip at the best-gradient free page near `hint`
    /// Returns (tip_id, page_lo) — tip will grow on subsequent ticks
    pub fn alloc(&mut self, pid: u32, pages_needed: usize, hint: usize) -> Option<(u32, usize)> {
        // Fast path: check spore cache first
        if let Some(spore) = self.spores.harvest(pages_needed) {
            self.total_alloc.fetch_add(1, Ordering::Relaxed);
            return Some((0, spore.page_lo)); // spore = instant allocation
        }

        // Find best seed page near hint
        let scan_lo = hint.saturating_sub(DIFFUSION_LAMBDA);
        let scan_hi = (hint + DIFFUSION_LAMBDA).min(self.substrate.total_pages);
        let mut best_page = hint;
        let mut best_score = 0u32;

        for p in scan_lo..scan_hi {
            if !self.substrate.is_free(p) { continue; }
            let score = self.substrate.gradient_score(p, pid, &self.lut);
            if score > best_score { best_score = score; best_page = p; }
        }

        if !self.substrate.claim(best_page, pid) { return None; }

        let tip_id = self.next_tip_id;
        self.next_tip_id = self.next_tip_id.wrapping_add(1);
        let tip = HyphalTip::new(tip_id, pid, best_page);
        self.hyphae.insert(tip_id, tip);
        self.total_alloc.fetch_add(1, Ordering::Relaxed);
        Some((tip_id, best_page))
    }

    /// Grow all tips one step — call this each scheduler tick
    pub fn grow_tick(&mut self) {
        self.tick += 1;
        self.substrate.tick_decay();

        let tip_ids: Vec<u32> = self.hyphae.keys().cloned().collect();

        for tip_id in tip_ids {
            // Advance tip
            {
                let tip = match self.hyphae.get_mut(&tip_id) { Some(t) => t, None => continue };
                let _ = tip.advance(&mut self.substrate, &self.lut);
                if tip.stalled { self.stalls.fetch_add(1, Ordering::Relaxed); }
            }

            // Sporulation: large mats shed spores
            if let Some(tip) = self.hyphae.get(&tip_id) {
                if tip.can_sporulate() {
                    let spore = Spore {
                        page_lo: tip.extent_lo,
                        page_hi: tip.extent_hi,
                        pid: tip.pid,
                        age: self.tick,
                    };
                    self.spores.deposit(spore);
                }
            }
        }

        // Anastomosis check: fuse tips from same PID within radius
        self.check_anastomosis();
    }

    fn check_anastomosis(&mut self) {
        let ids: Vec<u32> = self.hyphae.keys().cloned().collect();
        for i in 0..ids.len() {
            for j in (i+1)..ids.len() {
                let (a_id, b_id) = (ids[i], ids[j]);
                let (a_pos, a_pid, b_pos, b_pid) = match (
                    self.hyphae.get(&a_id), self.hyphae.get(&b_id)
                ) {
                    (Some(a), Some(b)) => (a.position, a.pid, b.position, b.pid),
                    _ => continue,
                };
                if a_pid != b_pid { continue; }
                if a_pos.abs_diff(b_pos) <= ANASTOMOSIS_RADIUS {
                    // Fuse: absorb b into a
                    let (b_lo, b_hi, b_count) = if let Some(b) = self.hyphae.get(&b_id) {
                        (b.extent_lo, b.extent_hi, b.page_count)
                    } else { continue; };
                    
                    if let Some(a) = self.hyphae.get_mut(&a_id) {
                        a.extent_lo = a.extent_lo.min(b_lo);
                        a.extent_hi = a.extent_hi.max(b_hi);
                        a.page_count += b_count;
                    }
                    if let Some(b) = self.hyphae.get_mut(&b_id) {
                        b.anastomosed = true;
                        b.merged_into = Some(a_id);
                    }
                    self.anastomoses.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        // Prune anastomosed tips
        self.hyphae.retain(|_, t| !t.anastomosed);
    }

    /// Free a range of pages — emits nutrient pulse, attracts nearest tip
    pub fn free_pages(&mut self, page_lo: usize, page_hi: usize) {
        for p in page_lo..page_hi.min(self.substrate.total_pages) {
            self.substrate.release(p);
        }
        self.total_free.fetch_add(1, Ordering::Relaxed);
    }

    pub fn stats(&self) -> MyceliumStats {
        MyceliumStats {
            free_pages: self.substrate.free_count.load(Ordering::Relaxed),
            total_pages: self.substrate.total_pages,
            active_hyphae: self.hyphae.len() as u32,
            spore_count: self.spores.count as u32,
            tick: self.tick,
            anastomoses: self.anastomoses.load(Ordering::Relaxed),
            stalls: self.stalls.load(Ordering::Relaxed),
            spore_hits: self.spores.hits.load(Ordering::Relaxed),
            spore_misses: self.spores.misses.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct MyceliumStats {
    pub free_pages:    usize,
    pub total_pages:   usize,
    pub active_hyphae: u32,
    pub spore_count:   u32,
    pub tick:          u64,
    pub anastomoses:   u64,
    pub stalls:        u64,
    pub spore_hits:    u64,
    pub spore_misses:  u64,
}
