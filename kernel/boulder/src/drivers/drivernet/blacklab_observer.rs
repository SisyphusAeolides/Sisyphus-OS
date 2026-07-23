use crate::capability::{Capability, FaultPolicyControl};
use crate::mnemosyne_ledger::{LedgerError, LedgerEvent, LedgerEventKind, MnemosyneLedger};
use crate::oracular_mesh::{OracleEvent, OracularError, OracularMesh, TemporalVerdict};

use super::telemetry::{DriverNetEvent, DriverNetEventKind, DriverNetObserver};

pub const ORACULAR_KIND_DRIVERNET_BASE: u16 = 0xd100;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlackLabObserverFault {
    Ledger(LedgerError),
    Temporal(OracularError),
}

pub struct BlackLabDriverObserver<'borrow, 'authority, const LEDGER: usize, const RULES: usize> {
    ledger: &'borrow MnemosyneLedger<LEDGER>,
    oracular: &'borrow OracularMesh<RULES>,
    authority: &'borrow Capability<'authority, FaultPolicyControl>,
    last_fault: Option<BlackLabObserverFault>,
    last_verdict: Option<TemporalVerdict>,
    observed: u64,
}

impl<'borrow, 'authority, const LEDGER: usize, const RULES: usize>
    BlackLabDriverObserver<'borrow, 'authority, LEDGER, RULES>
{
    pub const fn new(
        ledger: &'borrow MnemosyneLedger<LEDGER>,
        oracular: &'borrow OracularMesh<RULES>,
        authority: &'borrow Capability<'authority, FaultPolicyControl>,
    ) -> Self {
        Self {
            ledger,
            oracular,
            authority,
            last_fault: None,
            last_verdict: None,
            observed: 0,
        }
    }

    pub const fn last_fault(&self) -> Option<BlackLabObserverFault> {
        self.last_fault
    }

    pub const fn last_verdict(&self) -> Option<TemporalVerdict> {
        self.last_verdict
    }

    pub const fn observed(&self) -> u64 {
        self.observed
    }

    pub fn clear_fault(&mut self) {
        self.last_fault = None;
    }
}

impl<const LEDGER: usize, const RULES: usize> DriverNetObserver
    for BlackLabDriverObserver<'_, '_, LEDGER, RULES>
{
    fn observe(&mut self, event: DriverNetEvent) {
        self.observed = self.observed.saturating_add(1);

        if self.last_fault.is_none() {
            let ledger_event = LedgerEvent {
                tick: event.tick,
                subject: u64::from(event.address),
                data0: event.fingerprint_root,
                data1: event.root,
                kind: ledger_kind(event.kind),
                severity: event.severity,
                flags: event.strategy.index() as u8,
            };

            if let Err(error) = self.ledger.append(ledger_event, self.authority) {
                self.last_fault = Some(BlackLabObserverFault::Ledger(error));
            }
        }

        if self.last_fault.is_none() {
            let oracle_event = OracleEvent {
                tick: event.tick,
                kind: ORACULAR_KIND_DRIVERNET_BASE.saturating_add(event.kind as u16),
                severity: event.severity,
                flags: event.strategy.index() as u16,
                subject: u64::from(event.address),
                value: event.root,
            };

            match self.oracular.observe(oracle_event) {
                Ok(verdict) => self.last_verdict = Some(verdict),
                Err(error) => {
                    self.last_fault = Some(BlackLabObserverFault::Temporal(error));
                }
            }
        }
    }
}

fn ledger_kind(kind: DriverNetEventKind) -> LedgerEventKind {
    match kind {
        DriverNetEventKind::Fingerprint | DriverNetEventKind::CandidateAttempt => {
            LedgerEventKind::Observation
        }
        DriverNetEventKind::OracleDecision | DriverNetEventKind::PrimarySelected => {
            LedgerEventKind::PolicyDecision
        }
        DriverNetEventKind::Rollback => LedgerEventKind::Recovery,
        DriverNetEventKind::Commit => LedgerEventKind::Checkpoint,
        DriverNetEventKind::FirmwareFallback => LedgerEventKind::Recovery,
        DriverNetEventKind::Quarantine
        | DriverNetEventKind::InventoryOverflow
        | DriverNetEventKind::ConfigurationIncomplete
        | DriverNetEventKind::NoDisplay => LedgerEventKind::Quarantine,
    }
}
