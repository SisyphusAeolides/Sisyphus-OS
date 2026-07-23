use crate::sync::SpinLock;

pub const CAUSAL_PAYLOAD_BYTES: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
#[repr(C)]
pub struct CausalStamp {
    pub physical_tick: u64,
    pub logical_tick: u32,
    pub node: u16,
    pub reserved: u16,
}

impl CausalStamp {
    pub const ZERO: Self = Self {
        physical_tick: 0,
        logical_tick: 0,
        node: 0,
        reserved: 0,
    };
}

#[derive(Clone, Copy)]
struct ClockState {
    physical_tick: u64,
    logical_tick: u32,
}

pub struct CausalClock {
    node: u16,
    state: SpinLock<ClockState>,
}

impl CausalClock {
    pub const fn new(node: u16) -> Self {
        Self {
            node,
            state: SpinLock::new(ClockState {
                physical_tick: 0,
                logical_tick: 0,
            }),
        }
    }

    pub fn stamp(&self, wall_tick: u64) -> CausalStamp {
        let mut state = self.state.lock();

        if wall_tick > state.physical_tick {
            state.physical_tick = wall_tick;
            state.logical_tick = 0;
        } else {
            state.logical_tick = state.logical_tick.saturating_add(1);
        }

        CausalStamp {
            physical_tick: state.physical_tick,
            logical_tick: state.logical_tick,
            node: self.node,
            reserved: 0,
        }
    }

    pub fn observe(&self, remote: CausalStamp, wall_tick: u64) -> CausalStamp {
        let mut state = self.state.lock();

        let local_physical = state.physical_tick;
        let merged_physical = wall_tick.max(local_physical).max(remote.physical_tick);

        let merged_logical =
            if merged_physical == local_physical && merged_physical == remote.physical_tick {
                state
                    .logical_tick
                    .max(remote.logical_tick)
                    .saturating_add(1)
            } else if merged_physical == local_physical {
                state.logical_tick.saturating_add(1)
            } else if merged_physical == remote.physical_tick {
                remote.logical_tick.saturating_add(1)
            } else {
                0
            };

        state.physical_tick = merged_physical;
        state.logical_tick = merged_logical;

        CausalStamp {
            physical_tick: merged_physical,
            logical_tick: merged_logical,
            node: self.node,
            reserved: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C, align(64))]
pub struct CausalEnvelope {
    pub stamp: CausalStamp,
    pub nonce: u64,
    pub route: u32,
    pub length: u16,
    pub flags: u16,
    pub payload: [u8; CAUSAL_PAYLOAD_BYTES],
}

const _: () = assert!(core::mem::size_of::<CausalEnvelope>() == 64);

impl CausalEnvelope {
    pub fn new(stamp: CausalStamp, nonce: u64, route: u32, flags: u16, payload: &[u8]) -> Self {
        let mut envelope = Self {
            stamp,
            nonce,
            route,
            length: 0,
            flags,
            payload: [0; CAUSAL_PAYLOAD_BYTES],
        };

        let length = payload.len().min(CAUSAL_PAYLOAD_BYTES);
        envelope.length = length as u16;
        envelope.payload[..length].copy_from_slice(&payload[..length]);
        envelope
    }

    pub fn bytes(&self) -> &[u8] {
        let length = usize::from(self.length).min(CAUSAL_PAYLOAD_BYTES);
        &self.payload[..length]
    }
}

#[derive(Clone, Copy)]
struct ReplaySlot {
    active: bool,
    nonce: u64,
    stamp: CausalStamp,
}

impl ReplaySlot {
    const EMPTY: Self = Self {
        active: false,
        nonce: 0,
        stamp: CausalStamp::ZERO,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplayError {
    Duplicate,
    TooOld,
    TooFarAhead,
    InvalidLength,
    NoCapacity,
}

pub struct ReplayShield<const N: usize> {
    slots: [ReplaySlot; N],
    cursor: usize,
    accepted: u64,
    rejected: u64,
}

impl<const N: usize> ReplayShield<N> {
    pub const fn new() -> Self {
        Self {
            slots: [ReplaySlot::EMPTY; N],
            cursor: 0,
            accepted: 0,
            rejected: 0,
        }
    }

    pub fn accept(
        &mut self,
        envelope: &CausalEnvelope,
        now_tick: u64,
        maximum_past: u64,
        maximum_future: u64,
    ) -> Result<(), ReplayError> {
        if N == 0 {
            self.rejected = self.rejected.saturating_add(1);
            return Err(ReplayError::NoCapacity);
        }

        if usize::from(envelope.length) > CAUSAL_PAYLOAD_BYTES {
            self.rejected = self.rejected.saturating_add(1);
            return Err(ReplayError::InvalidLength);
        }

        if envelope.stamp.physical_tick.saturating_add(maximum_past) < now_tick {
            self.rejected = self.rejected.saturating_add(1);
            return Err(ReplayError::TooOld);
        }

        if envelope.stamp.physical_tick > now_tick.saturating_add(maximum_future) {
            self.rejected = self.rejected.saturating_add(1);
            return Err(ReplayError::TooFarAhead);
        }

        if self
            .slots
            .iter()
            .any(|slot| slot.active && slot.nonce == envelope.nonce)
        {
            self.rejected = self.rejected.saturating_add(1);
            return Err(ReplayError::Duplicate);
        }

        self.slots[self.cursor] = ReplaySlot {
            active: true,
            nonce: envelope.nonce,
            stamp: envelope.stamp,
        };

        self.cursor = (self.cursor + 1) % N;
        self.accepted = self.accepted.saturating_add(1);

        Ok(())
    }

    pub const fn totals(&self) -> (u64, u64) {
        (self.accepted, self.rejected)
    }
}

impl<const N: usize> Default for ReplayShield<N> {
    fn default() -> Self {
        Self::new()
    }
}
