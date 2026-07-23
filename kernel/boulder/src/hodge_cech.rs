// kernel/boulder/src/hodge_cech.rs
//! Hodge Laplacian on a Čech nerve (1-skeleton)
//!
//! Nerve G=(V,E) of a hardware/capability cover.
//! 0-cochains: load / temperature / grant mass on opens (vertices).
//! 1-cochains: oriented fluxes on overlaps (edges).
//!
//! δ₀: C⁰→C¹   (δf)(e=i→j) = f(j) - f(i)
//! δ₀*: C¹→C⁰  (δ*α)(v) = Σ_{e:·→v} α(e) - Σ_{e:v→·} α(e)
//! Δ₀ = δ₀* δ₀     (graph Laplacian, combinatorial)
//! Δ₁ = δ₀ δ₀*     (on 1-cochains; δ₁=0 if no 2-simplices)
//!
//! Heat flow:  f ← f - τ Δ₀ f
//! Harmonic test: ||Δ₀ f||² ≈ 0  ⇒  load is balanced on components.
//! Edge flux α = δ₀ f  is the discrete gradient used for migration.

#![allow(dead_code)]

pub const MAX_V: usize = 32;
pub const MAX_E: usize = 64;

/// 16.16 fixed point
pub type Fp = i32;
pub const FP_ONE: Fp = 0x1_0000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HodgeFault {
    Dim,
    BadEdge,
    NotOriented,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Edge {
    pub live: bool,
    /// oriented tail → head
    pub tail: u8,
    pub head: u8,
    /// metric weight w_e > 0 (integer; contributes as w in inner products)
    pub weight: u16,
}

impl Edge {
    pub const EMPTY: Self = Self {
        live: false,
        tail: 0,
        head: 0,
        weight: 1,
    };
}

/// Oriented Čech 1-skeleton + cochains.
pub struct HodgeNerve {
    pub n_v: usize,
    pub n_e: usize,
    pub edges: [Edge; MAX_E],
    /// 0-cochain (load on vertices)
    pub f0: [Fp; MAX_V],
    /// 1-cochain (flux on edges)
    pub f1: [Fp; MAX_E],
}

impl HodgeNerve {
    pub const fn new(n_v: usize) -> Self {
        Self {
            n_v: if n_v < MAX_V { n_v } else { MAX_V },
            n_e: 0,
            edges: [Edge::EMPTY; MAX_E],
            f0: [0; MAX_V],
            f1: [0; MAX_E],
        }
    }

    pub fn add_edge(&mut self, tail: u8, head: u8, weight: u16) -> Result<usize, HodgeFault> {
        if tail as usize >= self.n_v || head as usize >= self.n_v || tail == head {
            return Err(HodgeFault::BadEdge);
        }
        if self.n_e >= MAX_E {
            return Err(HodgeFault::Dim);
        }
        let id = self.n_e;
        self.edges[id] = Edge {
            live: true,
            tail,
            head,
            weight: weight.max(1),
        };
        self.n_e += 1;
        Ok(id)
    }

    pub fn set_load(&mut self, v: usize, value: Fp) -> Result<(), HodgeFault> {
        if v >= self.n_v {
            return Err(HodgeFault::Dim);
        }
        self.f0[v] = value;
        Ok(())
    }

    pub fn load(&self, v: usize) -> Fp {
        self.f0.get(v).copied().unwrap_or(0)
    }

    /// δ₀: f0 → out 1-cochain (caller buffer).
    pub fn coboundary0(&self, out: &mut [Fp; MAX_E]) {
        *out = [0; MAX_E];
        for (i, e) in self.edges.iter().enumerate().take(self.n_e) {
            if !e.live {
                continue;
            }
            let fi = self.f0[e.tail as usize];
            let fj = self.f0[e.head as usize];
            // (δf)_e = f(head) - f(tail)
            out[i] = fj.saturating_sub(fi);
        }
    }

    /// δ₀*: f1 → out 0-cochain.
    pub fn adjoint_coboundary0(&self, alpha: &[Fp; MAX_E], out: &mut [Fp; MAX_V]) {
        *out = [0; MAX_V];
        for (i, e) in self.edges.iter().enumerate().take(self.n_e) {
            if !e.live {
                continue;
            }
            let a = alpha[i];
            // weight scales the inner product ⟨α,δf⟩_w = Σ w α (δf)
            // δ* includes w: (δ*α)_v = Σ_{head=v} w α - Σ_{tail=v} w α
            let w = e.weight as Fp;
            let wa = mul_fp_int(a, w);
            out[e.head as usize] = out[e.head as usize].saturating_add(wa);
            out[e.tail as usize] = out[e.tail as usize].saturating_sub(wa);
        }
    }

    /// Δ₀ f = δ* δ f  (graph Laplacian applied to f0).
    pub fn laplace0(&self, out: &mut [Fp; MAX_V]) {
        let mut alpha = [0; MAX_E];
        self.coboundary0(&mut alpha);
        self.adjoint_coboundary0(&alpha, out);
    }

    /// Δ₁ α = δ δ* α  on 1-cochains (no δ₁).
    pub fn laplace1(&self, alpha: &[Fp; MAX_E], out: &mut [Fp; MAX_E]) {
        let mut tmp0 = [0; MAX_V];
        self.adjoint_coboundary0(alpha, &mut tmp0);
        // δ of tmp0
        *out = [0; MAX_E];
        for (i, e) in self.edges.iter().enumerate().take(self.n_e) {
            if !e.live {
                continue;
            }
            out[i] = tmp0[e.head as usize].saturating_sub(tmp0[e.tail as usize]);
        }
    }

    /// One heat-flow step: f ← f - τ Δ₀ f, τ in 16.16.
    pub fn heat_step0(&mut self, tau_fp: Fp) {
        let mut lf = [0; MAX_V];
        self.laplace0(&mut lf);
        for v in 0..self.n_v {
            let step = fp_mul(tau_fp, lf[v]);
            self.f0[v] = self.f0[v].saturating_sub(step);
        }
    }

    pub fn heat_flow0(&mut self, tau_fp: Fp, steps: usize) {
        for _ in 0..steps {
            self.heat_step0(tau_fp);
        }
    }

    /// ||Δ₀ f||² in 16.16 (energy of non-harmonic part).
    pub fn nonharmonic_energy0(&self) -> u64 {
        let mut lf = [0; MAX_V];
        self.laplace0(&mut lf);
        let mut s = 0i64;
        for v in 0..self.n_v {
            s = s.saturating_add((lf[v] as i64).saturating_mul(lf[v] as i64));
        }
        (s >> 16) as u64
    }

    /// Discrete gradient flux α = δ₀ f, stored into f1.
    pub fn store_gradient_flux(&mut self) {
        let mut alpha = [0; MAX_E];
        self.coboundary0(&mut alpha);
        self.f1 = alpha;
    }

    /// Suggest migration: for each edge with large |flux|, move mass head←tail
    /// if flux>0 (load higher at head... wait: δf = f(head)-f(tail); flux>0
    /// means head hotter → migrate load head→tail along -grad).
    pub fn migration_delta(&self, out_vertex_delta: &mut [Fp; MAX_V]) {
        *out_vertex_delta = [0; MAX_V];
        let mut alpha = [0; MAX_E];
        self.coboundary0(&mut alpha);
        for (i, e) in self.edges.iter().enumerate().take(self.n_e) {
            if !e.live {
                continue;
            }
            // move proportional to -w * (f(head)-f(tail)) from head toward tail
            let flux = alpha[i];
            let w = e.weight as Fp;
            let mv = fp_mul(flux, w) / 8; // gain
            out_vertex_delta[e.head as usize] =
                out_vertex_delta[e.head as usize].saturating_sub(mv);
            out_vertex_delta[e.tail as usize] =
                out_vertex_delta[e.tail as usize].saturating_add(mv);
        }
    }

    /// Rayleigh quotient f^T Δ f / f^T f  (algebraic connectivity probe on mean-zero f).
    pub fn rayleigh0(&self) -> Fp {
        let mut lf = [0; MAX_V];
        self.laplace0(&mut lf);
        let mut num = 0i64;
        let mut den = 0i64;
        for v in 0..self.n_v {
            num = num.saturating_add((self.f0[v] as i64).saturating_mul(lf[v] as i64));
            den = den.saturating_add((self.f0[v] as i64).saturating_mul(self.f0[v] as i64));
        }
        if den == 0 {
            return 0;
        }
        (num / (den >> 16).max(1)) as Fp
    }
}

#[inline]
fn fp_mul(a: Fp, b: Fp) -> Fp {
    ((a as i64 * b as i64) >> 16) as Fp
}

#[inline]
fn mul_fp_int(a: Fp, w: Fp) -> Fp {
    a.saturating_mul(w)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_is_harmonic() {
        let mut h = HodgeNerve::new(3);
        h.add_edge(0, 1, 1).unwrap();
        h.add_edge(1, 2, 1).unwrap();
        h.set_load(0, FP_ONE).unwrap();
        h.set_load(1, FP_ONE).unwrap();
        h.set_load(2, FP_ONE).unwrap();
        assert_eq!(h.nonharmonic_energy0(), 0);
    }

    #[test]
    fn heat_flow_reduces_energy() {
        let mut h = HodgeNerve::new(3);
        h.add_edge(0, 1, 1).unwrap();
        h.add_edge(1, 2, 1).unwrap();
        h.set_load(0, 4 * FP_ONE).unwrap();
        h.set_load(1, 0).unwrap();
        h.set_load(2, 0).unwrap();
        let e0 = h.nonharmonic_energy0();
        h.heat_flow0(FP_ONE / 8, 32);
        let e1 = h.nonharmonic_energy0();
        assert!(e1 < e0);
    }

    #[test]
    fn gradient_flux_nonzero_when_unbalanced() {
        let mut h = HodgeNerve::new(2);
        h.add_edge(0, 1, 1).unwrap();
        h.set_load(0, 0).unwrap();
        h.set_load(1, 2 * FP_ONE).unwrap();
        let mut a = [0; MAX_E];
        h.coboundary0(&mut a);
        assert!(a[0] > 0);
    }
}
