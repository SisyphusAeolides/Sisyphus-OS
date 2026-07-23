use core::marker::PhantomData;

use sisyphus_driver_abi::hermes::{
    HERMES_BOOT_STAGE_FIRMWARE, HERMES_BOOT_STAGE_IGNITE, HERMES_BOOT_STAGE_NEGOTIATE,
    HERMES_BOOT_STAGE_QUEUES, HERMES_EVENT_ASYNC, HERMES_EVENT_FAULT, HERMES_EVENT_REPLY,
    HERMES_MAX_NORMALIZED_PAYLOAD, HermesBootInstruction, HermesNormalizedCommand,
    HermesNormalizedEvent, HermesPciIdentity, HermesProbeEvidence, HermesTransportProfile,
};

pub const NVIDIA_VENDOR_ID: u16 = 0x10de;
pub const PCI_CLASS_DISPLAY: u8 = 0x03;

pub const HERMES_OPCODE_NEGOTIATE: u32 = 0x0000_0001;
pub const HERMES_OPCODE_SHUTDOWN: u32 = 0x0000_0002;
pub const HERMES_OPCODE_RECOVER: u32 = 0x0000_0003;

pub const HERMES_BOOT_END: u32 = 0;
pub const HERMES_BOOT_WRITE32: u32 = 1;
pub const HERMES_BOOT_RMW32: u32 = 2;
pub const HERMES_BOOT_POLL32: u32 = 3;
pub const HERMES_BOOT_DELAY: u32 = 4;
pub const HERMES_BOOT_FIRMWARE_ADDRESS: u32 = 5;
pub const HERMES_BOOT_COMMAND_QUEUE: u32 = 6;
pub const HERMES_BOOT_EVENT_QUEUE: u32 = 7;
pub const HERMES_BOOT_DOORBELL: u32 = 8;
pub const HERMES_BOOT_FENCE: u32 = 9;
pub const HERMES_BOOT_ASSERT32: u32 = 10;

