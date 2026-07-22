use aether::nexus_wire::{
    NexusCommand, NexusOpcode, NexusReply, NexusStatus, WireError,
};

use crate::capability::{
    Capability, ResonanceRight,
};
use crate::causal_lattice::{
    CausalClock, CausalEnvelope, CausalStamp, ReplayError, ReplayShield,
};
use crate::ghost_chronicle::GhostChronicle;
use crate::singularity::{
    SingularityGovernor, StabilityDecision, StabilitySample,
};
use crate::sync::SpinLock;

const GHOST_COMMAND: u16 = 0x100;
const GHOST_REPLY: u16 = 0x101;
const GHOST_DENIED: u16 = 0x102;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct NexusRights(u32);

impl NexusRights {
    pub const OBSERVE: Self = Self(1 << 0);
    pub const SCHEDULE: Self = Self(1 << 1);
    pub const RESONANCE: Self = Self(1 << 2);
    pub const CONTROL: Self = Self(1 << 3);

    pub const ALL: Self = Self(
        Self::OBSERVE.0
            | Self::SCHEDULE.0
            | Self::RESONANCE.0
            | Self::CONTROL.0,
    );

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }
}

#[derive(Clone, Copy)]
struct Grant {
    active: bool,
    handle: u64,
    rights: NexusRights,
    expires_tick: u64,
}

impl Grant {
    const EMPTY: Self = Self {
        active: false,
        handle: 0,
        rights: NexusRights(0),
        expires_tick: 0,
    };
}

struct GatewayState<
    const GRANTS: usize,
    const REPLAY: usize,
    const LOG: usize,
    const HISTORY: usize,
> {
    grants: [Grant; GRANTS],
    replay: ReplayShield<REPLAY>,
    chronicle: GhostChronicle<LOG>,
    governor: SingularityGovernor<HISTORY>,
}

impl<
        const GRANTS: usize,
        const REPLAY: usize,
        const LOG: usize,
        const HISTORY: usize,
    > GatewayState<GRANTS, REPLAY, LOG, HISTORY>
{
    const fn new(seed: u64) -> Self {
        Self {
            grants: [Grant::EMPTY; GRANTS],
            replay: ReplayShield::new(),
            chronicle: GhostChronicle::new(seed),
            governor: SingularityGovernor::new(),
        }
    }
}

pub struct NexusGateway<
    const GRANTS: usize,
    const REPLAY: usize,
    const LOG: usize,
    const HISTORY: usize,
> {
    clock: CausalClock,
    state: SpinLock<GatewayState<GRANTS, REPLAY, LOG, HISTORY>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Admission {
    pub sequence: u64,
    pub opcode: NexusOpcode,
    pub stamp: CausalStamp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GatewayError {
    Wire(WireError),
    Replay(ReplayError),
    Denied,
    Expired,
    Capacity,
}

impl<
        const GRANTS: usize,
        const REPLAY: usize,
        const LOG: usize,
        const HISTORY: usize,
    > NexusGateway<GRANTS, REPLAY, LOG, HISTORY>
{
    pub const fn new(node: u16, chronicle_seed: u64) -> Self {
        Self {
            clock: CausalClock::new(node),
            state: SpinLock::new(GatewayState::new(chronicle_seed)),
        }
    }

    pub fn install_grant(
        &self,
        handle: u64,
        rights: NexusRights,
        expires_tick: u64,
        _authority: &Capability<'_, ResonanceRight>,
    ) -> Result<(), GatewayError> {
        if handle == 0 {
            return Err(GatewayError::Denied);
        }

        let mut state = self.state.lock();

        if let Some(existing) = state
            .grants
            .iter_mut()
            .find(|grant| grant.active && grant.handle == handle)
        {
            existing.rights = rights;
            existing.expires_tick = expires_tick;
            return Ok(());
        }

        let slot = state
            .grants
            .iter_mut()
            .find(|grant| !grant.active)
            .ok_or(GatewayError::Capacity)?;

        *slot = Grant {
            active: true,
            handle,
            rights,
            expires_tick,
        };

        Ok(())
    }

    pub fn revoke_grant(
        &self,
        handle: u64,
        _authority: &Capability<'_, ResonanceRight>,
    ) -> bool {
        let mut state = self.state.lock();

        let Some(grant) = state
            .grants
            .iter_mut()
            .find(|grant| grant.active && grant.handle == handle)
        else {
            return false;
        };

        *grant = Grant::EMPTY;
        true
    }

    pub fn admit(
        &self,
        command: &NexusCommand,
        now_tick: u64,
    ) -> Result<Admission, GatewayError> {
        let opcode = command.validate().map_err(GatewayError::Wire)?;
        let required = required_rights(opcode);

        let stamp = self.clock.stamp(now_tick);
        let envelope = CausalEnvelope::new(
            stamp,
            command.sequence,
            command.opcode as u32,
            0,
            &[],
        );

        let mut state = self.state.lock();

        let Some(grant) = state.grants.iter().find(|grant| {
            grant.active && grant.handle == command.capability
        }) else {
            let _ = state.chronicle.record(
                now_tick,
                0,
                GHOST_DENIED,
                command.opcode as u32,
                command.sequence,
                command.capability,
            );

            return Err(GatewayError::Denied);
        };

        if grant.expires_tick != 0 && now_tick > grant.expires_tick {
            return Err(GatewayError::Expired);
        }

        if !grant.rights.contains(required) {
            return Err(GatewayError::Denied);
        }

        state
            .replay
            .accept(&envelope, now_tick, 0, 0)
            .map_err(GatewayError::Replay)?;

        let _ = state.chronicle.record(
            now_tick,
            0,
            GHOST_COMMAND,
            command.opcode as u32,
            command.sequence,
            command.checksum,
        );

        Ok(Admission {
            sequence: command.sequence,
            opcode,
            stamp,
        })
    }

    pub fn finish(
        &self,
        admission: Admission,
        status: NexusStatus,
        generation: u32,
        values: [u64; 3],
    ) -> NexusReply {
        let reply = NexusReply::new(
            status,
            admission.sequence,
            admission.stamp.physical_tick,
            generation,
            admission.opcode as u16,
            values,
        );

        let mut state = self.state.lock();

        let _ = state.chronicle.record(
            admission.stamp.physical_tick,
            0,
            GHOST_REPLY,
            status as u32,
            admission.sequence,
            reply.checksum,
        );

        reply
    }

    pub fn observe_stability(
        &self,
        sample: StabilitySample,
    ) -> StabilityDecision {
        self.state.lock().governor.observe(sample)
    }

    pub fn chronicle_is_valid(&self) -> bool {
        self.state.lock().chronicle.verify()
    }
}

fn required_rights(opcode: NexusOpcode) -> NexusRights {
    match opcode {
        NexusOpcode::QueryStats | NexusOpcode::QueryTelemetry => {
            NexusRights::OBSERVE
        }

        NexusOpcode::AttachTask | NexusOpcode::SetPriorityMass => {
            NexusRights::SCHEDULE
        }

        NexusOpcode::Entangle | NexusOpcode::OfferKairos => {
            NexusRights::RESONANCE
        }

        NexusOpcode::SetCollapseThreshold => NexusRights::CONTROL,
    }
}
