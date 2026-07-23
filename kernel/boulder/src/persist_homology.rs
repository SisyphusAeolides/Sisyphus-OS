// kernel/boulder/src/persist_homology.rs
//! PersistH — persistent homology (H₀) oracle for physical free memory
//!
//! Model: each free run is a 0-simplex at its midpoint, birth = 0.
//! Edge between runs i,j exists when gap(i,j) ≤ ε (filtration value).
//! H₀ barcode: components merge as ε grows (union-find by gap sort).
//!
//! Persistence pair (birth, death) for a component that merges:
//!   birth = 0, death = gap at merge
//! Long bars = stable large free regions.
//! Many short bars at high death values = pathological fragmentation.
//!
//! Decision: if count{ bars with death ∈ [ε_lo, ε_hi] } > Θ, compact.

#![allow(dead_code)]

pub const MAX_RUNS: usize = 128;
pub const MAX_BARS: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FreeRun {
    pub start_pfn: u64,
    pub npages: u64,
}

impl FreeRun {
    pub const EMPTY: Self = Self {
        start_pfn: 0,
        npages: 0,
    };

    #[inline]
    pub fn end_pfn(self) -> u64 {
        self.start_pfn.saturating_add(self.npages)
    }

    #[inline]
    pub fn mid_pfn(self) -> u64 {
        self.start_pfn.saturating_add(self.npages / 2)
    }
}

/// H₀ bar in the free-memory filtration (birth fixed at 0 for runs).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H0Bar {
    pub birth_gap: u64,
    pub death_gap: u64,
    /// Representative run index at birth
    pub run_idx: u16,
    pub pages: u64,
}

impl H0Bar {
    pub const EMPTY: Self = Self {
        birth_gap: 0,
        death_gap: u64::MAX,
        run_idx: 0,
        pages: 0,
    };

