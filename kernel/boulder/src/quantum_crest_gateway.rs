use slope::quantum_crest::{
    DESKTOP_RIGHT_ADMINISTER, DESKTOP_RIGHT_CAPTURE, DESKTOP_RIGHT_FOCUS, DESKTOP_RIGHT_OBSERVE,
    DESKTOP_RIGHT_PRESENT, DESKTOP_RIGHT_RECOVER, OPCODE_ACKNOWLEDGE_PLAN,
    OPCODE_CAPTURE_CHECKPOINT, OPCODE_PRESENT, OPCODE_QUERY_OBJECT, OPCODE_RELEASE_SURFACE,
    OPCODE_REQUEST_RECOVERY, OPCODE_SET_FOCUS, QuantumBlackLabState, QuantumCrestPage,
    QuantumDesktopCommand, QuantumDesktopReply, QuantumDisplayState, QuantumFrameCertificate,
    QuantumGpuState, QuantumPortalError, QuantumSystemSnapshot, SNAPSHOT_FLAG_BLACKLAB_DEGRADED,
    SNAPSHOT_FLAG_DISPLAY_ONLINE, SNAPSHOT_FLAG_DMA_REVOKED, SNAPSHOT_FLAG_HERMES_ONLINE,
    SNAPSHOT_FLAG_LEDGER_VERIFIED, SNAPSHOT_FLAG_QUARANTINE_ACTIVE, SNAPSHOT_FLAG_RECOVERY_PENDING,
    SNAPSHOT_FLAG_SAFE_MODE, STATUS_BACKEND, STATUS_BUSY, STATUS_CORRUPT, STATUS_DEADLINE,
    STATUS_DENIED, STATUS_INVALID, STATUS_OK, STATUS_STALE, STATUS_UNSUPPORTED, frame_root,
};

use crate::argus_sentinel::{ArgusAction, ArgusSeverity};
use crate::blacklab_control_plane::ControlOutcome;
use crate::capability::{Capability, DeviceMemoryControl, FaultPolicyControl, PolicyControl};
use crate::mnemosyne_ledger::LedgerSeal;
use crate::sync::SpinLock;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HermesDesktopSignal {
    pub online: bool,
    pub vendor_id: u16,
    pub device_id: u16,
    pub revision: u8,
    pub wire_major: u8,
    pub wire_minor: u16,
    pub epoch: u32,
    pub negotiated_features: u64,
    pub firmware_version: u64,
    pub temperature_q16: i32,
    pub pressure_q16: i32,
    pub corrected_faults: u32,
    pub fatal_faults: u32,
    pub dma_revoked: bool,
}

