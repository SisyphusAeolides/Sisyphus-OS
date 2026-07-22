use aether::lockfree::QueueError;
pub use aether::nexus_wire::{
    NexusCommand, NexusOpcode, NexusReply,
    NexusStatus, NexusTelemetry, WireError,
};
use aether::resonance_plane::ResonancePlane;

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
    UnexpectedReply {
        expected: u64,
        observed: u64,
    },
    Wire(WireError),
    Kernel(NexusStatus),
}

pub struct ResonancePlaneClient {
    plane: &'static ResonancePlane,
    capability: u64,
    next_sequence: u64,
    pending: Option<u64>,
}

impl ResonancePlaneClient {
    /// # Safety
    ///
    /// `address` must be the process's mapped ResonancePlane page and remain
    /// mapped for the process lifetime.
    pub unsafe fn from_address(
        address: usize,
        capability: u64,
    ) -> Result<Self, PlaneClientError> {
        if address == 0 {
            return Err(PlaneClientError::NullAddress);
        }

        if address % core::mem::align_of::<ResonancePlane>() != 0 {
            return Err(PlaneClientError::MisalignedAddress);
        }

        // SAFETY: Established by the caller's mapping contract.
        let plane = unsafe { &*(address as *const ResonancePlane) };

        if !plane.is_compatible() {
            return Err(PlaneClientError::IncompatiblePlane);
        }

        Ok(Self {
            plane,
            capability,
            next_sequence: 1,
            pending: None,
        })
    }

    pub fn telemetry(&self) -> Option<NexusTelemetry> {
        self.plane.telemetry(8)
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
        self.next_sequence =
            self.next_sequence.wrapping_add(1).max(1);

        let command = NexusCommand::new(
            opcode,
            sequence,
            self.capability,
            arguments,
        );

        self.plane
            .submit_command(command)
            .map_err(PlaneClientError::Queue)?;

        self.pending = Some(sequence);

        Ok(PendingCommand { sequence })
    }

    pub fn poll_reply(
        &mut self,
    ) -> Result<Option<NexusReply>, PlaneClientError> {
        let Some(expected) = self.pending else {
            return Ok(None);
        };

        let reply = match self.plane.take_reply() {
            Ok(reply) => reply,
            Err(QueueError::Empty) => return Ok(None),
            Err(error) => {
                return Err(PlaneClientError::Queue(error));
            }
        };

        if reply.sequence != expected {
            return Err(PlaneClientError::UnexpectedReply {
                expected,
                observed: reply.sequence,
            });
        }

        let status = reply
            .validate(expected)
            .map_err(PlaneClientError::Wire)?;

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
        self.plane.dropped_commands()
    }

    pub fn dropped_replies(&self) -> u64 {
        self.plane.dropped_replies()
    }
}