pub const MAXIMUM_WIRE_BYTES: usize = 4096;
pub const MAXIMUM_BOOT_STEPS: usize = 256;
pub const MAXIMUM_POLL_SAMPLES: usize = 65_536;
pub const MAXIMUM_BARS: usize = 6;
pub const DMA_GRANULE: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HermesFault {
    NotNvidia,
    NotDisplayController,
    NoPersonality,
    AmbiguousPersonality,
    PersonalityCapacity,
    PersonalityRejected,
    ProfileRejected,
    CodecRejected,
    FirmwareMissing,
    FirmwareUnexpected,
    FirmwareSize,
    FirmwareAlignment,
    FirmwareRejected,
    DeviceIsolation,
    BarUnavailable,
    MmioOutOfRange,
    MmioRead,
    MmioWrite,
    UnstableMmio,
    DmaAllocation,
    DmaAccess,
    DmaAddressOverflow,
    QueueGeometry,
    QueueFull,
    QueueCorrupt,
    PendingCapacity,
    DuplicateRequest,
    UnknownResponse,
    ResponseMismatch,
    ResponseExpired,
    ProtocolMismatch,
    RequiredFeatureMissing,
    BootFuelExhausted,
    BootInstructionRejected,
    DeadlineExpired,
    DeviceFault,
    RecoveryRequired,
    ArithmeticOverflow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DmaPurpose {
    Firmware,
    CommandRing,
    EventRing,
    CrashLedger,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MmioWindow<Handle: Copy + Eq> {
    pub handle: Handle,
    pub bar: u8,
    pub length: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmaRegion<Handle: Copy + Eq> {
    pub handle: Handle,
    pub device_address: u64,
    pub length: usize,
    pub alignment: usize,
    pub purpose: DmaPurpose,
}

pub trait HermesPlatform: Sync {
    type Domain: Copy + Eq;
    type Mmio: Copy + Eq;
    type Dma: Copy + Eq;

    fn isolate_device(&self, identity: HermesPciIdentity) -> Result<Self::Domain, HermesFault>;

    fn release_domain(&self, domain: Self::Domain);

    fn map_bar(
        &self,
        domain: Self::Domain,
        bar: u8,
        minimum_length: u64,
    ) -> Result<MmioWindow<Self::Mmio>, HermesFault>;

    fn unmap_bar(&self, window: MmioWindow<Self::Mmio>);

    fn read32(&self, window: MmioWindow<Self::Mmio>, offset: u32) -> Result<u32, HermesFault>;

    fn write32(
        &self,
        window: MmioWindow<Self::Mmio>,
        offset: u32,
        value: u32,
    ) -> Result<(), HermesFault>;

    fn io_fence(&self) -> Result<(), HermesFault>;

    fn allocate_dma(
        &self,
        domain: Self::Domain,
        length: usize,
        alignment: usize,
        purpose: DmaPurpose,
    ) -> Result<DmaRegion<Self::Dma>, HermesFault>;

    fn release_dma(&self, region: DmaRegion<Self::Dma>);

    fn dma_write(
        &self,
        region: DmaRegion<Self::Dma>,
        offset: usize,
        bytes: &[u8],
    ) -> Result<(), HermesFault>;

    fn dma_read(
        &self,
        region: DmaRegion<Self::Dma>,
        offset: usize,
        bytes: &mut [u8],
    ) -> Result<(), HermesFault>;

    fn dma_publish(
        &self,
        region: DmaRegion<Self::Dma>,
        offset: usize,
        length: usize,
    ) -> Result<(), HermesFault>;

    fn dma_acquire(
        &self,
        region: DmaRegion<Self::Dma>,
        offset: usize,
        length: usize,
    ) -> Result<(), HermesFault>;

    fn now_tick(&self) -> u64;

    fn relax(&self);
}

pub trait HermesCodec: Sync {
    fn personality_id(&self) -> u64;

    fn match_device(
        &self,
        identity: &HermesPciIdentity,
        evidence: &HermesProbeEvidence,
    ) -> Result<u32, HermesFault>;

    fn describe_transport(
        &self,
        identity: &HermesPciIdentity,
        evidence: &HermesProbeEvidence,
    ) -> Result<HermesTransportProfile, HermesFault>;

    fn boot_instruction(
        &self,
        identity: &HermesPciIdentity,
        evidence: &HermesProbeEvidence,
        stage: u32,
        index: u32,
    ) -> Result<Option<HermesBootInstruction>, HermesFault>;

    fn encode_command(
        &self,
        profile: &HermesTransportProfile,
        command: &HermesNormalizedCommand,
        output: &mut [u8],
    ) -> Result<usize, HermesFault>;

    fn decode_event(
        &self,
        profile: &HermesTransportProfile,
        input: &[u8],
    ) -> Result<HermesNormalizedEvent, HermesFault>;

    fn reset(&self, profile: &HermesTransportProfile, new_epoch: u32) -> Result<(), HermesFault>;
}

pub struct PersonalityRegistry<'a, const N: usize> {
    entries: [Option<&'a dyn HermesCodec>; N],
    length: usize,
}

impl<'a, const N: usize> PersonalityRegistry<'a, N> {
    pub const fn new() -> Self {
        Self {
            entries: [None; N],
            length: 0,
        }
    }

    pub fn register(&mut self, codec: &'a dyn HermesCodec) -> Result<(), HermesFault> {
        if codec.personality_id() == 0 {
            return Err(HermesFault::PersonalityRejected);
        }

        if self.entries[..self.length]
            .iter()
            .flatten()
            .any(|registered| registered.personality_id() == codec.personality_id())
        {
            return Err(HermesFault::PersonalityRejected);
        }

        let slot = self
            .entries
            .get_mut(self.length)
            .ok_or(HermesFault::PersonalityCapacity)?;
        *slot = Some(codec);
        self.length += 1;
        Ok(())
    }

    pub fn resolve(
        &self,
        identity: &HermesPciIdentity,
        evidence: &HermesProbeEvidence,
        minimum_score: u32,
    ) -> Result<&'a dyn HermesCodec, HermesFault> {
        let mut winner: Option<&'a dyn HermesCodec> = None;
        let mut winning_score = 0_u32;
        let mut tied = false;

        for codec in self.entries[..self.length].iter().flatten().copied() {
            let score = codec.match_device(identity, evidence)?;
            if score < minimum_score {
                continue;
            }

            if score > winning_score {
                winner = Some(codec);
                winning_score = score;
                tied = false;
            } else if score == winning_score && score != 0 {
                tied = true;
            }
        }

        if tied {
            return Err(HermesFault::AmbiguousPersonality);
        }

        winner.ok_or(HermesFault::NoPersonality)
    }
}

impl<const N: usize> Default for PersonalityRegistry<'_, N> {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy)]
pub struct FirmwareImage<'a> {
    pub bytes: &'a [u8],
    pub manifest_hash: [u8; 32],
    pub version: u64,
    pub flags: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FirmwareSeal {
    pub manifest_hash: [u8; 32],
    pub version: u64,
    pub policy_epoch: u64,
    pub trust_domain: u64,
}

pub trait FirmwareAuthority: Sync {
    fn authenticate(
        &self,
        identity: &HermesPciIdentity,
        evidence: &HermesProbeEvidence,
        profile: &HermesTransportProfile,
        image: &FirmwareImage<'_>,
    ) -> Result<FirmwareSeal, HermesFault>;
}

pub struct Profiled;
pub struct Staged;
pub struct Online;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingRequest {
    live: bool,
    epoch: u32,
    sequence: u32,
    opcode: u32,
    deadline_tick: u64,
}

impl PendingRequest {
    const EMPTY: Self = Self {
        live: false,
        epoch: 0,
        sequence: 0,
        opcode: 0,
        deadline_tick: 0,
    };
}

struct PendingTable<const N: usize> {
    entries: [PendingRequest; N],
}

impl<const N: usize> PendingTable<N> {
    const fn new() -> Self {
        Self {
            entries: [PendingRequest::EMPTY; N],
        }
    }

    fn insert(&mut self, request: PendingRequest) -> Result<(), HermesFault> {
        if self.entries.iter().any(|entry| {
            entry.live && entry.epoch == request.epoch && entry.sequence == request.sequence
        }) {
            return Err(HermesFault::DuplicateRequest);
        }

        let slot = self
            .entries
            .iter_mut()
            .find(|entry| !entry.live)
            .ok_or(HermesFault::PendingCapacity)?;
        *slot = request;
        Ok(())
    }

    fn cancel(&mut self, epoch: u32, sequence: u32) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.live && entry.epoch == epoch && entry.sequence == sequence)
        {
            *entry = PendingRequest::EMPTY;
        }
    }

    fn complete(
        &mut self,
        event: &HermesNormalizedEvent,
        now_tick: u64,
    ) -> Result<PendingRequest, HermesFault> {
        let entry = self
            .entries
            .iter_mut()
            .find(|entry| {
                entry.live
                    && entry.epoch == event.correlation_epoch
                    && entry.sequence == event.correlation_sequence
            })
            .ok_or(HermesFault::UnknownResponse)?;

        if tick_reached(now_tick, entry.deadline_tick) {
            *entry = PendingRequest::EMPTY;
            return Err(HermesFault::ResponseExpired);
        }

        if entry.opcode != event.opcode {
            *entry = PendingRequest::EMPTY;
            return Err(HermesFault::ResponseMismatch);
        }

        let completed = *entry;
        *entry = PendingRequest::EMPTY;
        Ok(completed)
    }

    fn expire_one(&mut self, now_tick: u64) -> Option<(u32, u32)> {
        let entry = self
            .entries
            .iter_mut()
            .find(|entry| entry.live && tick_reached(now_tick, entry.deadline_tick))?;

        let id = (entry.epoch, entry.sequence);
        *entry = PendingRequest::EMPTY;
        Some(id)
    }
}

