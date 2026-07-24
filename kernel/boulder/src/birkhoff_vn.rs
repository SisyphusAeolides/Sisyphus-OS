// kernel/boulder/src/birkhoff_vn.rs
//! Birkhoff–von Neumann decomposition of doubly stochastic matrices
//!
//! M n×n, M≥0, rows & cols sum to 1 (here: fixed-point 16.16).
//! Theorem: M = Σ_k θ_k P_k, θ_k≥0, Σ θ_k = 1, P_k permutation matrices.
//! Algorithm (constructive):
//!   while ||M||_1 > ε:
//!     find positive diagonal (permutation) via bipartite matching on support
//!     θ = min_{p_i→j} M_{ij}
//!     M ← M - θ P
//!     emit (θ, P)
//!
//! OS: M[i][j] = fraction of work from task class i on core j.
//! Decomposition → time slots with exclusive assignments (no fractional cores).


pub const N: usize = 8;
pub type Fp = u32;
pub const FP_ONE: Fp = 0x1_0000;
pub const EPS: Fp = 8; // ~2^-13

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BvnFault {
    NotSquare,
    NotNonneg,
    NotDoublyStochastic,
    NoPermutation,
    Capacity,
}

#[derive(Clone, Copy, Debug)]
pub struct Perm {
    /// perm[i] = column assigned to row i
    pub map: [u8; N],
}

impl Perm {
    pub const fn id() -> Self {
        let mut m = [0u8; N];
        let mut i = 0;
        while i < N {
            m[i] = i as u8;
            i += 1;
        }
        Self { map: m }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct BvnTerm {
    pub theta_fp: Fp,
    pub perm: Perm,
}

impl BvnTerm {
    pub const ZERO: Self = Self {
        theta_fp: 0,
        perm: Perm::id(),
    };
}

pub const MAX_TERMS: usize = N * N + 1;

pub struct BvnDecomposition {
    pub terms: [BvnTerm; MAX_TERMS],
    pub len: usize,
}

impl BvnDecomposition {
    pub const fn empty() -> Self {
        Self {
            terms: [BvnTerm::ZERO; MAX_TERMS],
            len: 0,
        }
    }
}

fn row_sum(m: &[[Fp; N]; N], r: usize, n: usize) -> u64 {
    let mut s = 0u64;
    for c in 0..n {
        s += m[r][c] as u64;
    }
    s
}

fn col_sum(m: &[[Fp; N]; N], c: usize, n: usize) -> u64 {
    let mut s = 0u64;
    for r in 0..n {
        s += m[r][c] as u64;
    }
    s
}

pub fn validate_doubly_stochastic(m: &[[Fp; N]; N], n: usize) -> Result<(), BvnFault> {
    if n == 0 || n > N {
        return Err(BvnFault::NotSquare);
    }
    for i in 0..n {
        for j in 0..n {
            // already unsigned
            let _ = m[i][j];
        }
        let rs = row_sum(m, i, n);
        let cs = col_sum(m, i, n);
        if rs.abs_diff(FP_ONE as u64) > EPS as u64 * 4 {
            return Err(BvnFault::NotDoublyStochastic);
        }
        if cs.abs_diff(FP_ONE as u64) > EPS as u64 * 4 {
            return Err(BvnFault::NotDoublyStochastic);
        }
    }
    Ok(())
}

/// DFS bipartite matching on support { M_ij >= min_pos }
fn find_perm(m: &[[Fp; N]; N], n: usize) -> Result<Perm, BvnFault> {
    let mut match_to = [0xFFu8; N]; // col -> row
    let mut seen = [false; N];

    fn dfs(
        row: usize,
        n: usize,
        m: &[[Fp; N]; N],
        match_to: &mut [u8; N],
        seen: &mut [bool; N],
    ) -> bool {
        for col in 0..n {
            if m[row][col] <= EPS || seen[col] {
                continue;
            }
            seen[col] = true;
            let mj = match_to[col];
            if mj == 0xFF || dfs(mj as usize, n, m, match_to, seen) {
                match_to[col] = row as u8;
                return true;
            }
        }
        false
    }

    for row in 0..n {
        seen = [false; N];
        if !dfs(row, n, m, &mut match_to, &mut seen) {
            return Err(BvnFault::NoPermutation);
        }
    }
    let mut p = Perm::id();
    for col in 0..n {
        let row = match_to[col] as usize;
        p.map[row] = col as u8;
    }
    Ok(p)
}

pub fn decompose(mut m: [[Fp; N]; N], n: usize) -> Result<BvnDecomposition, BvnFault> {
    validate_doubly_stochastic(&m, n)?;
    let mut out = BvnDecomposition::empty();
    let mut guard = 0usize;
    while guard < MAX_TERMS {
        guard += 1;
        // residual mass
        let mut mass = 0u64;
        for i in 0..n {
            for j in 0..n {
                mass += m[i][j] as u64;
            }
        }
        if mass <= EPS as u64 * n as u64 {
            break;
        }
        let perm = find_perm(&m, n)?;
        let mut theta = FP_ONE;
        for i in 0..n {
            let j = perm.map[i] as usize;
            if m[i][j] < theta {
                theta = m[i][j];
            }
        }
        if theta <= EPS {
            return Err(BvnFault::NoPermutation);
        }
        for i in 0..n {
            let j = perm.map[i] as usize;
            m[i][j] = m[i][j].saturating_sub(theta);
        }
        if out.len >= MAX_TERMS {
            return Err(BvnFault::Capacity);
        }
        out.terms[out.len] = BvnTerm {
            theta_fp: theta,
            perm,
        };
        out.len += 1;
    }
    Ok(out)
}

/// Pick permutation for this time slot given phase in [0, FP_ONE).
pub fn select_slot(d: &BvnDecomposition, phase_fp: Fp) -> Perm {
    let mut acc = 0u32;
    let phase = phase_fp % FP_ONE;
    for i in 0..d.len {
        acc = acc.saturating_add(d.terms[i].theta_fp);
        if phase < acc {
            return d.terms[i].perm;
        }
    }
    if d.len > 0 {
        d.terms[d.len - 1].perm
    } else {
        Perm::id()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_one_term() {
        let mut m = [[0u32; N]; N];
        for i in 0..4 {
            m[i][i] = FP_ONE;
        }
        let d = decompose(m, 4).unwrap();
        assert_eq!(d.len, 1);
        assert_eq!(d.terms[0].theta_fp, FP_ONE);
    }

    #[test]
    fn midpoint_two_perms() {
        // 50-50 swap on 2x2
        let mut m = [[0u32; N]; N];
        m[0][0] = FP_ONE / 2;
        m[0][1] = FP_ONE / 2;
        m[1][0] = FP_ONE / 2;
        m[1][1] = FP_ONE / 2;
        let d = decompose(m, 2).unwrap();
        assert!(d.len >= 2);
        let sum: u32 = d.terms.iter().take(d.len).map(|t| t.theta_fp).sum();
        assert!(sum.abs_diff(FP_ONE) < EPS * 4);
    }
}
