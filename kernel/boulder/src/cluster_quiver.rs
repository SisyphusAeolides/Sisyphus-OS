// kernel/boulder/src/cluster_quiver.rs
//! Cluster algebra mutations on resource quivers
//!
//! Quiver Q: directed graph, no loops, 2-cycles cancelled after mutation.
//! Cluster x[i] > 0: resource mass at node i (DMA credits, IRQ budgets,
//! scheduler weights, Noether ceilings — any positive token).
//!
//! Mutation μ_k (Fomin–Zelevinsky):
//!   1) for each i→k→j add edge i→j (multiplicity product)
//!   2) reverse edges incident to k
//!   3) cancel 2-cycles
//!   4) x'_k = (∏_{i→k} x_i  +  ∏_{k→j} x_j) / x_k
//!
//! OS policy: when node k is congested, μ_k redistributes algebraic mass.
//! Laurent property ⇒ x'_k is a Laurent polynomial in initial seeds —
//! denominators never accumulate arbitrary products (numerical stability
//! under fixed-point if you renormalize).

#![allow(dead_code)]

pub const MAX_N: usize = 12;
pub const MAX_E: usize = 48;

/// 16.16 positive fixed point
pub type Fp = u32;
pub const FP_ONE: Fp = 0x1_0000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClusterFault {
    Dim,
    BadNode,
    Loop,
    ZeroCluster,
    Overflow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Arrow {
    pub live: bool,
    pub from: u8,
    pub to: u8,
    /// multiplicity ≥ 1
    pub mult: u8,
}

impl Arrow {
    pub const EMPTY: Self = Self {
        live: false,
        from: 0,
        to: 0,
        mult: 0,
    };
}

#[derive(Clone, Debug)]
pub struct ResourceQuiver {
    pub n: usize,
    pub arrows: [Arrow; MAX_E],
    pub e_len: usize,
    /// cluster variables
    pub x: [Fp; MAX_N],
    /// congestion score (policy input)
    pub congestion: [Fp; MAX_N],
    pub mutation_count: u32,
}

impl ResourceQuiver {
    pub const fn new(n: usize) -> Self {
        Self {
            n: if n < MAX_N { n } else { MAX_N },
            arrows: [Arrow::EMPTY; MAX_E],
            e_len: 0,
            x: [FP_ONE; MAX_N],
            congestion: [0; MAX_N],
            mutation_count: 0,
        }
    }

    pub fn set_cluster(&mut self, i: usize, val: Fp) -> Result<(), ClusterFault> {
        if i >= self.n {
            return Err(ClusterFault::BadNode);
        }
        if val == 0 {
            return Err(ClusterFault::ZeroCluster);
        }
        self.x[i] = val;
        Ok(())
    }

    pub fn set_congestion(&mut self, i: usize, val: Fp) -> Result<(), ClusterFault> {
        if i >= self.n {
            return Err(ClusterFault::BadNode);
        }
        self.congestion[i] = val;
        Ok(())
    }

    pub fn add_arrow(&mut self, from: u8, to: u8, mult: u8) -> Result<(), ClusterFault> {
        if from as usize >= self.n || to as usize >= self.n {
            return Err(ClusterFault::BadNode);
        }
        if from == to {
            return Err(ClusterFault::Loop);
        }
        if mult == 0 {
            return Ok(());
        }
        // merge if exists
        for a in self.arrows.iter_mut().take(self.e_len) {
            if a.live && a.from == from && a.to == to {
                a.mult = a.mult.saturating_add(mult);
                return Ok(());
            }
        }
        if self.e_len >= MAX_E {
            return Err(ClusterFault::Dim);
        }
        self.arrows[self.e_len] = Arrow {
            live: true,
            from,
            to,
            mult,
        };
        self.e_len += 1;
        self.cancel_two_cycles();
        Ok(())
    }

    fn cancel_two_cycles(&mut self) {
        // For each pair i→j and j→i, subtract multiplicities
        for i in 0..self.e_len {
            if !self.arrows[i].live {
                continue;
            }
            let a = self.arrows[i];
            for j in (i + 1)..self.e_len {
                if !self.arrows[j].live {
                    continue;
                }
                let b = self.arrows[j];
                if a.from == b.to && a.to == b.from {
                    let m = a.mult.min(b.mult);
                    self.arrows[i].mult = a.mult - m;
                    self.arrows[j].mult = b.mult - m;
                    if self.arrows[i].mult == 0 {
                        self.arrows[i].live = false;
                    }
                    if self.arrows[j].mult == 0 {
                        self.arrows[j].live = false;
                    }
                    break;
                }
            }
        }
    }

    /// Product of x_i^{mult} along arrows into k (integer FP product / FP_ONE^{m-1}).
    fn prod_into(&self, k: usize) -> Result<Fp, ClusterFault> {
        let mut acc: u64 = FP_ONE as u64;
        let mut any = false;
        for a in self.arrows.iter().take(self.e_len) {
            if !a.live || a.to as usize != k {
                continue;
            }
            any = true;
            for _ in 0..a.mult {
                acc = acc
                    .checked_mul(self.x[a.from as usize] as u64)
                    .ok_or(ClusterFault::Overflow)?;
                acc /= FP_ONE as u64;
            }
        }
        if !any {
            // empty product = 1
            return Ok(FP_ONE);
        }
        Ok(acc.min(u32::MAX as u64) as Fp)
    }

    fn prod_out_of(&self, k: usize) -> Result<Fp, ClusterFault> {
        let mut acc: u64 = FP_ONE as u64;
        let mut any = false;
        for a in self.arrows.iter().take(self.e_len) {
            if !a.live || a.from as usize != k {
                continue;
            }
            any = true;
            for _ in 0..a.mult {
                acc = acc
                    .checked_mul(self.x[a.to as usize] as u64)
                    .ok_or(ClusterFault::Overflow)?;
                acc /= FP_ONE as u64;
            }
        }
        if !any {
            return Ok(FP_ONE);
        }
        Ok(acc.min(u32::MAX as u64) as Fp)
    }

    /// Mutate at vertex k.
    pub fn mutate(&mut self, k: usize) -> Result<(), ClusterFault> {
        if k >= self.n {
            return Err(ClusterFault::BadNode);
        }
        if self.x[k] == 0 {
            return Err(ClusterFault::ZeroCluster);
        }

        // --- cluster variable ---
        let p_in = self.prod_into(k)?;
        let p_out = self.prod_out_of(k)?;
        let num = (p_in as u64).saturating_add(p_out as u64);
        let xk = self.x[k] as u64;
        // x'_k = (p_in + p_out) / x_k   in 16.16:
        // (num << 16) / xk  but num already 16.16; xk is 16.16
        // value = num / (xk / FP_ONE) = num * FP_ONE / xk
        let xp = (num * FP_ONE as u64 / xk).min(u32::MAX as u64) as Fp;
        if xp == 0 {
            return Err(ClusterFault::ZeroCluster);
        }

        // --- quiver mutation ---
        // 1) collect arrows
        let mut into: [(u8, u8); MAX_N] = [(0, 0); MAX_N]; // (from, mult)
        let mut into_n = 0usize;
        let mut out: [(u8, u8); MAX_N] = [(0, 0); MAX_N];
        let mut out_n = 0usize;
        for a in self.arrows.iter().take(self.e_len) {
            if !a.live {
                continue;
            }
            if a.to as usize == k {
                into[into_n] = (a.from, a.mult);
                into_n += 1;
            } else if a.from as usize == k {
                out[out_n] = (a.to, a.mult);
                out_n += 1;
            }
        }

        // add composite edges i→j with mult_in * mult_out
        for ii in 0..into_n {
            for jj in 0..out_n {
                let (i, mi) = into[ii];
                let (j, mj) = out[jj];
                if i == j {
                    continue;
                }
                let m = (mi as u16 * mj as u16).min(255) as u8;
                self.add_arrow_raw(i, j, m)?;
            }
        }

        // 2) reverse edges incident to k: delete old, add reversed
        for a in self.arrows.iter_mut().take(self.e_len) {
            if !a.live {
                continue;
            }
            if a.from as usize == k || a.to as usize == k {
                a.live = false;
            }
        }
        for ii in 0..into_n {
            let (i, mi) = into[ii];
            // was i→k, reverse k→i
            self.add_arrow_raw(k as u8, i, mi)?;
        }
        for jj in 0..out_n {
            let (j, mj) = out[jj];
            // was k→j, reverse j→k
            self.add_arrow_raw(j, k as u8, mj)?;
        }

        self.cancel_two_cycles();
        self.x[k] = xp;
        self.mutation_count = self.mutation_count.saturating_add(1);
        Ok(())
    }

    fn add_arrow_raw(&mut self, from: u8, to: u8, mult: u8) -> Result<(), ClusterFault> {
        if mult == 0 || from == to {
            return Ok(());
        }
        for a in self.arrows.iter_mut().take(self.e_len) {
            if a.live && a.from == from && a.to == to {
                a.mult = a.mult.saturating_add(mult);
                return Ok(());
            }
        }
        // reuse dead slot
        for a in self.arrows.iter_mut().take(self.e_len) {
            if !a.live {
                *a = Arrow {
                    live: true,
                    from,
                    to,
                    mult,
                };
                return Ok(());
            }
        }
        if self.e_len >= MAX_E {
            return Err(ClusterFault::Dim);
        }
        self.arrows[self.e_len] = Arrow {
            live: true,
            from,
            to,
            mult,
        };
        self.e_len += 1;
        Ok(())
    }

    /// Mutate the most congested vertex if above threshold.
    pub fn mutate_hottest(&mut self, threshold_fp: Fp) -> Result<Option<usize>, ClusterFault> {
        let mut best = None;
        let mut best_c = threshold_fp;
        for i in 0..self.n {
            if self.congestion[i] > best_c {
                best_c = self.congestion[i];
                best = Some(i);
            }
        }
        if let Some(k) = best {
            self.mutate(k)?;
            Ok(Some(k))
        } else {
            Ok(None)
        }
    }

    /// Total cluster mass Σ x_i (diagnostic; not mutation-invariant).
    pub fn total_mass(&self) -> u64 {
        let mut s = 0u64;
        for i in 0..self.n {
            s += self.x[i] as u64;
        }
        s
    }

    pub fn live_arrows(&self) -> usize {
        self.arrows
            .iter()
            .take(self.e_len)
            .filter(|a| a.live)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a2_mutation_exchange() {
        // Quiver 0 → 1 , seed x0=x1=1
        let mut q = ResourceQuiver::new(2);
        q.set_cluster(0, FP_ONE).unwrap();
        q.set_cluster(1, FP_ONE).unwrap();
        q.add_arrow(0, 1, 1).unwrap();
        q.mutate(0).unwrap();
        // x0' = (∅prod_in=1 + x1) / x0 = (1+1)/1 = 2
        assert_eq!(q.x[0], 2 * FP_ONE);
        // edge should reverse to 1 → 0
        assert!(q.arrows.iter().any(|a| a.live && a.from == 1 && a.to == 0));
    }

    #[test]
    fn mutate_twice_a1_involution_like() {
        // On A1 (single arrow), μ_k^2 often returns toward seed family
        let mut q = ResourceQuiver::new(2);
        q.set_cluster(0, FP_ONE).unwrap();
        q.set_cluster(1, 2 * FP_ONE).unwrap();
        q.add_arrow(0, 1, 1).unwrap();
        let x1_before = q.x[1];
        q.mutate(0).unwrap();
        q.mutate(0).unwrap();
        // x1 unchanged by mut at 0
        assert_eq!(q.x[1], x1_before);
    }

    #[test]
    fn hottest_policy() {
        let mut q = ResourceQuiver::new(3);
        q.add_arrow(0, 1, 1).unwrap();
        q.add_arrow(1, 2, 1).unwrap();
        q.set_congestion(1, 5 * FP_ONE).unwrap();
        q.set_congestion(0, FP_ONE).unwrap();
        let k = q.mutate_hottest(2 * FP_ONE).unwrap();
        assert_eq!(k, Some(1));
    }
}
