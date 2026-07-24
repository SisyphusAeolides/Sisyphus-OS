use core::marker::PhantomData;

use sisyphus_driver_abi::gpu::{
    GpuCompatibilityManifest, GpuCompatibilityProof, GpuDeviceEvidence, evaluate_compatibility,
};
use sisyphus_driver_abi::hermes::{
    HERMES_BOOT_STAGE_FIRMWARE, HERMES_BOOT_STAGE_IGNITE, HERMES_BOOT_STAGE_NEGOTIATE,
    HERMES_BOOT_STAGE_QUEUES, HERMES_EVENT_ASYNC, HERMES_EVENT_FAULT, HERMES_EVENT_REPLY,
    HERMES_MAX_NORMALIZED_PAYLOAD, HermesBootInstruction, HermesNormalizedCommand,
    HermesNormalizedEvent, HermesPciIdentity, HermesProbeEvidence, HermesTransportProfile,
};

use super::hermes_service::{
    HermesAdmissionCertificate, HermesAdmissionFault, HermesServiceController, HermesServiceCurve,
};
use super::{
    drivernet::fingerprint::GpuFingerprint,
    gpu_portability::portable_evidence as portable_gpu_evidence,
};

pub const NVIDIA_VENDOR_ID: u16 = 0x10de;
pub const PCI_CLASS_DISPLAY: u8 = 0x03;

pub const HERMES_OPCODE_NEGOTIATE: u32 = 0x0000_0001;
pub const HERMES_OPCODE_SHUTDOWN: u32 = 0x0000_0002;

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

/// Immutable Hermes inputs derived from one measured DriverNet fingerprint.
///
/// This object proves only that discovery produced structurally consistent
/// identity, BAR, and portable compatibility evidence. It does not claim that
/// an IOMMU domain, DMA memory, firmware, or a transport personality exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HermesDiscovery {
    identity: HermesPciIdentity,
    probe_evidence: HermesProbeEvidence,
    portable_evidence: GpuDeviceEvidence,
}

impl HermesDiscovery {
    pub fn from_fingerprint(fingerprint: &GpuFingerprint) -> Result<Self, HermesFault> {
        let identity = HermesPciIdentity {
            segment: fingerprint.segment,
            bus: fingerprint.bus,
            slot: fingerprint.slot,
            function: fingerprint.function,
            revision: fingerprint.revision,
            vendor_id: fingerprint.vendor_id,
            device_id: fingerprint.device_id,
            subsystem_vendor_id: fingerprint.subsystem_vendor_id,
            subsystem_device_id: fingerprint.subsystem_device_id,
            class_code: fingerprint.class_code,
            subclass: fingerprint.subclass,
            programming_interface: fingerprint.programming_interface,
            reserved: 0,
        };
        validate_identity(&identity)?;

        let portable_evidence = portable_gpu_evidence(fingerprint);
        if !portable_evidence.valid() {
            return Err(HermesFault::CompatibilityRejected);
        }

        let mut probe_evidence = HermesProbeEvidence::empty();
        for (length, bar) in probe_evidence
            .bar_lengths
            .iter_mut()
            .zip(portable_evidence.bars)
        {
            *length = if bar.usable_mmio() { bar.length } else { 0 };
        }
        probe_evidence.bootrom_revision = portable_evidence.bootrom_revision;
        probe_evidence.architecture_hint = portable_evidence.architecture_hint;
        // PCI capability bits do not prove any Hermes transport feature. A
        // personality must establish those features during negotiation.
        probe_evidence.observed_features = 0;

        Ok(Self {
            identity,
            probe_evidence,
            portable_evidence,
        })
    }

    pub const fn identity(&self) -> HermesPciIdentity {
        self.identity
    }

    pub const fn probe_evidence(&self) -> HermesProbeEvidence {
        self.probe_evidence
    }

