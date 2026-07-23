//! PENROSE OR ENGINE
//!
//! Orchestrated objective reduction for scheduler degeneracy.
//!
//! When eigenthread reports two (or more) eigen-schedules with
//! |E_i - E_j| < OR_GAP_THRESHOLD, classical choice is ill-defined.
//!
//! Penrose OR: collapse time τ ≈ ħ / E_G  (gravitational self-energy).
//! We proxy E_G by:
//!   - working-set mass difference (pages * touch heat)
//!   - IRQ phonon temperature asymmetry (phononic crystal)
//!   - Chronovore entropy draw (true jitter)
//!
//! Collapse emits a single schedule index + a witness certificate
//! for the axiom manifold / ghost chronicle.

#![allow(dead_code)]

pub const MAX_BRANCHES: usize = 8;
/// 16.16
pub type Fp = u64;
pub const FP_ONE: Fp = 0x1_0000;
/// Minimum energy gap that still counts as degenerate
pub const OR_GAP_THRESHOLD_FP: Fp = FP_ONE / 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScheduleBranch {
    pub live: bool,
    /// Eigenthread eigenvalue proxy (lower = better ground)
    pub energy_fp: Fp,
    /// Total working-set pages in this co-schedule
    pub mass_pages: u32,
    /// Aggregate heat
    pub heat_fp: Fp,
    /// Phonon temperature of preferred cores
    pub phonon_temp_fp: Fp,
    /// Opaque schedule handle (bitset / id from eigenthread)
    pub handle: u64,
}

