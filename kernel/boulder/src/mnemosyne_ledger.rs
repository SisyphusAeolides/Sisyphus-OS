use crate::capability::{Capability, FaultPolicyControl};
use crate::sync::SpinLock;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum LedgerEventKind {
    Observation = 1,
    PolicyDecision = 2,
    DmaPrepared = 3,
    DmaCommitted = 4,
    DmaRevoked = 5,
    TemporalViolation = 6,
    Quarantine = 7,
    Recovery = 8,
    Checkpoint = 9,
    DeviceReset = 10,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LedgerEvent {
    pub tick: u64,
    pub subject: u64,
    pub data0: u64,
    pub data1: u64,
    pub kind: LedgerEventKind,
    pub severity: u8,
    pub flags: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C, align(64))]
pub struct LedgerEntry {
    pub sequence: u64,
    pub tick: u64,
    pub subject: u64,
    pub data0: u64,
    pub data1: u64,
    pub previous_root: u64,
    pub chain_root: u64,
    pub kind: u16,
    pub severity: u8,
    pub flags: u8,
    pub reserved: u32,
}

const _: () = assert!(core::mem::size_of::<LedgerEntry>() == 64);

impl LedgerEntry {
    const EMPTY: Self = Self {
        sequence: 0,
        tick: 0,
        subject: 0,
        data0: 0,
        data1: 0,
        previous_root: 0,
        chain_root: 0,
        kind: 0,
        severity: 0,
        flags: 0,
        reserved: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LedgerSeal {
    pub epoch: u64,
    pub retained: usize,
    pub overwritten: u64,
    pub first_sequence: u64,
    pub last_sequence: u64,
    pub anchor_root: u64,
    pub chain_root: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LedgerError {
    ZeroCapacity,
    Empty,
    SequenceExhausted,
    CorruptPreviousRoot,
    CorruptEntryRoot,
    CorruptSequence,
    InvalidEpoch,
}

struct LedgerState<const N: usize> {
    entries: [LedgerEntry; N],
    head: usize,
    length: usize,
    next_sequence: u64,
    epoch: u64,
    anchor_root: u64,
    overwritten: u64,
}

impl<const N: usize> LedgerState<N> {
    const fn new(secret: u64, epoch: u64) -> Self {
        Self {
            entries: [LedgerEntry::EMPTY; N],
            head: 0,
            length: 0,
            next_sequence: 1,
            epoch,
            anchor_root: genesis_root(secret, epoch),
            overwritten: 0,
        }
    }

    fn logical_index(&self, offset: usize) -> usize {
        (self.head + offset) % N
    }

    fn latest(&self) -> Option<LedgerEntry> {
        if self.length == 0 {
            None
        } else {
            Some(self.entries[self.logical_index(self.length - 1)])
        }
    }
}

pub struct MnemosyneLedger<const N: usize> {
    secret: u64,
    state: SpinLock<LedgerState<N>>,
}

impl<const N: usize> MnemosyneLedger<N> {
    pub const fn new(secret: u64, epoch: u64) -> Self {
        Self {
            secret,
            state: SpinLock::new(LedgerState::new(secret, epoch)),
        }
    }

    pub fn append(
        &self,
        event: LedgerEvent,
        _authority: &Capability<'_, FaultPolicyControl>,
    ) -> Result<LedgerEntry, LedgerError> {
        if N == 0 {
            return Err(LedgerError::ZeroCapacity);
        }

        let mut state = self.state.lock();
        let sequence = state.next_sequence;
        if sequence == 0 || sequence == u64::MAX {
            return Err(LedgerError::SequenceExhausted);
        }

        let previous_root = state
            .latest()
            .map(|entry| entry.chain_root)
            .unwrap_or(state.anchor_root);

        let index = if state.length < N {
            let index = state.logical_index(state.length);
            state.length += 1;
            index
        } else {
            let index = state.head;
            state.anchor_root = state.entries[index].chain_root;
            state.head = (state.head + 1) % N;
            state.overwritten = state.overwritten.saturating_add(1);
            index
        };

        let mut entry = LedgerEntry {
            sequence,
            tick: event.tick,
            subject: event.subject,
            data0: event.data0,
            data1: event.data1,
            previous_root,
            chain_root: 0,
            kind: event.kind as u16,
            severity: event.severity,
            flags: event.flags,
            reserved: 0,
        };
        entry.chain_root = entry_root(self.secret, state.epoch, &entry);

        state.entries[index] = entry;
        state.next_sequence = sequence + 1;

        Ok(entry)
    }

    pub fn seal(&self) -> LedgerSeal {
        let state = self.state.lock();
        let first = if state.length == 0 {
            0
        } else {
            state.entries[state.head].sequence
        };
        let last = state.latest().map(|entry| entry.sequence).unwrap_or(0);
        let root = state
            .latest()
            .map(|entry| entry.chain_root)
            .unwrap_or(state.anchor_root);

        LedgerSeal {
            epoch: state.epoch,
            retained: state.length,
            overwritten: state.overwritten,
            first_sequence: first,
            last_sequence: last,
            anchor_root: state.anchor_root,
            chain_root: root,
        }
    }

    pub fn verify(&self) -> Result<LedgerSeal, LedgerError> {
        if N == 0 {
            return Err(LedgerError::ZeroCapacity);
        }

        let state = self.state.lock();
        let mut expected_previous = state.anchor_root;
        let mut previous_sequence = None;

        for offset in 0..state.length {
            let entry = state.entries[state.logical_index(offset)];

            if entry.previous_root != expected_previous {
                return Err(LedgerError::CorruptPreviousRoot);
            }

            if let Some(previous) = previous_sequence {
                if entry.sequence != previous + 1 {
                    return Err(LedgerError::CorruptSequence);
                }
            } else if entry.sequence == 0 {
                return Err(LedgerError::CorruptSequence);
            }

            let expected_root = entry_root(self.secret, state.epoch, &entry);
            if entry.chain_root != expected_root {
                return Err(LedgerError::CorruptEntryRoot);
            }

            previous_sequence = Some(entry.sequence);
            expected_previous = entry.chain_root;
        }

        let first = if state.length == 0 {
            0
        } else {
            state.entries[state.head].sequence
        };
        let last = previous_sequence.unwrap_or(0);

        Ok(LedgerSeal {
            epoch: state.epoch,
            retained: state.length,
            overwritten: state.overwritten,
            first_sequence: first,
            last_sequence: last,
            anchor_root: state.anchor_root,
            chain_root: expected_previous,
        })
    }

    pub fn copy_recent(&self, output: &mut [LedgerEntry]) -> usize {
        if N == 0 || output.is_empty() {
            return 0;
        }

        let state = self.state.lock();
        let count = output.len().min(state.length);
        let start = state.length - count;

        for (destination, offset) in output.iter_mut().zip(start..state.length) {
            *destination = state.entries[state.logical_index(offset)];
        }

        count
    }

    pub fn reset_epoch(
        &self,
        new_epoch: u64,
        _authority: &Capability<'_, FaultPolicyControl>,
    ) -> Result<LedgerSeal, LedgerError> {
        if N == 0 {
            return Err(LedgerError::ZeroCapacity);
        }
        if new_epoch == 0 {
            return Err(LedgerError::InvalidEpoch);
        }

        let mut state = self.state.lock();
        for entry in &mut state.entries {
            *entry = LedgerEntry::EMPTY;
        }
        state.head = 0;
        state.length = 0;
        state.next_sequence = 1;
        state.epoch = new_epoch;
        state.anchor_root = genesis_root(self.secret, new_epoch);
        state.overwritten = 0;

        Ok(LedgerSeal {
            epoch: new_epoch,
            retained: 0,
            overwritten: 0,
            first_sequence: 0,
            last_sequence: 0,
            anchor_root: state.anchor_root,
            chain_root: state.anchor_root,
        })
    }
}

const fn genesis_root(secret: u64, epoch: u64) -> u64 {
    avalanche(secret ^ epoch.rotate_left(17) ^ 0x4d4e_454d_4f53_594e)
}

fn entry_root(secret: u64, epoch: u64, entry: &LedgerEntry) -> u64 {
    let mut state = genesis_root(secret ^ entry.previous_root, epoch);
    state = absorb(state, entry.sequence);
    state = absorb(state, entry.tick);
    state = absorb(state, entry.subject);
    state = absorb(state, entry.data0);
    state = absorb(state, entry.data1);
    state = absorb(state, u64::from(entry.kind));
    state = absorb(state, u64::from(entry.severity));
    state = absorb(state, u64::from(entry.flags));
    avalanche(state ^ secret.rotate_right(11))
}

fn absorb(state: u64, word: u64) -> u64 {
    avalanche(state ^ word.wrapping_mul(0x9e37_79b9_7f4a_7c15))
}

const fn avalanche(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Authority;

    #[test]
    fn appends_verifies_and_overwrites_in_order() {
        let authority = unsafe { Authority::assume_root() };
        let fault = authority.grant::<FaultPolicyControl>();
        let ledger = MnemosyneLedger::<3>::new(0x1234, 7);

        for tick in 1..=5 {
            ledger
                .append(
                    LedgerEvent {
                        tick,
                        subject: 9,
                        data0: tick * 10,
                        data1: 0,
                        kind: LedgerEventKind::Observation,
                        severity: 1,
                        flags: 0,
                    },
                    &fault,
                )
                .unwrap();
        }

        let seal = ledger.verify().unwrap();
        assert_eq!(seal.retained, 3);
        assert_eq!(seal.overwritten, 2);
        assert_eq!(seal.first_sequence, 3);
        assert_eq!(seal.last_sequence, 5);

        let mut entries = [LedgerEntry::EMPTY; 3];
        assert_eq!(ledger.copy_recent(&mut entries), 3);
        assert_eq!(entries[0].sequence, 3);
        assert_eq!(entries[2].sequence, 5);
    }

    #[test]
    fn epoch_reset_changes_the_anchor() {
        let authority = unsafe { Authority::assume_root() };
        let fault = authority.grant::<FaultPolicyControl>();
        let ledger = MnemosyneLedger::<4>::new(0x1234, 7);
        let before = ledger.seal();
        let after = ledger.reset_epoch(8, &fault).unwrap();

        assert_ne!(before.anchor_root, after.anchor_root);
        assert_eq!(after.retained, 0);
    }
}
