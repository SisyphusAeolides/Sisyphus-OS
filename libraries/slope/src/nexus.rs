use aether::grimoire;
use aether::nexus_wire::{
    NexusCommand, NexusOpcode, NexusReply, NexusStatus,
    NexusTelemetry, WireError,
};

use crate::{SyscallError, syscall};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NexusClientError {
    Syscall(SyscallError),
    Wire(WireError),
    Kernel(NexusStatus),
}

pub struct NexusClient {
    capability: u64,
    next_sequence: u64,
}

impl NexusClient {
    pub const fn new(capability: u64) -> Self {
        Self {
            capability,
            next_sequence: 1,
        }
    }

    pub fn transact(
        &mut self,
        opcode: NexusOpcode,
        arguments: [u64; 4],
    ) -> Result<NexusReply, NexusClientError> {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1).max(1);

        let command = NexusCommand::new(
            opcode,
            sequence,
            self.capability,
            arguments,
        );

        let mut reply = NexusReply::ZERO;

        // SAFETY: Both pointers reference complete stack-owned 64-byte wire
        // objects for the synchronous duration of the syscall.
        unsafe {
            syscall(
                grimoire::SYS_NEXUS_CONTROL,
                [
                    (&command as *const NexusCommand) as usize,
                    (&mut reply as *mut NexusReply) as usize,
                    core::mem::size_of::<NexusCommand>(),
                    core::mem::size_of::<NexusReply>(),
                    0,
                    0,
                ],
            )
        }
        .map_err(NexusClientError::Syscall)?;

        let status = reply
            .validate(sequence)
            .map_err(NexusClientError::Wire)?;

        if status != NexusStatus::Ok {
            return Err(NexusClientError::Kernel(status));
        }

        Ok(reply)
    }

    pub fn telemetry(
        &mut self,
    ) -> Result<NexusTelemetry, NexusClientError> {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1).max(1);

        let mut telemetry = NexusTelemetry::ZERO;

        // SAFETY: The pointer references a complete writable telemetry frame
        // for the synchronous duration of the syscall.
        unsafe {
            syscall(
                grimoire::SYS_NEXUS_TELEMETRY,
                [
                    (&mut telemetry as *mut NexusTelemetry) as usize,
                    core::mem::size_of::<NexusTelemetry>(),
                    sequence as usize,
                    0,
                    0,
                    0,
                ],
            )
        }
        .map_err(NexusClientError::Syscall)?;

        telemetry
            .validate()
            .map_err(NexusClientError::Wire)?;

        Ok(telemetry)
    }

    pub fn query_stats(
        &mut self,
    ) -> Result<NexusReply, NexusClientError> {
        self.transact(NexusOpcode::QueryStats, [0; 4])
    }

    pub fn attach_task(
        &mut self,
        task: u64,
        packed_hint: u64,
    ) -> Result<NexusReply, NexusClientError> {
        self.transact(
            NexusOpcode::AttachTask,
            [task, packed_hint, 0, 0],
        )
    }

    pub fn entangle(
        &mut self,
        task_a: u64,
        task_b: u64,
        phase_bin: u16,
        flags: u32,
        re_q16: i32,
        im_q16: i32,
    ) -> Result<NexusReply, NexusClientError> {
        let phase_and_flags =
            u64::from(phase_bin) | (u64::from(flags) << 32);

        let amplitude =
            u64::from(re_q16 as u32)
                | (u64::from(im_q16 as u32) << 32);

        self.transact(
            NexusOpcode::Entangle,
            [task_a, task_b, phase_and_flags, amplitude],
        )
    }

    pub fn set_collapse_threshold(
        &mut self,
        threshold: u64,
    ) -> Result<NexusReply, NexusClientError> {
        self.transact(
            NexusOpcode::SetCollapseThreshold,
            [threshold, 0, 0, 0],
        )
    }

    pub fn set_priority_mass(
        &mut self,
        mass: u16,
    ) -> Result<NexusReply, NexusClientError> {
        self.transact(
            NexusOpcode::SetPriorityMass,
            [u64::from(mass), 0, 0, 0],
        )
    }

    pub fn offer_kairos(
        &mut self,
        pair_index: usize,
    ) -> Result<NexusReply, NexusClientError> {
        self.transact(
            NexusOpcode::OfferKairos,
            [pair_index as u64, 0, 0, 0],
        )
    }
}

pub const fn task_handle(slot: u16, generation: u16) -> u64 {
    (slot as u64) | ((generation as u64) << 16)
}
