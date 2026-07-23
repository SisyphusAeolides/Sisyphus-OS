use core::ffi::c_void;

use sisyphus_driver_abi::hermes::{
    HERMES_PERSONALITY_ABI_VERSION, HermesBootInstruction, HermesNormalizedCommand,
    HermesNormalizedEvent, HermesPciIdentity, HermesProbeEvidence, HermesStatus,
    HermesTransportProfile,
};

pub const HERMES_MORPHIC_MATCH_DEVICE: u32 = 1;
pub const HERMES_MORPHIC_DESCRIBE_TRANSPORT: u32 = 2;
pub const HERMES_MORPHIC_BOOT_INSTRUCTION: u32 = 3;
pub const HERMES_MORPHIC_ENCODE_COMMAND: u32 = 4;
pub const HERMES_MORPHIC_DECODE_EVENT: u32 = 5;
pub const HERMES_MORPHIC_RESET_CODEC: u32 = 6;

pub const HERMES_MORPHIC_REPLY_INSTRUCTION: u32 = 1;
pub const HERMES_MORPHIC_REPLY_END_OF_STAGE: u32 = 2;

pub const HERMES_MORPHIC_FLAG_INPUT_READ_ONLY: u64 = 1 << 0;
pub const HERMES_MORPHIC_FLAG_OUTPUT_BOUNDED: u64 = 1 << 1;
pub const HERMES_MORPHIC_FLAG_NO_UNWIND: u64 = 1 << 2;
pub const HERMES_MORPHIC_FLAG_DETERMINISTIC: u64 = 1 << 3;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct HermesMorphicCall {
    pub abi_version: u32,
    pub struct_size: u32,
    pub operation: u32,
    pub reserved0: u32,
    pub authority_epoch: u64,
    pub execution_fuel: u64,
    pub flags: u64,

    pub identity: *const HermesPciIdentity,
    pub evidence: *const HermesProbeEvidence,
    pub profile: *const HermesTransportProfile,
    pub command: *const HermesNormalizedCommand,
    pub event: *mut HermesNormalizedEvent,
    pub boot_instruction: *mut HermesBootInstruction,

    pub input: *const u8,
    pub input_length: usize,
    pub output: *mut u8,
    pub output_capacity: usize,

    pub stage: u32,
    pub index: u32,
    pub epoch: u32,
    pub reserved1: u32,
}

impl HermesMorphicCall {
    pub const fn empty(operation: u32, authority_epoch: u64, execution_fuel: u64) -> Self {
        Self {
            abi_version: HERMES_PERSONALITY_ABI_VERSION,
            struct_size: core::mem::size_of::<Self>() as u32,
            operation,
            reserved0: 0,
            authority_epoch,
            execution_fuel,
            flags: HERMES_MORPHIC_FLAG_INPUT_READ_ONLY
                | HERMES_MORPHIC_FLAG_OUTPUT_BOUNDED
                | HERMES_MORPHIC_FLAG_NO_UNWIND,
            identity: core::ptr::null(),
            evidence: core::ptr::null(),
            profile: core::ptr::null(),
            command: core::ptr::null(),
            event: core::ptr::null_mut(),
            boot_instruction: core::ptr::null_mut(),
            input: core::ptr::null(),
            input_length: 0,
            output: core::ptr::null_mut(),
            output_capacity: 0,
            stage: 0,
            index: 0,
            epoch: 0,
            reserved1: 0,
        }
    }

    pub const fn structurally_valid(&self) -> bool {
        self.abi_version >> 16 == HERMES_PERSONALITY_ABI_VERSION >> 16
            && self.struct_size as usize >= core::mem::size_of::<Self>()
            && self.operation >= HERMES_MORPHIC_MATCH_DEVICE
            && self.operation <= HERMES_MORPHIC_RESET_CODEC
            && self.authority_epoch != 0
            && self.execution_fuel != 0
            && self.flags & HERMES_MORPHIC_FLAG_NO_UNWIND != 0
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HermesMorphicReply {
    pub abi_version: u32,
    pub struct_size: u32,
    pub operation: u32,
    pub disposition: u32,
    pub personality_status: HermesStatus,
    pub reserved0: u32,
    pub consumed_fuel: u64,
    pub output_length: usize,
    pub score: u32,
    pub fault_code: u32,
    pub transcript_root: [u8; 32],
}

impl HermesMorphicReply {
    pub const fn empty(operation: u32) -> Self {
        Self {
            abi_version: HERMES_PERSONALITY_ABI_VERSION,
            struct_size: core::mem::size_of::<Self>() as u32,
            operation,
            disposition: 0,
            personality_status: 0,
            reserved0: 0,
            consumed_fuel: 0,
            output_length: 0,
            score: 0,
            fault_code: 0,
            transcript_root: [0; 32],
        }
    }

    pub const fn structurally_valid_for(&self, operation: u32, fuel_limit: u64) -> bool {
        self.abi_version >> 16 == HERMES_PERSONALITY_ABI_VERSION >> 16
            && self.struct_size as usize >= core::mem::size_of::<Self>()
            && self.operation == operation
            && self.consumed_fuel <= fuel_limit
    }
}

pub type HermesMorphicDispatchFn = unsafe extern "C" fn(
    context: *mut c_void,
    call: *const HermesMorphicCall,
    reply: *mut HermesMorphicReply,
) -> HermesStatus;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct HermesMorphicDescriptor {
    pub abi_version: u32,
    pub struct_size: u32,
    pub personality_version: u64,
    pub personality_id: u64,
    pub protocol_family: u64,
    pub flags: u64,
    pub authority_epoch: u64,
    pub maximum_execution_fuel: u64,
    pub context: *mut c_void,
    pub dispatch: Option<HermesMorphicDispatchFn>,
}

impl HermesMorphicDescriptor {
    pub const fn empty() -> Self {
        Self {
            abi_version: HERMES_PERSONALITY_ABI_VERSION,
            struct_size: core::mem::size_of::<Self>() as u32,
            personality_version: 0,
            personality_id: 0,
            protocol_family: 0,
            flags: 0,
            authority_epoch: 0,
            maximum_execution_fuel: 0,
            context: core::ptr::null_mut(),
            dispatch: None,
        }
    }

    pub const fn structurally_complete(&self) -> bool {
        self.abi_version >> 16 == HERMES_PERSONALITY_ABI_VERSION >> 16
            && self.struct_size as usize >= core::mem::size_of::<Self>()
            && self.personality_version != 0
            && self.personality_id != 0
            && self.protocol_family != 0
            && self.authority_epoch != 0
            && self.maximum_execution_fuel != 0
            && self.dispatch.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_call_fails_without_authority() {
        let call = HermesMorphicCall::empty(HERMES_MORPHIC_MATCH_DEVICE, 0, 100);
        assert!(!call.structurally_valid());
    }

    #[test]
    fn dispatch_surface_has_three_machine_arguments() {
        assert_eq!(
            core::mem::size_of::<Option<HermesMorphicDispatchFn>>(),
            core::mem::size_of::<usize>()
        );
    }
}
