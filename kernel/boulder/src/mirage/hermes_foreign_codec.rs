use sisyphus_driver_abi::gpu::GpuCompatibilityManifest;
use sisyphus_driver_abi::hermes::{
    HERMES_STATUS_OK, HermesBootInstruction, HermesNormalizedCommand, HermesNormalizedEvent,
    HermesPciIdentity, HermesProbeEvidence, HermesTransportProfile,
};

use crate::drivers::hermes_gsp::{HermesCodec, HermesFault};
use crate::mirage::hermes_morphic_abi::{
    HERMES_MORPHIC_BOOT_INSTRUCTION, HERMES_MORPHIC_DECODE_EVENT,
    HERMES_MORPHIC_DESCRIBE_TRANSPORT, HERMES_MORPHIC_ENCODE_COMMAND, HERMES_MORPHIC_MATCH_DEVICE,
    HERMES_MORPHIC_REPLY_END_OF_STAGE, HERMES_MORPHIC_REPLY_INSTRUCTION,
    HERMES_MORPHIC_RESET_CODEC, HermesMorphicCall, HermesMorphicReply,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MorphicHermesEntrypoint {
    pub personality_id: u64,
    pub protocol_family: u64,
    pub personality_version: u64,
    pub context_address: usize,
    pub dispatch_address: u64,
    pub authority_epoch: u64,
    pub maximum_execution_fuel: u64,
    pub compatibility: GpuCompatibilityManifest,
}

impl MorphicHermesEntrypoint {
    pub const fn is_complete(&self) -> bool {
        self.personality_id != 0
            && self.protocol_family != 0
            && self.personality_version != 0
            && self.dispatch_address != 0
            && self.authority_epoch != 0
            && self.maximum_execution_fuel != 0
            && self.compatibility.valid()
            && self.compatibility.driver_id == self.personality_id
            && self.compatibility.vendor_id == 0x10de
    }
}

/// Executes one normalized three-argument dispatch call inside the kernel's
/// fault membrane.
///
/// Implementations own the dangerous boundary and must:
/// - enter a rollback journal before invoking the executable thunk;
/// - enforce the call's execution-fuel ceiling;
/// - forbid unwinding across the thunk;
/// - validate every input and output pointer against the call frame;
/// - revoke the personality after a fault threshold;
/// - record the call in the driver behavior ledger.
pub trait ForeignDispatchGate: Sync {
    fn dispatch(
        &self,
        entry_address: u64,
        context_address: usize,
        call: &HermesMorphicCall,
        reply: &mut HermesMorphicReply,
    ) -> Result<(), HermesFault>;
}

pub struct ForeignHermesCodec<'a, Gate: ForeignDispatchGate + ?Sized> {
    gate: &'a Gate,
    entrypoint: MorphicHermesEntrypoint,
}

impl<'a, Gate: ForeignDispatchGate + ?Sized> ForeignHermesCodec<'a, Gate> {
    pub fn new(gate: &'a Gate, entrypoint: MorphicHermesEntrypoint) -> Result<Self, HermesFault> {
        if !entrypoint.is_complete() {
            return Err(HermesFault::PersonalityRejected);
        }

        Ok(Self { gate, entrypoint })
    }

    pub const fn entrypoint(&self) -> MorphicHermesEntrypoint {
        self.entrypoint
    }

    fn execute(
        &self,
        call: &HermesMorphicCall,
        reply: &mut HermesMorphicReply,
    ) -> Result<(), HermesFault> {
        if !call.structurally_valid()
            || call.authority_epoch != self.entrypoint.authority_epoch
            || call.execution_fuel > self.entrypoint.maximum_execution_fuel
        {
            return Err(HermesFault::PersonalityRejected);
        }

        self.gate.dispatch(
            self.entrypoint.dispatch_address,
            self.entrypoint.context_address,
            call,
            reply,
        )?;

        if !reply.structurally_valid_for(call.operation, call.execution_fuel)
            || reply.personality_status != HERMES_STATUS_OK
        {
            return Err(HermesFault::CodecRejected);
        }

        Ok(())
    }

    fn call(&self, operation: u32) -> HermesMorphicCall {
        HermesMorphicCall::empty(
            operation,
            self.entrypoint.authority_epoch,
            self.entrypoint.maximum_execution_fuel,
        )
    }
}

