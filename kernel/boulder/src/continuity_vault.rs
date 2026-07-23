use core::mem::MaybeUninit;

use crate::sync::SpinLock;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CheckpointId {
    pub slot: u16,
    pub generation: u64,
    pub state_root: u64,
    pub tick: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VaultError {
    ZeroCapacity,
    InvalidCheckpoint,
    StaleCheckpoint,
}

struct VaultSlot<T> {
    initialized: bool,
    generation: u64,
    state_root: u64,
    tick: u64,
    value: MaybeUninit<T>,
}

impl<T> VaultSlot<T> {
    const fn new() -> Self {
        Self {
            initialized: false,
            generation: 0,
            state_root: 0,
            tick: 0,
            value: MaybeUninit::uninit(),
        }
    }

    fn replace(&mut self, generation: u64, state_root: u64, tick: u64, value: T) {
        if self.initialized {
            // SAFETY: initialized guarantees a live T.
            unsafe {
                self.value.assume_init_drop();
            }
        }

        self.value.write(value);
        self.initialized = true;
        self.generation = generation;
        self.state_root = state_root;
        self.tick = tick;
    }

    fn clone_value(&self) -> Option<T>
    where
        T: Clone,
    {
        if !self.initialized {
            return None;
        }

        // SAFETY: initialized guarantees a live T.
        Some(unsafe { self.value.assume_init_ref().clone() })
    }

    fn clear(&mut self) {
        if self.initialized {
            // SAFETY: initialized guarantees a live T.
            unsafe {
                self.value.assume_init_drop();
            }
        }

        self.initialized = false;
        self.generation = 0;
        self.state_root = 0;
        self.tick = 0;
    }
}

struct VaultState<T, const N: usize> {
    slots: [VaultSlot<T>; N],
    cursor: usize,
    next_generation: u64,
}

impl<T, const N: usize> VaultState<T, N> {
    const fn new() -> Self {
        Self {
            slots: [const { VaultSlot::new() }; N],
            cursor: 0,
            next_generation: 1,
        }
    }
}

impl<T, const N: usize> Drop for VaultState<T, N> {
    fn drop(&mut self) {
        for slot in &mut self.slots {
            slot.clear();
        }
    }
}

pub struct ContinuityVault<T, const N: usize> {
    state: SpinLock<VaultState<T, N>>,
}

impl<T: Clone, const N: usize> ContinuityVault<T, N> {
    pub const fn new() -> Self {
        Self {
            state: SpinLock::new(VaultState::new()),
        }
    }

    pub fn checkpoint(
        &self,
        value: &T,
        state_root: u64,
        tick: u64,
    ) -> Result<CheckpointId, VaultError> {
        if N == 0 {
            return Err(VaultError::ZeroCapacity);
        }

        let mut state = self.state.lock();

        let slot_index = state.cursor;
        state.cursor = (state.cursor + 1) % N;

        let generation = state.next_generation.max(1);

        state.next_generation = state.next_generation.wrapping_add(1).max(1);

        state.slots[slot_index].replace(generation, state_root, tick, value.clone());

        Ok(CheckpointId {
            slot: slot_index as u16,
            generation,
            state_root,
            tick,
        })
    }

    pub fn restore(&self, checkpoint: CheckpointId) -> Result<T, VaultError> {
        let state = self.state.lock();

        let slot = state
            .slots
            .get(usize::from(checkpoint.slot))
            .ok_or(VaultError::InvalidCheckpoint)?;

        if !slot.initialized {
            return Err(VaultError::InvalidCheckpoint);
        }

        if slot.generation != checkpoint.generation || slot.state_root != checkpoint.state_root {
            return Err(VaultError::StaleCheckpoint);
        }

        slot.clone_value().ok_or(VaultError::InvalidCheckpoint)
    }

    pub fn latest(&self) -> Option<CheckpointId> {
        let state = self.state.lock();

        state
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.initialized)
            .max_by_key(|(_, slot)| slot.generation)
            .map(|(index, slot)| CheckpointId {
                slot: index as u16,
                generation: slot.generation,
                state_root: slot.state_root,
                tick: slot.tick,
            })
    }

    pub fn discard(&self, checkpoint: CheckpointId) -> Result<(), VaultError> {
        let mut state = self.state.lock();

        let slot = state
            .slots
            .get_mut(usize::from(checkpoint.slot))
            .ok_or(VaultError::InvalidCheckpoint)?;

        if !slot.initialized || slot.generation != checkpoint.generation {
            return Err(VaultError::StaleCheckpoint);
        }

        slot.clear();
        Ok(())
    }
}

impl<T: Clone, const N: usize> Default for ContinuityVault<T, N> {
    fn default() -> Self {
        Self::new()
    }
}
