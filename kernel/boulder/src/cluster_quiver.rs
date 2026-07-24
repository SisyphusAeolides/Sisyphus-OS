// kernel/boulder/src/cluster_quiver.rs
//! Cluster algebra mutations on resource quivers (Fomin–Zelevinsky)
//!
//! x'_k = (∏_{i→k} x_i + ∏_{k→j} x_j) / x_k
//! Quiver: add composites, reverse incident, cancel 2-cycles.


pub const MAX_N: usize = 16;
pub const MAX_E: usize = 64;
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum NodeKind {
    Bridge = 0,
    Display = 1,
    Network = 2,
    Storage = 3,
    Usb = 4,
    Other = 5,
    Strategy = 6,
    DmaPool = 7,
    IrqBudget = 8,
}

#[derive(Clone, Debug)]
pub struct ResourceQuiver {
    pub n: usize,
    pub arrows: [Arrow; MAX_E],
    pub e_len: usize,
    pub x: [Fp; MAX_N],
    pub congestion: [Fp; MAX_N],
    pub kind: [NodeKind; MAX_N],
    /// PCI inventory index or strategy ordinal (0xFFFF = none)
    pub tag: [u16; MAX_N],
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
            kind: [NodeKind::Other; MAX_N],
            tag: [0xFFFF; MAX_N],
            mutation_count: 0,
        }
    }

    pub fn set_node(
        &mut self,
        i: usize,
        kind: NodeKind,
        tag: u16,
        x: Fp,
    ) -> Result<(), ClusterFault> {
        if i >= self.n {
            return Err(ClusterFault::BadNode);
        }
        if x == 0 {
            return Err(ClusterFault::ZeroCluster);
        }
        self.kind[i] = kind;
        self.tag[i] = tag;
        self.x[i] = x;
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
        self.add_arrow_raw(from, to, mult)?;
        self.cancel_two_cycles();
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

    fn cancel_two_cycles(&mut self) {
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
                    self.arrows[i].mult -= m;
                    self.arrows[j].mult -= m;
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

    pub fn mutate(&mut self, k: usize) -> Result<(), ClusterFault> {
        if k >= self.n {
            return Err(ClusterFault::BadNode);
        }
        if self.x[k] == 0 {
            return Err(ClusterFault::ZeroCluster);
        }
        let p_in = self.prod_into(k)?;
        let p_out = self.prod_out_of(k)?;
        let num = (p_in as u64).saturating_add(p_out as u64);
        let xp = (num * FP_ONE as u64 / self.x[k] as u64).min(u32::MAX as u64) as Fp;
        if xp == 0 {
            return Err(ClusterFault::ZeroCluster);
        }

        let mut into: [(u8, u8); MAX_N] = [(0, 0); MAX_N];
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
        for ii in 0..into_n {
            for jj in 0..out_n {
                let (i, mi) = into[ii];
                let (j, mj) = out[jj];
                if i != j {
                    let m = (mi as u16 * mj as u16).min(255) as u8;
                    self.add_arrow_raw(i, j, m)?;
                }
            }
        }
        for a in self.arrows.iter_mut().take(self.e_len) {
            if a.live && (a.from as usize == k || a.to as usize == k) {
                a.live = false;
            }
        }
        for ii in 0..into_n {
            let (i, mi) = into[ii];
            self.add_arrow_raw(k as u8, i, mi)?;
        }
        for jj in 0..out_n {
            let (j, mj) = out[jj];
            self.add_arrow_raw(j, k as u8, mj)?;
        }
        self.cancel_two_cycles();
        self.x[k] = xp;
        self.mutation_count = self.mutation_count.saturating_add(1);
        Ok(())
    }

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

    /// Export cluster values as Noether-style ceiling scales (16.16).
    pub fn ceiling_scales(&self, out: &mut [Fp; MAX_N]) {
        *out = [0; MAX_N];
        for i in 0..self.n {
            out[i] = self.x[i];
        }
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
    fn a2_mut() {
        let mut q = ResourceQuiver::new(2);
        q.set_node(0, NodeKind::Bridge, 0, FP_ONE).unwrap();
        q.set_node(1, NodeKind::Display, 1, FP_ONE).unwrap();
        q.add_arrow(0, 1, 1).unwrap();
        q.mutate(0).unwrap();
        assert_eq!(q.x[0], 2 * FP_ONE);
    }
}
