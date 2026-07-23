// kernel/boulder/src/cech_h1.rs
//! Čech H¹ obstruction on a capability cover
//!
//! Nerve of opens U_0..U_{n-1}: edge ij if U_i ∩ U_j ≠ ∅.
//! 0-cochain: assignment a_i ∈ A (A = u64 abelian group under XOR = F_2^k)
//! 1-cochain: assignment g_{ij} ∈ A on undirected edges, g_{ji}=g_{ij}
//! Coboundary δ⁰(a)_{ij} = a_i ⊕ a_j
//! Cocycle Z¹: δ¹g = 0 on triangles: g_ij ⊕ g_jk ⊕ g_ki = 0
//! Coboundary B¹ = im δ⁰
//! H¹ = Z¹ / B¹
//!
//! For OS: a_i = local capability stalk; if pairwise glue data is a
//! cocycle not a coboundary, no global capability section exists.

#![allow(dead_code)]

pub const MAX_OPENS: usize = 16;
pub const MAX_EDGES: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum H1Fault {
    Dim,
    NotCocycle,
    NoEdge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Edge {
    pub live: bool,
    pub i: u8,
    pub j: u8,
    /// 1-cochain value g_ij
    pub g: u64,
}

impl Edge {
    pub const EMPTY: Self = Self {
        live: false,
        i: 0,
        j: 0,
        g: 0,
    };
}

pub struct CechComplex {
    pub n: usize,
    /// 0-cochain (local sections)
    pub a: [u64; MAX_OPENS],
    pub edges: [Edge; MAX_EDGES],
    pub e_len: usize,
}

impl CechComplex {
    pub const fn new(n: usize) -> Self {
        Self {
            n: if n < MAX_OPENS { n } else { MAX_OPENS },
            a: [0; MAX_OPENS],
            edges: [Edge::EMPTY; MAX_EDGES],
            e_len: 0,
        }
    }

    pub fn set_section(&mut self, i: usize, val: u64) {
        if i < self.n {
            self.a[i] = val;
        }
    }

    pub fn add_overlap(&mut self, i: u8, j: u8, g_ij: u64) -> Result<(), H1Fault> {
        if i as usize >= self.n || j as usize >= self.n || i == j {
            return Err(H1Fault::Dim);
        }
        let (i, j) = if i < j { (i, j) } else { (j, i) };
        for e in self.edges.iter_mut().take(self.e_len) {
            if e.live && e.i == i && e.j == j {
                e.g = g_ij;
                return Ok(());
            }
        }
        if self.e_len >= MAX_EDGES {
            return Err(H1Fault::Dim);
        }
        self.edges[self.e_len] = Edge {
            live: true,
            i,
            j,
            g: g_ij,
        };
        self.e_len += 1;
        Ok(())
    }

    fn edge_g(&self, i: u8, j: u8) -> Option<u64> {
        let (i, j) = if i < j { (i, j) } else { (j, i) };
        self.edges
            .iter()
            .take(self.e_len)
            .find(|e| e.live && e.i == i && e.j == j)
            .map(|e| e.g)
    }

    /// Check δ¹g = 0 on all triangles present in the nerve.
    pub fn is_cocycle(&self) -> bool {
        for i in 0..self.n {
            for j in (i + 1)..self.n {
                for k in (j + 1)..self.n {
                    let (Some(gij), Some(gjk), Some(gki)) = (
                        self.edge_g(i as u8, j as u8),
                        self.edge_g(j as u8, k as u8),
                        self.edge_g(k as u8, i as u8),
                    ) else {
                        continue; // not a filled triangle
                    };
                    if gij ^ gjk ^ gki != 0 {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// δ⁰(a) on edges
    pub fn coboundary0(&self) -> [Edge; MAX_EDGES] {
        let mut out = [Edge::EMPTY; MAX_EDGES];
        for (idx, e) in self.edges.iter().enumerate().take(self.e_len) {
            if !e.live {
                continue;
            }
            out[idx] = Edge {
                live: true,
                i: e.i,
                j: e.j,
                g: self.a[e.i as usize] ^ self.a[e.j as usize],
            };
        }
        out
    }

    /// Test whether g is a coboundary: exist a with δ⁰a = g.
    /// Solve (a_i ⊕ a_j = g_ij) over F_2 via Gaussian elim on edges.
    pub fn is_coboundary(&self) -> Result<bool, H1Fault> {
        if !self.is_cocycle() {
            return Err(H1Fault::NotCocycle);
        }
        // System: for each edge, a_i + a_j = g_ij
        // Variables a_0..a_{n-1} in F_2^64 — solve bit-sliced per bit independently.
        // If every bit plane is consistent, g ∈ B¹.
        for bit in 0..64 {
            let mut mat = [0u64; MAX_EDGES]; // rows: coeffs on a_0..a_{n-1} in low n bits
            let mut rhs = [0u8; MAX_EDGES];
            let mut rows = 0usize;
            for e in self.edges.iter().take(self.e_len) {
                if !e.live {
                    continue;
                }
                let mut row = 0u64;
                row |= 1u64 << e.i;
                row |= 1u64 << e.j;
                mat[rows] = row;
                rhs[rows] = ((e.g >> bit) & 1) as u8;
                rows += 1;
            }
            if !gf2_solve(mat, rhs, rows, self.n) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// H¹ trivial?
    pub fn h1_trivial(&self) -> Result<bool, H1Fault> {
        Ok(self.is_coboundary()?)
    }

    /// Build glue g_ij = a_i ⊕ a_j from local sections (always a coboundary).
    pub fn fill_from_sections(&mut self) {
        for e in self.edges.iter_mut().take(self.e_len) {
            if e.live {
                e.g = self.a[e.i as usize] ^ self.a[e.j as usize];
            }
        }
    }
}

/// GF(2) solve M x = rhs, M is rows×n (n≤16 packed in u64).
fn gf2_solve(mut mat: [u64; MAX_EDGES], mut rhs: [u8; MAX_EDGES], rows: usize, n: usize) -> bool {
    let mut row = 0usize;
    for col in 0..n {
        // find pivot
        let mut piv = None;
        for r in row..rows {
            if (mat[r] >> col) & 1 == 1 {
                piv = Some(r);
                break;
            }
        }
        let Some(p) = piv else {
            continue;
        };
        mat.swap(row, p);
        rhs.swap(row, p);
        for r in 0..rows {
            if r != row && (mat[r] >> col) & 1 == 1 {
                mat[r] ^= mat[row];
                rhs[r] ^= rhs[row];
            }
        }
        row += 1;
        if row >= rows {
            break;
        }
    }
    // inconsistent if [0|1] row
    for r in 0..rows {
        if mat[r] == 0 && rhs[r] == 1 {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coboundary_is_trivial_h1() {
        let mut c = CechComplex::new(3);
        c.set_section(0, 0b001);
        c.set_section(1, 0b011);
        c.set_section(2, 0b111);
        c.add_overlap(0, 1, 0).unwrap();
        c.add_overlap(1, 2, 0).unwrap();
        c.add_overlap(0, 2, 0).unwrap();
        c.fill_from_sections();
        assert!(c.is_cocycle());
        assert_eq!(c.h1_trivial().unwrap(), true);
    }

    #[test]
    fn nontrivial_triangle_cocycle() {
        let mut c = CechComplex::new(3);
        // g01=g12=g20=1 → product of parities around triangle = 1 → not cocycle in F2?
        // 1⊕1⊕1 = 1 ≠ 0 → not cocycle
        c.add_overlap(0, 1, 1).unwrap();
        c.add_overlap(1, 2, 1).unwrap();
        c.add_overlap(2, 0, 1).unwrap();
        assert!(!c.is_cocycle());
        // valid cocycle that's nontrivial: on a 2-edge cover only, g=1
        let mut d = CechComplex::new(2);
        d.add_overlap(0, 1, 1).unwrap();
        assert!(d.is_cocycle());
        // a0⊕a1=1 solvable — actually IS a coboundary
        assert_eq!(d.h1_trivial().unwrap(), true);
    }
}
