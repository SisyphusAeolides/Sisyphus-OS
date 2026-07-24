// kernel/boulder/src/fiedler_cut.rs
//! Fiedler cut — spectral bipartition via the normalized Laplacian
//!
//! Graph G=(V,E) on processes / NUMA nodes / IRQ affinity.
//! Adjacency A symmetric, degree D.
//! Normalized L_sym = I - D^{-1/2} A D^{-1/2}
//! Fiedler vector = eigenvector for λ_2 (algebraic connectivity).
//! Sign(fiedler[i]) assigns vertex i to part L/R.
//!
//! We compute λ_2 by:
//!   1) power iteration → approx λ_max eigenvector (or skip if regular)
//!   2) deflation / inverse-free subspace iteration on random start
//!      orthogonalized against the constant (D^{1/2} 1) mode
//! Fixed-point 16.16 throughout; n ≤ 32.


pub const N_MAX: usize = 32;
pub type Fp = i32;
pub const FP_ONE: Fp = 0x1_0000;
pub const FP_HALF: Fp = FP_ONE / 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpecFault {
    TooSmall,
    Dim,
    ZeroDegree,
    NoConvergence,
}

#[derive(Clone, Copy, Debug)]
pub struct FiedlerReport {
    pub n: u8,
    /// algebraic connectivity proxy (Rayleigh quotient), 16.16
    pub lambda2_fp: Fp,
    pub part: [u8; N_MAX], // 0 or 1
    pub cut_weight: u32,
    pub left: u8,
    pub right: u8,
}

pub struct Graph32 {
    pub n: usize,
    /// upper triangle packed unused; full matrix for speed
    pub w: [[u16; N_MAX]; N_MAX],
}

impl Graph32 {
    pub const fn empty(n: usize) -> Self {
        Self {
            n: if n > N_MAX { N_MAX } else { n },
            w: [[0; N_MAX]; N_MAX],
        }
    }

    pub fn add_undirected(&mut self, i: usize, j: usize, weight: u16) {
        if i >= self.n || j >= self.n || i == j {
            return;
        }
        self.w[i][j] = self.w[i][j].saturating_add(weight);
        self.w[j][i] = self.w[j][i].saturating_add(weight);
    }

    fn degree(&self, i: usize) -> u32 {
        let mut d = 0u32;
        for j in 0..self.n {
            d = d.saturating_add(self.w[i][j] as u32);
        }
        d
    }
}

#[inline]
fn fp_mul(a: Fp, b: Fp) -> Fp {
    ((a as i64 * b as i64) >> 16) as Fp
}

#[inline]
fn i_sqrt(mut x: u32) -> u32 {
    if x == 0 {
        return 0;
    }
    let mut r = 0u32;
    let mut bit = 1u32 << 30;
    while bit > x {
        bit >>= 2;
    }
    while bit != 0 {
        if x >= r + bit {
            x -= r + bit;
            r = (r >> 1) + bit;
        } else {
            r >>= 1;
        }
        bit >>= 2;
    }
    r
}


fn inv_sqrt_fp_fixed(d: u32) -> Fp {
    if d == 0 {
        return 0;
    }
    i_sqrt64((1u64 << 48) / d as u64) as Fp
}

fn dot(a: &[Fp], b: &[Fp], n: usize) -> i64 {
    let mut s = 0i64;
    for i in 0..n {
        s += a[i] as i64 * b[i] as i64;
    }
    s >> 16
}

fn i_sqrt64(mut x: u64) -> u64 {
    if x == 0 {
        return 0;
    }
    let mut r = 0u64;
    let mut bit = 1u64 << 62;
    while bit > x {
        bit >>= 2;
    }
    while bit != 0 {
        if x >= r + bit {
            x -= r + bit;
            r = (r >> 1) + bit;
        } else {
            r >>= 1;
        }
        bit >>= 2;
    }
    r
}

fn norm2(a: &[Fp], n: usize) -> Fp {
    let mut s = 0i64;
    for i in 0..n {
        s = s.saturating_add((a[i] as i64).saturating_mul(a[i] as i64));
    }
    if s <= 0 {
        return 0;
    }
    i_sqrt64(s as u64) as Fp
}

fn normalize(v: &mut [Fp], n: usize) {
    let nr = norm2(v, n);
    if nr == 0 {
        return;
    }
    for i in 0..n {
        v[i] = ((v[i] as i64 * FP_ONE as i64) / nr as i64) as Fp;
    }
}

fn axpy(y: &mut [Fp], a: Fp, x: &[Fp], n: usize) {
    for i in 0..n {
        y[i] = y[i].saturating_add(fp_mul(a, x[i]));
    }
}

