// kernel/boulder/src/stabilizer_tableau.rs
//! Stabilizer tableau — Gottesman–Knill linear constraints over GF(2)
//!
//! State space: n-bit vector x ∈ F_2^n  (capability / entitlement bits).
//! Stabilizer generators: rows (X-block | Z-block | phase) as in Aaronson–Gottesman.
//! For OS use we specialize to *Z-type constraints* (parity checks on caps):
//!   each generator is a row h ∈ F_2^n with constraint ⟨h, x⟩ = p
//! Full binary tableau supports CNOT/H/S on the *constraint system*
//! (pushforward of linear policies under reversible rewrites).
//!
//! Mutation Δ allowed iff for every generator h: ⟨h, Δ⟩ = 0 when phase locked,
//! else syndrome bit fires → Noether/policy reject.

pub const N_MAX: usize = 64; // bit width
pub const M_MAX: usize = 64; // number of stabilizers

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StabFault {
    RankDeficient,
    Inconsistent,
    Dim,
    NotFound,
}

/// Packed GF(2) row — one u64 limb (n≤64).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub struct Row64 {
    pub bits: u64,
    /// phase bit: expected parity ⟨h,x⟩ == phase
    pub phase: u8,
}

impl Row64 {
    pub const ZERO: Self = Self { bits: 0, phase: 0 };

    #[inline]
    pub fn dot(self, x: u64) -> u8 {
        (self.bits & x).count_ones() as u8 & 1
    }

    #[inline]
    pub fn xor_with(self, o: Self) -> Self {
        Self {
            bits: self.bits ^ o.bits,
            phase: self.phase ^ o.phase,
        }
    }
}

/// Tableau: m independent parity checks on n bits.
pub struct StabilizerTableau {
    pub n_bits: u8,
    pub m: u8,
    pub rows: [Row64; M_MAX],
}

impl StabilizerTableau {
    pub const fn new(n_bits: u8) -> Self {
        Self {
            n_bits: if n_bits > 64 { 64 } else { n_bits },
            m: 0,
            rows: [Row64::ZERO; M_MAX],
        }
    }

    fn mask(&self) -> u64 {
        if self.n_bits >= 64 {
            u64::MAX
        } else {
            (1u64 << self.n_bits) - 1
        }
    }

    /// Insert constraint ⟨h,x⟩=p; Gaussian-eliminate to keep rows independent.
    pub fn add_constraint(&mut self, mut h: u64, p: u8) -> Result<(), StabFault> {
        h &= self.mask();
        let mut phase = p & 1;
        // Eliminate against existing pivots
        for i in 0..self.m as usize {
            let piv = self.pivot_bit(i);
            if piv == 0xFF {
                continue;
            }
            if (h >> piv) & 1 == 1 {
                h ^= self.rows[i].bits;
                phase ^= self.rows[i].phase;
            }
        }
        if h == 0 {
            return if phase == 0 {
                Ok(()) // redundant consistent
            } else {
                Err(StabFault::Inconsistent)
            };
        }
        if self.m as usize >= M_MAX {
            return Err(StabFault::Dim);
        }
        // Place new row and eliminate upward
        let idx = self.m as usize;
        self.rows[idx] = Row64 { bits: h, phase };
        self.m += 1;
        let piv = h.trailing_zeros() as usize;
        for i in 0..idx {
            if (self.rows[i].bits >> piv) & 1 == 1 {
                self.rows[i] = self.rows[i].xor_with(self.rows[idx]);
            }
        }
        Ok(())
    }

    fn pivot_bit(&self, row: usize) -> u8 {
        let b = self.rows[row].bits;
        if b == 0 {
            0xFF
        } else {
            b.trailing_zeros() as u8
        }
    }

    /// Syndrome of state x: bit i = 1 if generator i violated.
    pub fn syndrome(&self, x: u64) -> u64 {
        let x = x & self.mask();
        let mut s = 0u64;
        for i in 0..self.m as usize {
            let chk = self.rows[i].dot(x) ^ self.rows[i].phase;
            if chk == 1 {
                s |= 1u64 << i;
            }
        }
        s
    }

    pub fn is_stabilized(&self, x: u64) -> bool {
        self.syndrome(x) == 0
    }

    /// Allow delta if x and x⊕Δ both stabilized (linear policy closed under Δ)
    /// or more strictly: Δ in orthogonal complement of rowspace when phases 0.
    pub fn allows_delta(&self, x: u64, delta: u64) -> bool {
        let x = x & self.mask();
        let d = delta & self.mask();
        self.is_stabilized(x) && self.is_stabilized(x ^ d)
    }

    /// CNOT on constraint space: bit c control, t target — columns of all rows.
    /// (h_t, h_c) := (h_t⊕h_c, h_c) on each generator — standard tableau CNOT.
    pub fn cnot(&mut self, control: u8, target: u8) -> Result<(), StabFault> {
        if control >= self.n_bits || target >= self.n_bits || control == target {
            return Err(StabFault::Dim);
        }
        for i in 0..self.m as usize {
            let hc = (self.rows[i].bits >> control) & 1;
            if hc == 1 {
                self.rows[i].bits ^= 1u64 << target;
            }
        }
        Ok(())
    }

    /// Swap bits i,j (permutation of tensor factors).
    pub fn swap_bits(&mut self, a: u8, b: u8) -> Result<(), StabFault> {
        if a >= self.n_bits || b >= self.n_bits {
            return Err(StabFault::Dim);
        }
        if a == b {
            return Ok(());
        }
        for i in 0..self.m as usize {
            let ba = (self.rows[i].bits >> a) & 1;
            let bb = (self.rows[i].bits >> b) & 1;
            if ba != bb {
                self.rows[i].bits ^= (1u64 << a) | (1u64 << b);
            }
        }
        Ok(())
    }

    /// Measure bit k in computational basis: collapse constraint or learn value.
    /// Returns Ok(value) if determined, or adds projector if free.
    pub fn measure_z(&mut self, k: u8, observed: u8) -> Result<u8, StabFault> {
        if k >= self.n_bits {
            return Err(StabFault::Dim);
        }
        // If some row is exactly e_k, phase is value
        for i in 0..self.m as usize {
            if self.rows[i].bits == (1u64 << k) {
                return Ok(self.rows[i].phase);
            }
        }
        // Otherwise add constraint x_k = observed
        self.add_constraint(1u64 << k, observed & 1)?;
        Ok(observed & 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parity_policy() {
        let mut t = StabilizerTableau::new(8);
        // x0 ⊕ x1 ⊕ x2 = 0
        t.add_constraint(0b111, 0).unwrap();
        // x3 = 1
        t.add_constraint(0b1000, 1).unwrap();
        assert!(t.is_stabilized(0b1000)); // 8
        assert!(!t.is_stabilized(0b1001));
        assert!(t.allows_delta(0b1000, 0b011)); // flip 0,1 keeps parity
        assert!(!t.allows_delta(0b1000, 0b001));
    }

    #[test]
    fn inconsistent_reject() {
        let mut t = StabilizerTableau::new(4);
        t.add_constraint(0b1, 0).unwrap();
        assert!(t.add_constraint(0b1, 1).is_err());
    }
}