struct Runtime<const PENDING: usize> {
    epoch: u32,
    next_sequence: u32,
    command_producer: u32,
    event_consumer: u32,
    feature_epoch: u32,
    negotiated_features: u64,
    pending: PendingTable<PENDING>,
}

impl<const PENDING: usize> Runtime<PENDING> {
    fn new(identity: &HermesPciIdentity, now_tick: u64) -> Self {
        let location = ((identity.bus as u32) << 16)
            | ((identity.slot as u32) << 8)
            | identity.function as u32;
        let mut epoch = (now_tick as u32)
            ^ (now_tick >> 32) as u32
            ^ location
            ^ ((identity.device_id as u32) << 1)
            ^ 0x4845_524d;
        if epoch == 0 {
            epoch = 1;
        }

        Self {
            epoch,
            next_sequence: 1,
            command_producer: 0,
            event_consumer: 0,
            feature_epoch: 0,
            negotiated_features: 0,
            pending: PendingTable::new(),
        }
    }

    fn allocate_sequence(&mut self) -> u32 {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        if self.next_sequence == 0 {
            self.next_sequence = 1;
            self.epoch = self.epoch.wrapping_add(1).max(1);
        }
        sequence
    }
}

struct HermesLease<'a, Backend: HermesPlatform + ?Sized> {
    backend: &'a Backend,
    domain: Option<Backend::Domain>,
    bars: [Option<MmioWindow<Backend::Mmio>>; MAXIMUM_BARS],
    firmware: Option<DmaRegion<Backend::Dma>>,
    command_ring: Option<DmaRegion<Backend::Dma>>,
    event_ring: Option<DmaRegion<Backend::Dma>>,
    crash_ledger: Option<DmaRegion<Backend::Dma>>,
}

impl<'a, Backend: HermesPlatform + ?Sized> HermesLease<'a, Backend> {
    fn new(backend: &'a Backend) -> Self {
        Self {
            backend,
            domain: None,
            bars: [None; MAXIMUM_BARS],
            firmware: None,
            command_ring: None,
            event_ring: None,
            crash_ledger: None,
        }
    }

    fn bar(&self, index: u8) -> Result<MmioWindow<Backend::Mmio>, HermesFault> {
        self.bars
            .get(index as usize)
            .copied()
            .flatten()
            .ok_or(HermesFault::BarUnavailable)
    }

    fn firmware(&self) -> Result<DmaRegion<Backend::Dma>, HermesFault> {
        self.firmware.ok_or(HermesFault::FirmwareMissing)
    }

    fn command_ring(&self) -> Result<DmaRegion<Backend::Dma>, HermesFault> {
        self.command_ring.ok_or(HermesFault::DmaAccess)
    }

    fn event_ring(&self) -> Result<DmaRegion<Backend::Dma>, HermesFault> {
        self.event_ring.ok_or(HermesFault::DmaAccess)
    }
}

impl<Backend: HermesPlatform + ?Sized> Drop for HermesLease<'_, Backend> {
    fn drop(&mut self) {
        if let Some(region) = self.crash_ledger.take() {
            self.backend.release_dma(region);
        }
        if let Some(region) = self.event_ring.take() {
            self.backend.release_dma(region);
        }
        if let Some(region) = self.command_ring.take() {
            self.backend.release_dma(region);
        }
        if let Some(region) = self.firmware.take() {
            self.backend.release_dma(region);
        }
        for window in self.bars.iter_mut().rev() {
            if let Some(window) = window.take() {
                self.backend.unmap_bar(window);
            }
        }
        if let Some(domain) = self.domain.take() {
            self.backend.release_domain(domain);
        }
    }
}

pub struct Hermes<'a, Backend: HermesPlatform + ?Sized, State, const PENDING: usize> {
    backend: &'a Backend,
    codec: &'a dyn HermesCodec,
    identity: HermesPciIdentity,
    evidence: HermesProbeEvidence,
    profile: HermesTransportProfile,
    firmware_seal: Option<FirmwareSeal>,
    lease: HermesLease<'a, Backend>,
    runtime: Runtime<PENDING>,
    _state: PhantomData<State>,
}

