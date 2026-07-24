//! NOETHER GUARD — Symmetry-conserving resource ledger
//!
//! Each Capability class defines a continuous symmetry on the axiom manifold.
//! The associated Noether charge is tracked in fixed-point units.
//! Any transition that would violate Σ charge_in = Σ charge_out + sink
//! is rejected before the axiom reactor commits.
//!
//! Integrates with:
//!   - capability::{Capability, Authority}
//!   - axiom_manifold state cells (CELL_DMA_RESERVE, CELL_THERMAL_BUDGET, ...)
//!   - charybdis_dma_firewall (DMA charge sink)

use core::sync::atomic::{AtomicI64, Ordering};

/// Charge kinds — one per conserved symmetry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum NoetherCharge {
    /// Bytes of DMA-capable physical memory outstanding
    DmaBytes = 0,
    /// Milliwatt-ticks of thermal budget consumed this epoch
    ThermalMwTick = 1,
    /// Live IRQ registrations
    IrqLive = 2,
    /// MMIO windows currently mapped
    MmioWindows = 3,
    /// Capability tokens outstanding (grant − revoke)
    CapTokens = 4,
    /// Speculative axiom draft depth
    AxiomDraftDepth = 5,
}

pub const CHARGE_KINDS: usize = 6;

/// 16.16 fixed-point charge unit.
pub type ChargeFp = i64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NoetherFault {
    /// Proposed delta would drive a charge below zero (leak / double-free)
    Undershoot {
        kind: NoetherCharge,
        have: ChargeFp,
        need: ChargeFp,
    },
    /// Proposed delta would exceed hard ceiling (runaway grant)
    Overshoot {
        kind: NoetherCharge,
        have: ChargeFp,
        limit: ChargeFp,
    },
    /// Epoch mismatch — stale ticket from a previous manifold epoch
    StaleEpoch { ticket: u64, current: u64 },
    /// Charge kind unknown
    UnknownKind,
}

/// Global conserved ledger. Lock-free via atomics; epoch fences publish.
pub struct NoetherLedger {
    balances: [AtomicI64; CHARGE_KINDS],
    ceilings: [AtomicI64; CHARGE_KINDS],
    epoch: AtomicI64,
    /// Cumulative rejected transitions (telemetry)
    rejects: AtomicI64,
}

impl NoetherLedger {
    pub const fn new() -> Self {
        Self {
            balances: [
                AtomicI64::new(0),
                AtomicI64::new(0),
                AtomicI64::new(0),
                AtomicI64::new(0),
                AtomicI64::new(0),
                AtomicI64::new(0),
            ],
            // Default ceilings — override at boot from Kairos profile
            ceilings: [
                AtomicI64::new(1 << 34), // 16 GiB DMA
                AtomicI64::new(1 << 40), // thermal
                AtomicI64::new(256),     // IRQs
                AtomicI64::new(4096),    // MMIO windows
                AtomicI64::new(1 << 20), // caps
                AtomicI64::new(128),     // axiom drafts
            ],
            epoch: AtomicI64::new(1),
            rejects: AtomicI64::new(0),
        }
    }

    pub fn set_ceiling(&self, kind: NoetherCharge, ceiling: ChargeFp) {
        self.ceilings[kind as usize].store(ceiling, Ordering::Release);
    }

