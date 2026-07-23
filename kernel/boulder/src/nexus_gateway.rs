use aether::boot_cell::BootCell;
use aether::nexus_wire::{NexusCommand, NexusOpcode, NexusReply, NexusStatus, WireError};

use crate::capability::{Capability, ResonanceRight};
use crate::causal_lattice::{CausalClock, CausalEnvelope, CausalStamp, ReplayError, ReplayShield};
use crate::ghost_chronicle::GhostChronicle;
use crate::lease_lattice::{LeaseError, LeaseLattice, LeaseRights, LeaseToken};
use crate::singularity::{SingularityGovernor, StabilityDecision, StabilitySample};
use crate::sync::SpinLock;

const GHOST_COMMAND: u16 = 0x100;
const GHOST_REPLY: u16 = 0x101;
const GHOST_DENIED: u16 = 0x102;

pub static LEASES: BootCell<LeaseLattice<256>> = BootCell::new();

struct GatewayState<
    const GRANTS: usize,
    const REPLAY: usize,
    const LOG: usize,
    const HISTORY: usize,
> {
    replay: ReplayShield<REPLAY>,
    chronicle: GhostChronicle<LOG>,
    governor: SingularityGovernor<HISTORY>,
}

impl<const GRANTS: usize, const REPLAY: usize, const LOG: usize, const HISTORY: usize>
    GatewayState<GRANTS, REPLAY, LOG, HISTORY>
{
    const fn new(seed: u64) -> Self {
        Self {
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
    NotReady,
    Lease(LeaseError),
}

impl<const GRANTS: usize, const REPLAY: usize, const LOG: usize, const HISTORY: usize>
    NexusGateway<GRANTS, REPLAY, LOG, HISTORY>
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
        rights: LeaseRights,
        expires_tick: u64,
        authority: &Capability<'_, ResonanceRight>,
    ) -> Result<(), GatewayError> {
        let leases = LEASES.get().ok_or(GatewayError::NotReady)?;
        // We use handle as quota as it's the only sensible mapping if we don't return a new handle,
        // or we just issue a default quota. Let's use 1000 or handle as u32.
        let quota = if handle > 0 { handle as u32 } else { 1000 };
        let _ = leases.issue_root(rights, 0, expires_tick, quota, authority).map_err(GatewayError::Lease)?;
        Ok(())
    }

    pub fn revoke_grant(&self, handle: u64, authority: &Capability<'_, ResonanceRight>) -> bool {
        if let Some(leases) = LEASES.get() {
            leases.revoke(LeaseToken::from_raw(handle), authority).is_ok()
        } else {
            false
        }
    }

    pub fn admit(&self, command: &NexusCommand, now_tick: u64) -> Result<Admission, GatewayError> {
        let opcode = command.validate().map_err(GatewayError::Wire)?;

        let leases = LEASES.get().ok_or(GatewayError::NotReady)?;

        leases
            .admit(
                LeaseToken::from_raw(command.capability),
                rights_for_opcode(opcode),
                now_tick,
            )
            .map_err(GatewayError::Lease)?;

        let stamp = self.clock.stamp(now_tick);
        let envelope = CausalEnvelope::new(stamp, command.sequence, command.opcode as u32, 0, &[]);

        let mut state = self.state.lock();

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

    pub fn observe_stability(&self, sample: StabilitySample) -> StabilityDecision {
        self.state.lock().governor.observe(sample)
    }

    pub fn chronicle_is_valid(&self) -> bool {
        self.state.lock().chronicle.verify()
    }
}

fn rights_for_opcode(opcode: NexusOpcode) -> LeaseRights {
    match opcode {
        NexusOpcode::QueryStats | NexusOpcode::QueryTelemetry => LeaseRights::OBSERVE,

        NexusOpcode::AttachTask | NexusOpcode::SetPriorityMass => LeaseRights::SCHEDULE,

        NexusOpcode::Entangle | NexusOpcode::OfferKairos => LeaseRights::RESONANCE,

        NexusOpcode::SetCollapseThreshold => LeaseRights::CONTROL,
    }
}