impl HermesDesktopSignal {
    pub const OFFLINE: Self = Self {
        online: false,
        vendor_id: 0,
        device_id: 0,
        revision: 0,
        wire_major: 0,
        wire_minor: 0,
        epoch: 0,
        negotiated_features: 0,
        firmware_version: 0,
        temperature_q16: 0,
        pressure_q16: 0,
        corrected_faults: 0,
        fatal_faults: 0,
        dma_revoked: false,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplayDesktopSignal {
    pub online: bool,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub format: u32,
    pub refresh_millihertz: u32,
    pub beam_position: u32,
    pub present_sequence: u64,
    pub frame_budget_ticks: u64,
    pub predicted_render_ticks: u64,
    pub damage_tiles: u32,
    pub total_tiles: u32,
}

impl DisplayDesktopSignal {
    pub const OFFLINE: Self = Self {
        online: false,
        width: 0,
        height: 0,
        pitch: 0,
        format: 0,
        refresh_millihertz: 0,
        beam_position: 0,
        present_sequence: 0,
        frame_budget_ticks: 0,
        predicted_render_ticks: 0,
        damage_tiles: 0,
        total_tiles: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuantumKernelSignal {
    pub logical_tick: u64,
    pub topology_epoch: u64,
    pub policy_epoch: u64,
    pub capability_epoch: u64,
    pub hermes: HermesDesktopSignal,
    pub display: DisplayDesktopSignal,
    pub safe_mode: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresentIntent {
    pub command_sequence: u64,
    pub framebuffer_object: u64,
    pub certificate: QuantumFrameCertificate,
    pub deadline_tick: u64,
    pub flags: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresentReceipt {
    pub present_sequence: u64,
    pub present_tick: u64,
    pub beam_position: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CheckpointIntent {
    pub command_sequence: u64,
    pub object: u64,
    pub scene_epoch: u64,
    pub scene_root: u64,
    pub deadline_tick: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CheckpointReceipt {
    pub checkpoint_handle: u64,
    pub checkpoint_generation: u64,
    pub state_root: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecoveryIntent {
    pub command_sequence: u64,
    pub object: u64,
    pub plan_root: u64,
    pub recovery_mode: u32,
    pub deadline_tick: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecoveryReceipt {
    pub recovery_epoch: u64,
    pub state_root: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FocusIntent {
    pub command_sequence: u64,
    pub surface: u64,
    pub focus_generation: u32,
    pub deadline_tick: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObjectQuery {
    pub object: u64,
    pub selector: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObjectReply {
    pub kind: u64,
    pub rights: u64,
    pub generation: u64,
    pub state_root: u64,
}

pub trait QuantumCrestPlatform {
    fn present(&mut self, intent: PresentIntent) -> Result<PresentReceipt, i32>;

    fn capture_checkpoint(&mut self, intent: CheckpointIntent) -> Result<CheckpointReceipt, i32>;

    fn request_recovery(&mut self, intent: RecoveryIntent) -> Result<RecoveryReceipt, i32>;

    fn set_focus(&mut self, intent: FocusIntent) -> Result<u64, i32>;

    fn release_surface(&mut self, surface: u64) -> Result<(), i32>;

    fn query_object(&mut self, query: ObjectQuery) -> Result<ObjectReply, i32>;

    fn acknowledge_plan(&mut self, plan_root: u64, command_sequence: u64) -> Result<u64, i32>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuantumGatewayError {
    InvalidSecret,
    InvalidSession,
    InvalidSignal,
    InvalidRights,
    Portal(QuantumPortalError),
}

impl From<QuantumPortalError> for QuantumGatewayError {
    fn from(error: QuantumPortalError) -> Self {
        Self::Portal(error)
    }
}

struct GatewayState {
    epoch: u64,
    snapshot_sequence: u64,
    last_command_sequence: u64,
    session: u64,
    generation: u32,
    rights: u64,
    topology_epoch: u64,
    capability_epoch: u64,
    latest_plan_root: u64,
    latest_scene_root: u64,
    latest_frame_sequence: u64,
}

impl GatewayState {
    const fn new(session: u64, generation: u32, rights: u64) -> Self {
        Self {
            epoch: 1,
            snapshot_sequence: 0,
            last_command_sequence: 0,
            session,
            generation,
            rights,
            topology_epoch: 0,
            capability_epoch: 0,
            latest_plan_root: 0,
            latest_scene_root: 0,
            latest_frame_sequence: 0,
        }
    }
}

pub struct QuantumCrestGateway<'a> {
    page: &'a QuantumCrestPage,
    snapshot_secret: u64,
    command_secret: u64,
    state: SpinLock<GatewayState>,
}

impl<'a> QuantumCrestGateway<'a> {
    pub fn new(
        page: &'a QuantumCrestPage,
        snapshot_secret: u64,
        command_secret: u64,
        session: u64,
        generation: u32,
        rights: u64,
    ) -> Result<Self, QuantumGatewayError> {
        if snapshot_secret == 0 || command_secret == 0 || snapshot_secret == command_secret {
            return Err(QuantumGatewayError::InvalidSecret);
        }

        if session == 0 || generation == 0 {
            return Err(QuantumGatewayError::InvalidSession);
        }

        if rights & DESKTOP_RIGHT_OBSERVE == 0 {
            return Err(QuantumGatewayError::InvalidRights);
        }

        Ok(Self {
            page,
            snapshot_secret,
            command_secret,
            state: SpinLock::new(GatewayState::new(session, generation, rights)),
        })
    }

    pub fn publish_blacklab(
        &self,
        signal: QuantumKernelSignal,
        outcome: &ControlOutcome,
        ledger: LedgerSeal,
        _authority: &Capability<'_, FaultPolicyControl>,
    ) -> Result<QuantumSystemSnapshot, QuantumGatewayError> {
        if signal.logical_tick == 0 || signal.policy_epoch == 0 || ledger.epoch == 0 {
            return Err(QuantumGatewayError::InvalidSignal);
        }

        let mut state = self.state.lock();
        state.snapshot_sequence = state.snapshot_sequence.wrapping_add(1).max(1);
        state.epoch = state.epoch.max(signal.policy_epoch).max(1);
        state.topology_epoch = signal.topology_epoch;
        state.capability_epoch = signal.capability_epoch;
        state.latest_plan_root = outcome.plan.plan_root;

        let mut flags = 0_u64;
        if signal.hermes.online {
            flags |= SNAPSHOT_FLAG_HERMES_ONLINE;
        }
        if signal.display.online {
            flags |= SNAPSHOT_FLAG_DISPLAY_ONLINE;
        }
        if !matches!(outcome.assessment.severity, ArgusSeverity::Stable) {
            flags |= SNAPSHOT_FLAG_BLACKLAB_DEGRADED;
        }
        if matches!(
            outcome.assessment.action,
            ArgusAction::Quarantine
                | ArgusAction::RevokeDma
                | ArgusAction::ResetDevice
                | ArgusAction::RetireResource
        ) {
            flags |= SNAPSHOT_FLAG_QUARANTINE_ACTIVE;
        }
        if signal.hermes.dma_revoked || matches!(outcome.assessment.action, ArgusAction::RevokeDma)
        {
            flags |= SNAPSHOT_FLAG_DMA_REVOKED;
        }
        if matches!(
            outcome.assessment.action,
            ArgusAction::ResetDevice | ArgusAction::RetireResource
        ) {
            flags |= SNAPSHOT_FLAG_RECOVERY_PENDING;
        }
        if outcome.ledger_verified {
            flags |= SNAPSHOT_FLAG_LEDGER_VERIFIED;
        }
        if signal.safe_mode {
            flags |= SNAPSHOT_FLAG_SAFE_MODE;
        }

        let gpu = QuantumGpuState {
            vendor_id: signal.hermes.vendor_id,
            device_id: signal.hermes.device_id,
            revision: signal.hermes.revision,
            gsp_wire_major: signal.hermes.wire_major,
            gsp_wire_minor: signal.hermes.wire_minor,
            gsp_epoch: signal.hermes.epoch,
            negotiated_features: signal.hermes.negotiated_features,
            firmware_version: signal.hermes.firmware_version,
            temperature_q16: signal.hermes.temperature_q16,
            pressure_q16: signal.hermes.pressure_q16,
            corrected_faults: signal.hermes.corrected_faults,
            fatal_faults: signal.hermes.fatal_faults,
        };

        let blacklab = QuantumBlackLabState {
            policy_epoch: signal.policy_epoch,
            risk: outcome.assessment.risk,
            severity: severity_code(outcome.assessment.severity),
            action: action_code(outcome.assessment.action),
            temporal_violations: outcome.temporal.violation_count.min(u16::MAX as usize) as u16,
            evidence_votes: outcome.plan.votes,
            plan_steps: outcome.plan.step_count.min(u16::MAX as usize) as u16,
            required_quorum: outcome.plan.required_quorum,
            reserved0: 0,
            ledger_epoch: ledger.epoch,
            ledger_root: ledger.chain_root,
            evidence_root: outcome.plan.evidence_root,
            plan_root: outcome.plan.plan_root,
            forecast_tick: outcome.assessment.forecast_tick.unwrap_or(0),
        };

        let display = QuantumDisplayState {
            width: signal.display.width,
            height: signal.display.height,
            pitch: signal.display.pitch,
            format: signal.display.format,
            refresh_millihertz: signal.display.refresh_millihertz,
            beam_position: signal.display.beam_position,
            present_sequence: signal.display.present_sequence,
            frame_budget_ticks: signal.display.frame_budget_ticks,
            predicted_render_ticks: signal.display.predicted_render_ticks,
            damage_tiles: signal.display.damage_tiles,
            total_tiles: signal.display.total_tiles,
        };

        let mut snapshot = QuantumSystemSnapshot::empty();
        snapshot.sequence = state.snapshot_sequence;
        snapshot.epoch = state.epoch;
        snapshot.logical_tick = signal.logical_tick;
        snapshot.topology_epoch = signal.topology_epoch;
        snapshot.capability_epoch = signal.capability_epoch;
        snapshot.flags = flags;
        snapshot.gpu = gpu;
        snapshot.blacklab = blacklab;
        snapshot.display = display;
        snapshot.desktop_rights = state.rights;
        snapshot.desktop_session = state.session;
        snapshot.desktop_generation = state.generation;
        snapshot.seal(self.snapshot_secret);

        drop(state);
        self.page.kernel_publish_snapshot(snapshot)?;
        Ok(snapshot)
    }

    pub fn rotate_session(
        &self,
        session: u64,
        generation: u32,
        rights: u64,
        _authority: &Capability<'_, PolicyControl>,
    ) -> Result<(), QuantumGatewayError> {
        if session == 0 || generation == 0 || rights & DESKTOP_RIGHT_OBSERVE == 0 {
            return Err(QuantumGatewayError::InvalidSession);
        }

        let mut state = self.state.lock();
        state.session = session;
        state.generation = generation;
        state.rights = rights;
        state.epoch = state.epoch.wrapping_add(1).max(1);
        state.last_command_sequence = 0;
        state.latest_scene_root = 0;
        state.latest_frame_sequence = 0;
        Ok(())
    }

    pub fn service_one(
        &self,
        now_tick: u64,
        platform: &mut dyn QuantumCrestPlatform,
        _policy: &Capability<'_, PolicyControl>,
        _fault: &Capability<'_, FaultPolicyControl>,
        _device: &Capability<'_, DeviceMemoryControl>,
    ) -> Result<Option<QuantumDesktopReply>, QuantumGatewayError> {
        let Some(command) = self.page.kernel_take_command()? else {
            return Ok(None);
        };

        let reply = self.execute_command(now_tick, command, platform);
        self.page.kernel_publish_reply(reply)?;
        Ok(Some(reply))
    }

    fn execute_command(
        &self,
        now_tick: u64,
        command: QuantumDesktopCommand,
        platform: &mut dyn QuantumCrestPlatform,
    ) -> QuantumDesktopReply {
        let mut reply = QuantumDesktopReply::empty();
        reply.command_sequence = command.sequence;

        let snapshot_sequence = self.state.lock().snapshot_sequence;
        reply.snapshot_sequence = snapshot_sequence;

        let validation = self.validate_command(now_tick, &command);
        if let Err(status) = validation {
            reply.status = status;
            reply.seal(self.command_secret);
            return reply;
        }

        let result = match command.opcode {
            OPCODE_PRESENT => self.execute_present(command, now_tick, platform, &mut reply),
            OPCODE_ACKNOWLEDGE_PLAN => self.execute_acknowledge(command, platform, &mut reply),
            OPCODE_CAPTURE_CHECKPOINT => self.execute_checkpoint(command, platform, &mut reply),
            OPCODE_REQUEST_RECOVERY => self.execute_recovery(command, platform, &mut reply),
            OPCODE_SET_FOCUS => self.execute_focus(command, platform, &mut reply),
            OPCODE_RELEASE_SURFACE => self.execute_release(command, platform, &mut reply),
            OPCODE_QUERY_OBJECT => self.execute_query(command, platform, &mut reply),
            _ => Err(STATUS_UNSUPPORTED),
        };

        reply.status = result.unwrap_or_else(|status| status);
        reply.seal(self.command_secret);
        reply
    }

    fn validate_command(&self, now_tick: u64, command: &QuantumDesktopCommand) -> Result<(), i32> {
        if !command.verify(self.command_secret) {
            return Err(STATUS_CORRUPT);
        }

        let required = required_right(command.opcode).ok_or(STATUS_UNSUPPORTED)?;
        let mut state = self.state.lock();

        if command.epoch != state.epoch
            || command.session != state.session
            || command.generation != state.generation
        {
            return Err(STATUS_STALE);
        }

        if command.sequence <= state.last_command_sequence {
            return Err(STATUS_STALE);
        }

        if command.deadline_tick != 0 && now_tick >= command.deadline_tick {
            return Err(STATUS_DEADLINE);
        }

        if state.rights & required != required {
            return Err(STATUS_DENIED);
        }

        state.last_command_sequence = command.sequence;
        Ok(())
    }

    fn execute_present(
        &self,
        command: QuantumDesktopCommand,
        now_tick: u64,
        platform: &mut dyn QuantumCrestPlatform,
        reply: &mut QuantumDesktopReply,
    ) -> Result<i32, i32> {
        let rendered_tiles = (command.arguments[5] >> 32) as u32;
        let skipped_tiles = command.arguments[5] as u32;
        let lane_votes = command.arguments[6] as u8;

        if command.arguments[0] == 0
            || command.arguments[1] == 0
            || command.arguments[2] == 0
            || command.arguments[3] == 0
            || command.arguments[4] == 0
            || lane_votes == 0
        {
            return Err(STATUS_INVALID);
        }

        let mut certificate = QuantumFrameCertificate {
            frame_sequence: command.arguments[0],
            snapshot_sequence: command.arguments[1],
            scene_epoch: command.arguments[2],
            damage_root: command.arguments[3],
            scene_root: command.arguments[4],
            present_tick: now_tick,
            rendered_tiles,
            skipped_tiles,
            lane_votes,
            reserved: [0; 7],
            root: 0,
        };
        certificate.root = frame_root(self.command_secret, &certificate);

        {
            let state = self.state.lock();
            if certificate.snapshot_sequence > state.snapshot_sequence
                || certificate.frame_sequence <= state.latest_frame_sequence
            {
                return Err(STATUS_STALE);
            }
        }

        let receipt = platform
            .present(PresentIntent {
                command_sequence: command.sequence,
                framebuffer_object: command.object,
                certificate,
                deadline_tick: command.deadline_tick,
                flags: command.flags,
            })
            .map_err(normalize_backend_status)?;

        {
            let mut state = self.state.lock();
            state.latest_frame_sequence = certificate.frame_sequence;
            state.latest_scene_root = certificate.scene_root;
        }

        reply.outputs[0] = receipt.present_sequence;
        reply.outputs[1] = receipt.present_tick;
        reply.outputs[2] = u64::from(receipt.beam_position);
        reply.frame = certificate;
        Ok(STATUS_OK)
    }

    fn execute_acknowledge(
        &self,
        command: QuantumDesktopCommand,
        platform: &mut dyn QuantumCrestPlatform,
        reply: &mut QuantumDesktopReply,
    ) -> Result<i32, i32> {
        let plan_root = command.arguments[0];
        let expected = self.state.lock().latest_plan_root;
        if plan_root == 0 || plan_root != expected {
            return Err(STATUS_STALE);
        }

        let acknowledgement = platform
            .acknowledge_plan(plan_root, command.sequence)
            .map_err(normalize_backend_status)?;
        reply.outputs[0] = acknowledgement;
        Ok(STATUS_OK)
    }

    fn execute_checkpoint(
        &self,
        command: QuantumDesktopCommand,
        platform: &mut dyn QuantumCrestPlatform,
        reply: &mut QuantumDesktopReply,
    ) -> Result<i32, i32> {
        let scene_epoch = command.arguments[0];
        let scene_root = command.arguments[1];
        if scene_epoch == 0 || scene_root == 0 {
            return Err(STATUS_INVALID);
        }

        let receipt = platform
            .capture_checkpoint(CheckpointIntent {
                command_sequence: command.sequence,
                object: command.object,
                scene_epoch,
                scene_root,
                deadline_tick: command.deadline_tick,
            })
            .map_err(normalize_backend_status)?;

        reply.outputs[0] = receipt.checkpoint_handle;
        reply.outputs[1] = receipt.checkpoint_generation;
        reply.outputs[2] = receipt.state_root;
        Ok(STATUS_OK)
    }

    fn execute_recovery(
        &self,
        command: QuantumDesktopCommand,
        platform: &mut dyn QuantumCrestPlatform,
        reply: &mut QuantumDesktopReply,
    ) -> Result<i32, i32> {
        let plan_root = command.arguments[0];
        let recovery_mode = command.arguments[1] as u32;
        let expected = self.state.lock().latest_plan_root;

        if plan_root == 0 || plan_root != expected || recovery_mode == 0 {
            return Err(STATUS_STALE);
        }

        let receipt = platform
            .request_recovery(RecoveryIntent {
                command_sequence: command.sequence,
                object: command.object,
                plan_root,
                recovery_mode,
                deadline_tick: command.deadline_tick,
            })
            .map_err(normalize_backend_status)?;

        reply.outputs[0] = receipt.recovery_epoch;
        reply.outputs[1] = receipt.state_root;
        Ok(STATUS_OK)
    }

    fn execute_focus(
        &self,
        command: QuantumDesktopCommand,
        platform: &mut dyn QuantumCrestPlatform,
        reply: &mut QuantumDesktopReply,
    ) -> Result<i32, i32> {
        if command.object == 0 || command.arguments[0] == 0 {
            return Err(STATUS_INVALID);
        }

        let focus_root = platform
            .set_focus(FocusIntent {
                command_sequence: command.sequence,
                surface: command.object,
                focus_generation: command.arguments[0] as u32,
                deadline_tick: command.deadline_tick,
            })
            .map_err(normalize_backend_status)?;

        reply.outputs[0] = focus_root;
        Ok(STATUS_OK)
    }

    fn execute_release(
        &self,
        command: QuantumDesktopCommand,
        platform: &mut dyn QuantumCrestPlatform,
        _reply: &mut QuantumDesktopReply,
    ) -> Result<i32, i32> {
        if command.object == 0 {
            return Err(STATUS_INVALID);
        }

        platform
            .release_surface(command.object)
            .map_err(normalize_backend_status)?;
        Ok(STATUS_OK)
    }

    fn execute_query(
        &self,
        command: QuantumDesktopCommand,
        platform: &mut dyn QuantumCrestPlatform,
        reply: &mut QuantumDesktopReply,
    ) -> Result<i32, i32> {
        if command.object == 0 {
            return Err(STATUS_INVALID);
        }

        let object = platform
            .query_object(ObjectQuery {
                object: command.object,
                selector: command.arguments[0],
            })
            .map_err(normalize_backend_status)?;

        reply.outputs[0] = object.kind;
        reply.outputs[1] = object.rights;
        reply.outputs[2] = object.generation;
        reply.outputs[3] = object.state_root;
        Ok(STATUS_OK)
    }

    pub fn page(&self) -> &'a QuantumCrestPage {
        self.page
    }
}

fn required_right(opcode: u32) -> Option<u64> {
    match opcode {
        OPCODE_PRESENT => Some(DESKTOP_RIGHT_PRESENT),
        OPCODE_ACKNOWLEDGE_PLAN => Some(DESKTOP_RIGHT_OBSERVE),
        OPCODE_CAPTURE_CHECKPOINT => Some(DESKTOP_RIGHT_CAPTURE),
        OPCODE_REQUEST_RECOVERY => Some(DESKTOP_RIGHT_RECOVER),
        OPCODE_SET_FOCUS => Some(DESKTOP_RIGHT_FOCUS),
        OPCODE_RELEASE_SURFACE => Some(DESKTOP_RIGHT_ADMINISTER),
        OPCODE_QUERY_OBJECT => Some(DESKTOP_RIGHT_OBSERVE),
        _ => None,
    }
}

fn severity_code(severity: ArgusSeverity) -> u8 {
    match severity {
        ArgusSeverity::Stable => 0,
        ArgusSeverity::Watch => 1,
        ArgusSeverity::Degraded => 2,
        ArgusSeverity::Critical => 3,
        ArgusSeverity::Terminal => 4,
    }
}

fn action_code(action: ArgusAction) -> u8 {
    match action {
        ArgusAction::Observe => 0,
        ArgusAction::IncreaseSampling => 1,
        ArgusAction::Quarantine => 2,
        ArgusAction::RevokeDma => 3,
        ArgusAction::ResetDevice => 4,
        ArgusAction::RetireResource => 5,
    }
}

fn normalize_backend_status(status: i32) -> i32 {
    match status {
        STATUS_OK | STATUS_INVALID | STATUS_DENIED | STATUS_STALE | STATUS_BUSY
        | STATUS_UNSUPPORTED | STATUS_CORRUPT | STATUS_BACKEND | STATUS_DEADLINE => status,
        _ => STATUS_BACKEND,
    }
}
