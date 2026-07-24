//! RICCI MEMORY FLOW
//!
//! Pages are vertices; recent co-access creates metric edges.
//! Scalar curvature S_i high → contested / fragmented region.
//! Flow step: move free-frame "mass" along -grad S (integer).
//! After K steps, flat cold regions are compaction candidates
//! for mycelium reclaim; deep wells pin working sets.


pub const MAX_NODES: usize = 128;
pub const MAX_EDGES: usize = 512;
/// 16.16 fixed point
pub type Fp = i32;
pub const FP_ONE: Fp = 0x1_0000;

#[derive(Clone, Copy, Debug)]
pub struct MemNode {
    pub live: bool,
    /// Owning process / region id
    pub region: u32,
    /// Touch temperature (decayed)
    pub heat: Fp,
    /// Scalar curvature estimate
    pub scalar: Fp,
    /// Free pages in this region (mass)
    pub free_mass: u32,
    /// Total pages
    pub total: u32,
}

impl MemNode {
    pub const EMPTY: Self = Self {
        live: false,
        region: 0,
        heat: 0,
        scalar: 0,
        free_mass: 0,
        total: 0,
    };
}

#[derive(Clone, Copy, Debug)]
pub struct MemEdge {
    pub live: bool,
    pub a: u16,
    pub b: u16,
    /// Metric distance inverse (co-access strength)
    pub weight: Fp,
}

impl MemEdge {
    pub const EMPTY: Self = Self {
        live: false,
        a: 0,
        b: 0,
        weight: 0,
    };
}

pub struct RicciManifold {
    nodes: [MemNode; MAX_NODES],
    edges: [MemEdge; MAX_EDGES],
    node_len: usize,
    edge_len: usize,
    /// Flow time step τ in 16.16
    tau: Fp,
}

impl RicciManifold {
    pub const fn new() -> Self {
        Self {
            nodes: [MemNode::EMPTY; MAX_NODES],
            edges: [MemEdge::EMPTY; MAX_EDGES],
            node_len: 0,
            edge_len: 0,
            tau: FP_ONE / 8,
        }
    }

    pub fn upsert_region(
        &mut self,
        region: u32,
        free_mass: u32,
        total: u32,
        heat: Fp,
    ) -> Option<usize> {
        for (i, n) in self.nodes.iter_mut().enumerate().take(self.node_len) {
            if n.live && n.region == region {
                n.free_mass = free_mass;
                n.total = total;
                n.heat = heat;
                return Some(i);
            }
        }
        if self.node_len >= MAX_NODES {
            return None;
        }
        let i = self.node_len;
        self.nodes[i] = MemNode {
            live: true,
            region,
            heat,
            scalar: 0,
            free_mass,
            total,
        };
        self.node_len += 1;
        Some(i)
    }

    pub fn touch_edge(&mut self, a: u16, b: u16, delta: Fp) {
        if a == b {
            return;
        }
        for e in self.edges.iter_mut().take(self.edge_len) {
            if e.live && ((e.a == a && e.b == b) || (e.a == b && e.b == a)) {
                e.weight = e.weight.saturating_add(delta);
                return;
            }
        }
        if self.edge_len >= MAX_EDGES {
            return;
        }
        self.edges[self.edge_len] = MemEdge {
            live: true,
            a,
            b,
            weight: delta.max(1),
        };
        self.edge_len += 1;
    }

    /// Recompute scalar curvature proxy:
    /// S_i = heat_i * degree_i - Σ_j weight_ij * heat_j
    /// (discrete Bochner-ish: high local heat + weak coupling ⇒ positive S ⇒ wants to shed mass)
    pub fn recompute_curvature(&mut self) {
        for n in self.nodes.iter_mut().take(self.node_len) {
            n.scalar = 0;
        }
        // degree heat
        let mut degree = [0i32; MAX_NODES];
        for e in self.edges.iter().take(self.edge_len) {
            if !e.live {
                continue;
            }
            degree[e.a as usize] = degree[e.a as usize].saturating_add(e.weight);
            degree[e.b as usize] = degree[e.b as usize].saturating_add(e.weight);
        }
        for i in 0..self.node_len {
            if !self.nodes[i].live {
                continue;
            }
            let h = self.nodes[i].heat;
            self.nodes[i].scalar = ((h as i64 * degree[i] as i64) >> 16) as i32;
        }
        for e in self.edges.iter().take(self.edge_len) {
            if !e.live {
                continue;
            }
            let ha = self.nodes[e.a as usize].heat;
            let hb = self.nodes[e.b as usize].heat;
            let pull_a = ((e.weight as i64 * hb as i64) >> 16) as i32;
            let pull_b = ((e.weight as i64 * ha as i64) >> 16) as i32;
            self.nodes[e.a as usize].scalar =
                self.nodes[e.a as usize].scalar.saturating_sub(pull_a);
            self.nodes[e.b as usize].scalar =
                self.nodes[e.b as usize].scalar.saturating_sub(pull_b);
        }
    }

    /// One Ricci flow step: move free_mass from high S → low S along edges.
    pub fn flow_step(&mut self) {
        self.recompute_curvature();
        // Gather planned deltas then apply (avoid order bias)
        let mut delta = [0i32; MAX_NODES];
        for e in self.edges.iter().take(self.edge_len) {
            if !e.live {
                continue;
            }
            let sa = self.nodes[e.a as usize].scalar;
            let sb = self.nodes[e.b as usize].scalar;
            let diff = sa.saturating_sub(sb);
            // mass flux ∝ weight * diff * tau
            let flux = ((diff as i64 * e.weight as i64 * self.tau as i64) >> 32) as i32;
            if flux == 0 {
                continue;
            }
            // flux > 0: mass flow a → b (a has higher curvature)
            let a = e.a as usize;
            let b = e.b as usize;
            let avail = self.nodes[a].free_mass as i32;
            let f = flux.clamp(-(self.nodes[b].free_mass as i32), avail);
            delta[a] -= f;
            delta[b] += f;
        }
        for i in 0..self.node_len {
            if !self.nodes[i].live {
                continue;
            }
            let next = self.nodes[i].free_mass as i32 + delta[i];
            self.nodes[i].free_mass = next.max(0) as u32;
        }
    }

    /// Regions with low |scalar| and low heat → mycelium reclaim candidates.
    pub fn reclaim_candidates(&self, out: &mut [u32]) -> usize {
        let mut n = 0usize;
        for node in self.nodes.iter().take(self.node_len) {
            if !node.live || n >= out.len() {
                continue;
            }
            let flat = node.scalar.unsigned_abs() < (FP_ONE as u32 / 16);
            let cold = node.heat < FP_ONE / 32;
            let spare = node.free_mass > 0 && node.total > 0;
            if flat && cold && spare {
                out[n] = node.region;
                n += 1;
            }
        }
        n
    }

    /// Deep wells (negative scalar, high heat) — pin, never compact.
    pub fn pin_candidates(&self, out: &mut [u32]) -> usize {
        let mut n = 0usize;
        for node in self.nodes.iter().take(self.node_len) {
            if !node.live || n >= out.len() {
                continue;
            }
            if node.scalar < -(FP_ONE / 8) && node.heat > FP_ONE / 4 {
                out[n] = node.region;
                n += 1;
            }
        }
        n
    }
}