impl<'a, Backend: HermesPlatform + ?Sized, const PENDING: usize>
    Hermes<'a, Backend, Profiled, PENDING>
{
    pub fn bind<const PERSONALITIES: usize>(
        backend: &'a Backend,
        registry: &'a PersonalityRegistry<'a, PERSONALITIES>,
        identity: HermesPciIdentity,
        evidence: HermesProbeEvidence,
        minimum_personality_score: u32,
    ) -> Result<Self, HermesFault> {
        validate_identity(&identity)?;

        let codec = registry.resolve(&identity, &evidence, minimum_personality_score)?;
        let profile = codec.describe_transport(&identity, &evidence)?;
        validate_profile(codec.personality_id(), &profile, &evidence)?;

        Ok(Self {
            backend,
            codec,
            identity,
            evidence,
            profile,
            firmware_seal: None,
            lease: HermesLease::new(backend),
            runtime: Runtime::new(&identity, backend.now_tick()),
            _state: PhantomData,
        })
    }

    pub fn stage(
        mut self,
        firmware: Option<FirmwareImage<'_>>,
        authority: &dyn FirmwareAuthority,
    ) -> Result<Hermes<'a, Backend, Staged, PENDING>, HermesFault> {
        let domain = self
            .backend
            .isolate_device(self.identity)
            .map_err(|_| HermesFault::DeviceIsolation)?;
        self.lease.domain = Some(domain);

        for bar in 0..MAXIMUM_BARS {
            let required = self.profile.required_bar_lengths[bar];
            if required == 0 {
                continue;
            }
            if self.evidence.bar_lengths[bar] < required {
                return Err(HermesFault::BarUnavailable);
            }
            let window = self.backend.map_bar(domain, bar as u8, required)?;
            if window.length < required {
                return Err(HermesFault::BarUnavailable);
            }
            self.lease.bars[bar] = Some(window);
        }

        match firmware {
            Some(image) => {
                validate_firmware(&self.profile, &image)?;
                let seal = authority.authenticate(
                    &self.identity,
                    &self.evidence,
                    &self.profile,
                    &image,
                )?;
                if seal.manifest_hash != image.manifest_hash || seal.version != image.version {
                    return Err(HermesFault::FirmwareRejected);
                }

                let alignment = self.profile.firmware_alignment as usize;
                let region = self.backend.allocate_dma(
                    domain,
                    image.bytes.len(),
                    alignment,
                    DmaPurpose::Firmware,
                )?;
                validate_dma_region(region, image.bytes.len(), alignment)?;
                self.backend.dma_write(region, 0, image.bytes)?;
                self.backend.dma_publish(region, 0, image.bytes.len())?;
                self.lease.firmware = Some(region);
                self.firmware_seal = Some(seal);
            }
            None => {
                if self.profile.firmware_minimum_bytes != 0 {
                    return Err(HermesFault::FirmwareMissing);
                }
            }
        }

        let command_bytes =
            ring_bytes(self.profile.command_depth, self.profile.command_slot_bytes)?;
        let event_bytes = ring_bytes(self.profile.event_depth, self.profile.event_slot_bytes)?;

        let command_ring = self.backend.allocate_dma(
            domain,
            command_bytes,
            DMA_GRANULE,
            DmaPurpose::CommandRing,
        )?;
        validate_dma_region(command_ring, command_bytes, DMA_GRANULE)?;
        zero_dma(self.backend, command_ring)?;

        let event_ring =
            self.backend
                .allocate_dma(domain, event_bytes, DMA_GRANULE, DmaPurpose::EventRing)?;
        validate_dma_region(event_ring, event_bytes, DMA_GRANULE)?;
        zero_dma(self.backend, event_ring)?;

        self.lease.command_ring = Some(command_ring);
        self.lease.event_ring = Some(event_ring);

        self.codec.reset(&self.profile, self.runtime.epoch)?;

        Ok(Hermes {
            backend: self.backend,
            codec: self.codec,
            identity: self.identity,
            evidence: self.evidence,
            profile: self.profile,
            firmware_seal: self.firmware_seal,
            lease: self.lease,
            runtime: self.runtime,
            _state: PhantomData,
        })
    }
}

impl<'a, Backend: HermesPlatform + ?Sized, const PENDING: usize>
    Hermes<'a, Backend, Staged, PENDING>
{
    pub fn ignite(
        mut self,
        deadline_tick: u64,
    ) -> Result<Hermes<'a, Backend, Online, PENDING>, HermesFault> {
        self.execute_stage(HERMES_BOOT_STAGE_FIRMWARE, deadline_tick)?;
        self.execute_stage(HERMES_BOOT_STAGE_QUEUES, deadline_tick)?;
        self.execute_stage(HERMES_BOOT_STAGE_IGNITE, deadline_tick)?;
        self.await_ready(deadline_tick)?;
        self.execute_stage(HERMES_BOOT_STAGE_NEGOTIATE, deadline_tick)?;

        let desired_features = self.profile.required_features | self.profile.optional_features;
        let mut negotiate = HermesNormalizedCommand::empty();
        negotiate.epoch = self.runtime.epoch;
        negotiate.sequence = self.runtime.allocate_sequence();
        negotiate.opcode = HERMES_OPCODE_NEGOTIATE;
        negotiate.deadline_tick = deadline_tick;
        negotiate.arguments[0] = desired_features;
        negotiate.arguments[1] = ((self.profile.wire_major as u64) << 32)
            | ((self.profile.wire_minor_minimum as u64) << 16)
            | self.profile.wire_minor_maximum as u64;

        self.send_wire(&negotiate)?;

        let event = loop {
            if tick_reached(self.backend.now_tick(), deadline_tick) {
                return Err(HermesFault::DeadlineExpired);
            }
            if let Some(event) = self.read_wire_event()? {
                break event;
            }
            self.backend.relax();
        };

        if event.event_kind == HERMES_EVENT_FAULT {
            return Err(HermesFault::DeviceFault);
        }

        if event.event_kind != HERMES_EVENT_REPLY
            || event.correlation_epoch != negotiate.epoch
            || event.correlation_sequence != negotiate.sequence
            || event.opcode != HERMES_OPCODE_NEGOTIATE
            || event.status != 0
        {
            return Err(HermesFault::ProtocolMismatch);
        }

        let negotiated_features = event.arguments[0];
        let negotiated_wire = event.arguments[1];
        let negotiated_major = (negotiated_wire >> 32) as u16;
        let negotiated_minor = negotiated_wire as u16;

        if negotiated_major != self.profile.wire_major
            || negotiated_minor < self.profile.wire_minor_minimum
            || negotiated_minor > self.profile.wire_minor_maximum
        {
            return Err(HermesFault::ProtocolMismatch);
        }

        if negotiated_features & self.profile.required_features != self.profile.required_features {
            return Err(HermesFault::RequiredFeatureMissing);
        }

        self.runtime.negotiated_features = negotiated_features;
        self.runtime.feature_epoch = self.runtime.feature_epoch.wrapping_add(1).max(1);

        Ok(Hermes {
            backend: self.backend,
            codec: self.codec,
            identity: self.identity,
            evidence: self.evidence,
            profile: self.profile,
            firmware_seal: self.firmware_seal,
            lease: self.lease,
            runtime: self.runtime,
            _state: PhantomData,
        })
    }
}