/// y = L_sym x, with L_sym = I - D^{-1/2} A D^{-1/2}
fn apply_lsym(g: &Graph32, x: &[Fp], y: &mut [Fp], inv_sqrt_d: &[Fp]) {
    let n = g.n;
    // t = D^{-1/2} x
    let mut t = [0; N_MAX];
    for i in 0..n {
        t[i] = fp_mul(inv_sqrt_d[i], x[i]);
    }
    // u = A t
    let mut u = [0; N_MAX];
    for i in 0..n {
        let mut s = 0i64;
        for j in 0..n {
            if g.w[i][j] != 0 {
                s += g.w[i][j] as i64 * t[j] as i64;
            }
        }
        u[i] = (s >> 0) as Fp; // weights small integers; scale as fp later
        // treat weight as integer * FP_ONE factor: w * t already t in fp
        u[i] = (s) as Fp; // if t is 16.16 and w is int, s is sum w*t
    }
    // For w integer and t 16.16, s = Σ w_ij t_j is 16.16 units when w counts.
    // v = D^{-1/2} u
    for i in 0..n {
        let v = fp_mul(inv_sqrt_d[i], u[i]);
        // y = x - v
        y[i] = x[i].saturating_sub(v);
    }
}

/// Orthogonalize v against D^{1/2} 1 (nullspace of L for connected graphs).
fn orth_against_sqrt_d(v: &mut [Fp], sqrt_d: &[Fp], n: usize) {
    let mut num = 0i64;
    let mut den = 0i64;
    for i in 0..n {
        num += v[i] as i64 * sqrt_d[i] as i64;
        den += sqrt_d[i] as i64 * sqrt_d[i] as i64;
    }
    if den == 0 {
        return;
    }
    let alpha = (num / (den >> 16).max(1)) as Fp; // rough 16.16
    for i in 0..n {
        v[i] = v[i].saturating_sub(fp_mul(alpha, sqrt_d[i]));
    }
}

pub fn fiedler_bipartition(g: &Graph32, iters: usize) -> Result<FiedlerReport, SpecFault> {
    let n = g.n;
    if n < 2 {
        return Err(SpecFault::TooSmall);
    }
    let mut inv_sqrt_d = [0; N_MAX];
    let mut sqrt_d = [0; N_MAX];
    for i in 0..n {
        let d = g.degree(i);
        if d == 0 {
            return Err(SpecFault::ZeroDegree);
        }
        inv_sqrt_d[i] = inv_sqrt_fp_fixed(d);
        // use 16.16 sqrt: sqrt(d)*256 approx via i_sqrt(d<<16)
        sqrt_d[i] = i_sqrt(d << 16) as Fp;
    }

    // RNG-free start: alternating + hash of degrees
    let mut v = [0; N_MAX];
    for i in 0..n {
        let sign = if i & 1 == 0 { FP_ONE } else { -FP_ONE };
        v[i] = sign.saturating_add((g.degree(i) as Fp) * 16);
    }
    orth_against_sqrt_d(&mut v, &sqrt_d, n);
    normalize(&mut v, n);

    let mut y = [0; N_MAX];
    for _ in 0..iters.max(8) {
        apply_lsym(g, &v, &mut y, &inv_sqrt_d);
        // We want smallest nontrivial eigenpair. Power on L gives largest.
        // Use inverse iteration proxy: gradient descent on Rayleigh =
        // shift-free: v <- v - α L v + project (heat-flow toward Fiedler for small α)
        // Spectral shift: iterate (λ_max I - L) roughly via
        //   v <- Mv where M = c I - L_sym, c≈2
        for i in 0..n {
            // M v = 2v - L v = 2v - y
            v[i] = (2i32 * v[i]).saturating_sub(y[i]);
        }
        orth_against_sqrt_d(&mut v, &sqrt_d, n);
        normalize(&mut v, n);
    }

    // Rayleigh r = v^T L v
    apply_lsym(g, &v, &mut y, &inv_sqrt_d);
    let lambda2 = dot(&v, &y, n) as Fp;

    let mut part = [0u8; N_MAX];
    let mut left = 0u8;
    let mut right = 0u8;
    for i in 0..n {
        if v[i] >= 0 {
            part[i] = 0;
            left = left.saturating_add(1);
        } else {
            part[i] = 1;
            right = right.saturating_add(1);
        }
    }
    // avoid empty part
    if left == 0 || right == 0 {
        for i in 0..n {
            part[i] = if i < n / 2 { 0 } else { 1 };
        }
        left = (n / 2) as u8;
        right = (n - n / 2) as u8;
    }

    let mut cut = 0u32;
    for i in 0..n {
        for j in (i + 1)..n {
            if part[i] != part[j] {
                cut = cut.saturating_add(g.w[i][j] as u32);
            }
        }
    }

    Ok(FiedlerReport {
        n: n as u8,
        lambda2_fp: lambda2,
        part,
        cut_weight: cut,
        left,
        right,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_cliques_bridge() {
        let mut g = Graph32::empty(6);
        // clique 0-1-2
        g.add_undirected(0, 1, 10);
        g.add_undirected(1, 2, 10);
        g.add_undirected(0, 2, 10);
        // clique 3-4-5
        g.add_undirected(3, 4, 10);
        g.add_undirected(4, 5, 10);
        g.add_undirected(3, 5, 10);
        // bridge
        g.add_undirected(2, 3, 1);
        let r = fiedler_bipartition(&g, 64).unwrap();
        // bridge endpoints should be different parts ideally
        assert_eq!(r.left + r.right, 6);
        assert!(r.cut_weight <= 12);
    }
}
