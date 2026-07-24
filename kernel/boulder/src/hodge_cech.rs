// kernel/boulder/src/hodge_cech.rs
//! Hodge Laplacian on a Čech nerve with 2-simplices
//!
//! Cochain complex (real coefficients, fixed-point):
//!   C⁰ --δ₀--> C¹ --δ₁--> C²
//!
//! (δ₀ f)(e: t→h)     = f(h) - f(t)
//! (δ₁ α)(σ: i,j,k)   = α(ij) + α(jk) - α(ik)   (oriented boundary)
//!
//! δ₀*: C¹→C⁰,  δ₁*: C²→C¹   (weighted adjoints)
//! Δ₀ = δ₀* δ₀
//! Δ₁ = δ₀ δ₀* + δ₁* δ₁
//!
//! Heat flow on 0-cochains balances load across the cover.
//! Harmonic 1-forms (Δ₁ α = 0) are cycle fluxes with no triangular sources.


pub const MAX_V: usize = 32;
pub const MAX_E: usize = 64;
pub const MAX_F: usize = 48; // 2-simplices (triangles)

pub type Fp = i32;
pub const FP_ONE: Fp = 0x1_0000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HodgeFault {
    Dim,
    BadEdge,
    BadFace,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Edge {
    pub live: bool,
    pub tail: u8,
    pub head: u8,
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

/// Oriented triangle (i,j,k) with boundary ij + jk - ik.
/// Edge refs store indices into the edge table (0xFFFF = missing).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Face {
    pub live: bool,
    pub v: [u8; 3],
    pub e_ij: u16,
    pub e_jk: u16,
    pub e_ik: u16,
    /// sign for each edge vs canonical tail→head orientation: +1 or -1 stored as i8
    pub s_ij: i8,
    pub s_jk: i8,
    pub s_ik: i8,
    pub weight: u16,
}

impl Face {
    pub const EMPTY: Self = Self {
        live: false,
        v: [0; 3],
        e_ij: 0xFFFF,
        e_jk: 0xFFFF,
        e_ik: 0xFFFF,
        s_ij: 1,
        s_jk: 1,
        s_ik: 1,
        weight: 1,
    };
}

pub struct HodgeNerve {
    pub n_v: usize,
    pub n_e: usize,
    pub n_f: usize,
    pub edges: [Edge; MAX_E],
    pub faces: [Face; MAX_F],
    pub f0: [Fp; MAX_V],
    pub f1: [Fp; MAX_E],
    pub f2: [Fp; MAX_F],
}

impl HodgeNerve {
    pub const fn new(n_v: usize) -> Self {
        Self {
            n_v: if n_v < MAX_V { n_v } else { MAX_V },
            n_e: 0,
            n_f: 0,
            edges: [Edge::EMPTY; MAX_E],
            faces: [Face::EMPTY; MAX_F],
            f0: [0; MAX_V],
            f1: [0; MAX_E],
            f2: [0; MAX_F],
        }
    }

    pub fn set_load(&mut self, v: usize, value: Fp) -> Result<(), HodgeFault> {
        if v >= self.n_v {
            return Err(HodgeFault::Dim);
        }
        self.f0[v] = value;
        Ok(())
    }

    pub fn add_edge(&mut self, tail: u8, head: u8, weight: u16) -> Result<usize, HodgeFault> {
        if tail as usize >= self.n_v || head as usize >= self.n_v || tail == head {
            return Err(HodgeFault::BadEdge);
        }
        // reuse undirected pair if present
        for (i, e) in self.edges.iter().enumerate().take(self.n_e) {
            if e.live && e.tail == tail && e.head == head {
                return Ok(i);
            }
            if e.live && e.tail == head && e.head == tail {
                return Ok(i); // existing opposite orientation
            }
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

    /// Find edge index and sign so that oriented a→b = sign * stored_edge.
    fn oriented_edge(&self, a: u8, b: u8) -> Option<(u16, i8)> {
        for (i, e) in self.edges.iter().enumerate().take(self.n_e) {
            if !e.live {
                continue;
            }
            if e.tail == a && e.head == b {
                return Some((i as u16, 1));
            }
            if e.tail == b && e.head == a {
                return Some((i as u16, -1));
            }
        }
        None
    }

    /// Add 2-simplex on vertices (i,j,k). Ensures all three edges exist.
    pub fn add_face(&mut self, i: u8, j: u8, k: u8, weight: u16) -> Result<usize, HodgeFault> {
        if i == j || j == k || i == k {
            return Err(HodgeFault::BadFace);
        }
        if i as usize >= self.n_v || j as usize >= self.n_v || k as usize >= self.n_v {
            return Err(HodgeFault::BadFace);
        }
        // ensure edges
        let _ = self.add_edge(i, j, 1)?;
        let _ = self.add_edge(j, k, 1)?;
        let _ = self.add_edge(i, k, 1)?;
        let (e_ij, s_ij) = self.oriented_edge(i, j).ok_or(HodgeFault::BadFace)?;
        let (e_jk, s_jk) = self.oriented_edge(j, k).ok_or(HodgeFault::BadFace)?;
        let (e_ik, s_ik) = self.oriented_edge(i, k).ok_or(HodgeFault::BadFace)?;
        if self.n_f >= MAX_F {
            return Err(HodgeFault::Dim);
        }
        let id = self.n_f;
        self.faces[id] = Face {
            live: true,
            v: [i, j, k],
            e_ij,
            e_jk,
            e_ik,
            s_ij,
            s_jk,
            s_ik,
            weight: weight.max(1),
        };
        self.n_f += 1;
        Ok(id)
    }

    /// Auto-fill faces: every triangle in the undirected graph becomes a 2-simplex.
    pub fn fill_clique_triangles(&mut self) -> usize {
        let mut added = 0usize;
        let n = self.n_v;
        for i in 0..n {
            for j in (i + 1)..n {
                for k in (j + 1)..n {
                    let ij = self.oriented_edge(i as u8, j as u8).is_some();
                    let jk = self.oriented_edge(j as u8, k as u8).is_some();
                    let ik = self.oriented_edge(i as u8, k as u8).is_some();
                    if ij && jk && ik {
                        if self.add_face(i as u8, j as u8, k as u8, 1).is_ok() {
                            added += 1;
                        }
                    }
                }
            }
        }
        added
    }

    // ----- coboundaries -----

    pub fn coboundary0(&self, out: &mut [Fp; MAX_E]) {
        *out = [0; MAX_E];
        for (idx, e) in self.edges.iter().enumerate().take(self.n_e) {
            if !e.live {
                continue;
            }
            out[idx] = self.f0[e.head as usize].saturating_sub(self.f0[e.tail as usize]);
        }
    }

    /// (δ₁ α)_σ = s_ij α_ij + s_jk α_jk - s_ik α_ik
    pub fn coboundary1(&self, alpha: &[Fp; MAX_E], out: &mut [Fp; MAX_F]) {
        *out = [0; MAX_F];
        for (fi, f) in self.faces.iter().enumerate().take(self.n_f) {
            if !f.live {
                continue;
            }
            let a_ij = alpha[f.e_ij as usize].saturating_mul(f.s_ij as Fp);
            let a_jk = alpha[f.e_jk as usize].saturating_mul(f.s_jk as Fp);
            let a_ik = alpha[f.e_ik as usize].saturating_mul(f.s_ik as Fp);
            out[fi] = a_ij.saturating_add(a_jk).saturating_sub(a_ik);
        }
    }

    pub fn adjoint_coboundary0(&self, alpha: &[Fp; MAX_E], out: &mut [Fp; MAX_V]) {
        *out = [0; MAX_V];
        for (i, e) in self.edges.iter().enumerate().take(self.n_e) {
            if !e.live {
                continue;
            }
            let wa = alpha[i].saturating_mul(e.weight as Fp);
            out[e.head as usize] = out[e.head as usize].saturating_add(wa);
            out[e.tail as usize] = out[e.tail as usize].saturating_sub(wa);
        }
    }

    /// δ₁*: C²→C¹. For each face contribution to its three edges.
    pub fn adjoint_coboundary1(&self, beta: &[Fp; MAX_F], out: &mut [Fp; MAX_E]) {
        *out = [0; MAX_E];
        for (fi, f) in self.faces.iter().enumerate().take(self.n_f) {
            if !f.live {
                continue;
            }
            let wb = beta[fi].saturating_mul(f.weight as Fp);
            // ⟨δ₁ α, β⟩ = ⟨α, δ₁* β⟩
            // δ₁ α = s_ij α_ij + s_jk α_jk - s_ik α_ik
            // so (δ₁* β)_ij += s_ij w β, etc; (δ₁* β)_ik += -s_ik w β
            let eij = f.e_ij as usize;
            let ejk = f.e_jk as usize;
            let eik = f.e_ik as usize;
            out[eij] = out[eij].saturating_add(wb.saturating_mul(f.s_ij as Fp));
            out[ejk] = out[ejk].saturating_add(wb.saturating_mul(f.s_jk as Fp));
            out[eik] = out[eik].saturating_sub(wb.saturating_mul(f.s_ik as Fp));
        }
    }

    pub fn laplace0(&self, out: &mut [Fp; MAX_V]) {
        let mut alpha = [0; MAX_E];
        self.coboundary0(&mut alpha);
        self.adjoint_coboundary0(&alpha, out);
    }

    /// Δ₁ α = δ₀ δ₀* α + δ₁* δ₁ α
    pub fn laplace1(&self, alpha: &[Fp; MAX_E], out: &mut [Fp; MAX_E]) {
        // term A: δ₀ δ₀* α
        let mut tmp0 = [0; MAX_V];
        self.adjoint_coboundary0(alpha, &mut tmp0);
        let mut term_a = [0; MAX_E];
        for (i, e) in self.edges.iter().enumerate().take(self.n_e) {
            if !e.live {
                continue;
            }
            term_a[i] = tmp0[e.head as usize].saturating_sub(tmp0[e.tail as usize]);
        }
        // term B: δ₁* δ₁ α
        let mut beta = [0; MAX_F];
        self.coboundary1(alpha, &mut beta);
        let mut term_b = [0; MAX_E];
        self.adjoint_coboundary1(&beta, &mut term_b);

        *out = [0; MAX_E];
        for i in 0..self.n_e {
            out[i] = term_a[i].saturating_add(term_b[i]);
        }
    }

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

    pub fn nonharmonic_energy0(&self) -> u64 {
        let mut lf = [0; MAX_V];
        self.laplace0(&mut lf);
        let mut s = 0i64;
        for v in 0..self.n_v {
            s += lf[v] as i64 * lf[v] as i64;
        }
        (s >> 16) as u64
    }

    pub fn nonharmonic_energy1(&self) -> u64 {
        let mut l1 = [0; MAX_E];
        self.laplace1(&self.f1, &mut l1);
        let mut s = 0i64;
        for i in 0..self.n_e {
            s += l1[i] as i64 * l1[i] as i64;
        }
        (s >> 16) as u64
    }

    pub fn store_gradient_flux(&mut self) {
        let mut a = [0; MAX_E];
        self.coboundary0(&mut a);
        self.f1 = a;
    }

    pub fn migration_delta(&self, out: &mut [Fp; MAX_V]) {
        *out = [0; MAX_V];
        let mut alpha = [0; MAX_E];
        self.coboundary0(&mut alpha);
        for (i, e) in self.edges.iter().enumerate().take(self.n_e) {
            if !e.live {
                continue;
            }
            let mv = fp_mul(alpha[i], e.weight as Fp) / 8;
            out[e.head as usize] = out[e.head as usize].saturating_sub(mv);
            out[e.tail as usize] = out[e.tail as usize].saturating_add(mv);
        }
    }
}

#[inline]
fn fp_mul(a: Fp, b: Fp) -> Fp {
    ((a as i64 * b as i64) >> 16) as Fp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_harmonic0() {
        let mut h = HodgeNerve::new(3);
        h.add_edge(0, 1, 1).unwrap();
        h.add_edge(1, 2, 1).unwrap();
        h.add_edge(0, 2, 1).unwrap();
        h.add_face(0, 1, 2, 1).unwrap();
        for v in 0..3 {
            h.set_load(v, FP_ONE).unwrap();
        }
        assert_eq!(h.nonharmonic_energy0(), 0);
    }

    #[test]
    fn heat_reduces_energy() {
        let mut h = HodgeNerve::new(3);
        h.add_edge(0, 1, 1).unwrap();
        h.add_edge(1, 2, 1).unwrap();
        h.add_face(0, 1, 2, 1).ok();
        h.set_load(0, 4 * FP_ONE).unwrap();
        let e0 = h.nonharmonic_energy0();
        h.heat_flow0(FP_ONE / 8, 32);
        assert!(h.nonharmonic_energy0() < e0);
    }

    #[test]
    fn delta1_on_gradient_is_zero() {
        // δ₁ δ₀ = 0 (complex property)
        let mut h = HodgeNerve::new(3);
        h.add_edge(0, 1, 1).unwrap();
        h.add_edge(1, 2, 1).unwrap();
        h.add_edge(0, 2, 1).unwrap();
        h.add_face(0, 1, 2, 1).unwrap();
        h.set_load(0, 0).unwrap();
        h.set_load(1, FP_ONE).unwrap();
        h.set_load(2, 2 * FP_ONE).unwrap();
        let mut alpha = [0; MAX_E];
        h.coboundary0(&mut alpha);
        let mut beta = [0; MAX_F];
        h.coboundary1(&alpha, &mut beta);
        assert_eq!(beta[0], 0);
    }

    #[test]
    fn fill_clique() {
        let mut h = HodgeNerve::new(4);
        for i in 0..4 {
            for j in (i + 1)..4 {
                h.add_edge(i as u8, j as u8, 1).unwrap();
            }
        }
        let n = h.fill_clique_triangles();
        assert_eq!(n, 4); // C(4,3)=4
    }
}