impl<Backend: HermesPlatform + ?Sized, const PENDING: usize> Hermes<'_, Backend, Online, PENDING> {
    pub const fn identity(&self) -> HermesPciIdentity {
        self.identity
    }

    pub const fn profile(&self) -> HermesTransportProfile {
        self.profile
    }

    pub const fn negotiated_features(&self) -> u64 {
        self.runtime.negotiated_features
    }

    pub const fn firmware_seal(&self) -> Option<FirmwareSeal> {
        self.firmware_seal
    }

    pub fn submit(
        &mut self,
        opcode: u32,
        flags: u32,
        object: u64,
        arguments: [u64; 8],
        payload: &[u8],
        deadline_tick: u64,
    ) -> Result<(u32, u32), HermesFault> {
        if opcode == 0 || payload.len() > HERMES_MAX_NORMALIZED_PAYLOAD {
            return Err(HermesFault::ProtocolMismatch);
        }
        if tick_reached(self.backend.now_tick(), deadline_tick) {
            return Err(HermesFault::DeadlineExpired);
        }

        let sequence = self.runtime.allocate_sequence();
        let epoch = self.runtime.epoch;

        let mut command = HermesNormalizedCommand::empty();
        command.epoch = epoch;
        command.sequence = sequence;
        command.opcode = opcode;
        command.flags = flags;
        command.object = object;
        command.deadline_tick = deadline_tick;
        command.arguments = arguments;
        command.payload_length = payload.len() as u16;
        command.payload[..payload.len()].copy_from_slice(payload);

        self.runtime.pending.insert(PendingRequest {
            live: true,
            epoch,
            sequence,
            opcode,
            deadline_tick,
        })?;

        if let Err(error) = self.send_wire(&command) {
            self.runtime.pending.cancel(epoch, sequence);
            return Err(error);
        }

        Ok((epoch, sequence))
    }

    pub fn poll_event(&mut self) -> Result<Option<ReceivedEvent>, HermesFault> {
        let Some(event) = self.read_wire_event()? else {
            return Ok(None);
        };

        if event.event_kind == HERMES_EVENT_FAULT {
            return Err(HermesFault::RecoveryRequired);
        }

        let disposition = if event.event_kind == HERMES_EVENT_REPLY {
            let pending = self
                .runtime
                .pending
                .complete(&event, self.backend.now_tick())?;
            EventDisposition::Reply {
                epoch: pending.epoch,
                sequence: pending.sequence,
            }
        } else if event.event_kind == HERMES_EVENT_ASYNC {
            EventDisposition::Asynchronous
        } else {
            EventDisposition::Unclassified
        };

        Ok(Some(ReceivedEvent { event, disposition }))
    }

    pub fn expire_one(&mut self) -> Option<(u32, u32)> {
        self.runtime.pending.expire_one(self.backend.now_tick())
    }
}