    pub const fn portable_evidence(&self) -> &GpuDeviceEvidence {
        &self.portable_evidence
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HermesFault {
    NotNvidia,
    NotDisplayController,
    NoPersonality,
    AmbiguousPersonality,
    PersonalityCapacity,
    PersonalityRejected,
    CompatibilityRejected,
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
    ServiceCurveRejected,
    ServiceBacklogSaturated,
    ServiceArrivalEnvelopeExceeded,
    ServiceDeadlineUnsafe,
    ServiceTimeRegression,
    ServiceReservationCorrupt,
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
    CorrelationSpaceExhausted,
    ArithmeticOverflow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DmaPurpose {
    Firmware,
    CommandRing,
    EventRing,
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

    fn compatibility_manifest(&self) -> GpuCompatibilityManifest;

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

#[derive(Clone, Copy)]
pub struct ResolvedPersonality<'a> {
    pub codec: &'a dyn HermesCodec,
    pub proof: GpuCompatibilityProof,
    pub score: u32,
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
        let manifest = codec.compatibility_manifest();
        if codec.personality_id() == 0
            || !manifest.valid()
            || manifest.driver_id != codec.personality_id()
            || manifest.vendor_id != NVIDIA_VENDOR_ID
        {
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
        portable_evidence: &GpuDeviceEvidence,
        proof_secret: u64,
        minimum_score: u32,
    ) -> Result<ResolvedPersonality<'a>, HermesFault> {
        if proof_secret == 0 || !portable_identity_matches(identity, portable_evidence) {
            return Err(HermesFault::CompatibilityRejected);
        }

        let mut winner: Option<ResolvedPersonality<'a>> = None;
        let mut tied = false;

        for codec in self.entries[..self.length].iter().flatten().copied() {
            let proof = evaluate_compatibility(
                &codec.compatibility_manifest(),
                portable_evidence,
                proof_secret,
            );
            if !proof.accepted() {
                continue;
            }

            let codec_score = codec.match_device(identity, evidence)?;
            if codec_score < minimum_score {
                continue;
            }
            let score = codec_score.saturating_add(proof.score_q16);
            let candidate = ResolvedPersonality {
                codec,
                proof,
                score,
            };

            match winner {
                None => {
                    winner = Some(candidate);
                    tied = false;
                }
                Some(current) if score > current.score => {
                    winner = Some(candidate);
                    tied = false;
                }
                Some(current) if score == current.score => {
                    tied = true;
                }
                Some(_) => {}
            }
        }

        if tied {
            return Err(HermesFault::AmbiguousPersonality);
        }

        winner.ok_or(HermesFault::NoPersonality)
    }
}

fn portable_identity_matches(identity: &HermesPciIdentity, portable: &GpuDeviceEvidence) -> bool {
    identity.segment == portable.identity.segment
        && identity.bus == portable.identity.bus
        && identity.slot == portable.identity.slot
        && identity.function == portable.identity.function
        && identity.revision == portable.identity.revision
        && identity.vendor_id == portable.identity.vendor_id
        && identity.device_id == portable.identity.device_id
        && identity.subsystem_vendor_id == portable.identity.subsystem_vendor_id
        && identity.subsystem_device_id == portable.identity.subsystem_device_id
        && identity.class_code == portable.identity.class_code
        && identity.subclass == portable.identity.subclass
        && identity.programming_interface == portable.identity.programming_interface
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
    admission: HermesAdmissionCertificate,
}

impl PendingRequest {
    const EMPTY: Self = Self {
        live: false,
        epoch: 0,
        sequence: 0,
        opcode: 0,
        deadline_tick: 0,
        admission: HermesAdmissionCertificate::EMPTY,
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

    fn take_response(
        &mut self,
        event: &HermesNormalizedEvent,
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

        let completed = *entry;
        *entry = PendingRequest::EMPTY;
        Ok(completed)
    }

    fn live_count(&self) -> usize {
        self.entries.iter().filter(|entry| entry.live).count()
    }

    fn expire_one(&mut self, now_tick: u64) -> Option<PendingRequest> {
        let entry = self
            .entries
            .iter_mut()
            .find(|entry| entry.live && tick_reached(now_tick, entry.deadline_tick))?;

        let expired = *entry;
        *entry = PendingRequest::EMPTY;
        Some(expired)
    }
}

struct Runtime<const PENDING: usize> {
    epoch: u32,
    next_sequence: u32,
    command_producer: u32,
    event_consumer: u32,
    negotiated_features: u64,
    poisoned: bool,
    pending: PendingTable<PENDING>,
    service: HermesServiceController,
    last_admission: HermesAdmissionCertificate,
}

impl<const PENDING: usize> Runtime<PENDING> {
    fn new(
        identity: &HermesPciIdentity,
        profile: &HermesTransportProfile,
        now_tick: u64,
        service_secret: u64,
    ) -> Result<Self, HermesFault> {
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

        let service = HermesServiceController::new(
            HermesServiceCurve {
                minimum_completions_per_window: profile.minimum_completions_per_window,
                maximum_admissions_per_window: profile.maximum_admissions_per_window,
                window_ticks: profile.service_window_ticks,
                latency_ticks: profile.service_latency_ticks,
                maximum_backlog: profile.maximum_admission_backlog,
            },
            now_tick,
            service_secret,
        )
        .map_err(|_| HermesFault::ServiceCurveRejected)?;

        Ok(Self {
            epoch,
            next_sequence: 1,
            command_producer: 0,
            event_consumer: 0,
            negotiated_features: 0,
            poisoned: false,
            pending: PendingTable::new(),
            service,
            last_admission: HermesAdmissionCertificate::EMPTY,
        })
    }

    /// Allocates one correlation identity without ever reusing an earlier
    /// `(epoch, sequence)` pair. Sequence zero is the permanently retired
    /// state reached after the terminal `u32::MAX/u32::MAX` identity.
    fn allocate_correlation(&mut self) -> Result<(u32, u32), HermesFault> {
        if self.epoch == 0 || self.next_sequence == 0 {
            return Err(HermesFault::CorrelationSpaceExhausted);
        }

        let correlation = (self.epoch, self.next_sequence);
        if self.next_sequence == u32::MAX {
            match self.epoch.checked_add(1) {
                Some(next_epoch) => {
                    self.epoch = next_epoch;
                    self.next_sequence = 1;
                }
                None => {
                    // The terminal pair remains valid for the caller, but no
                    // future command may alias an identity from epoch one.
                    self.next_sequence = 0;
                }
            }
        } else {
            self.next_sequence += 1;
        }
        Ok(correlation)
    }
}

struct HermesLease<'a, Backend: HermesPlatform + ?Sized> {
    backend: &'a Backend,
    domain: Option<Backend::Domain>,
    bars: [Option<MmioWindow<Backend::Mmio>>; MAXIMUM_BARS],
    firmware: Option<DmaRegion<Backend::Dma>>,
    command_ring: Option<DmaRegion<Backend::Dma>>,
    event_ring: Option<DmaRegion<Backend::Dma>>,
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
    compatibility_proof: GpuCompatibilityProof,
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
        portable_evidence: &GpuDeviceEvidence,
        compatibility_secret: u64,
        minimum_personality_score: u32,
    ) -> Result<Self, HermesFault> {
        validate_identity(&identity)?;

        let resolved = registry.resolve(
            &identity,
            &evidence,
            portable_evidence,
            compatibility_secret,
            minimum_personality_score,
        )?;
        let codec = resolved.codec;
        let profile = codec.describe_transport(&identity, &evidence)?;
        validate_profile(codec.personality_id(), &profile, &evidence)?;
        if PENDING == 0 || usize::from(profile.maximum_admission_backlog) > PENDING {
            return Err(HermesFault::ServiceCurveRejected);
        }

        Ok(Self {
            backend,
            codec,
            identity,
            evidence,
            profile,
            compatibility_proof: resolved.proof,
            firmware_seal: None,
            lease: HermesLease::new(backend),
            runtime: Runtime::new(
                &identity,
                &profile,
                backend.now_tick(),
                resolved.proof.proof_root,
            )?,
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
            self.lease.bars[bar] = Some(window);
            if window.length < required {
                return Err(HermesFault::BarUnavailable);
            }
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
                self.lease.firmware = Some(region);
                validate_dma_region(region, image.bytes.len(), alignment)?;
                self.backend.dma_write(region, 0, image.bytes)?;
                self.backend.dma_publish(region, 0, image.bytes.len())?;
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
        self.lease.command_ring = Some(command_ring);
        validate_dma_region(command_ring, command_bytes, DMA_GRANULE)?;
        zero_dma(self.backend, command_ring)?;

        let event_ring =
            self.backend
                .allocate_dma(domain, event_bytes, DMA_GRANULE, DmaPurpose::EventRing)?;
        self.lease.event_ring = Some(event_ring);
        validate_dma_region(event_ring, event_bytes, DMA_GRANULE)?;
        zero_dma(self.backend, event_ring)?;

        self.codec.reset(&self.profile, self.runtime.epoch)?;

        Ok(Hermes {
            backend: self.backend,
            codec: self.codec,
            identity: self.identity,
            evidence: self.evidence,
            profile: self.profile,
            compatibility_proof: self.compatibility_proof,
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
        let (epoch, sequence) = self.runtime.allocate_correlation()?;
        negotiate.epoch = epoch;
        negotiate.sequence = sequence;
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

        Ok(Hermes {
            backend: self.backend,
            codec: self.codec,
            identity: self.identity,
            evidence: self.evidence,
            profile: self.profile,
            compatibility_proof: self.compatibility_proof,
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

    pub const fn compatibility_proof(&self) -> GpuCompatibilityProof {
        self.compatibility_proof
    }

    pub const fn negotiated_features(&self) -> u64 {
        self.runtime.negotiated_features
    }

    pub const fn transport_poisoned(&self) -> bool {
        self.runtime.poisoned
    }

    pub const fn firmware_seal(&self) -> Option<FirmwareSeal> {
        self.firmware_seal
    }

    pub const fn service_curve(&self) -> HermesServiceCurve {
        self.runtime.service.curve()
    }

    pub const fn last_admission_certificate(&self) -> HermesAdmissionCertificate {
        self.runtime.last_admission
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
        if self.runtime.poisoned {
            return Err(HermesFault::RecoveryRequired);
        }
        if opcode == 0 || payload.len() > HERMES_MAX_NORMALIZED_PAYLOAD {
            return Err(HermesFault::ProtocolMismatch);
        }
        let now_tick = self.backend.now_tick();
        if tick_reached(now_tick, deadline_tick) {
            return Err(HermesFault::DeadlineExpired);
        }

        let admission = self
            .runtime
            .service
            .admit(now_tick, deadline_tick, self.runtime.pending.live_count())
            .map_err(map_admission_fault)?;

        let (epoch, sequence) = match self.runtime.allocate_correlation() {
            Ok(correlation) => correlation,
            Err(error) => {
                self.runtime
                    .service
                    .rollback(admission)
                    .map_err(|_| HermesFault::ServiceReservationCorrupt)?;
                return Err(error);
            }
        };

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

        if let Err(error) = self.runtime.pending.insert(PendingRequest {
            live: true,
            epoch,
            sequence,
            opcode,
            deadline_tick,
            admission,
        }) {
            self.runtime
                .service
                .rollback(admission)
                .map_err(|_| HermesFault::ServiceReservationCorrupt)?;
            return Err(error);
        }

        if let Err(error) = self.send_wire(&command) {
            self.runtime.pending.cancel(epoch, sequence);
            self.runtime
                .service
                .rollback(admission)
                .map_err(|_| HermesFault::ServiceReservationCorrupt)?;
            return Err(error);
        }

        self.runtime.last_admission = admission;
        Ok((epoch, sequence))
    }

    pub fn poll_event(&mut self) -> Result<Option<ReceivedEvent>, HermesFault> {
        if self.runtime.poisoned {
            return Err(HermesFault::RecoveryRequired);
        }
        let Some(event) = self.read_wire_event()? else {
            return Ok(None);
        };

        let now_tick = self.backend.now_tick();
        if event.event_kind == HERMES_EVENT_FAULT {
            self.runtime.poisoned = true;
            if event.correlation_epoch != 0 && event.correlation_sequence != 0 {
                if let Ok(pending) = self.runtime.pending.take_response(&event) {
                    self.runtime
                        .service
                        .observe_completion(pending.admission, now_tick)
                        .map_err(map_admission_fault)?;
                }
            }
            return Err(HermesFault::RecoveryRequired);
        }

        let disposition = if event.event_kind == HERMES_EVENT_REPLY {
            let pending = match self.runtime.pending.take_response(&event) {
                Ok(pending) => pending,
                Err(error) => return self.poison(error),
            };
            if let Err(error) = self
                .runtime
                .service
                .observe_completion(pending.admission, now_tick)
            {
                return self.poison(map_admission_fault(error));
            }

            if tick_reached(now_tick, pending.deadline_tick) {
                return Err(HermesFault::ResponseExpired);
            }
            if pending.opcode != event.opcode {
                return self.poison(HermesFault::ResponseMismatch);
            }

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

    pub fn expire_one(&mut self) -> Result<Option<(u32, u32)>, HermesFault> {
        if self.runtime.poisoned {
            return Err(HermesFault::RecoveryRequired);
        }
        let now_tick = self.backend.now_tick();
        let Some(expired) = self.runtime.pending.expire_one(now_tick) else {
            return Ok(None);
        };
        self.runtime
            .service
            .observe_completion(expired.admission, now_tick)
            .map_err(map_admission_fault)?;
        Ok(Some((expired.epoch, expired.sequence)))
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
                self.publish_queue(
                    instruction,
                    ring,
                    self.profile.command_depth,
                    self.profile.command_slot_bytes,
                )
            }
            HERMES_BOOT_EVENT_QUEUE => {
                let ring = self.lease.event_ring()?;
                self.publish_queue(
                    instruction,
                    ring,
                    self.profile.event_depth,
                    self.profile.event_slot_bytes,
                )
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
        expected_slot: u16,
    ) -> Result<(), HermesFault> {
        let bar = word_u8(instruction.words[0])?;
        let low_offset = word_u32(instruction.words[1])?;
        let high_offset = word_u32(instruction.words[2])?;
        let depth_offset = word_u32(instruction.words[3])?;
        let slot_offset = word_u32(instruction.words[4])?;
        let declared_depth = word_u16(instruction.words[5])?;
        let declared_slot = word_u16(instruction.words[6])?;

        validate_queue_publication(
            ring.length,
            expected_depth,
            expected_slot,
            declared_depth,
            declared_slot,
        )?;

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
            return self.poison(HermesFault::QueueCorrupt);
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
        if let Err(error) = self.backend.dma_publish(ring, offset, slot_bytes) {
            return self.poison(error);
        }
        if let Err(error) = self.backend.io_fence() {
            return self.poison(error);
        }

        let next_producer = self.runtime.command_producer.wrapping_add(1);
        if let Err(error) = self.write32(
            self.profile.control_bar,
            self.profile.command_producer_offset,
            next_producer,
        ) {
            return self.poison(error);
        }
        if let Err(error) = self.write32(
            self.profile.doorbell_bar,
            self.profile.command_doorbell_offset,
            next_producer,
        ) {
            return self.poison(error);
        }
        if let Err(error) = self.backend.io_fence() {
            return self.poison(error);
        }
        self.runtime.command_producer = next_producer;
        Ok(())
    }

    fn read_wire_event(&mut self) -> Result<Option<HermesNormalizedEvent>, HermesFault> {
        let producer = self.read32(self.profile.control_bar, self.profile.event_producer_offset)?;
        let depth = self.profile.event_depth as u32;
        let available = producer.wrapping_sub(self.runtime.event_consumer);

        if available > depth {
            return self.poison(HermesFault::QueueCorrupt);
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

        let event = match self.codec.decode_event(&self.profile, &wire[..slot_bytes]) {
            Ok(event) => event,
            Err(error) => return self.poison(error),
        };

        if (event.struct_size as usize) < core::mem::size_of::<HermesNormalizedEvent>()
            || event.payload_length as usize > HERMES_MAX_NORMALIZED_PAYLOAD
            || event.epoch != self.runtime.epoch
        {
            return self.poison(HermesFault::ProtocolMismatch);
        }

        let next_consumer = self.runtime.event_consumer.wrapping_add(1);
        if let Err(error) = self.write32(
            self.profile.control_bar,
            self.profile.event_consumer_offset,
            next_consumer,
        ) {
            return self.poison(error);
        }
        if let Err(error) = self.write32(
            self.profile.doorbell_bar,
            self.profile.event_doorbell_offset,
            next_consumer,
        ) {
            return self.poison(error);
        }
        if let Err(error) = self.backend.io_fence() {
            return self.poison(error);
        }
        self.runtime.event_consumer = next_consumer;

        Ok(Some(event))
    }

    fn poison<T>(&mut self, error: HermesFault) -> Result<T, HermesFault> {
        self.runtime.poisoned = true;
        Err(error)
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

fn map_admission_fault(fault: HermesAdmissionFault) -> HermesFault {
    match fault {
        HermesAdmissionFault::InvalidCurve | HermesAdmissionFault::ArithmeticOverflow => {
            HermesFault::ServiceCurveRejected
        }
        HermesAdmissionFault::BacklogSaturated => HermesFault::ServiceBacklogSaturated,
        HermesAdmissionFault::ArrivalEnvelopeExceeded => {
            HermesFault::ServiceArrivalEnvelopeExceeded
        }
        HermesAdmissionFault::DeadlineUnsafe => HermesFault::ServiceDeadlineUnsafe,
        HermesAdmissionFault::TimeRegression => HermesFault::ServiceTimeRegression,
        HermesAdmissionFault::StaleReservation | HermesAdmissionFault::CorruptObservation => {
            HermesFault::ServiceReservationCorrupt
        }
    }
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

    if profile.minimum_completions_per_window == 0
        || profile.maximum_admissions_per_window == 0
        || profile.maximum_admission_backlog == 0
        || profile.maximum_admission_backlog >= profile.command_depth
        || profile.minimum_completions_per_window > profile.maximum_admission_backlog
        || profile.maximum_admissions_per_window > profile.maximum_admission_backlog
        || profile.service_window_ticks == 0
    {
        return Err(HermesFault::ServiceCurveRejected);
    }

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

fn validate_queue_publication(
    ring_length: usize,
    expected_depth: u16,
    expected_slot: u16,
    declared_depth: u16,
    declared_slot: u16,
) -> Result<(), HermesFault> {
    if declared_depth != expected_depth
        || declared_slot != expected_slot
        || ring_length < ring_bytes(expected_depth, expected_slot)?
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
    use crate::drivers::drivernet::fingerprint::{
        BAR_64BIT, BAR_IO, BAR_PRESENT, BarEvidence, TOPOLOGY_IOMMU_ISOLATED,
    };
    use crate::drivers::gpu_portability::{HERMES_DRIVER_ID, manifest_for};
    use crate::sync::SpinLock;
    use sisyphus_driver_abi::gpu::{
        GPU_BAR_PRESENT, GPU_TOPOLOGY_IOMMU_ISOLATED, GpuBarEvidence, GpuDeviceEvidence,
    };

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FaultPoint {
        ShortBar,
        FirmwarePublish,
        CommandValidation,
        CommandPublish,
        EventAllocation,
        EventInitialization,
        CommandDoorbell,
        EventDecode,
        EventAcknowledge,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Cleanup {
        None,
        Dma(DmaPurpose),
        Bar(u8),
        Domain,
    }

    struct PlatformState {
        cleanup: [Cleanup; 8],
        cleanup_length: usize,
    }

    impl PlatformState {
        const fn new() -> Self {
            Self {
                cleanup: [Cleanup::None; 8],
                cleanup_length: 0,
            }
        }

        fn record_cleanup(&mut self, cleanup: Cleanup) {
            self.cleanup[self.cleanup_length] = cleanup;
            self.cleanup_length += 1;
        }
    }

    struct FaultPlatform {
        fault: FaultPoint,
        state: SpinLock<PlatformState>,
    }

    impl FaultPlatform {
        const fn new(fault: FaultPoint) -> Self {
            Self {
                fault,
                state: SpinLock::new(PlatformState::new()),
            }
        }

        fn assert_cleanup(&self, expected: &[Cleanup]) {
            let state = self.state.lock();
            assert_eq!(state.cleanup_length, expected.len());
            assert_eq!(&state.cleanup[..state.cleanup_length], expected);
        }
    }

    impl HermesPlatform for FaultPlatform {
        type Domain = u8;
        type Mmio = u8;
        type Dma = u8;

        fn isolate_device(
            &self,
            _identity: HermesPciIdentity,
        ) -> Result<Self::Domain, HermesFault> {
            Ok(1)
        }

        fn release_domain(&self, _domain: Self::Domain) {
            self.state.lock().record_cleanup(Cleanup::Domain);
        }

        fn map_bar(
            &self,
            _domain: Self::Domain,
            bar: u8,
            minimum_length: u64,
        ) -> Result<MmioWindow<Self::Mmio>, HermesFault> {
            Ok(MmioWindow {
                handle: bar,
                bar,
                length: if self.fault == FaultPoint::ShortBar {
                    minimum_length - 1
                } else {
                    minimum_length
                },
            })
        }

        fn unmap_bar(&self, window: MmioWindow<Self::Mmio>) {
            self.state.lock().record_cleanup(Cleanup::Bar(window.bar));
        }

        fn read32(&self, _window: MmioWindow<Self::Mmio>, offset: u32) -> Result<u32, HermesFault> {
            if offset == 8
                && matches!(
                    self.fault,
                    FaultPoint::EventDecode | FaultPoint::EventAcknowledge
                )
            {
                Ok(1)
            } else {
                Ok(0)
            }
        }

        fn write32(
            &self,
            _window: MmioWindow<Self::Mmio>,
            offset: u32,
            _value: u32,
        ) -> Result<(), HermesFault> {
            if (self.fault == FaultPoint::CommandDoorbell && offset == 16)
                || (self.fault == FaultPoint::EventAcknowledge && offset == 12)
            {
                Err(HermesFault::MmioWrite)
            } else {
                Ok(())
            }
        }

        fn io_fence(&self) -> Result<(), HermesFault> {
            Ok(())
        }

        fn allocate_dma(
            &self,
            _domain: Self::Domain,
            length: usize,
            alignment: usize,
            purpose: DmaPurpose,
        ) -> Result<DmaRegion<Self::Dma>, HermesFault> {
            if self.fault == FaultPoint::EventAllocation && purpose == DmaPurpose::EventRing {
                return Err(HermesFault::DmaAllocation);
            }
            let handle = match purpose {
                DmaPurpose::Firmware => 1,
                DmaPurpose::CommandRing => 2,
                DmaPurpose::EventRing => 3,
            };
            Ok(DmaRegion {
                handle,
                device_address: u64::from(handle) * DMA_GRANULE as u64,
                length: if self.fault == FaultPoint::CommandValidation
                    && purpose == DmaPurpose::CommandRing
                {
                    length - 1
                } else {
                    length
                },
                alignment,
                purpose,
            })
        }

        fn release_dma(&self, region: DmaRegion<Self::Dma>) {
            self.state
                .lock()
                .record_cleanup(Cleanup::Dma(region.purpose));
        }

        fn dma_write(
            &self,
            _region: DmaRegion<Self::Dma>,
            _offset: usize,
            _bytes: &[u8],
        ) -> Result<(), HermesFault> {
            Ok(())
        }

        fn dma_read(
            &self,
            _region: DmaRegion<Self::Dma>,
            _offset: usize,
            _bytes: &mut [u8],
        ) -> Result<(), HermesFault> {
            Ok(())
        }

        fn dma_publish(
            &self,
            region: DmaRegion<Self::Dma>,
            _offset: usize,
            _length: usize,
        ) -> Result<(), HermesFault> {
            let injected = matches!(
                (self.fault, region.purpose),
                (FaultPoint::FirmwarePublish, DmaPurpose::Firmware)
                    | (FaultPoint::CommandPublish, DmaPurpose::CommandRing)
                    | (FaultPoint::EventInitialization, DmaPurpose::EventRing)
            );
            if injected {
                Err(HermesFault::DmaAccess)
            } else {
                Ok(())
            }
        }

        fn dma_acquire(
            &self,
            _region: DmaRegion<Self::Dma>,
            _offset: usize,
            _length: usize,
        ) -> Result<(), HermesFault> {
            Ok(())
        }

        fn now_tick(&self) -> u64 {
            1
        }

        fn relax(&self) {}
    }

    struct TestCodec {
        profile: HermesTransportProfile,
        decode_error: bool,
    }

    impl HermesCodec for TestCodec {
        fn personality_id(&self) -> u64 {
            HERMES_DRIVER_ID
        }

        fn compatibility_manifest(&self) -> GpuCompatibilityManifest {
            manifest_for(crate::drivers::drivernet::model::DriverStrategy::HermesNvidia).unwrap()
        }

        fn match_device(
            &self,
            _identity: &HermesPciIdentity,
            _evidence: &HermesProbeEvidence,
        ) -> Result<u32, HermesFault> {
            Ok(10_000)
        }

        fn describe_transport(
            &self,
            _identity: &HermesPciIdentity,
            _evidence: &HermesProbeEvidence,
        ) -> Result<HermesTransportProfile, HermesFault> {
            Ok(self.profile)
        }

        fn boot_instruction(
            &self,
            _identity: &HermesPciIdentity,
            _evidence: &HermesProbeEvidence,
            _stage: u32,
            _index: u32,
        ) -> Result<Option<HermesBootInstruction>, HermesFault> {
            Ok(None)
        }

        fn encode_command(
            &self,
            _profile: &HermesTransportProfile,
            _command: &HermesNormalizedCommand,
            output: &mut [u8],
        ) -> Result<usize, HermesFault> {
            output[..16].fill(0x5a);
            Ok(16)
        }

        fn decode_event(
            &self,
            _profile: &HermesTransportProfile,
            _input: &[u8],
        ) -> Result<HermesNormalizedEvent, HermesFault> {
            if self.decode_error {
                return Err(HermesFault::CodecRejected);
            }
            let mut event = HermesNormalizedEvent::empty();
            event.epoch = 7;
            event.event_kind = HERMES_EVENT_ASYNC;
            Ok(event)
        }

        fn reset(
            &self,
            _profile: &HermesTransportProfile,
            _new_epoch: u32,
        ) -> Result<(), HermesFault> {
            Ok(())
        }
    }

    struct TestAuthority;

    impl FirmwareAuthority for TestAuthority {
        fn authenticate(
            &self,
            _identity: &HermesPciIdentity,
            _evidence: &HermesProbeEvidence,
            _profile: &HermesTransportProfile,
            image: &FirmwareImage<'_>,
        ) -> Result<FirmwareSeal, HermesFault> {
            Ok(FirmwareSeal {
                manifest_hash: image.manifest_hash,
                version: image.version,
                policy_epoch: 1,
                trust_domain: 1,
            })
        }
    }

    fn transport_profile(firmware: bool) -> HermesTransportProfile {
        let mut profile = HermesTransportProfile::empty();
        profile.personality_id = HERMES_DRIVER_ID;
        profile.protocol_family = 1;
        profile.wire_major = 1;
        profile.command_slot_bytes = 256;
        profile.event_slot_bytes = 256;
        profile.command_depth = 2;
        profile.event_depth = 2;
        profile.required_bar_lengths[0] = 4096;
        if firmware {
            profile.firmware_minimum_bytes = 64;
            profile.firmware_maximum_bytes = 64;
            profile.firmware_alignment = DMA_GRANULE as u32;
        }
        profile.command_producer_offset = 0;
        profile.command_consumer_offset = 4;
        profile.event_producer_offset = 8;
        profile.event_consumer_offset = 12;
        profile.command_doorbell_offset = 16;
        profile.event_doorbell_offset = 20;
        profile.status_offset = 24;
        profile.ready_mask = 1;
        profile.ready_value = 1;
        profile.fault_mask = 2;
        profile.maximum_wire_bytes = 256;
        profile.maximum_boot_steps = 8;
        profile.minimum_completions_per_window = 1;
        profile.maximum_admissions_per_window = 1;
        profile.maximum_admission_backlog = 1;
        profile.service_window_ticks = 100;
        profile.service_latency_ticks = 1;
        profile
    }

    fn identity() -> HermesPciIdentity {
        HermesPciIdentity {
            segment: 0,
            bus: 1,
            slot: 0,
            function: 0,
            revision: 1,
            vendor_id: NVIDIA_VENDOR_ID,
            device_id: 0x2684,
            subsystem_vendor_id: NVIDIA_VENDOR_ID,
            subsystem_device_id: 1,
            class_code: PCI_CLASS_DISPLAY,
            subclass: 0,
            programming_interface: 0,
            reserved: 0,
        }
    }

    fn probe_evidence() -> HermesProbeEvidence {
        let mut evidence = HermesProbeEvidence::empty();
        evidence.bar_lengths[0] = 4096;
        evidence
    }

    fn portable_evidence() -> GpuDeviceEvidence {
        let identity = identity();
        let mut evidence = GpuDeviceEvidence::EMPTY;
        evidence.identity.segment = identity.segment;
        evidence.identity.bus = identity.bus;
        evidence.identity.slot = identity.slot;
        evidence.identity.function = identity.function;
        evidence.identity.revision = identity.revision;
        evidence.identity.vendor_id = identity.vendor_id;
        evidence.identity.device_id = identity.device_id;
        evidence.identity.subsystem_vendor_id = identity.subsystem_vendor_id;
        evidence.identity.subsystem_device_id = identity.subsystem_device_id;
        evidence.identity.class_code = identity.class_code;
        evidence.identity.subclass = identity.subclass;
        evidence.identity.programming_interface = identity.programming_interface;
        evidence.bars[0] = GpuBarEvidence {
            physical_address: 0x8000_0000,
            length: 4096,
            flags: GPU_BAR_PRESENT,
            reserved: 0,
        };
        evidence.topology_flags = GPU_TOPOLOGY_IOMMU_ISOLATED;
        evidence.evidence_root = 7;
        evidence
    }

    fn stage_failure(fault: FaultPoint, firmware: bool) -> (HermesFault, FaultPlatform) {
        let platform = FaultPlatform::new(fault);
        let codec = TestCodec {
            profile: transport_profile(firmware),
            decode_error: false,
        };
        let mut registry = PersonalityRegistry::<1>::new();
        registry.register(&codec).unwrap();
        let profiled = Hermes::<_, Profiled, 1>::bind(
            &platform,
            &registry,
            identity(),
            probe_evidence(),
            &portable_evidence(),
            17,
            1,
        )
        .unwrap();
        let firmware_bytes = [0x5a; 64];
        let image = firmware.then_some(FirmwareImage {
            bytes: &firmware_bytes,
            manifest_hash: [1; 32],
            version: 1,
            flags: 0,
        });
        let error = match profiled.stage(image, &TestAuthority) {
            Ok(_) => panic!("fault injection unexpectedly staged the transport"),
            Err(error) => error,
        };
        (error, platform)
    }

    fn online<'a>(
        platform: &'a FaultPlatform,
        codec: &'a TestCodec,
    ) -> Hermes<'a, FaultPlatform, Online, 1> {
        let profile = codec.profile;
        let mut runtime = Runtime::new(&identity(), &profile, 1, 17).unwrap();
        runtime.epoch = 7;
        let mut lease = HermesLease::new(platform);
        lease.domain = Some(1);
        lease.bars[0] = Some(MmioWindow {
            handle: 0,
            bar: 0,
            length: 4096,
        });
        lease.command_ring = Some(DmaRegion {
            handle: 2,
            device_address: 0x2000,
            length: 512,
            alignment: DMA_GRANULE,
            purpose: DmaPurpose::CommandRing,
        });
        lease.event_ring = Some(DmaRegion {
            handle: 3,
            device_address: 0x3000,
            length: 512,
            alignment: DMA_GRANULE,
            purpose: DmaPurpose::EventRing,
        });
        Hermes {
            backend: platform,
            codec,
            identity: identity(),
            evidence: probe_evidence(),
            profile,
            compatibility_proof: GpuCompatibilityProof::EMPTY,
            firmware_seal: None,
            lease,
            runtime,
            _state: PhantomData,
        }
    }

    #[test]
    fn short_bar_is_owned_before_validation_failure() {
        let (error, platform) = stage_failure(FaultPoint::ShortBar, false);
        assert_eq!(error, HermesFault::BarUnavailable);
        platform.assert_cleanup(&[Cleanup::Bar(0), Cleanup::Domain]);
    }

    #[test]
    fn firmware_publish_failure_releases_the_firmware_region_once() {
        let (error, platform) = stage_failure(FaultPoint::FirmwarePublish, true);
        assert_eq!(error, HermesFault::DmaAccess);
        platform.assert_cleanup(&[
            Cleanup::Dma(DmaPurpose::Firmware),
            Cleanup::Bar(0),
            Cleanup::Domain,
        ]);
    }

    #[test]
    fn command_ring_failures_release_the_ring_once() {
        for fault in [FaultPoint::CommandValidation, FaultPoint::CommandPublish] {
            let (error, platform) = stage_failure(fault, false);
            assert!(matches!(
                error,
                HermesFault::DmaAddressOverflow | HermesFault::DmaAccess
            ));
            platform.assert_cleanup(&[
                Cleanup::Dma(DmaPurpose::CommandRing),
                Cleanup::Bar(0),
                Cleanup::Domain,
            ]);
        }
    }

    #[test]
    fn event_failures_release_all_prior_dma_in_reverse_order() {
        let (allocation_error, allocation_platform) =
            stage_failure(FaultPoint::EventAllocation, false);
        assert_eq!(allocation_error, HermesFault::DmaAllocation);
        allocation_platform.assert_cleanup(&[
            Cleanup::Dma(DmaPurpose::CommandRing),
            Cleanup::Bar(0),
            Cleanup::Domain,
        ]);

        let (initialization_error, initialization_platform) =
            stage_failure(FaultPoint::EventInitialization, false);
        assert_eq!(initialization_error, HermesFault::DmaAccess);
        initialization_platform.assert_cleanup(&[
            Cleanup::Dma(DmaPurpose::EventRing),
            Cleanup::Dma(DmaPurpose::CommandRing),
            Cleanup::Bar(0),
            Cleanup::Domain,
        ]);
    }

    #[test]
    fn failed_command_publication_permanently_poisons_the_transport() {
        for (fault, expected) in [
            (FaultPoint::CommandPublish, HermesFault::DmaAccess),
            (FaultPoint::CommandDoorbell, HermesFault::MmioWrite),
        ] {
            let platform = FaultPlatform::new(fault);
            let codec = TestCodec {
                profile: transport_profile(false),
                decode_error: false,
            };
            let mut hermes = online(&platform, &codec);

            assert_eq!(hermes.submit(0x40, 0, 0, [0; 8], &[], 1_000), Err(expected),);
            assert!(hermes.transport_poisoned());
            assert_eq!(hermes.runtime.command_producer, 0);
            assert_eq!(
                hermes.submit(0x41, 0, 0, [0; 8], &[], 1_000),
                Err(HermesFault::RecoveryRequired),
            );
            assert_eq!(hermes.poll_event(), Err(HermesFault::RecoveryRequired));
            assert_eq!(hermes.expire_one(), Err(HermesFault::RecoveryRequired));
        }
    }

    #[test]
    fn event_decode_and_acknowledgement_failures_poison_without_advancing_consumer() {
        for (fault, decode_error, expected) in [
            (FaultPoint::EventDecode, true, HermesFault::CodecRejected),
            (FaultPoint::EventAcknowledge, false, HermesFault::MmioWrite),
        ] {
            let platform = FaultPlatform::new(fault);
            let codec = TestCodec {
                profile: transport_profile(false),
                decode_error,
            };
            let mut hermes = online(&platform, &codec);

            assert_eq!(hermes.poll_event(), Err(expected));
            assert!(hermes.transport_poisoned());
            assert_eq!(hermes.runtime.event_consumer, 0);
            assert_eq!(hermes.poll_event(), Err(HermesFault::RecoveryRequired));
        }
    }

    #[test]
    fn discovery_handoff_preserves_measured_pci_and_bar_evidence() {
        let mut fingerprint = GpuFingerprint::EMPTY;
        fingerprint.segment = 2;
        fingerprint.bus = 3;
        fingerprint.slot = 4;
        fingerprint.function = 1;
        fingerprint.revision = 0xa1;
        fingerprint.vendor_id = NVIDIA_VENDOR_ID;
        fingerprint.device_id = 0x2684;
        fingerprint.subsystem_vendor_id = 0x1043;
        fingerprint.subsystem_device_id = 0x8899;
        fingerprint.class_code = PCI_CLASS_DISPLAY;
        fingerprint.subclass = 2;
        fingerprint.programming_interface = 7;
        fingerprint.capability_flags = 0x55;
        fingerprint.topology_flags = TOPOLOGY_IOMMU_ISOLATED;
        fingerprint.bars[0] = BarEvidence {
            raw_low: 0x8000_0004,
            raw_high: 1,
            length: 16 * 1024 * 1024,
            flags: BAR_PRESENT | BAR_64BIT,
        };
        fingerprint.bars[2] = BarEvidence {
            raw_low: 0xc001,
            raw_high: 0,
            length: 256,
            flags: BAR_PRESENT | BAR_IO,
        };
        fingerprint.evidence_root = 0x1234;

        let discovery = HermesDiscovery::from_fingerprint(&fingerprint).unwrap();
        let identity = discovery.identity();
        assert_eq!(identity.segment, fingerprint.segment);
        assert_eq!(identity.bus, fingerprint.bus);
        assert_eq!(identity.slot, fingerprint.slot);
        assert_eq!(identity.function, fingerprint.function);
        assert_eq!(identity.revision, fingerprint.revision);
        assert_eq!(identity.vendor_id, fingerprint.vendor_id);
        assert_eq!(identity.device_id, fingerprint.device_id);
        assert_eq!(
            identity.subsystem_vendor_id,
            fingerprint.subsystem_vendor_id
        );
        assert_eq!(
            identity.subsystem_device_id,
            fingerprint.subsystem_device_id
        );
        assert_eq!(identity.class_code, fingerprint.class_code);
        assert_eq!(identity.subclass, fingerprint.subclass);
        assert_eq!(
            identity.programming_interface,
            fingerprint.programming_interface
        );

        let probe = discovery.probe_evidence();
        assert_eq!(probe.bar_lengths[0], fingerprint.bars[0].length);
        assert_eq!(probe.bar_lengths[2], 0);
        assert_eq!(probe.observed_features, 0);
        assert_eq!(
            discovery.portable_evidence().evidence_root,
            fingerprint.evidence_root
        );

        fingerprint.bars[0].length = 0;
        assert_eq!(
            HermesDiscovery::from_fingerprint(&fingerprint),
            Err(HermesFault::CompatibilityRejected),
        );
    }

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
    fn queue_publication_must_match_the_allocated_geometry() {
        assert_eq!(
            validate_queue_publication(64 * 256, 64, 256, 64, 256),
            Ok(())
        );
        assert_eq!(
            validate_queue_publication(64 * 256, 64, 256, 64, 128),
            Err(HermesFault::QueueGeometry),
        );
        assert_eq!(
            validate_queue_publication(64 * 256 - 1, 64, 256, 64, 256),
            Err(HermesFault::QueueGeometry),
        );
    }

    #[test]
    fn wrapped_deadlines_preserve_half_range_ordering() {
        assert!(tick_reached(100, 100));
        assert!(tick_reached(101, 100));
        assert!(!tick_reached(99, 100));
    }

    #[test]
    fn correlation_rotation_is_atomic_and_terminal_space_never_wraps() {
        let profile = transport_profile(false);
        let mut runtime = Runtime::<1>::new(&identity(), &profile, 1, 17).unwrap();

        runtime.epoch = 41;
        runtime.next_sequence = u32::MAX;
        assert_eq!(runtime.allocate_correlation(), Ok((41, u32::MAX)));
        assert_eq!(runtime.allocate_correlation(), Ok((42, 1)));

        runtime.epoch = u32::MAX;
        runtime.next_sequence = u32::MAX;
        assert_eq!(runtime.allocate_correlation(), Ok((u32::MAX, u32::MAX)));
        assert_eq!(runtime.next_sequence, 0);
        assert_eq!(
            runtime.allocate_correlation(),
            Err(HermesFault::CorrelationSpaceExhausted)
        );
    }

    #[test]
    fn exhausted_correlation_rolls_back_service_admission_exactly() {
        let platform = FaultPlatform::new(FaultPoint::ShortBar);
        let codec = TestCodec {
            profile: transport_profile(false),
            decode_error: false,
        };
        let mut hermes = online(&platform, &codec);
        hermes.runtime.epoch = u32::MAX;
        hermes.runtime.next_sequence = 0;
        let accepted = hermes.runtime.service.accepted();
        let virtual_backlog = hermes.runtime.service.virtual_backlog_q16();

        assert_eq!(
            hermes.submit(1, 0, 0, [0; 8], &[], 10_000),
            Err(HermesFault::CorrelationSpaceExhausted)
        );
        assert_eq!(hermes.runtime.pending.live_count(), 0);
        assert_eq!(hermes.runtime.service.accepted(), accepted);
        assert_eq!(
            hermes.runtime.service.virtual_backlog_q16(),
            virtual_backlog
        );
        assert_eq!(
            hermes.runtime.last_admission,
            HermesAdmissionCertificate::EMPTY
        );
    }

    fn admission(sequence: u64) -> HermesAdmissionCertificate {
        HermesAdmissionCertificate {
            reservation_sequence: sequence,
            admitted_tick: 10,
            window_start: 10,
            backlog_before: 0,
            admitted_before: 0,
            deterministic_delay_ticks: 20,
            uncertainty_guard_ticks: 1,
            drift_penalty_ticks: 0,
            delay_bound_ticks: 21,
            deadline_slack_ticks: 79,
            curve_root: 7,
            calibration_root: 11,
            certificate_root: 13,
        }
    }

    #[test]
    fn correlated_reply_preserves_admission_evidence() {
        let mut table = PendingTable::<2>::new();
        table
            .insert(PendingRequest {
                live: true,
                epoch: 3,
                sequence: 9,
                opcode: 0x44,
                deadline_tick: 100,
                admission: admission(1),
            })
            .unwrap();

        let mut event = HermesNormalizedEvent::empty();
        event.correlation_epoch = 3;
        event.correlation_sequence = 9;
        let completed = table.take_response(&event).unwrap();

        assert_eq!(completed.opcode, 0x44);
        assert_eq!(completed.admission, admission(1));
        assert_eq!(table.live_count(), 0);
    }

    #[test]
    fn expiry_returns_the_complete_reservation() {
        let mut table = PendingTable::<2>::new();
        table
            .insert(PendingRequest {
                live: true,
                epoch: 4,
                sequence: 10,
                opcode: 0x55,
                deadline_tick: 50,
                admission: admission(2),
            })
            .unwrap();

        let expired = table.expire_one(50).unwrap();
        assert_eq!(expired.epoch, 4);
        assert_eq!(expired.sequence, 10);
        assert_eq!(expired.admission, admission(2));
        assert_eq!(table.live_count(), 0);
    }
}
