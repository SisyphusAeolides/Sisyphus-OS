use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

pub const QUANTUM_CREST_ABI_MAJOR: u32 = 1;
pub const QUANTUM_CREST_ABI_MINOR: u32 = 0;
pub const QUANTUM_CREST_ABI_VERSION: u32 =
    (QUANTUM_CREST_ABI_MAJOR << 16) | QUANTUM_CREST_ABI_MINOR;

pub const QUANTUM_CREST_PAYLOAD_BYTES: usize = 128;
pub const QUANTUM_CREST_ARGUMENTS: usize = 8;

pub const SNAPSHOT_FLAG_HERMES_ONLINE: u64 = 1 << 0;
pub const SNAPSHOT_FLAG_DISPLAY_ONLINE: u64 = 1 << 1;
pub const SNAPSHOT_FLAG_BLACKLAB_DEGRADED: u64 = 1 << 2;
pub const SNAPSHOT_FLAG_QUARANTINE_ACTIVE: u64 = 1 << 3;
pub const SNAPSHOT_FLAG_DMA_REVOKED: u64 = 1 << 4;
pub const SNAPSHOT_FLAG_RECOVERY_PENDING: u64 = 1 << 5;
pub const SNAPSHOT_FLAG_LEDGER_VERIFIED: u64 = 1 << 6;
pub const SNAPSHOT_FLAG_FRAME_DEADLINE_AT_RISK: u64 = 1 << 7;
pub const SNAPSHOT_FLAG_INPUT_PREDICTION_AVAILABLE: u64 = 1 << 8;
pub const SNAPSHOT_FLAG_SAFE_MODE: u64 = 1 << 9;

pub const DESKTOP_RIGHT_OBSERVE: u64 = 1 << 0;
pub const DESKTOP_RIGHT_PRESENT: u64 = 1 << 1;
pub const DESKTOP_RIGHT_FOCUS: u64 = 1 << 2;
pub const DESKTOP_RIGHT_CAPTURE: u64 = 1 << 3;
pub const DESKTOP_RIGHT_RECOVER: u64 = 1 << 4;
pub const DESKTOP_RIGHT_ADMINISTER: u64 = 1 << 5;

pub const OPCODE_PRESENT: u32 = 1;
pub const OPCODE_ACKNOWLEDGE_PLAN: u32 = 2;
pub const OPCODE_CAPTURE_CHECKPOINT: u32 = 3;
pub const OPCODE_REQUEST_RECOVERY: u32 = 4;
pub const OPCODE_SET_FOCUS: u32 = 5;
pub const OPCODE_RELEASE_SURFACE: u32 = 6;
pub const OPCODE_QUERY_OBJECT: u32 = 7;

pub const COMMAND_FLAG_PRESENT_CERTIFICATE_ONLY: u64 = 1 << 0;
pub const COMMAND_FLAG_PRESENT_ALLOW_TEARING: u64 = 1 << 1;
pub const COMMAND_FLAG_PRESENT_RECOVERY_FRAME: u64 = 1 << 2;
pub const COMMAND_FLAG_PRESENT_SECURE_CONTENT: u64 = 1 << 3;

pub const STATUS_OK: i32 = 0;
pub const STATUS_INVALID: i32 = -1;
pub const STATUS_DENIED: i32 = -2;
pub const STATUS_STALE: i32 = -3;
pub const STATUS_BUSY: i32 = -4;
pub const STATUS_UNSUPPORTED: i32 = -5;
pub const STATUS_CORRUPT: i32 = -6;
pub const STATUS_BACKEND: i32 = -7;
pub const STATUS_DEADLINE: i32 = -8;

const MAILBOX_FREE: u32 = 0;
const MAILBOX_WRITING: u32 = 1;
const MAILBOX_READY: u32 = 2;
const MAILBOX_READING: u32 = 3;

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuantumGpuState {
    pub vendor_id: u16,
    pub device_id: u16,
    pub revision: u8,
    pub gsp_wire_major: u8,
    pub gsp_wire_minor: u16,
    pub gsp_epoch: u32,
    pub negotiated_features: u64,
    pub firmware_version: u64,
    pub temperature_q16: i32,
    pub pressure_q16: i32,
    pub corrected_faults: u32,
    pub fatal_faults: u32,
}

