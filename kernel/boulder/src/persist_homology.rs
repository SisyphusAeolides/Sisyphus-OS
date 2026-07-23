//! Exact H0 persistence for one-dimensional free-memory intervals.
//!
//! This is a drop-in replacement for the first PersistH implementation.  It
//! preserves the absorbed component representative before union and applies
//! the elder rule deterministically.

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

    pub const fn end_pfn(self) -> u64 {
        self.start_pfn.saturating_add(self.npages)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H0Bar {
    pub birth_gap: u64,
    pub death_gap: u64,
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

    pub const fn persistence(self) -> u64 {
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

#[derive(Clone, Copy)]
struct GapEdge {
    left: u16,
    right: u16,
    gap: u64,
}

impl GapEdge {
    const EMPTY: Self = Self {
        left: 0,
        right: 0,
        gap: 0,
    };
}

struct ElderUnionFind {
    parent: [u16; MAX_RUNS],
    rank: [u8; MAX_RUNS],
    elder: [u16; MAX_RUNS],
    pages: [u64; MAX_RUNS],
}

impl ElderUnionFind {
    fn new(runs: &[FreeRun], length: usize) -> Self {
        let mut union_find = Self {
            parent: [0; MAX_RUNS],
            rank: [0; MAX_RUNS],
            elder: [0; MAX_RUNS],
            pages: [0; MAX_RUNS],
        };

        for index in 0..length {
            union_find.parent[index] = index as u16;
            union_find.elder[index] = index as u16;
            union_find.pages[index] = runs[index].npages;
        }

        union_find
    }

    fn find(&mut self, value: u16) -> u16 {
        let mut cursor = value;
        while self.parent[cursor as usize] != cursor {
            cursor = self.parent[cursor as usize];
        }
        let root = cursor;

        cursor = value;
        while self.parent[cursor as usize] != cursor {
            let parent = self.parent[cursor as usize];
            self.parent[cursor as usize] = root;
            cursor = parent;
        }

        root
    }

    fn union(&mut self, left: u16, right: u16) -> Option<(u16, u16, u16, u64)> {
        let mut left_root = self.find(left);
        let mut right_root = self.find(right);
        if left_root == right_root {
            return None;
        }

        let left_elder = self.elder[left_root as usize];
        let right_elder = self.elder[right_root as usize];

        let (survivor, dead) = if left_elder < right_elder {
            (left_root, right_root)
        } else if right_elder < left_elder {
            (right_root, left_root)
        } else if self.rank[left_root as usize] >= self.rank[right_root as usize] {
            (left_root, right_root)
        } else {
            (right_root, left_root)
        };

        left_root = survivor;
        right_root = dead;
        let dead_elder = self.elder[right_root as usize];
        let dead_pages = self.pages[right_root as usize];

        self.parent[right_root as usize] = left_root;
        self.pages[left_root as usize] =
            self.pages[left_root as usize].saturating_add(self.pages[right_root as usize]);
        self.elder[left_root as usize] =
            self.elder[left_root as usize].min(self.elder[right_root as usize]);

        if self.rank[left_root as usize] == self.rank[right_root as usize] {
            self.rank[left_root as usize] = self.rank[left_root as usize].saturating_add(1);
        }

        Some((left_root, right_root, dead_elder, dead_pages))
    }
}

pub struct PersistH {
    pub eps_lo: u64,
    pub eps_hi: u64,
    pub bar_threshold: u16,
}

impl PersistH {
    pub const fn default_oracle() -> Self {
        Self {
            eps_lo: 8,
            eps_hi: 512,
            bar_threshold: 12,
        }
    }

    pub fn analyze(&self, runs: &[FreeRun]) -> HomologyReport {
        let length = runs.len().min(MAX_RUNS);
        if length == 0 {
            return HomologyReport {
                beta0_at_zero: 0,
                beta0_at_eps: 0,
                bars_in_band: 0,
                longest_persistence: 0,
                total_free_pages: 0,
                recommend_compact: false,
            };
        }

        let mut ordered = [FreeRun::EMPTY; MAX_RUNS];
        ordered[..length].copy_from_slice(&runs[..length]);
        ordered[..length].sort_unstable_by_key(|run| run.start_pfn);

        let mut total_free_pages = 0_u64;
        for run in &ordered[..length] {
            total_free_pages = total_free_pages.saturating_add(run.npages);
        }

        let mut edges = [GapEdge::EMPTY; MAX_RUNS];
        let edge_count = length.saturating_sub(1);
        for index in 0..edge_count {
            edges[index] = GapEdge {
                left: index as u16,
                right: (index + 1) as u16,
                gap: ordered[index + 1]
                    .start_pfn
                    .saturating_sub(ordered[index].end_pfn()),
            };
        }
        edges[..edge_count].sort_unstable_by_key(|edge| (edge.gap, edge.left, edge.right));

        let mut union_find = ElderUnionFind::new(&ordered, length);
        let mut bars = [H0Bar::EMPTY; MAX_BARS];
        let mut bar_count = 0_usize;
        let mut components = length as u16;
        let beta0_at_zero = components;
        let mut beta0_at_eps = components;

        for edge in edges[..edge_count].iter().copied() {
            if let Some((_survivor, _dead, dead_elder, dead_pages)) =
                union_find.union(edge.left, edge.right)
            {
                bars[bar_count] = H0Bar {
                    birth_gap: 0,
                    death_gap: edge.gap,
                    run_idx: dead_elder,
                    pages: dead_pages,
                };
                bar_count += 1;
                components = components.saturating_sub(1);

                if edge.gap <= self.eps_hi {
                    beta0_at_eps = components;
                }
            }
        }

        let essential_root = union_find.find(0);
        bars[bar_count] = H0Bar {
            birth_gap: 0,
            death_gap: u64::MAX,
            run_idx: union_find.elder[essential_root as usize],
            pages: union_find.pages[essential_root as usize],
        };
        bar_count += 1;

        let mut bars_in_band = 0_u16;
        let mut longest_persistence = 0_u64;

        for bar in &bars[..bar_count] {
            if bar.death_gap == u64::MAX {
                continue;
            }

            longest_persistence = longest_persistence.max(bar.persistence());
            if bar.death_gap >= self.eps_lo && bar.death_gap <= self.eps_hi {
                bars_in_band = bars_in_band.saturating_add(1);
            }
        }

        HomologyReport {
            beta0_at_zero,
            beta0_at_eps,
            bars_in_band,
            longest_persistence,
            total_free_pages,
            recommend_compact: bars_in_band >= self.bar_threshold
                && beta0_at_zero >= self.bar_threshold,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elder_component_survives_every_merge() {
        let runs = [
            FreeRun {
                start_pfn: 0,
                npages: 4,
            },
            FreeRun {
                start_pfn: 8,
                npages: 4,
            },
            FreeRun {
                start_pfn: 32,
                npages: 4,
            },
        ];

        let report = PersistH::default_oracle().analyze(&runs);
        assert_eq!(report.beta0_at_zero, 3);
        assert_eq!(report.beta0_at_eps, 1);
        assert_eq!(report.longest_persistence, 20);
    }
}
