// kernel/boulder/src/rev_tape.rs
//! RevTape — Bennett-style reversible mutation tape
//!
//! Invariant: for every committed prefix of the tape,
//!   fold(state0, forwards) = state_n
//!   fold(state_n, inverses_rev) = state0
//!
//! Witness bits are the Landauer "garbage" that makes the step
//! locally reversible. Commit drops witnesses (irreversible erase).
//! Abort runs inverses; witnesses are consumed, not guessed.

#![allow(dead_code)]

pub const TAPE_CAP: usize = 256;
pub const WITNESS_WORDS: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RevFault {
    Overflow,
    Underflow,
    BrokenInverse,
    Sealed,
    BadWitness,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum MutOp {
    /// Noether-style counter: add delta to cell
    CounterAdd = 1,
    /// Bit-or flags into a capability word
    CapOr = 2,
    /// Bit-and-not (clear bits)
    CapAndNot = 3,
    /// Map slot: write handle into table[idx] if was EMPTY
    TableInstall = 4,
    /// Table clear: write EMPTY if was handle
    TableClear = 5,
    /// FIFO push index
    QueuePush = 6,
    /// FIFO pop index
    QueuePop = 7,
}

#[derive(Clone, Copy, Debug)]
pub struct Witness {
    pub words: [u64; WITNESS_WORDS],
}

impl Witness {
    pub const ZERO: Self = Self {
        words: [0; WITNESS_WORDS],
    };
}

#[derive(Clone, Copy, Debug)]
pub struct RevStep {
    pub op: MutOp,
    /// Target cell / table slot / queue id
    pub target: u32,
    pub arg0: u64,
    pub arg1: u64,
    pub witness: Witness,
}

impl RevStep {
    pub const EMPTY: Self = Self {
        op: MutOp::CounterAdd,
        target: 0,
        arg0: 0,
        arg1: 0,
        witness: Witness::ZERO,
    };
}

/// Kernel-facing state the tape is allowed to touch.
/// Keep this small and explicit — reversibility is per-field.
#[derive(Clone, Debug)]
pub struct RevWorld {
    pub counters: [i64; 32],
    pub caps: [u64; 32],
    pub table: [u64; 64],
    pub queue: [u32; 64],
    pub q_head: u32,
    pub q_tail: u32,
    pub q_len: u32,
}

impl RevWorld {
    pub const TABLE_EMPTY: u64 = 0;
    pub const fn new() -> Self {
        Self {
            counters: [0; 32],
            caps: [0; 32],
            table: [0; 64],
            queue: [0; 64],
            q_head: 0,
            q_tail: 0,
            q_len: 0,
        }
    }
}

pub struct RevTape {
    steps: [RevStep; TAPE_CAP],
    len: usize,
    sealed: bool,
    /// Monotonic; bumped on seal (irreversible epoch)
    epoch: u64,
}

impl RevTape {
    pub const fn new() -> Self {
        Self {
            steps: [RevStep::EMPTY; TAPE_CAP],
            len: 0,
            sealed: false,
            epoch: 0,
        }
    }

    pub fn is_sealed(&self) -> bool {
        self.sealed
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// Apply forward op, record witness for inverse.
    pub fn apply(&mut self, world: &mut RevWorld, mut step: RevStep) -> Result<(), RevFault> {
        if self.sealed {
            return Err(RevFault::Sealed);
        }
        if self.len >= TAPE_CAP {
            return Err(RevFault::Overflow);
        }
        step.witness = forward(world, &step)?;
        self.steps[self.len] = step;
        self.len += 1;
        Ok(())
    }

    /// Bennett abort: run inverses from tip to root.
    pub fn abort(&mut self, world: &mut RevWorld) -> Result<(), RevFault> {
        if self.sealed {
            return Err(RevFault::Sealed);
        }
        while self.len > 0 {
            let step = self.steps[self.len - 1];
            inverse(world, &step)?;
            self.len -= 1;
        }
        Ok(())
    }

    /// Irreversible commit: drop witnesses (Landauer erase), seal prefix.
    pub fn commit(&mut self) {
        for i in 0..self.len {
            self.steps[i].witness = Witness::ZERO;
        }
        self.len = 0;
        self.sealed = false; // tape reused; epoch marks irreversibility
        self.epoch = self.epoch.wrapping_add(1);
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }
}

fn forward(world: &mut RevWorld, step: &RevStep) -> Result<Witness, RevFault> {
    let mut w = Witness::ZERO;
    match step.op {
        MutOp::CounterAdd => {
            let i = step.target as usize;
            if i >= world.counters.len() {
                return Err(RevFault::BrokenInverse);
            }
            w.words[0] = world.counters[i] as u64;
            world.counters[i] = world.counters[i].wrapping_add(step.arg0 as i64);
        }
        MutOp::CapOr => {
            let i = step.target as usize;
            if i >= world.caps.len() {
                return Err(RevFault::BrokenInverse);
            }
            w.words[0] = world.caps[i];
            world.caps[i] |= step.arg0;
        }
        MutOp::CapAndNot => {
            let i = step.target as usize;
            if i >= world.caps.len() {
                return Err(RevFault::BrokenInverse);
            }
            w.words[0] = world.caps[i];
            world.caps[i] &= !step.arg0;
        }
        MutOp::TableInstall => {
            let i = step.target as usize;
            if i >= world.table.len() {
                return Err(RevFault::BrokenInverse);
            }
            if world.table[i] != RevWorld::TABLE_EMPTY {
                return Err(RevFault::BrokenInverse);
            }
            w.words[0] = RevWorld::TABLE_EMPTY;
            world.table[i] = step.arg0;
        }
        MutOp::TableClear => {
            let i = step.target as usize;
            if i >= world.table.len() {
                return Err(RevFault::BrokenInverse);
            }
            w.words[0] = world.table[i];
            if world.table[i] != step.arg0 {
                return Err(RevFault::BadWitness);
            }
            world.table[i] = RevWorld::TABLE_EMPTY;
        }
        MutOp::QueuePush => {
            if world.q_len as usize >= world.queue.len() {
                return Err(RevFault::Overflow);
            }
            w.words[0] = world.q_tail as u64;
            world.queue[world.q_tail as usize] = step.arg0 as u32;
            world.q_tail = (world.q_tail + 1) % world.queue.len() as u32;
            world.q_len += 1;
        }
        MutOp::QueuePop => {
            if world.q_len == 0 {
                return Err(RevFault::Underflow);
            }
            let v = world.queue[world.q_head as usize];
            w.words[0] = v as u64;
            w.words[1] = world.q_head as u64;
            world.q_head = (world.q_head + 1) % world.queue.len() as u32;
            world.q_len -= 1;
            // arg0 must match for strict reversibility contracts
            if step.arg0 != 0 && step.arg0 != v as u64 {
                return Err(RevFault::BadWitness);
            }
        }
    }
    Ok(w)
}

fn inverse(world: &mut RevWorld, step: &RevStep) -> Result<(), RevFault> {
    match step.op {
        MutOp::CounterAdd => {
            let i = step.target as usize;
            // Strict: restore witness snapshot (not just subtract) —
            // survives wrapping and concurrent bugs.
            world.counters[i] = step.witness.words[0] as i64;
        }
        MutOp::CapOr | MutOp::CapAndNot => {
            let i = step.target as usize;
            world.caps[i] = step.witness.words[0];
        }
        MutOp::TableInstall => {
            let i = step.target as usize;
            if world.table[i] != step.arg0 {
                return Err(RevFault::BrokenInverse);
            }
            world.table[i] = step.witness.words[0];
        }
        MutOp::TableClear => {
            let i = step.target as usize;
            world.table[i] = step.witness.words[0];
        }
        MutOp::QueuePush => {
            if world.q_len == 0 {
                return Err(RevFault::BrokenInverse);
            }
            world.q_tail = step.witness.words[0] as u32;
            world.q_len -= 1;
        }
        MutOp::QueuePop => {
            // re-insert at old head
            world.q_head = step.witness.words[1] as u32;
            world.queue[world.q_head as usize] = step.witness.words[0] as u32;
            world.q_len += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abort_restores_counters() {
        let mut w = RevWorld::new();
        let mut t = RevTape::new();
        t.apply(
            &mut w,
            RevStep {
                op: MutOp::CounterAdd,
                target: 0,
                arg0: 7,
                arg1: 0,
                witness: Witness::ZERO,
            },
        )
        .unwrap();
        t.apply(
            &mut w,
            RevStep {
                op: MutOp::CounterAdd,
                target: 0,
                arg0: 5,
                arg1: 0,
                witness: Witness::ZERO,
            },
        )
        .unwrap();
        assert_eq!(w.counters[0], 12);
        t.abort(&mut w).unwrap();
        assert_eq!(w.counters[0], 0);
    }

    #[test]
    fn table_install_inverse() {
        let mut w = RevWorld::new();
        let mut t = RevTape::new();
        t.apply(
            &mut w,
            RevStep {
                op: MutOp::TableInstall,
                target: 3,
                arg0: 0xdead_beef,
                arg1: 0,
                witness: Witness::ZERO,
            },
        )
        .unwrap();
        assert_eq!(w.table[3], 0xdead_beef);
        t.abort(&mut w).unwrap();
        assert_eq!(w.table[3], 0);
    }
}
