use aether::lockfree::QueueError;
pub use aether::nexus_wire::{
    NexusCommand, NexusOpcode, NexusReply, NexusStatus, NexusTelemetry, WireError,
};
use aether::resonance_split::{ResonanceIngressPage, ResonanceObservationPage};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingCommand {
    pub sequence: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaneClientError {
    NullAddress,
    MisalignedAddress,
    IncompatiblePlane,
    CommandAlreadyPending,
    Queue(QueueError),
    UnexpectedReply { expected: u64, observed: u64 },
    Wire(WireError),
    Kernel(NexusStatus),
}

pub struct ResonancePlaneClient {
    ingress: &'static ResonanceIngressPage,
    observation: &'static ResonanceObservationPage,
    capability: u64,
    next_sequence: u64,
    pending: Option<u64>,
}

impl ResonancePlaneClient {
    /// # Safety
    ///
    /// `ingress_address` and `observation_address` must be the process's mapped
    /// Resonance split pages and remain mapped for the process lifetime.
    pub unsafe fn from_addresses(
        ingress_address: usize,
        observation_address: usize,
        capability: u64,
    ) -> Result<Self, PlaneClientError> {
        if ingress_address == 0 || observation_address == 0 {
            return Err(PlaneClientError::NullAddress);
        }

        if ingress_address % core::mem::align_of::<ResonanceIngressPage>() != 0 {
            return Err(PlaneClientError::MisalignedAddress);
        }

        if observation_address % core::mem::align_of::<ResonanceObservationPage>() != 0 {
            return Err(PlaneClientError::MisalignedAddress);
        }

        // SAFETY: Established by the caller's mapping contract.
        let ingress = unsafe { &*(ingress_address as *const ResonanceIngressPage) };
        let observation = unsafe { &*(observation_address as *const ResonanceObservationPage) };

        if !ingress.compatible() || !observation.compatible() {
            return Err(PlaneClientError::IncompatiblePlane);
        }

        Ok(Self {
            ingress,
            observation,
            capability,
            next_sequence: 1,
            pending: None,
        })
    }

    pub fn telemetry(&self) -> Option<NexusTelemetry> {
        self.observation.telemetry()
    }

    pub fn submit(
        &mut self,
        opcode: NexusOpcode,
        arguments: [u64; 4],
    ) -> Result<PendingCommand, PlaneClientError> {
        if self.pending.is_some() {
            return Err(PlaneClientError::CommandAlreadyPending);
        }

        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1).max(1);

        let command = NexusCommand::new(opcode, sequence, self.capability, arguments);

        self.ingress.submit(&command);

        self.pending = Some(sequence);

        Ok(PendingCommand { sequence })
    }

    pub fn poll_reply(&mut self) -> Result<Option<NexusReply>, PlaneClientError> {
        let Some(expected) = self.pending else {
            return Ok(None);
        };

        let Some(reply) = self.observation.reply(expected) else {
            return Ok(None);
        };

        let status = reply.validate(expected).map_err(PlaneClientError::Wire)?;

        self.pending = None;

        if status != NexusStatus::Ok {
            return Err(PlaneClientError::Kernel(status));
        }

        Ok(Some(reply))
    }

    pub const fn has_pending_command(&self) -> bool {
        self.pending.is_some()
    }

    pub fn dropped_commands(&self) -> u64 {
        0
    }

    pub fn dropped_replies(&self) -> u64 {
        0
    }
}
