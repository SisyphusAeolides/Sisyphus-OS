use slope::quantum_crest::{
    DESKTOP_RIGHT_CAPTURE, DESKTOP_RIGHT_FOCUS, DESKTOP_RIGHT_OBSERVE, DESKTOP_RIGHT_PRESENT,
    DESKTOP_RIGHT_RECOVER, OPCODE_ACKNOWLEDGE_PLAN, OPCODE_CAPTURE_CHECKPOINT, OPCODE_PRESENT,
    OPCODE_QUERY_OBJECT, OPCODE_RELEASE_SURFACE, OPCODE_REQUEST_RECOVERY, OPCODE_SET_FOCUS,
    QuantumCrestPage, QuantumDesktopCommand, QuantumDesktopReply, QuantumFrameCertificate,
    QuantumPortalError, QuantumSystemSnapshot, command_root, frame_root,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PortalClientError {
    InvalidSecret,
    NoSnapshot,
    CorruptSnapshot,
    SnapshotRegression,
    SessionChanged,
    CapabilityRevoked,
    Busy,
    NoPendingCommand,
    ReplyMismatch,
    CorruptReply,
    Portal(QuantumPortalError),
}

impl From<QuantumPortalError> for PortalClientError {
    fn from(error: QuantumPortalError) -> Self {
        match error {
            QuantumPortalError::Busy => Self::Busy,
            other => Self::Portal(other),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresentCommand {
    pub framebuffer_object: u64,
    pub frame_sequence: u64,
    pub snapshot_sequence: u64,
    pub scene_epoch: u64,
    pub damage_root: u64,
    pub scene_root: u64,
    pub rendered_tiles: u32,
    pub skipped_tiles: u32,
    pub lane_votes: u8,
    pub deadline_tick: u64,
    pub flags: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PortalSession {
    pub snapshot: QuantumSystemSnapshot,
    pub command_sequence: u64,
    pub pending_sequence: Option<u64>,
}

pub struct QuantumPortalClient<'a> {
    page: &'a QuantumCrestPage,
    snapshot_secret: u64,
    command_secret: u64,
    snapshot: Option<QuantumSystemSnapshot>,
    last_snapshot_sequence: u64,
    next_command_sequence: u64,
    pending_sequence: Option<u64>,
}

impl<'a> QuantumPortalClient<'a> {
    pub fn new(
        page: &'a QuantumCrestPage,
        snapshot_secret: u64,
        command_secret: u64,
    ) -> Result<Self, PortalClientError> {
        if snapshot_secret == 0 || command_secret == 0 || snapshot_secret == command_secret {
            return Err(PortalClientError::InvalidSecret);
        }

        Ok(Self {
            page,
            snapshot_secret,
            command_secret,
            snapshot: None,
            last_snapshot_sequence: 0,
            next_command_sequence: 1,
            pending_sequence: None,
        })
    }

    pub fn synchronize(&mut self) -> Result<QuantumSystemSnapshot, PortalClientError> {
        let Some(snapshot) = self.page.user_read_snapshot()? else {
            return self.snapshot.ok_or(PortalClientError::NoSnapshot);
        };

        if !snapshot.verify(self.snapshot_secret) {
            return Err(PortalClientError::CorruptSnapshot);
        }
        if snapshot.sequence < self.last_snapshot_sequence {
            return Err(PortalClientError::SnapshotRegression);
        }

        if let Some(previous) = self.snapshot {
            if snapshot.desktop_session != previous.desktop_session
                || snapshot.desktop_generation != previous.desktop_generation
            {
                self.pending_sequence = None;
                self.next_command_sequence = 1;
            }
        }

        self.last_snapshot_sequence = snapshot.sequence;
        self.snapshot = Some(snapshot);
        Ok(snapshot)
    }

    pub const fn snapshot(&self) -> Option<QuantumSystemSnapshot> {
        self.snapshot
    }

    pub fn session(&self) -> Result<PortalSession, PortalClientError> {
        Ok(PortalSession {
            snapshot: self.snapshot.ok_or(PortalClientError::NoSnapshot)?,
            command_sequence: self.next_command_sequence,
            pending_sequence: self.pending_sequence,
        })
    }

    pub fn submit_present(&mut self, present: PresentCommand) -> Result<u64, PortalClientError> {
        self.require_right(DESKTOP_RIGHT_PRESENT)?;

        let packed_tiles =
            (u64::from(present.rendered_tiles) << 32) | u64::from(present.skipped_tiles);

        let mut command = self.base_command(
            OPCODE_PRESENT,
            present.framebuffer_object,
            present.deadline_tick,
        )?;
        command.flags = present.flags;
        command.arguments[0] = present.frame_sequence;
        command.arguments[1] = present.snapshot_sequence;
        command.arguments[2] = present.scene_epoch;
        command.arguments[3] = present.damage_root;
        command.arguments[4] = present.scene_root;
        command.arguments[5] = packed_tiles;
        command.arguments[6] = u64::from(present.lane_votes);

        self.submit(command)
    }

    pub fn acknowledge_plan(
        &mut self,
        plan_root: u64,
        deadline_tick: u64,
    ) -> Result<u64, PortalClientError> {
        self.require_right(DESKTOP_RIGHT_OBSERVE)?;
        let mut command = self.base_command(OPCODE_ACKNOWLEDGE_PLAN, 0, deadline_tick)?;
        command.arguments[0] = plan_root;
        self.submit(command)
    }

    pub fn capture_checkpoint(
        &mut self,
        object: u64,
        scene_epoch: u64,
        scene_root: u64,
        deadline_tick: u64,
    ) -> Result<u64, PortalClientError> {
        self.require_right(DESKTOP_RIGHT_CAPTURE)?;
        let mut command = self.base_command(OPCODE_CAPTURE_CHECKPOINT, object, deadline_tick)?;
        command.arguments[0] = scene_epoch;
        command.arguments[1] = scene_root;
        self.submit(command)
    }

    pub fn request_recovery(
        &mut self,
        object: u64,
        plan_root: u64,
        recovery_mode: u32,
        deadline_tick: u64,
    ) -> Result<u64, PortalClientError> {
        self.require_right(DESKTOP_RIGHT_RECOVER)?;
        let mut command = self.base_command(OPCODE_REQUEST_RECOVERY, object, deadline_tick)?;
        command.arguments[0] = plan_root;
        command.arguments[1] = u64::from(recovery_mode);
        self.submit(command)
    }

    pub fn set_focus(
        &mut self,
        surface: u64,
        focus_generation: u32,
        deadline_tick: u64,
    ) -> Result<u64, PortalClientError> {
        self.require_right(DESKTOP_RIGHT_FOCUS)?;
        let mut command = self.base_command(OPCODE_SET_FOCUS, surface, deadline_tick)?;
        command.arguments[0] = u64::from(focus_generation);
        self.submit(command)
    }

    pub fn release_surface(
        &mut self,
        surface: u64,
        deadline_tick: u64,
    ) -> Result<u64, PortalClientError> {
        let command = self.base_command(OPCODE_RELEASE_SURFACE, surface, deadline_tick)?;
        self.submit(command)
    }

    pub fn query_object(
        &mut self,
        object: u64,
        selector: u64,
        deadline_tick: u64,
    ) -> Result<u64, PortalClientError> {
        self.require_right(DESKTOP_RIGHT_OBSERVE)?;
        let mut command = self.base_command(OPCODE_QUERY_OBJECT, object, deadline_tick)?;
        command.arguments[0] = selector;
        self.submit(command)
    }

    pub fn take_reply(&mut self) -> Result<Option<QuantumDesktopReply>, PortalClientError> {
        let Some(reply) = self.page.user_take_reply()? else {
            return Ok(None);
        };

        if !reply.verify(self.command_secret) {
            return Err(PortalClientError::CorruptReply);
        }

        let pending = self
            .pending_sequence
            .ok_or(PortalClientError::NoPendingCommand)?;
        if reply.command_sequence != pending {
            return Err(PortalClientError::ReplyMismatch);
        }
        if reply.snapshot_sequence < self.last_snapshot_sequence.saturating_sub(1) {
            return Err(PortalClientError::ReplyMismatch);
        }
        if reply.frame.frame_sequence != 0
            && reply.frame.root != frame_root(self.command_secret, &reply.frame)
        {
            return Err(PortalClientError::CorruptReply);
        }

        self.pending_sequence = None;
        Ok(Some(reply))
    }

    fn base_command(
        &mut self,
        opcode: u32,
        object: u64,
        deadline_tick: u64,
    ) -> Result<QuantumDesktopCommand, PortalClientError> {
        if self.pending_sequence.is_some() {
            return Err(PortalClientError::Busy);
        }

        let snapshot = self.snapshot.ok_or(PortalClientError::NoSnapshot)?;
        let sequence = self.next_command_sequence;
        self.next_command_sequence = self.next_command_sequence.wrapping_add(1).max(1);

        let mut command = QuantumDesktopCommand::empty();
        command.sequence = sequence;
        command.epoch = snapshot.epoch;
        command.session = snapshot.desktop_session;
        command.generation = snapshot.desktop_generation;
        command.opcode = opcode;
        command.object = object;
        command.deadline_tick = deadline_tick;
        Ok(command)
    }

    fn submit(&mut self, mut command: QuantumDesktopCommand) -> Result<u64, PortalClientError> {
        command.seal(self.command_secret);
        if command.root != command_root(self.command_secret, &command) {
            return Err(PortalClientError::CorruptReply);
        }

        self.page.user_submit_command(command)?;
        self.pending_sequence = Some(command.sequence);
        Ok(command.sequence)
    }

    fn require_right(&self, right: u64) -> Result<(), PortalClientError> {
        let snapshot = self.snapshot.ok_or(PortalClientError::NoSnapshot)?;
        if snapshot.desktop_rights & right != right {
            return Err(PortalClientError::CapabilityRevoked);
        }
        Ok(())
    }
}