impl<Gate: ForeignDispatchGate + ?Sized> HermesCodec for ForeignHermesCodec<'_, Gate> {
    fn personality_id(&self) -> u64 {
        self.entrypoint.personality_id
    }

    fn compatibility_manifest(&self) -> GpuCompatibilityManifest {
        self.entrypoint.compatibility
    }

    fn match_device(
        &self,
        identity: &HermesPciIdentity,
        evidence: &HermesProbeEvidence,
    ) -> Result<u32, HermesFault> {
        let mut call = self.call(HERMES_MORPHIC_MATCH_DEVICE);
        call.identity = identity;
        call.evidence = evidence;

        let mut reply = HermesMorphicReply::empty(call.operation);
        self.execute(&call, &mut reply)?;
        Ok(reply.score)
    }

    fn describe_transport(
        &self,
        identity: &HermesPciIdentity,
        evidence: &HermesProbeEvidence,
    ) -> Result<HermesTransportProfile, HermesFault> {
        let mut profile = HermesTransportProfile::empty();
        let mut call = self.call(HERMES_MORPHIC_DESCRIBE_TRANSPORT);
        call.identity = identity;
        call.evidence = evidence;
        call.output = (&mut profile as *mut HermesTransportProfile).cast::<u8>();
        call.output_capacity = core::mem::size_of::<HermesTransportProfile>();

        let mut reply = HermesMorphicReply::empty(call.operation);
        self.execute(&call, &mut reply)?;

        if reply.output_length != core::mem::size_of::<HermesTransportProfile>()
            || profile.personality_id != self.entrypoint.personality_id
            || profile.protocol_family != self.entrypoint.protocol_family
        {
            return Err(HermesFault::ProfileRejected);
        }

        Ok(profile)
    }

    fn boot_instruction(
        &self,
        identity: &HermesPciIdentity,
        evidence: &HermesProbeEvidence,
        stage: u32,
        index: u32,
    ) -> Result<Option<HermesBootInstruction>, HermesFault> {
        let mut instruction = HermesBootInstruction::empty();
        let mut call = self.call(HERMES_MORPHIC_BOOT_INSTRUCTION);
        call.identity = identity;
        call.evidence = evidence;
        call.boot_instruction = &mut instruction;
        call.stage = stage;
        call.index = index;

        let mut reply = HermesMorphicReply::empty(call.operation);
        self.execute(&call, &mut reply)?;

        match reply.disposition {
            HERMES_MORPHIC_REPLY_INSTRUCTION => Ok(Some(instruction)),
            HERMES_MORPHIC_REPLY_END_OF_STAGE => Ok(None),
            _ => Err(HermesFault::BootInstructionRejected),
        }
    }

    fn encode_command(
        &self,
        profile: &HermesTransportProfile,
        command: &HermesNormalizedCommand,
        output: &mut [u8],
    ) -> Result<usize, HermesFault> {
        let mut call = self.call(HERMES_MORPHIC_ENCODE_COMMAND);
        call.profile = profile;
        call.command = command;
        call.output = output.as_mut_ptr();
        call.output_capacity = output.len();

        let mut reply = HermesMorphicReply::empty(call.operation);
        self.execute(&call, &mut reply)?;

        if reply.output_length == 0 || reply.output_length > output.len() {
            return Err(HermesFault::CodecRejected);
        }

        Ok(reply.output_length)
    }

    fn decode_event(
        &self,
        profile: &HermesTransportProfile,
        input: &[u8],
    ) -> Result<HermesNormalizedEvent, HermesFault> {
        let mut event = HermesNormalizedEvent::empty();
        let mut call = self.call(HERMES_MORPHIC_DECODE_EVENT);
        call.profile = profile;
        call.event = &mut event;
        call.input = input.as_ptr();
        call.input_length = input.len();

        let mut reply = HermesMorphicReply::empty(call.operation);
        self.execute(&call, &mut reply)?;

        if reply.output_length != core::mem::size_of::<HermesNormalizedEvent>() {
            return Err(HermesFault::CodecRejected);
        }

        Ok(event)
    }

    fn reset(&self, profile: &HermesTransportProfile, new_epoch: u32) -> Result<(), HermesFault> {
        let mut call = self.call(HERMES_MORPHIC_RESET_CODEC);
        call.profile = profile;
        call.epoch = new_epoch;

        let mut reply = HermesMorphicReply::empty(call.operation);
        self.execute(&call, &mut reply)
    }
}