impl ScheduleBranch {
    pub const EMPTY: Self = Self {
        live: false,
        energy_fp: 0,
        mass_pages: 0,
        heat_fp: 0,
        phonon_temp_fp: 0,
        handle: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OrCertificate {
    pub winner: u8,
    pub branches: u8,
    pub gap_fp: Fp,
    pub e_g_fp: Fp,
    pub tau_ticks: u64,
    pub entropy_draw: u64,
    pub winner_handle: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OrFault {
    NotDegenerate,
    NoBranches,
    EntropyStarved,
}

pub struct PenroseOr {
    branches: [ScheduleBranch; MAX_BRANCHES],
    length: usize,
    /// ħ proxy in 16.16 tick-energy units
    hbar_fp: Fp,
}

impl PenroseOr {
    pub const fn new() -> Self {
        Self {
            branches: [ScheduleBranch::EMPTY; MAX_BRANCHES],
            length: 0,
            hbar_fp: FP_ONE, // calibrated at boot from Chronovore mean jitter
        }
    }

    pub fn clear(&mut self) {
        self.length = 0;
        self.branches = [ScheduleBranch::EMPTY; MAX_BRANCHES];
    }

    pub fn push(&mut self, branch: ScheduleBranch) -> bool {
        if self.length >= MAX_BRANCHES || !branch.live {
            return false;
        }
        self.branches[self.length] = branch;
        self.length += 1;
        true
    }

    pub fn calibrate_hbar(&mut self, mean_jitter_tsc: u64) {
        // Map jitter into ħ-ish scale; clamp
        let j = mean_jitter_tsc.min(1_000_000).max(1);
        self.hbar_fp = (j as Fp).min(FP_ONE.saturating_mul(16)).max(1);
    }

    /// True if top-2 energies are within OR gap.
    pub fn is_degenerate(&self) -> bool {
        if self.length < 2 {
            return false;
        }
        let mut best = Fp::MAX;
        let mut second = Fp::MAX;
        for b in self.branches.iter().take(self.length) {
            if !b.live {
                continue;
            }
            if b.energy_fp < best {
                second = best;
                best = b.energy_fp;
            } else if b.energy_fp < second {
                second = b.energy_fp;
            }
        }
        second.saturating_sub(best) < OR_GAP_THRESHOLD_FP
    }

    /// Gravitational self-energy proxy between two branches.
    fn e_g(a: &ScheduleBranch, b: &ScheduleBranch) -> Fp {
        let dm = (a.mass_pages as i64 - b.mass_pages as i64).unsigned_abs() as Fp;
        let dh = if a.heat_fp > b.heat_fp {
            a.heat_fp - b.heat_fp
        } else {
            b.heat_fp - a.heat_fp
        };
        let dt = if a.phonon_temp_fp > b.phonon_temp_fp {
            a.phonon_temp_fp - b.phonon_temp_fp
        } else {
            b.phonon_temp_fp - a.phonon_temp_fp
        };
        // E_G ∝ Δm * (Δheat + Δphonon)
        let mix = dh.saturating_add(dt).max(1);
        dm.saturating_mul(mix).max(1)
    }

    /// Collapse. `entropy` from Chronovore::gen_u64().
    pub fn collapse(&self, entropy: u64) -> Result<OrCertificate, OrFault> {
        if self.length == 0 {
            return Err(OrFault::NoBranches);
        }
        if self.length == 1 {
            let w = self.branches[0];
            return Ok(OrCertificate {
                winner: 0,
                branches: 1,
                gap_fp: 0,
                e_g_fp: 0,
                tau_ticks: 0,
                entropy_draw: entropy,
                winner_handle: w.handle,
            });
        }
        if !self.is_degenerate() {
            // Classical: pick minimum energy
            let mut best_i = 0usize;
            let mut best_e = Fp::MAX;
            for (i, b) in self.branches.iter().enumerate().take(self.length) {
                if b.live && b.energy_fp < best_e {
                    best_e = b.energy_fp;
                    best_i = i;
                }
            }
            return Ok(OrCertificate {
                winner: best_i as u8,
                branches: self.length as u8,
                gap_fp: OR_GAP_THRESHOLD_FP,
                e_g_fp: 0,
                tau_ticks: 0,
                entropy_draw: entropy,
                winner_handle: self.branches[best_i].handle,
            });
        }

        // Degenerate OR path: weight each branch by 1/E_G vs ground,
        // then use entropy to sample.
        let mut ground_i = 0usize;
        let mut ground_e = Fp::MAX;
        for (i, b) in self.branches.iter().enumerate().take(self.length) {
            if b.live && b.energy_fp < ground_e {
                ground_e = b.energy_fp;
                ground_i = i;
            }
        }
        let ground = self.branches[ground_i];

        let mut weights = [0u64; MAX_BRANCHES];
        let mut total = 0u64;
        for (i, b) in self.branches.iter().enumerate().take(self.length) {
            if !b.live {
                continue;
            }
            let eg = Self::e_g(&ground, b).max(1);
            // Prefer lower E_G difference → larger weight; also prefer lower energy
            let w = (FP_ONE.saturating_mul(1024) / eg)
                .saturating_add(FP_ONE.saturating_mul(64) / b.energy_fp.max(1));
            weights[i] = w;
            total = total.saturating_add(w);
        }
        if total == 0 {
            return Err(OrFault::EntropyStarved);
        }

        let draw = entropy % total;
        let mut acc = 0u64;
        let mut winner = ground_i;
        for (i, w) in weights.iter().enumerate().take(self.length) {
            acc = acc.saturating_add(*w);
            if draw < acc {
                winner = i;
                break;
            }
        }

        let mut second_e = Fp::MAX;
        for (i, b) in self.branches.iter().enumerate().take(self.length) {
            if i != ground_i && b.live && b.energy_fp < second_e {
                second_e = b.energy_fp;
            }
        }
        let gap = second_e.saturating_sub(ground_e);
        let eg = Self::e_g(&ground, &self.branches[winner]);
        let tau = (self.hbar_fp.saturating_mul(FP_ONE)) / eg.max(1);

        Ok(OrCertificate {
            winner: winner as u8,
            branches: self.length as u8,
            gap_fp: gap,
            e_g_fp: eg,
            tau_ticks: tau,
            entropy_draw: entropy,
            winner_handle: self.branches[winner].handle,
        })
    }
}
