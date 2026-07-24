//! PHONONIC IRQ CRYSTAL
//!
//! Model the IDT/IOAPIC vector space as a 1D phonon lattice.
//! Each pending IRQ is a localized vibration mode.
//! Dispersion: ω(k) = ω0 * |sin(π k / N)|  (fixed-point)
//! Group velocity decides migration toward quieter cores.
//! Anharmonic scattering merges near-frequency IRQs (coalesce).
//! Temperature = mean phonon occupation → storm throttle.


pub const LATTICE_SITES: usize = 256; // vector space
pub const MAX_PHONONS: usize = 64;
/// 16.16 fixed point
pub type Fp = u32;
pub const FP_ONE: Fp = 0x1_0000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Phonon {
    pub live: bool,
    pub vector: u8,
    pub core: u8,
    /// Occupation number (how many coalesced events)
    pub occupation: u16,
    /// Crystal momentum k in [0, LATTICE_SITES)
    pub k: u16,
    /// Frequency ω in 16.16 (derived from dispersion)
    pub omega: Fp,
    pub last_tsc: u64,
}

impl Phonon {
    pub const EMPTY: Self = Self {
        live: false,
        vector: 0,
        core: 0,
        occupation: 0,
        k: 0,
        omega: 0,
        last_tsc: 0,
    };
}

pub struct PhononicCrystal {
    sites: [Phonon; MAX_PHONONS],
    /// Per-core temperature (mean occupation), 16.16
    temperature: [Fp; 64],
    /// Storm threshold — above this, drop lowest-priority phonons
    storm_theta: Fp,
    /// Chronovore-supplied next quiet TSC (emission band open)
    quiet_window_tsc: u64,
    length: usize,
}

impl PhononicCrystal {
    pub const fn new() -> Self {
        Self {
            sites: [Phonon::EMPTY; MAX_PHONONS],
            temperature: [0; 64],
            storm_theta: FP_ONE.saturating_mul(8), // occupation ~8
            quiet_window_tsc: 0,
            length: 0,
        }
    }

    /// Dispersion relation ω(k) = ω0 * |sin(π k / N)| approximated in integer.
    /// ω0 fixed at FP_ONE for baseline vector frequency unit.
    pub fn dispersion(k: u16) -> Fp {
        // sin(π k / N) ≈ 2*(k/N) for small, use quarter-wave table-free approx:
        // fold k into [0, N/2], linear ramp then mirror — good enough for ranking
        let n = LATTICE_SITES as u32;
        let kk = (k as u32) % n;
        let half = n / 2;
        let tri = if kk <= half { kk } else { n - kk };
        // map [0, half] → [0, FP_ONE]
        ((tri as u64 * FP_ONE as u64) / half as u64) as Fp
    }

    pub fn set_quiet_window(&mut self, tsc: u64) {
        self.quiet_window_tsc = tsc;
    }

    /// Inject an IRQ event as a phonon at vector `vec` on `core`.
    pub fn excite(&mut self, vector: u8, core: u8, now_tsc: u64) -> bool {
        // Try scatter-merge into existing near-frequency phonon on same core
        let k = vector as u16;
        let omega = Self::dispersion(k);
        for slot in self.sites.iter_mut().take(self.length) {
            if !slot.live || slot.core != core {
                continue;
            }
            let domega = if slot.omega > omega {
                slot.omega - omega
            } else {
                omega - slot.omega
            };
            // Anharmonic scattering window: 5% of FP_ONE
            if domega < FP_ONE / 20 {
                slot.occupation = slot.occupation.saturating_add(1);
                slot.last_tsc = now_tsc;
                self.recompute_temperature(core);
                return true;
            }
        }
        if self.length >= MAX_PHONONS {
            return self.thermal_drop(core, vector, now_tsc, k, omega);
        }
        self.sites[self.length] = Phonon {
            live: true,
            vector,
            core,
            occupation: 1,
            k,
            omega,
            last_tsc: now_tsc,
        };
        self.length += 1;
        self.recompute_temperature(core);
        true
    }

    fn thermal_drop(&mut self, core: u8, vector: u8, now_tsc: u64, k: u16, omega: Fp) -> bool {
        // Drop coldest (lowest occupation) phonon on this core, replace
        let mut victim = None;
        let mut best_occ = u16::MAX;
        for (i, slot) in self.sites.iter().enumerate().take(self.length) {
            if slot.live && slot.core == core && slot.occupation < best_occ {
                best_occ = slot.occupation;
                victim = Some(i);
            }
        }
        if let Some(i) = victim {
            self.sites[i] = Phonon {
                live: true,
                vector,
                core,
                occupation: 1,
                k,
                omega,
                last_tsc: now_tsc,
            };
            self.recompute_temperature(core);
            true
        } else {
            false
        }
    }

    fn recompute_temperature(&mut self, core: u8) {
        if core as usize >= self.temperature.len() {
            return;
        }
        let mut sum = 0u32;
        let mut n = 0u32;
        for slot in self.sites.iter().take(self.length) {
            if slot.live && slot.core == core {
                sum += slot.occupation as u32;
                n += 1;
            }
        }
        self.temperature[core as usize] = if n == 0 {
            0
        } else {
            ((sum as u64 * FP_ONE as u64) / n as u64) as Fp
        };
    }

    /// Storm? temperature above theta on any core.
    pub fn in_thermal_runaway(&self) -> bool {
        self.temperature.iter().any(|&t| t >= self.storm_theta)
    }

    /// Emit next phonon allowed in the Chronovore quiet band.
    /// Returns (vector, core, occupation) to deliver.
    pub fn emit(&mut self, now_tsc: u64) -> Option<(u8, u8, u16)> {
        // If quiet window set and we are early, hold high-omega (noisy) phonons
        let hold_noisy = self.quiet_window_tsc != 0 && now_tsc < self.quiet_window_tsc;

        let mut best: Option<usize> = None;
        let mut best_score: Fp = 0;
        for (i, slot) in self.sites.iter().enumerate().take(self.length) {
            if !slot.live {
                continue;
            }
            if hold_noisy && slot.omega > FP_ONE / 2 {
                continue;
            }
            // Prefer high occupation (coalesced work) and low omega during quiet
            let score = (slot.occupation as u32)
                .saturating_mul(FP_ONE / 16)
                .saturating_add(FP_ONE.saturating_sub(slot.omega) / 4);
            if score >= best_score {
                best_score = score;
                best = Some(i);
            }
        }
        let i = best?;
        let p = self.sites[i];
        self.sites[i].live = false;
        // compact
        if i + 1 < self.length {
            self.sites[i] = self.sites[self.length - 1];
        }
        self.length -= 1;
        self.recompute_temperature(p.core);
        Some((p.vector, p.core, p.occupation))
    }

    pub fn temperature(&self, core: u8) -> Fp {
        self.temperature.get(core as usize).copied().unwrap_or(0)
    }
}

use crate::sync::SpinLock;

pub static IRQ_CRYSTAL: SpinLock<PhononicCrystal> = SpinLock::new(PhononicCrystal::new());

/// Call from IRQ stub entry (very early).
pub fn phonon_excite(vector: u8, core: u8, tsc: u64) {
    let _ = IRQ_CRYSTAL.lock().excite(vector, core, tsc);
}

/// Call from Chronovore feed path when crystal period known.
pub fn phonon_set_quiet(tsc: u64) {
    IRQ_CRYSTAL.lock().set_quiet_window(tsc);
}