impl<Backend: HermesPlatform + ?Sized, State, const PENDING: usize>
    Hermes<'_, Backend, State, PENDING>
{
    fn execute_stage(&mut self, stage: u32, deadline_tick: u64) -> Result<(), HermesFault> {
        let profile_limit = self.profile.maximum_boot_steps as usize;
        let maximum = profile_limit.min(MAXIMUM_BOOT_STEPS);
        if maximum == 0 {
            return Err(HermesFault::BootFuelExhausted);
        }

        for index in 0..maximum {
            if tick_reached(self.backend.now_tick(), deadline_tick) {
                return Err(HermesFault::DeadlineExpired);
            }

            let Some(instruction) =
                self.codec
                    .boot_instruction(&self.identity, &self.evidence, stage, index as u32)?
            else {
                return Ok(());
            };

            validate_boot_instruction(stage, &instruction)?;

            if instruction.opcode == HERMES_BOOT_END {
                return Ok(());
            }

            self.execute_instruction(&instruction, deadline_tick)?;
        }

        Err(HermesFault::BootFuelExhausted)
    }

    fn execute_instruction(
        &mut self,
        instruction: &HermesBootInstruction,
        deadline_tick: u64,
    ) -> Result<(), HermesFault> {
        match instruction.opcode {
            HERMES_BOOT_WRITE32 => {
                let bar = word_u8(instruction.words[0])?;
                let offset = word_u32(instruction.words[1])?;
                let value = word_u32(instruction.words[2])?;
                self.write32(bar, offset, value)
            }
            HERMES_BOOT_RMW32 => {
                let bar = word_u8(instruction.words[0])?;
                let offset = word_u32(instruction.words[1])?;
                let clear_mask = word_u32(instruction.words[2])?;
                let set_mask = word_u32(instruction.words[3])?;
                let current = self.read_consensus32(bar, offset)?;
                let next = (current & !clear_mask) | set_mask;
                self.write32(bar, offset, next)
            }
            HERMES_BOOT_POLL32 => {
                let bar = word_u8(instruction.words[0])?;
                let offset = word_u32(instruction.words[1])?;
                let mask = word_u32(instruction.words[2])?;
                let expected = word_u32(instruction.words[3])?;
                let timeout = instruction.words[4];
                let local_deadline = self
                    .backend
                    .now_tick()
                    .checked_add(timeout)
                    .ok_or(HermesFault::ArithmeticOverflow)?;
                let effective_deadline = earlier_deadline(local_deadline, deadline_tick);
                self.poll32(bar, offset, mask, expected, effective_deadline)
            }
            HERMES_BOOT_DELAY => {
                let delay = instruction.words[0];
                let local_deadline = self
                    .backend
                    .now_tick()
                    .checked_add(delay)
                    .ok_or(HermesFault::ArithmeticOverflow)?;
                let effective_deadline = earlier_deadline(local_deadline, deadline_tick);
                while !tick_reached(self.backend.now_tick(), effective_deadline) {
                    self.backend.relax();
                }
                if effective_deadline == deadline_tick
                    && tick_reached(self.backend.now_tick(), deadline_tick)
                    && delay != 0
                {
                    return Err(HermesFault::DeadlineExpired);
                }
                Ok(())
            }
            HERMES_BOOT_FIRMWARE_ADDRESS => {
                let firmware = self.lease.firmware()?;
                let bar = word_u8(instruction.words[0])?;
                let low_offset = word_u32(instruction.words[1])?;
                let high_offset = word_u32(instruction.words[2])?;
                let length_offset = word_u32(instruction.words[3])?;
                self.write32(bar, low_offset, firmware.device_address as u32)?;
                self.write32(bar, high_offset, (firmware.device_address >> 32) as u32)?;
                self.write32(bar, length_offset, usize_to_u32(firmware.length)?)?;
                self.backend.io_fence()
            }
            HERMES_BOOT_COMMAND_QUEUE => {
                let ring = self.lease.command_ring()?;
                self.publish_queue(instruction, ring, self.profile.command_depth)
            }
            HERMES_BOOT_EVENT_QUEUE => {
                let ring = self.lease.event_ring()?;
                self.publish_queue(instruction, ring, self.profile.event_depth)
            }
            HERMES_BOOT_DOORBELL => {
                let bar = word_u8(instruction.words[0])?;
                let offset = word_u32(instruction.words[1])?;
                let value = word_u32(instruction.words[2])?;
                self.write32(bar, offset, value)?;
                self.backend.io_fence()
            }
            HERMES_BOOT_FENCE => self.backend.io_fence(),
            HERMES_BOOT_ASSERT32 => {
                let bar = word_u8(instruction.words[0])?;
                let offset = word_u32(instruction.words[1])?;
                let mask = word_u32(instruction.words[2])?;
                let expected = word_u32(instruction.words[3])?;
                let value = self.read_consensus32(bar, offset)?;
                if value & mask != expected & mask {
                    return Err(HermesFault::BootInstructionRejected);
                }
                Ok(())
            }
            _ => Err(HermesFault::BootInstructionRejected),
        }
    }

    fn publish_queue(
        &self,
        instruction: &HermesBootInstruction,
        ring: DmaRegion<Backend::Dma>,
        expected_depth: u16,
    ) -> Result<(), HermesFault> {
        let bar = word_u8(instruction.words[0])?;
        let low_offset = word_u32(instruction.words[1])?;
        let high_offset = word_u32(instruction.words[2])?;
        let depth_offset = word_u32(instruction.words[3])?;
        let slot_offset = word_u32(instruction.words[4])?;
        let declared_depth = word_u16(instruction.words[5])?;
        let declared_slot = word_u16(instruction.words[6])?;

        if declared_depth != expected_depth {
            return Err(HermesFault::QueueGeometry);
        }

        self.write32(bar, low_offset, ring.device_address as u32)?;
        self.write32(bar, high_offset, (ring.device_address >> 32) as u32)?;
        self.write32(bar, depth_offset, declared_depth as u32)?;
        self.write32(bar, slot_offset, declared_slot as u32)?;
        self.backend.io_fence()
    }

    fn await_ready(&self, deadline_tick: u64) -> Result<(), HermesFault> {
        self.poll32(
            self.profile.control_bar,
            self.profile.status_offset,
            self.profile.ready_mask,
            self.profile.ready_value,
            deadline_tick,
        )
    }

    fn poll32(
        &self,
        bar: u8,
        offset: u32,
        mask: u32,
        expected: u32,
        deadline_tick: u64,
    ) -> Result<(), HermesFault> {
        let mut samples = 0_usize;
        loop {
            let value = self.read_consensus32(bar, offset)?;
            if value & self.profile.fault_mask != 0 {
                return Err(HermesFault::DeviceFault);
            }
            if value & mask == expected & mask {
                return Ok(());
            }
            if tick_reached(self.backend.now_tick(), deadline_tick) {
                return Err(HermesFault::DeadlineExpired);
            }
            samples += 1;
            if samples >= MAXIMUM_POLL_SAMPLES {
                return Err(HermesFault::BootFuelExhausted);
            }
            self.backend.relax();
        }
    }

    fn read_consensus32(&self, bar: u8, offset: u32) -> Result<u32, HermesFault> {
        let first = self.read32(bar, offset)?;
        let second = self.read32(bar, offset)?;
        let third = self.read32(bar, offset)?;

        if first == second || first == third {
            Ok(first)
        } else if second == third {
            Ok(second)
        } else {
            Err(HermesFault::UnstableMmio)
        }
    }

    fn read32(&self, bar: u8, offset: u32) -> Result<u32, HermesFault> {
        let window = self.lease.bar(bar)?;
        validate_mmio_access(window, offset, 4)?;
        self.backend.read32(window, offset)
    }

    fn write32(&self, bar: u8, offset: u32, value: u32) -> Result<(), HermesFault> {
        let window = self.lease.bar(bar)?;
        validate_mmio_access(window, offset, 4)?;
        self.backend.write32(window, offset, value)
    }

    fn send_wire(&mut self, command: &HermesNormalizedCommand) -> Result<(), HermesFault> {
        let depth = self.profile.command_depth as u32;
        let slot_bytes = self.profile.command_slot_bytes as usize;
        let consumer = self.read32(
            self.profile.control_bar,
            self.profile.command_consumer_offset,
        )?;

        let outstanding = self.runtime.command_producer.wrapping_sub(consumer);
        if outstanding > depth {
            return Err(HermesFault::QueueCorrupt);
        }
        if outstanding == depth {
            return Err(HermesFault::QueueFull);
        }

        let mut wire = [0_u8; MAXIMUM_WIRE_BYTES];
        let encoded = self
            .codec
            .encode_command(&self.profile, command, &mut wire[..slot_bytes])?;
        if encoded == 0
            || encoded > slot_bytes
            || encoded > self.profile.maximum_wire_bytes as usize
        {
            return Err(HermesFault::CodecRejected);
        }

        let ring = self.lease.command_ring()?;
        let slot = (self.runtime.command_producer & (depth - 1)) as usize;
        let offset = slot
            .checked_mul(slot_bytes)
            .ok_or(HermesFault::ArithmeticOverflow)?;

        self.backend.dma_write(ring, offset, &wire[..slot_bytes])?;
        self.backend.dma_publish(ring, offset, slot_bytes)?;
        self.backend.io_fence()?;

        self.runtime.command_producer = self.runtime.command_producer.wrapping_add(1);
        self.write32(
            self.profile.control_bar,
            self.profile.command_producer_offset,
            self.runtime.command_producer,
        )?;
        self.write32(
            self.profile.doorbell_bar,
            self.profile.command_doorbell_offset,
            self.runtime.command_producer,
        )?;
        self.backend.io_fence()
    }

    fn read_wire_event(&mut self) -> Result<Option<HermesNormalizedEvent>, HermesFault> {
        let producer = self.read32(self.profile.control_bar, self.profile.event_producer_offset)?;
        let depth = self.profile.event_depth as u32;
        let available = producer.wrapping_sub(self.runtime.event_consumer);

        if available > depth {
            return Err(HermesFault::QueueCorrupt);
        }
        if available == 0 {
            return Ok(None);
        }

        let slot_bytes = self.profile.event_slot_bytes as usize;
        let slot = (self.runtime.event_consumer & (depth - 1)) as usize;
        let offset = slot
            .checked_mul(slot_bytes)
            .ok_or(HermesFault::ArithmeticOverflow)?;
        let ring = self.lease.event_ring()?;
        let mut wire = [0_u8; MAXIMUM_WIRE_BYTES];

        self.backend.dma_acquire(ring, offset, slot_bytes)?;
        self.backend
            .dma_read(ring, offset, &mut wire[..slot_bytes])?;

        self.runtime.event_consumer = self.runtime.event_consumer.wrapping_add(1);
        self.write32(
            self.profile.control_bar,
            self.profile.event_consumer_offset,
            self.runtime.event_consumer,
        )?;
        self.write32(
            self.profile.doorbell_bar,
            self.profile.event_doorbell_offset,
            self.runtime.event_consumer,
        )?;
        self.backend.io_fence()?;

        let event = self
            .codec
            .decode_event(&self.profile, &wire[..slot_bytes])?;

        if (event.struct_size as usize) < core::mem::size_of::<HermesNormalizedEvent>()
            || event.payload_length as usize > HERMES_MAX_NORMALIZED_PAYLOAD
            || event.epoch != self.runtime.epoch
        {
            return Err(HermesFault::ProtocolMismatch);
        }

        Ok(Some(event))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventDisposition {
    Reply { epoch: u32, sequence: u32 },
    Asynchronous,
    Unclassified,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReceivedEvent {
    pub event: HermesNormalizedEvent,
    pub disposition: EventDisposition,
}

fn validate_identity(identity: &HermesPciIdentity) -> Result<(), HermesFault> {
    if identity.vendor_id != NVIDIA_VENDOR_ID {
        return Err(HermesFault::NotNvidia);
    }
    if identity.class_code != PCI_CLASS_DISPLAY {
        return Err(HermesFault::NotDisplayController);
    }
    if identity.slot >= 32 || identity.function >= 8 {
        return Err(HermesFault::PersonalityRejected);
    }
    Ok(())
}

fn validate_profile(
    personality_id: u64,
    profile: &HermesTransportProfile,
    evidence: &HermesProbeEvidence,
) -> Result<(), HermesFault> {
    if (profile.struct_size as usize) < core::mem::size_of::<HermesTransportProfile>()
        || profile.profile_version == 0
        || profile.personality_id != personality_id
        || profile.protocol_family == 0
        || profile.wire_major == 0
        || profile.wire_minor_minimum > profile.wire_minor_maximum
        || profile.control_bar as usize >= MAXIMUM_BARS
        || profile.doorbell_bar as usize >= MAXIMUM_BARS
        || profile.maximum_boot_steps == 0
        || profile.maximum_boot_steps as usize > MAXIMUM_BOOT_STEPS
        || profile.maximum_wire_bytes == 0
        || profile.maximum_wire_bytes as usize > MAXIMUM_WIRE_BYTES
    {
        return Err(HermesFault::ProfileRejected);
    }

    validate_ring_geometry(profile.command_depth, profile.command_slot_bytes)?;
    validate_ring_geometry(profile.event_depth, profile.event_slot_bytes)?;

    if profile.command_slot_bytes as u32 > profile.maximum_wire_bytes
        || profile.event_slot_bytes as u32 > profile.maximum_wire_bytes
    {
        return Err(HermesFault::QueueGeometry);
    }

    if profile.firmware_minimum_bytes > profile.firmware_maximum_bytes
        || (profile.firmware_maximum_bytes != 0 && profile.firmware_alignment == 0)
        || (profile.firmware_alignment != 0 && !profile.firmware_alignment.is_power_of_two())
    {
        return Err(HermesFault::FirmwareAlignment);
    }

    for bar in 0..MAXIMUM_BARS {
        let required = profile.required_bar_lengths[bar];
        if required > evidence.bar_lengths[bar] {
            return Err(HermesFault::BarUnavailable);
        }
    }

    let register_offsets = [
        (profile.control_bar, profile.command_producer_offset),
        (profile.control_bar, profile.command_consumer_offset),
        (profile.control_bar, profile.event_producer_offset),
        (profile.control_bar, profile.event_consumer_offset),
        (profile.control_bar, profile.status_offset),
        (profile.doorbell_bar, profile.command_doorbell_offset),
        (profile.doorbell_bar, profile.event_doorbell_offset),
    ];

    for (bar, offset) in register_offsets {
        let length = profile.required_bar_lengths[bar as usize];
        if length == 0
            || (offset as u64)
                .checked_add(4)
                .is_none_or(|end| end > length)
        {
            return Err(HermesFault::MmioOutOfRange);
        }
    }

    Ok(())
}

fn validate_ring_geometry(depth: u16, slot_bytes: u16) -> Result<(), HermesFault> {
    if depth < 2
        || !depth.is_power_of_two()
        || slot_bytes == 0
        || slot_bytes as usize > MAXIMUM_WIRE_BYTES
        || slot_bytes as usize % 4 != 0
    {
        return Err(HermesFault::QueueGeometry);
    }
    Ok(())
}

fn validate_firmware(
    profile: &HermesTransportProfile,
    image: &FirmwareImage<'_>,
) -> Result<(), HermesFault> {
    if profile.firmware_maximum_bytes == 0 {
        return Err(HermesFault::FirmwareUnexpected);
    }

    let length = u32::try_from(image.bytes.len()).map_err(|_| HermesFault::FirmwareSize)?;
    if length < profile.firmware_minimum_bytes || length > profile.firmware_maximum_bytes {
        return Err(HermesFault::FirmwareSize);
    }

    let alignment = profile.firmware_alignment as usize;
    if alignment == 0 || !alignment.is_power_of_two() {
        return Err(HermesFault::FirmwareAlignment);
    }

    if image.manifest_hash == [0; 32] || image.version == 0 {
        return Err(HermesFault::FirmwareRejected);
    }

    Ok(())
}

fn validate_dma_region<Handle: Copy + Eq>(
    region: DmaRegion<Handle>,
    expected_length: usize,
    expected_alignment: usize,
) -> Result<(), HermesFault> {
    if region.length < expected_length
        || region.alignment < expected_alignment
        || region.device_address == 0
        || region.device_address % expected_alignment as u64 != 0
        || region
            .device_address
            .checked_add(expected_length as u64)
            .is_none()
    {
        return Err(HermesFault::DmaAddressOverflow);
    }
    Ok(())
}

fn validate_mmio_access<Handle: Copy + Eq>(
    window: MmioWindow<Handle>,
    offset: u32,
    width: u64,
) -> Result<(), HermesFault> {
    if offset & 3 != 0
        || (offset as u64)
            .checked_add(width)
            .is_none_or(|end| end > window.length)
    {
        return Err(HermesFault::MmioOutOfRange);
    }
    Ok(())
}

fn validate_boot_instruction(
    stage: u32,
    instruction: &HermesBootInstruction,
) -> Result<(), HermesFault> {
    if instruction.instruction_version == 0
        || (instruction.struct_size as usize) < core::mem::size_of::<HermesBootInstruction>()
        || instruction.stage != stage
    {
        return Err(HermesFault::BootInstructionRejected);
    }
    Ok(())
}

fn ring_bytes(depth: u16, slot_bytes: u16) -> Result<usize, HermesFault> {
    validate_ring_geometry(depth, slot_bytes)?;
    (depth as usize)
        .checked_mul(slot_bytes as usize)
        .ok_or(HermesFault::ArithmeticOverflow)
}

fn zero_dma<Backend: HermesPlatform + ?Sized>(
    backend: &Backend,
    region: DmaRegion<Backend::Dma>,
) -> Result<(), HermesFault> {
    let zeroes = [0_u8; 128];
    let mut offset = 0_usize;
    while offset < region.length {
        let count = (region.length - offset).min(zeroes.len());
        backend.dma_write(region, offset, &zeroes[..count])?;
        offset += count;
    }
    backend.dma_publish(region, 0, region.length)
}

fn word_u8(word: u64) -> Result<u8, HermesFault> {
    u8::try_from(word).map_err(|_| HermesFault::BootInstructionRejected)
}

fn word_u16(word: u64) -> Result<u16, HermesFault> {
    u16::try_from(word).map_err(|_| HermesFault::BootInstructionRejected)
}

fn word_u32(word: u64) -> Result<u32, HermesFault> {
    u32::try_from(word).map_err(|_| HermesFault::BootInstructionRejected)
}

fn usize_to_u32(value: usize) -> Result<u32, HermesFault> {
    u32::try_from(value).map_err(|_| HermesFault::ArithmeticOverflow)
}

fn tick_reached(now: u64, deadline: u64) -> bool {
    now.wrapping_sub(deadline) < (1_u64 << 63)
}

fn earlier_deadline(first: u64, second: u64) -> u64 {
    if tick_reached(first, second) {
        second
    } else {
        first
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_nvidia_identity() {
        let identity = HermesPciIdentity {
            segment: 0,
            bus: 1,
            slot: 0,
            function: 0,
            revision: 0,
            vendor_id: 0x1234,
            device_id: 1,
            subsystem_vendor_id: 0,
            subsystem_device_id: 0,
            class_code: PCI_CLASS_DISPLAY,
            subclass: 0,
            programming_interface: 0,
            reserved: 0,
        };
        assert_eq!(validate_identity(&identity), Err(HermesFault::NotNvidia));
    }

    #[test]
    fn ring_geometry_is_bounded_and_power_of_two() {
        assert!(validate_ring_geometry(256, 256).is_ok());
        assert_eq!(
            validate_ring_geometry(255, 256),
            Err(HermesFault::QueueGeometry)
        );
        assert_eq!(
            validate_ring_geometry(256, 0),
            Err(HermesFault::QueueGeometry)
        );
    }

    #[test]
    fn wrapped_deadlines_preserve_half_range_ordering() {
        assert!(tick_reached(100, 100));
        assert!(tick_reached(101, 100));
        assert!(!tick_reached(99, 100));
    }
}