    pub fn balance(&self, kind: NoetherCharge) -> ChargeFp {
        self.balances[kind as usize].load(Ordering::Acquire)
    }

    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire) as u64
    }

    /// Attempt a charge transition. Positive delta = consume, negative = release.
    pub fn transition(
        &self,
        kind: NoetherCharge,
        delta: ChargeFp,
        ticket_epoch: u64,
    ) -> Result<ChargeFp, NoetherFault> {
        let idx = kind as usize;
        if idx >= CHARGE_KINDS {
            return Err(NoetherFault::UnknownKind);
        }
        let current_epoch = self.epoch();
        if ticket_epoch != current_epoch {
            self.rejects.fetch_add(1, Ordering::Relaxed);
            return Err(NoetherFault::StaleEpoch {
                ticket: ticket_epoch,
                current: current_epoch,
            });
        }

        let ceiling = self.ceilings[idx].load(Ordering::Acquire);
        loop {
            let have = self.balances[idx].load(Ordering::Acquire);
            let next = match have.checked_add(delta) {
                Some(v) => v,
                None => {
                    self.rejects.fetch_add(1, Ordering::Relaxed);
                    return Err(NoetherFault::Overshoot {
                        kind,
                        have,
                        limit: ceiling,
                    });
                }
            };
            if next < 0 {
                self.rejects.fetch_add(1, Ordering::Relaxed);
                return Err(NoetherFault::Undershoot {
                    kind,
                    have,
                    need: delta.abs(),
                });
            }
            if next > ceiling {
                self.rejects.fetch_add(1, Ordering::Relaxed);
                return Err(NoetherFault::Overshoot {
                    kind,
                    have,
                    limit: ceiling,
                });
            }
            match self.balances[idx].compare_exchange_weak(
                have,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(next),
                Err(_) => core::hint::spin_loop(),
            }
        }
    }

    /// Advance epoch — invalidates all outstanding tickets (revocation wave).
    pub fn collapse_epoch(&self) -> u64 {
        (self.epoch.fetch_add(1, Ordering::AcqRel) + 1) as u64
    }

    pub fn rejects(&self) -> u64 {
        self.rejects.load(Ordering::Relaxed) as u64
    }
}

/// Boot-global ledger.
pub static NOETHER: NoetherLedger = NoetherLedger::new();

/// RAII-ish ticket: holds epoch + kind so drop can release.
#[derive(Clone, Copy, Debug)]
pub struct NoetherTicket {
    pub kind: NoetherCharge,
    pub amount: ChargeFp,
    pub epoch: u64,
}

impl NoetherTicket {
    pub fn acquire(kind: NoetherCharge, amount: ChargeFp) -> Result<Self, NoetherFault> {
        let epoch = NOETHER.epoch();
        NOETHER.transition(kind, amount, epoch)?;
        Ok(Self {
            kind,
            amount,
            epoch,
        })
    }

    pub fn release(self) -> Result<(), NoetherFault> {
        NOETHER.transition(self.kind, -self.amount, self.epoch)?;
        Ok(())
    }
}

/// Map drivernet / DriverHost operations onto charges.
pub fn charge_dma_alloc(bytes: usize) -> Result<NoetherTicket, NoetherFault> {
    NoetherTicket::acquire(NoetherCharge::DmaBytes, bytes as ChargeFp)
}

pub fn charge_irq_register() -> Result<NoetherTicket, NoetherFault> {
    NoetherTicket::acquire(NoetherCharge::IrqLive, 1)
}

pub fn charge_mmio_map() -> Result<NoetherTicket, NoetherFault> {
    NoetherTicket::acquire(NoetherCharge::MmioWindows, 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conserves_dma() {
        let led = NoetherLedger::new();
        led.set_ceiling(NoetherCharge::DmaBytes, 4096);
        let e = led.epoch();
        assert!(led.transition(NoetherCharge::DmaBytes, 4096, e).is_ok());
        assert!(led.transition(NoetherCharge::DmaBytes, 1, e).is_err());
        assert!(led.transition(NoetherCharge::DmaBytes, -4096, e).is_ok());
        assert_eq!(led.balance(NoetherCharge::DmaBytes), 0);
    }

    #[test]
    fn epoch_collapse_invalidates() {
        let led = NoetherLedger::new();
        let e = led.epoch();
        led.collapse_epoch();
        assert!(matches!(
            led.transition(NoetherCharge::IrqLive, 1, e),
            Err(NoetherFault::StaleEpoch { .. })
        ));
    }
}