impl QuantumGpuState {
    pub const ZERO: Self = Self {
        vendor_id: 0,
        device_id: 0,
        revision: 0,
        gsp_wire_major: 0,
        gsp_wire_minor: 0,
        gsp_epoch: 0,
        negotiated_features: 0,
        firmware_version: 0,
        temperature_q16: 0,
        pressure_q16: 0,
        corrected_faults: 0,
        fatal_faults: 0,
    };
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuantumBlackLabState {
    pub policy_epoch: u64,
    pub risk: u16,
    pub severity: u8,
    pub action: u8,
    pub temporal_violations: u16,
    pub evidence_votes: u16,
    pub plan_steps: u16,
    pub required_quorum: u8,
    pub reserved0: u8,
    pub ledger_epoch: u64,
    pub ledger_root: u64,
    pub evidence_root: u64,
    pub plan_root: u64,
    pub forecast_tick: u64,
}

impl QuantumBlackLabState {
    pub const ZERO: Self = Self {
        policy_epoch: 0,
        risk: 0,
        severity: 0,
        action: 0,
        temporal_violations: 0,
        evidence_votes: 0,
        plan_steps: 0,
        required_quorum: 0,
        reserved0: 0,
        ledger_epoch: 0,
        ledger_root: 0,
        evidence_root: 0,
        plan_root: 0,
        forecast_tick: 0,
    };
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuantumDisplayState {
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

impl QuantumDisplayState {
    pub const ZERO: Self = Self {
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

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuantumSystemSnapshot {
    pub abi_version: u32,
    pub struct_size: u32,
    pub sequence: u64,
    pub epoch: u64,
    pub logical_tick: u64,
    pub topology_epoch: u64,
    pub capability_epoch: u64,
    pub flags: u64,
    pub gpu: QuantumGpuState,
    pub blacklab: QuantumBlackLabState,
    pub display: QuantumDisplayState,
    pub desktop_rights: u64,
    pub desktop_session: u64,
    pub desktop_generation: u32,
    pub reserved0: u32,
    pub root: u64,
}

impl QuantumSystemSnapshot {
    pub const fn empty() -> Self {
        Self {
            abi_version: QUANTUM_CREST_ABI_VERSION,
            struct_size: core::mem::size_of::<Self>() as u32,
            sequence: 0,
            epoch: 0,
            logical_tick: 0,
            topology_epoch: 0,
            capability_epoch: 0,
            flags: 0,
            gpu: QuantumGpuState::ZERO,
            blacklab: QuantumBlackLabState::ZERO,
            display: QuantumDisplayState::ZERO,
            desktop_rights: 0,
            desktop_session: 0,
            desktop_generation: 0,
            reserved0: 0,
            root: 0,
        }
    }

    pub fn seal(&mut self, secret: u64) {
        self.root = 0;
        self.root = snapshot_root(secret, self);
    }

    pub fn verify(&self, secret: u64) -> bool {
        self.abi_version >> 16 == QUANTUM_CREST_ABI_MAJOR
            && self.struct_size as usize >= core::mem::size_of::<Self>()
            && self.sequence != 0
            && self.epoch != 0
            && self.desktop_session != 0
            && self.desktop_generation != 0
            && self.root == snapshot_root(secret, self)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuantumDesktopCommand {
    pub abi_version: u32,
    pub struct_size: u32,
    pub sequence: u64,
    pub epoch: u64,
    pub session: u64,
    pub generation: u32,
    pub opcode: u32,
    pub flags: u64,
    pub object: u64,
    pub deadline_tick: u64,
    pub arguments: [u64; QUANTUM_CREST_ARGUMENTS],
    pub payload_length: u16,
    pub reserved: [u8; 6],
    pub payload: [u8; QUANTUM_CREST_PAYLOAD_BYTES],
    pub root: u64,
}

impl QuantumDesktopCommand {
    pub const fn empty() -> Self {
        Self {
            abi_version: QUANTUM_CREST_ABI_VERSION,
            struct_size: core::mem::size_of::<Self>() as u32,
            sequence: 0,
            epoch: 0,
            session: 0,
            generation: 0,
            opcode: 0,
            flags: 0,
            object: 0,
            deadline_tick: 0,
            arguments: [0; QUANTUM_CREST_ARGUMENTS],
            payload_length: 0,
            reserved: [0; 6],
            payload: [0; QUANTUM_CREST_PAYLOAD_BYTES],
            root: 0,
        }
    }

    pub fn seal(&mut self, secret: u64) {
        self.root = 0;
        self.root = command_root(secret, self);
    }

    pub fn verify(&self, secret: u64) -> bool {
        self.abi_version >> 16 == QUANTUM_CREST_ABI_MAJOR
            && self.struct_size as usize >= core::mem::size_of::<Self>()
            && self.sequence != 0
            && self.epoch != 0
            && self.session != 0
            && self.generation != 0
            && self.opcode != 0
            && usize::from(self.payload_length) <= QUANTUM_CREST_PAYLOAD_BYTES
            && self.root == command_root(secret, self)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuantumFrameCertificate {
    pub frame_sequence: u64,
    pub snapshot_sequence: u64,
    pub scene_epoch: u64,
    pub damage_root: u64,
    pub scene_root: u64,
    pub present_tick: u64,
    pub rendered_tiles: u32,
    pub skipped_tiles: u32,
    pub lane_votes: u8,
    pub reserved: [u8; 7],
    pub root: u64,
}

impl QuantumFrameCertificate {
    pub const ZERO: Self = Self {
        frame_sequence: 0,
        snapshot_sequence: 0,
        scene_epoch: 0,
        damage_root: 0,
        scene_root: 0,
        present_tick: 0,
        rendered_tiles: 0,
        skipped_tiles: 0,
        lane_votes: 0,
        reserved: [0; 7],
        root: 0,
    };
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuantumDesktopReply {
    pub abi_version: u32,
    pub struct_size: u32,
    pub command_sequence: u64,
    pub snapshot_sequence: u64,
    pub status: i32,
    pub flags: u32,
    pub outputs: [u64; QUANTUM_CREST_ARGUMENTS],
    pub frame: QuantumFrameCertificate,
    pub root: u64,
}

impl QuantumDesktopReply {
    pub const fn empty() -> Self {
        Self {
            abi_version: QUANTUM_CREST_ABI_VERSION,
            struct_size: core::mem::size_of::<Self>() as u32,
            command_sequence: 0,
            snapshot_sequence: 0,
            status: STATUS_INVALID,
            flags: 0,
            outputs: [0; QUANTUM_CREST_ARGUMENTS],
            frame: QuantumFrameCertificate::ZERO,
            root: 0,
        }
    }

    pub fn seal(&mut self, secret: u64) {
        self.root = 0;
        self.root = reply_root(secret, self);
    }

    pub fn verify(&self, secret: u64) -> bool {
        self.abi_version >> 16 == QUANTUM_CREST_ABI_MAJOR
            && self.struct_size as usize >= core::mem::size_of::<Self>()
            && self.command_sequence != 0
            && self.root == reply_root(secret, self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuantumPortalError {
    Busy,
    Empty,
    Corrupt,
    Invalid,
}

#[repr(C)]
struct SnapshotCell {
    state: AtomicU32,
    value: UnsafeCell<QuantumSystemSnapshot>,
}

impl SnapshotCell {
    const fn new() -> Self {
        Self {
            state: AtomicU32::new(MAILBOX_FREE),
            value: UnsafeCell::new(QuantumSystemSnapshot::empty()),
        }
    }

    fn publish(&self, value: QuantumSystemSnapshot) -> Result<(), QuantumPortalError> {
        self.state
            .compare_exchange(
                MAILBOX_FREE,
                MAILBOX_WRITING,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .map_err(|_| QuantumPortalError::Busy)?;

        // SAFETY: MAILBOX_WRITING grants the kernel publisher exclusive access.
        unsafe {
            self.value.get().write(value);
        }

        self.state.store(MAILBOX_READY, Ordering::Release);
        Ok(())
    }

    fn take(&self) -> Result<Option<QuantumSystemSnapshot>, QuantumPortalError> {
        if self
            .state
            .compare_exchange(
                MAILBOX_READY,
                MAILBOX_READING,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_err()
        {
            return Ok(None);
        }

        // SAFETY: MAILBOX_READING grants the user consumer exclusive access.
        let value = unsafe { self.value.get().read() };
        self.state.store(MAILBOX_FREE, Ordering::Release);
        Ok(Some(value))
    }
}

// SAFETY: the mailbox state machine transfers exclusive access between the
// kernel publisher and user consumer.
unsafe impl Sync for SnapshotCell {}

#[repr(C)]
struct CommandMailbox {
    state: AtomicU32,
    value: UnsafeCell<QuantumDesktopCommand>,
}

impl CommandMailbox {
    const fn new() -> Self {
        Self {
            state: AtomicU32::new(MAILBOX_FREE),
            value: UnsafeCell::new(QuantumDesktopCommand::empty()),
        }
    }

    fn submit(&self, value: QuantumDesktopCommand) -> Result<(), QuantumPortalError> {
        self.state
            .compare_exchange(
                MAILBOX_FREE,
                MAILBOX_WRITING,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .map_err(|_| QuantumPortalError::Busy)?;
        // SAFETY: MAILBOX_WRITING grants this producer exclusive access.
        unsafe {
            self.value.get().write(value);
        }
        self.state.store(MAILBOX_READY, Ordering::Release);
        Ok(())
    }

    fn take(&self) -> Result<Option<QuantumDesktopCommand>, QuantumPortalError> {
        if self
            .state
            .compare_exchange(
                MAILBOX_READY,
                MAILBOX_READING,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_err()
        {
            return Ok(None);
        }
        // SAFETY: MAILBOX_READING grants this consumer exclusive access.
        let value = unsafe { self.value.get().read() };
        self.state.store(MAILBOX_FREE, Ordering::Release);
        Ok(Some(value))
    }
}

// SAFETY: the state machine transfers exclusive access between producer and
// consumer.
unsafe impl Sync for CommandMailbox {}

#[repr(C)]
struct ReplyMailbox {
    state: AtomicU32,
    value: UnsafeCell<QuantumDesktopReply>,
}

impl ReplyMailbox {
    const fn new() -> Self {
        Self {
            state: AtomicU32::new(MAILBOX_FREE),
            value: UnsafeCell::new(QuantumDesktopReply::empty()),
        }
    }

    fn publish(&self, value: QuantumDesktopReply) -> Result<(), QuantumPortalError> {
        self.state
            .compare_exchange(
                MAILBOX_FREE,
                MAILBOX_WRITING,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .map_err(|_| QuantumPortalError::Busy)?;
        // SAFETY: MAILBOX_WRITING grants this producer exclusive access.
        unsafe {
            self.value.get().write(value);
        }
        self.state.store(MAILBOX_READY, Ordering::Release);
        Ok(())
    }

    fn take(&self) -> Result<Option<QuantumDesktopReply>, QuantumPortalError> {
        if self
            .state
            .compare_exchange(
                MAILBOX_READY,
                MAILBOX_READING,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_err()
        {
            return Ok(None);
        }
        // SAFETY: MAILBOX_READING grants this consumer exclusive access.
        let value = unsafe { self.value.get().read() };
        self.state.store(MAILBOX_FREE, Ordering::Release);
        Ok(Some(value))
    }
}

// SAFETY: the state machine transfers exclusive access between producer and
// consumer.
unsafe impl Sync for ReplyMailbox {}

#[repr(C, align(4096))]
pub struct QuantumCrestPage {
    snapshot: SnapshotCell,
    command: CommandMailbox,
    reply: ReplyMailbox,
    kernel_epoch: AtomicU64,
    user_epoch: AtomicU64,
}

const _: () = assert!(core::mem::size_of::<QuantumCrestPage>() == 4096);
const _: () = assert!(core::mem::align_of::<QuantumCrestPage>() == 4096);

impl QuantumCrestPage {
    pub const fn new() -> Self {
        Self {
            snapshot: SnapshotCell::new(),
            command: CommandMailbox::new(),
            reply: ReplyMailbox::new(),
            kernel_epoch: AtomicU64::new(0),
            user_epoch: AtomicU64::new(0),
        }
    }

    pub fn kernel_publish_snapshot(
        &self,
        snapshot: QuantumSystemSnapshot,
    ) -> Result<(), QuantumPortalError> {
        if snapshot.epoch == 0 || snapshot.sequence == 0 {
            return Err(QuantumPortalError::Invalid);
        }
        self.kernel_epoch.store(snapshot.epoch, Ordering::Release);
        self.snapshot.publish(snapshot)
    }

    pub fn user_read_snapshot(&self) -> Result<Option<QuantumSystemSnapshot>, QuantumPortalError> {
        self.snapshot.take()
    }

    pub fn user_submit_command(
        &self,
        command: QuantumDesktopCommand,
    ) -> Result<(), QuantumPortalError> {
        if command.epoch == 0 || command.sequence == 0 {
            return Err(QuantumPortalError::Invalid);
        }
        self.user_epoch.store(command.epoch, Ordering::Release);
        self.command.submit(command)
    }

    pub fn kernel_take_command(&self) -> Result<Option<QuantumDesktopCommand>, QuantumPortalError> {
        self.command.take()
    }

    pub fn kernel_publish_reply(
        &self,
        reply: QuantumDesktopReply,
    ) -> Result<(), QuantumPortalError> {
        if reply.command_sequence == 0 {
            return Err(QuantumPortalError::Invalid);
        }
        self.reply.publish(reply)
    }

    pub fn user_take_reply(&self) -> Result<Option<QuantumDesktopReply>, QuantumPortalError> {
        self.reply.take()
    }

    pub fn epochs(&self) -> (u64, u64) {
        (
            self.kernel_epoch.load(Ordering::Acquire),
            self.user_epoch.load(Ordering::Acquire),
        )
    }
}

impl Default for QuantumCrestPage {
    fn default() -> Self {
        Self::new()
    }
}

pub fn snapshot_root(secret: u64, snapshot: &QuantumSystemSnapshot) -> u64 {
    let mut state = domain(secret, 0x534e_4150_5348_4f54);
    state = absorb(state, u64::from(snapshot.abi_version));
    state = absorb(state, snapshot.sequence);
    state = absorb(state, snapshot.epoch);
    state = absorb(state, snapshot.logical_tick);
    state = absorb(state, snapshot.topology_epoch);
    state = absorb(state, snapshot.capability_epoch);
    state = absorb(state, snapshot.flags);
    state = absorb_gpu(state, snapshot.gpu);
    state = absorb_blacklab(state, snapshot.blacklab);
    state = absorb_display(state, snapshot.display);
    state = absorb(state, snapshot.desktop_rights);
    state = absorb(state, snapshot.desktop_session);
    state = absorb(state, u64::from(snapshot.desktop_generation));
    avalanche(state ^ secret.rotate_left(17))
}

pub fn command_root(secret: u64, command: &QuantumDesktopCommand) -> u64 {
    let mut state = domain(secret, 0x434f_4d4d_414e_4421);
    state = absorb(state, u64::from(command.abi_version));
    state = absorb(state, command.sequence);
    state = absorb(state, command.epoch);
    state = absorb(state, command.session);
    state = absorb(state, u64::from(command.generation));
    state = absorb(state, u64::from(command.opcode));
    state = absorb(state, command.flags);
    state = absorb(state, command.object);
    state = absorb(state, command.deadline_tick);
    for argument in command.arguments {
        state = absorb(state, argument);
    }
    state = absorb(state, u64::from(command.payload_length));
    for chunk in command.payload.chunks(8) {
        state = absorb(state, bytes_word(chunk));
    }
    avalanche(state ^ secret.rotate_right(9))
}

pub fn reply_root(secret: u64, reply: &QuantumDesktopReply) -> u64 {
    let mut state = domain(secret, 0x5245_504c_5921_2121);
    state = absorb(state, u64::from(reply.abi_version));
    state = absorb(state, reply.command_sequence);
    state = absorb(state, reply.snapshot_sequence);
    state = absorb(state, reply.status as u32 as u64);
    state = absorb(state, u64::from(reply.flags));
    for output in reply.outputs {
        state = absorb(state, output);
    }
    state = absorb_frame(state, reply.frame);
    avalanche(state ^ secret.rotate_left(31))
}

pub fn frame_root(secret: u64, frame: &QuantumFrameCertificate) -> u64 {
    avalanche(absorb_frame(domain(secret, 0x4652_414d_4521_2121), *frame))
}

fn absorb_gpu(mut state: u64, gpu: QuantumGpuState) -> u64 {
    state = absorb(state, u64::from(gpu.vendor_id));
    state = absorb(state, u64::from(gpu.device_id));
    state = absorb(state, u64::from(gpu.revision));
    state = absorb(state, u64::from(gpu.gsp_wire_major));
    state = absorb(state, u64::from(gpu.gsp_wire_minor));
    state = absorb(state, u64::from(gpu.gsp_epoch));
    state = absorb(state, gpu.negotiated_features);
    state = absorb(state, gpu.firmware_version);
    state = absorb(state, gpu.temperature_q16 as u32 as u64);
    state = absorb(state, gpu.pressure_q16 as u32 as u64);
    state = absorb(state, u64::from(gpu.corrected_faults));
    absorb(state, u64::from(gpu.fatal_faults))
}

fn absorb_blacklab(mut state: u64, blacklab: QuantumBlackLabState) -> u64 {
    state = absorb(state, blacklab.policy_epoch);
    state = absorb(state, u64::from(blacklab.risk));
    state = absorb(state, u64::from(blacklab.severity));
    state = absorb(state, u64::from(blacklab.action));
    state = absorb(state, u64::from(blacklab.temporal_violations));
    state = absorb(state, u64::from(blacklab.evidence_votes));
    state = absorb(state, u64::from(blacklab.plan_steps));
    state = absorb(state, u64::from(blacklab.required_quorum));
    state = absorb(state, blacklab.ledger_epoch);
    state = absorb(state, blacklab.ledger_root);
    state = absorb(state, blacklab.evidence_root);
    state = absorb(state, blacklab.plan_root);
    absorb(state, blacklab.forecast_tick)
}

fn absorb_display(mut state: u64, display: QuantumDisplayState) -> u64 {
    state = absorb(state, u64::from(display.width));
    state = absorb(state, u64::from(display.height));
    state = absorb(state, u64::from(display.pitch));
    state = absorb(state, u64::from(display.format));
    state = absorb(state, u64::from(display.refresh_millihertz));
    state = absorb(state, u64::from(display.beam_position));
    state = absorb(state, display.present_sequence);
    state = absorb(state, display.frame_budget_ticks);
    state = absorb(state, display.predicted_render_ticks);
    state = absorb(state, u64::from(display.damage_tiles));
    absorb(state, u64::from(display.total_tiles))
}

fn absorb_frame(mut state: u64, frame: QuantumFrameCertificate) -> u64 {
    state = absorb(state, frame.frame_sequence);
    state = absorb(state, frame.snapshot_sequence);
    state = absorb(state, frame.scene_epoch);
    state = absorb(state, frame.damage_root);
    state = absorb(state, frame.scene_root);
    state = absorb(state, frame.present_tick);
    state = absorb(state, u64::from(frame.rendered_tiles));
    state = absorb(state, u64::from(frame.skipped_tiles));
    absorb(state, u64::from(frame.lane_votes))
}

fn domain(secret: u64, tag: u64) -> u64 {
    avalanche(secret ^ tag.rotate_left(23))
}

fn absorb(state: u64, word: u64) -> u64 {
    avalanche(state ^ word.wrapping_mul(0x9e37_79b9_7f4a_7c15))
}

fn bytes_word(bytes: &[u8]) -> u64 {
    let mut word = [0_u8; 8];
    word[..bytes.len()].copy_from_slice(bytes);
    u64::from_le_bytes(word)
}

fn avalanche(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_is_exactly_one_mapping_granule() {
        assert_eq!(core::mem::size_of::<QuantumCrestPage>(), 4096);
        assert_eq!(core::mem::align_of::<QuantumCrestPage>(), 4096);
    }

    #[test]
    fn snapshot_round_trips_through_the_seqlock() {
        let page = QuantumCrestPage::new();
        let mut snapshot = QuantumSystemSnapshot::empty();
        snapshot.sequence = 1;
        snapshot.epoch = 7;
        snapshot.desktop_session = 9;
        snapshot.desktop_generation = 1;
        snapshot.seal(0x1234);

        page.kernel_publish_snapshot(snapshot).unwrap();
        assert_eq!(page.user_read_snapshot().unwrap(), Some(snapshot));
        assert!(snapshot.verify(0x1234));
    }

    #[test]
    fn command_and_reply_mailboxes_transfer_ownership() {
        let page = QuantumCrestPage::new();
        let mut command = QuantumDesktopCommand::empty();
        command.sequence = 1;
        command.epoch = 7;
        command.session = 9;
        command.generation = 1;
        command.opcode = OPCODE_QUERY_OBJECT;
        command.seal(0x5678);

        page.user_submit_command(command).unwrap();
        assert_eq!(page.kernel_take_command().unwrap(), Some(command));
        assert_eq!(page.kernel_take_command().unwrap(), None);

        let mut reply = QuantumDesktopReply::empty();
        reply.command_sequence = command.sequence;
        reply.snapshot_sequence = 2;
        reply.status = STATUS_OK;
        reply.seal(0x5678);
        page.kernel_publish_reply(reply).unwrap();
        assert_eq!(page.user_take_reply().unwrap(), Some(reply));
    }
}
