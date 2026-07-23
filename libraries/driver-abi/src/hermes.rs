#![no_std]

use core::ffi::c_void;

pub type HermesStatus = i32;

pub const HERMES_STATUS_OK: HermesStatus = 0;
pub const HERMES_STATUS_INVALID_ARGUMENT: HermesStatus = -1;
pub const HERMES_STATUS_UNSUPPORTED: HermesStatus = -2;
pub const HERMES_STATUS_BUFFER_TOO_SMALL: HermesStatus = -3;
pub const HERMES_STATUS_PROTOCOL_MISMATCH: HermesStatus = -4;
pub const HERMES_STATUS_CORRUPT: HermesStatus = -5;
pub const HERMES_STATUS_DENIED: HermesStatus = -6;

pub const HERMES_PERSONALITY_ABI_MAJOR: u32 = 1;
pub const HERMES_PERSONALITY_ABI_MINOR: u32 = 0;
pub const HERMES_PERSONALITY_ABI_VERSION: u32 =
    (HERMES_PERSONALITY_ABI_MAJOR << 16) | HERMES_PERSONALITY_ABI_MINOR;

pub const HERMES_PERSONALITY_CODEC_ONLY: u64 = 1 << 0;
pub const HERMES_PERSONALITY_REQUIRES_FIRMWARE_MANIFEST: u64 = 1 << 1;
pub const HERMES_PERSONALITY_SUPPORTS_RECOVERY: u64 = 1 << 2;
pub const HERMES_PERSONALITY_SUPPORTS_EVENTS: u64 = 1 << 3;

pub const HERMES_FEATURE_BOOT_RPC: u64 = 1 << 0;
pub const HERMES_FEATURE_COMMAND_RING: u64 = 1 << 1;
pub const HERMES_FEATURE_EVENT_RING: u64 = 1 << 2;
pub const HERMES_FEATURE_RECOVERY: u64 = 1 << 3;
pub const HERMES_FEATURE_DISPLAY: u64 = 1 << 4;
pub const HERMES_FEATURE_COMPUTE: u64 = 1 << 5;
pub const HERMES_FEATURE_COPY_ENGINE: u64 = 1 << 6;
pub const HERMES_FEATURE_TELEMETRY: u64 = 1 << 7;
pub const HERMES_FEATURE_POWER: u64 = 1 << 8;
pub const HERMES_FEATURE_MEMORY_MANAGEMENT: u64 = 1 << 9;

pub const HERMES_BOOT_STAGE_DISCOVER: u32 = 1;
pub const HERMES_BOOT_STAGE_FIRMWARE: u32 = 2;
pub const HERMES_BOOT_STAGE_QUEUES: u32 = 3;
pub const HERMES_BOOT_STAGE_IGNITE: u32 = 4;
pub const HERMES_BOOT_STAGE_NEGOTIATE: u32 = 5;
pub const HERMES_BOOT_STAGE_RECOVER: u32 = 6;

pub const HERMES_EVENT_REPLY: u32 = 1;
pub const HERMES_EVENT_ASYNC: u32 = 2;
pub const HERMES_EVENT_FAULT: u32 = 3;
pub const HERMES_EVENT_TELEMETRY: u32 = 4;
pub const HERMES_EVENT_RESET: u32 = 5;

