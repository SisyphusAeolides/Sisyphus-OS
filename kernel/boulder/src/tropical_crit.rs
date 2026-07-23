// kernel/boulder/src/tropical_crit.rs
//! TropicalCrit — min-plus critical path over a task dependency graph
//!
//! Semiring K = (u64 ∪ {∞}, min, plus, ∞, 0)
//! Matrix mul: (A ⊙ B)[i,j] = min_k A[i,k] + B[k,j]
//!
//! After n-1 tropical powers of adjacency, dist[s,t] = critical path cost.
//! Ready queue priority = -dist[task, sink] (longer outstanding path first).

#![allow(dead_code)]

pub const N: usize = 16;
pub const INF: u64 = u64::MAX / 4;

#[derive(Clone, Copy, Debug)]
pub struct TropMat {
    pub v: [[u64; N]; N],
}

impl TropMat {
    pub const fn infinite() -> Self {
        Self { v: [[INF; N]; N] }
    }

    pub const fn identity() -> Self {
        let mut m = Self::infinite();
        let mut i = 0;
        while i < N {
            m.v[i][i] = 0;
            i += 1;
        }
        m
    }

    pub fn set_edge(&mut self, from: usize, to: usize, cost: u64) {
        if from < N && to < N {
            m_min(&mut self.v[from][to], cost);
        }
    }

    /// Tropical matrix multiply: self ⊙ other
    pub fn mul(self, other: &Self) -> Self {
        let mut out = Self::infinite();
        for i in 0..N {
            for j in 0..N {
                let mut best = INF;
                for k in 0..N {
                    let a = self.v[i][k];
                    let b = other.v[k][j];
                    if a != INF && b != INF {
                        let s = a.saturating_add(b);
                        if s < best {
                            best = s;
                        }
                    }
                }
                out.v[i][j] = best;
            }
        }
        out
    }

    /// Tropical closure-ish: max path via repeated squaring up to N-1
    pub fn critical_paths(self) -> Self {
        let mut r = Self::identity();
        let mut base = self;
        let mut exp = N - 1;
        while exp > 0 {
            if exp & 1 == 1 {
                r = r.mul(&base);
            }
            base = base.mul(&base);
            exp >>= 1;
        }
        r
    }
}

fn m_min(dst: &mut u64, v: u64) {
    if v < *dst {
        *dst = v;
    }
}

pub struct TropicalScheduler {
    /// adjacency in tropical semiring
    pub adj: TropMat,
    /// dist[i] = critical path from i to sink
    pub dist_to_sink: [u64; N],
    pub sink: usize,
    pub live: [bool; N],
}

impl TropicalScheduler {
    pub const fn new(sink: usize) -> Self {
        Self {
            adj: TropMat::identity(),
            dist_to_sink: [INF; N],
            sink,
            live: [false; N],
        }
    }

    pub fn add_task(&mut self, id: usize, cost_to_succ: u64, succ: usize) {
        if id >= N {
            return;
        }
        self.live[id] = true;
        self.adj.set_edge(id, succ, cost_to_succ);
    }

    pub fn recompute(&mut self) {
        let paths = self.adj.critical_paths();
        for i in 0..N {
            self.dist_to_sink[i] = paths.v[i][self.sink];
        }
        self.dist_to_sink[self.sink] = 0;
    }

    /// Higher score = schedule first (longer path remaining).
    pub fn priority(&self, id: usize) -> u64 {
        if id >= N || !self.live[id] {
            return 0;
        }
        let d = self.dist_to_sink[id];
        if d >= INF / 2 { 0 } else { d }
    }

    pub fn pick_ready(&self, ready: &[usize]) -> Option<usize> {
        let mut best = None;
        let mut best_p = 0u64;
        for &id in ready {
            let p = self.priority(id);
            if p >= best_p {
                best_p = p;
                best = Some(id);
            }
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn critical_path_prefers_long_chain() {
        let mut s = TropicalScheduler::new(3);
        // 0 -> 1 -> 3 cost 10+10
        // 2 -> 3 cost 5
        s.add_task(0, 10, 1);
        s.add_task(1, 10, 3);
        s.add_task(2, 5, 3);
        s.live[3] = true;
        s.recompute();
        assert!(s.priority(0) > s.priority(2));
        assert_eq!(s.pick_ready(&[2, 0]), Some(0));
    }
}