    pub fn persistence(self) -> u64 {
        self.death_gap.saturating_sub(self.birth_gap)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HomologyReport {
    pub beta0_at_zero: u16,
    pub beta0_at_eps: u16,
    pub bars_in_band: u16,
    pub longest_persistence: u64,
    pub total_free_pages: u64,
    pub recommend_compact: bool,
}

struct Uf {
    parent: [u16; MAX_RUNS],
    rank: [u8; MAX_RUNS],
    /// pages in component
    mass: [u64; MAX_RUNS],
}

impl Uf {
    fn new(n: usize, runs: &[FreeRun]) -> Self {
        let mut uf = Self {
            parent: [0; MAX_RUNS],
            rank: [0; MAX_RUNS],
            mass: [0; MAX_RUNS],
        };
        for i in 0..n {
            uf.parent[i] = i as u16;
            uf.mass[i] = runs[i].npages;
        }
        uf
    }

    fn find(&mut self, mut x: u16) -> u16 {
        while self.parent[x as usize] != x {
            let p = self.parent[x as usize];
            self.parent[x as usize] = self.parent[p as usize];
            x = self.parent[x as usize];
        }
        x
    }

    fn union(&mut self, a: u16, b: u16) -> Option<(u16, u16)> {
        let mut ra = self.find(a);
        let mut rb = self.find(b);
        if ra == rb {
            return None;
        }
        // return (survivor, dead) for barcode
        if self.rank[ra as usize] < self.rank[rb as usize] {
            core::mem::swap(&mut ra, &mut rb);
        }
        self.parent[rb as usize] = ra;
        self.mass[ra as usize] = self.mass[ra as usize].saturating_add(self.mass[rb as usize]);
        if self.rank[ra as usize] == self.rank[rb as usize] {
            self.rank[ra as usize] = self.rank[ra as usize].saturating_add(1);
        }
        Some((ra, rb))
    }
}

#[derive(Clone, Copy)]
struct GapEdge {
    i: u16,
    j: u16,
    gap: u64,
}

pub struct PersistH {
    /// Compaction if this many bars die inside [eps_lo, eps_hi]
    pub eps_lo: u64,
    pub eps_hi: u64,
    pub bar_threshold: u16,
}

impl PersistH {
    pub const fn default_oracle() -> Self {
        Self {
            eps_lo: 8,   // pages
            eps_hi: 512, // pages
            bar_threshold: 12,
        }
    }

    /// runs must be sorted by start_pfn, non-overlapping.
    pub fn analyze(&self, runs: &[FreeRun]) -> HomologyReport {
        let n = runs.len().min(MAX_RUNS);
        if n == 0 {
            return HomologyReport {
                beta0_at_zero: 0,
                beta0_at_eps: 0,
                bars_in_band: 0,
                longest_persistence: 0,
                total_free_pages: 0,
                recommend_compact: false,
            };
        }

        let mut total = 0u64;
        for r in runs.iter().take(n) {
            total = total.saturating_add(r.npages);
        }

        // Build edges between geometrically successive runs (1-D complex is a path
        // plus optional k-NN by address — successive is exact for intervals on a line).
        let mut edges = [GapEdge { i: 0, j: 0, gap: 0 }; MAX_RUNS];
        let mut e_len = 0usize;
        for i in 0..n.saturating_sub(1) {
            let gap = runs[i + 1].start_pfn.saturating_sub(runs[i].end_pfn());
            edges[e_len] = GapEdge {
                i: i as u16,
                j: (i + 1) as u16,
                gap,
            };
            e_len += 1;
        }
        // sort edges by gap (insertion — n≤128)
        let mut a = 1usize;
        while a < e_len {
            let mut b = a;
            while b > 0 && edges[b].gap < edges[b - 1].gap {
                edges.swap(b, b - 1);
                b -= 1;
            }
            a += 1;
        }

        let mut uf = Uf::new(n, runs);
        let mut bars = [H0Bar::EMPTY; MAX_BARS];
        let mut bar_len = 0usize;
        // each run births a component at gap=0
        for i in 0..n {
            if bar_len < MAX_BARS {
                bars[bar_len] = H0Bar {
                    birth_gap: 0,
                    death_gap: u64::MAX, // essential until merge
                    run_idx: i as u16,
                    pages: runs[i].npages,
                };
                bar_len += 1;
            }
        }

        let mut components = n as u16;
        let beta0_at_zero = components;
        let mut beta0_at_eps = beta0_at_zero;
        let eps_probe = self.eps_hi;

        for e in edges.iter().take(e_len) {
            if let Some((_live, dead)) = uf.union(e.i, e.j) {
                // kill the bar whose rep is dead
                for b in bars.iter_mut().take(bar_len) {
                    if b.death_gap == u64::MAX && uf.find(b.run_idx) == uf.find(dead) {
                        // after union dead's find is live; mark by run match pre-find
                    }
                }
                // Simpler death mark: the component represented by edge.j's old root
                // We saved `dead` as the absorbed root index.
                for b in bars.iter_mut().take(bar_len) {
                    if b.death_gap == u64::MAX && b.run_idx == dead {
                        b.death_gap = e.gap;
                        break;
                    }
                }
                // If not found, mark any bar still in dead's former family:
                for b in bars.iter_mut().take(bar_len) {
                    if b.death_gap == u64::MAX && b.run_idx == e.j && e.gap > 0 {
                        // fallback
                    }
                }
                components = components.saturating_sub(1);
                if e.gap <= eps_probe {
                    beta0_at_eps = components;
                }
            }
        }

        // Count bars with death in band (merged only by bridging a gap in band)
        let mut in_band = 0u16;
        let mut longest = 0u64;
        for b in bars.iter().take(bar_len) {
            let p = b.persistence();
            if p > longest && p != u64::MAX {
                longest = p;
            }
            if b.death_gap >= self.eps_lo && b.death_gap <= self.eps_hi {
                in_band = in_band.saturating_add(1);
            }
        }

        let recommend = in_band >= self.bar_threshold && n as u16 >= self.bar_threshold;

        HomologyReport {
            beta0_at_zero,
            beta0_at_eps,
            bars_in_band: in_band,
            longest_persistence: longest,
            total_free_pages: total,
            recommend_compact: recommend,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesced_vs_fragmented() {
        let oracle = PersistH::default_oracle();
        // one big run
        let nice = [FreeRun {
            start_pfn: 0,
            npages: 10_000,
        }];
        let r1 = oracle.analyze(&nice);
        assert!(!r1.recommend_compact);
        assert_eq!(r1.beta0_at_zero, 1);

        // many tiny runs separated by medium gaps
        let mut ugly = [FreeRun::EMPTY; 20];
        for i in 0..20 {
            ugly[i] = FreeRun {
                start_pfn: i as u64 * 64,
                npages: 4,
            };
        }
        let r2 = oracle.analyze(&ugly);
        assert!(r2.beta0_at_zero == 20);
        // medium gaps → many deaths in band
        assert!(r2.bars_in_band >= 10);
    }
}