pub const HERMES_MAX_NORMALIZED_PAYLOAD: usize = 192;
pub const HERMES_MAX_BOOT_WORDS: usize = 16;

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HermesPciIdentity {
    pub segment: u16,
    pub bus: u8,
    pub slot: u8,
    pub function: u8,
    pub revision: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub subsystem_vendor_id: u16,
    pub subsystem_device_id: u16,
    pub class_code: u8,
    pub subclass: u8,
    pub programming_interface: u8,
    pub reserved: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HermesProbeEvidence {
    pub evidence_version: u32,
    pub struct_size: u32,
    pub bar_lengths: [u64; 6],
    pub firmware_manifest_hash: [u8; 32],
    pub firmware_version: u64,
    pub bootrom_revision: u32,
    pub architecture_hint: u32,
    pub observed_features: u64,
    pub policy_flags: u64,
}

impl HermesProbeEvidence {
    pub const fn empty() -> Self {
        Self {
            evidence_version: 1,
            struct_size: core::mem::size_of::<Self>() as u32,
            bar_lengths: [0; 6],
            firmware_manifest_hash: [0; 32],
            firmware_version: 0,
            bootrom_revision: 0,
            architecture_hint: 0,
            observed_features: 0,
            policy_flags: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HermesTransportProfile {
    pub profile_version: u32,
    pub struct_size: u32,
    pub personality_id: u64,
    pub protocol_family: u64,
    pub wire_major: u16,
    pub wire_minor_minimum: u16,
    pub wire_minor_maximum: u16,
    pub command_slot_bytes: u16,
    pub event_slot_bytes: u16,
    pub command_depth: u16,
    pub event_depth: u16,
    pub control_bar: u8,
    pub doorbell_bar: u8,
    pub reserved0: u16,
    pub required_bar_lengths: [u64; 6],
    pub firmware_minimum_bytes: u32,
    pub firmware_maximum_bytes: u32,
    pub firmware_alignment: u32,
    pub firmware_policy_flags: u32,
    pub command_producer_offset: u32,
    pub command_consumer_offset: u32,
    pub event_producer_offset: u32,
    pub event_consumer_offset: u32,
    pub command_doorbell_offset: u32,
    pub event_doorbell_offset: u32,
    pub status_offset: u32,
    pub ready_mask: u32,
    pub ready_value: u32,
    pub fault_mask: u32,
    pub required_features: u64,
    pub optional_features: u64,
    pub maximum_wire_bytes: u32,
    pub maximum_boot_steps: u16,
    pub reserved1: u16,
}

impl HermesTransportProfile {
    pub const fn empty() -> Self {
        Self {
            profile_version: 1,
            struct_size: core::mem::size_of::<Self>() as u32,
            personality_id: 0,
            protocol_family: 0,
            wire_major: 0,
            wire_minor_minimum: 0,
            wire_minor_maximum: 0,
            command_slot_bytes: 0,
            event_slot_bytes: 0,
            command_depth: 0,
            event_depth: 0,
            control_bar: 0,
            doorbell_bar: 0,
            reserved0: 0,
            required_bar_lengths: [0; 6],
            firmware_minimum_bytes: 0,
            firmware_maximum_bytes: 0,
            firmware_alignment: 0,
            firmware_policy_flags: 0,
            command_producer_offset: 0,
            command_consumer_offset: 0,
            event_producer_offset: 0,
            event_consumer_offset: 0,
            command_doorbell_offset: 0,
            event_doorbell_offset: 0,
            status_offset: 0,
            ready_mask: 0,
            ready_value: 0,
            fault_mask: 0,
            required_features: 0,
            optional_features: 0,
            maximum_wire_bytes: 0,
            maximum_boot_steps: 0,
            reserved1: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HermesNormalizedCommand {
    pub command_version: u32,
    pub struct_size: u32,
    pub epoch: u32,
    pub sequence: u32,
    pub opcode: u32,
    pub flags: u32,
    pub object: u64,
    pub deadline_tick: u64,
    pub arguments: [u64; 8],
    pub payload_length: u16,
    pub reserved: [u8; 6],
    pub payload: [u8; HERMES_MAX_NORMALIZED_PAYLOAD],
}

impl HermesNormalizedCommand {
    pub const fn empty() -> Self {
        Self {
            command_version: 1,
            struct_size: core::mem::size_of::<Self>() as u32,
            epoch: 0,
            sequence: 0,
            opcode: 0,
            flags: 0,
            object: 0,
            deadline_tick: 0,
            arguments: [0; 8],
            payload_length: 0,
            reserved: [0; 6],
            payload: [0; HERMES_MAX_NORMALIZED_PAYLOAD],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HermesNormalizedEvent {
    pub event_version: u32,
    pub struct_size: u32,
    pub epoch: u32,
    pub sequence: u32,
    pub correlation_epoch: u32,
    pub correlation_sequence: u32,
    pub event_kind: u32,
    pub opcode: u32,
    pub status: i32,
    pub flags: u32,
    pub object: u64,
    pub arguments: [u64; 8],
    pub payload_length: u16,
    pub reserved: [u8; 6],
    pub payload: [u8; HERMES_MAX_NORMALIZED_PAYLOAD],
}

impl HermesNormalizedEvent {
    pub const fn empty() -> Self {
        Self {
            event_version: 1,
            struct_size: core::mem::size_of::<Self>() as u32,
            epoch: 0,
            sequence: 0,
            correlation_epoch: 0,
            correlation_sequence: 0,
            event_kind: 0,
            opcode: 0,
            status: 0,
            flags: 0,
            object: 0,
            arguments: [0; 8],
            payload_length: 0,
            reserved: [0; 6],
            payload: [0; HERMES_MAX_NORMALIZED_PAYLOAD],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HermesBootInstruction {
    pub instruction_version: u32,
    pub struct_size: u32,
    pub stage: u32,
    pub opcode: u32,
    pub words: [u64; HERMES_MAX_BOOT_WORDS],
}

impl HermesBootInstruction {
    pub const fn empty() -> Self {
        Self {
            instruction_version: 1,
            struct_size: core::mem::size_of::<Self>() as u32,
            stage: 0,
            opcode: 0,
            words: [0; HERMES_MAX_BOOT_WORDS],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct HermesBootQuery {
    pub identity: *const HermesPciIdentity,
    pub evidence: *const HermesProbeEvidence,
    pub stage: u32,
    pub index: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct HermesEncodeRequest {
    pub profile: *const HermesTransportProfile,
    pub command: *const HermesNormalizedCommand,
    pub output_capacity: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct HermesDecodeRequest {
    pub profile: *const HermesTransportProfile,
    pub input: *const u8,
    pub input_length: usize,
}

pub type HermesMatchDeviceFn = unsafe extern "C" fn(
    context: *mut c_void,
    identity: *const HermesPciIdentity,
    evidence: *const HermesProbeEvidence,
    out_score: *mut u32,
) -> HermesStatus;

pub type HermesDescribeTransportFn = unsafe extern "C" fn(
    context: *mut c_void,
    identity: *const HermesPciIdentity,
    evidence: *const HermesProbeEvidence,
    out_profile: *mut HermesTransportProfile,
) -> HermesStatus;

pub type HermesBootInstructionFn = unsafe extern "C" fn(
    context: *mut c_void,
    query: *const HermesBootQuery,
    out_instruction: *mut HermesBootInstruction,
) -> HermesStatus;

pub type HermesEncodeCommandFn = unsafe extern "C" fn(
    context: *mut c_void,
    request: *const HermesEncodeRequest,
    output: *mut u8,
    out_length: *mut usize,
) -> HermesStatus;

pub type HermesDecodeEventFn = unsafe extern "C" fn(
    context: *mut c_void,
    request: *const HermesDecodeRequest,
    out_event: *mut HermesNormalizedEvent,
) -> HermesStatus;

pub type HermesResetCodecFn = unsafe extern "C" fn(
    context: *mut c_void,
    profile: *const HermesTransportProfile,
    new_epoch: u32,
) -> HermesStatus;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct HermesPersonalityDescriptor {
    pub abi_version: u32,
    pub struct_size: u32,
    pub personality_version: u64,
    pub personality_id: u64,
    pub protocol_family: u64,
    pub flags: u64,
    pub name: *const u8,
    pub name_length: usize,
    pub context: *mut c_void,
    pub match_device: Option<HermesMatchDeviceFn>,
    pub describe_transport: Option<HermesDescribeTransportFn>,
    pub boot_instruction: Option<HermesBootInstructionFn>,
    pub encode_command: Option<HermesEncodeCommandFn>,
    pub decode_event: Option<HermesDecodeEventFn>,
    pub reset_codec: Option<HermesResetCodecFn>,
}

impl HermesPersonalityDescriptor {
    pub const fn empty() -> Self {
        Self {
            abi_version: HERMES_PERSONALITY_ABI_VERSION,
            struct_size: core::mem::size_of::<Self>() as u32,
            personality_version: 0,
            personality_id: 0,
            protocol_family: 0,
            flags: HERMES_PERSONALITY_CODEC_ONLY,
            name: core::ptr::null(),
            name_length: 0,
            context: core::ptr::null_mut(),
            match_device: None,
            describe_transport: None,
            boot_instruction: None,
            encode_command: None,
            decode_event: None,
            reset_codec: None,
        }
    }

    pub const fn is_structurally_complete(&self) -> bool {
        self.abi_version >> 16 == HERMES_PERSONALITY_ABI_MAJOR
            && self.struct_size as usize >= core::mem::size_of::<Self>()
            && self.personality_id != 0
            && self.protocol_family != 0
            && self.match_device.is_some()
            && self.describe_transport.is_some()
            && self.boot_instruction.is_some()
            && self.encode_command.is_some()
            && self.decode_event.is_some()
    }
}

pub type HermesPersonalityEntryFn = unsafe extern "C" fn(
    out_descriptor: *mut HermesPersonalityDescriptor,
    out_descriptor_size: usize,
) -> HermesStatus;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_descriptor_fails_closed() {
        assert!(!HermesPersonalityDescriptor::empty().is_structurally_complete());
    }

    #[test]
    fn normalized_payloads_are_fixed_capacity() {
        assert_eq!(
            HermesNormalizedCommand::empty().payload.len(),
            HERMES_MAX_NORMALIZED_PAYLOAD
        );
        assert_eq!(
            HermesNormalizedEvent::empty().payload.len(),
            HERMES_MAX_NORMALIZED_PAYLOAD
        );
    }
}
