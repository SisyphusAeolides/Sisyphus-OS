// kernel/boulder/src/drivernet_host.rs
//! Boulder integration host for measured GPU discovery and fail-closed activation.

use crate::blacklab_bootstrap::{BlackLabComplex, BlackLabSeeds};
use crate::boot::acpi::{DmarEndpoint, DmarInfo};
use crate::boot::multiboot2::BootFramebuffer;
use crate::capability::{
    Authority, Capability, DeviceMemoryControl, DmaControl, FaultPolicyControl, PolicyControl,
};
use crate::drivers::drivernet::blacklab_observer::BlackLabDriverObserver;
use crate::drivers::drivernet::brokers::{
    FirmwareFramebufferBroker, FirmwareFramebufferHost, NativeBrokerAdapter, NativeStrategyHost,
    QuarantineBroker,
};
use crate::drivers::drivernet::dispatch::{
    ActivationReceipt, BackendFault, DispatchStage, FaultCode, HealthReceipt, ProbeObservation,
};
use crate::drivers::drivernet::fingerprint::{
    FirmwareFramebufferEvidence, FirmwareFramebufferKind, GpuFingerprint,
    LegacyConfigurationReader, TopologyEvidence, TOPOLOGY_IOMMU_PRESENT,
};
use crate::drivers::drivernet::inventory::{DisplayFunctionInventory, InventoryError};
use crate::drivers::drivernet::model::{
    DriverStrategy, ModelExpectation, OracleDecision, OraclePolicy,
};
use crate::drivers::drivernet::model_weights::{
    MODEL_CORPUS_ROOT, MODEL_ROOT, MODEL_SCHEMA_VERSION,
};
use crate::drivers::drivernet::platform::{
    BlackLabDriverPlatform, BrokerTransaction, DeviceIsolationBroker, DriverNetClock,
    IsolationReceipt, PlatformError,
};
use crate::drivers::drivernet::registry::{
    ProbeSemantic, ProbeStep, ShimDescriptor, PROBE_EVIDENCE_HEALTH, PROBE_EVIDENCE_IDENTITY,
    PROBE_EVIDENCE_IOMMU_DOMAIN, PROBE_EVIDENCE_MMIO_DECODE,
};
use crate::drivers::drivernet::topology::{BootTopologyTable, TopologyRecord, TopologyTableError};
use crate::drivers::drivernet::{
    DriverNet, DriverNetError, DriverNetScratch, DriverNetSecrets, DriverNetSummary,
};
use crate::drivers::firmware_display;
use crate::drivers::gpu_portability::{bar_address, GpuPortabilityResolver, PortabilityFault};
use crate::drivers::hermes_gsp::HermesDiscovery;
use crate::hw::pci::PciInventory;
use crate::mmio::MmioWindow;
use crate::predictive_control::hash::{hkdf_expand, hmac_sha256, HashError, Sha256};

const GPU_BOOT_DOMAIN_SALT: &[u8] = b"Sisyphus-OS GPU boot domains v1";
const GPU_BOOT_TRANSCRIPT_DOMAIN: &[u8] = b"Sisyphus-OS GPU boot transcript v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GpuBootDomains {
    pub blacklab: BlackLabSeeds,
    pub drivernet: DriverNetSecrets,
    pub transcript_root: [u8; 32],
    pub schedule_root: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GpuBootDomainError {
    EmptyMeasurement,
    IncompleteInventory,
    Hash(HashError),
    InvalidSchedule,
}

impl From<HashError> for GpuBootDomainError {
    fn from(error: HashError) -> Self {
        Self::Hash(error)
    }
}

pub fn derive_gpu_boot_domains(
    measured_image_digest: [u8; 32],
    boot_counter: u64,
    inventory: &PciInventory,
    framebuffer: Option<BootFramebuffer>,
) -> Result<GpuBootDomains, GpuBootDomainError> {
    if measured_image_digest.iter().all(|byte| *byte == 0) {
        return Err(GpuBootDomainError::EmptyMeasurement);
    }
    if inventory.overflowed() {
        return Err(GpuBootDomainError::IncompleteInventory);
    }

    let mut transcript = Sha256::new();
    transcript.update(GPU_BOOT_TRANSCRIPT_DOMAIN)?;
    transcript.update_u64(inventory.devices().len() as u64)?;
    for device in inventory.devices() {
        transcript.update_u8(device.address.bus)?;
        transcript.update_u8(device.address.slot)?;
        transcript.update_u8(device.address.function)?;
        transcript.update_u16(device.vendor_id)?;
        transcript.update_u16(device.device_id)?;
        transcript.update_u8(device.class_code)?;
        transcript.update_u8(device.subclass)?;
        transcript.update_u8(device.programming_interface)?;
        transcript.update_u8(device.revision)?;
        transcript.update_u8(device.header_type)?;
        transcript.update_u8(device.interrupt_line)?;
        transcript.update_u8(device.interrupt_pin)?;
    }
    match framebuffer {
        Some(framebuffer) => {
            transcript.update_u8(1)?;
            transcript.update_u64(framebuffer.physical_address)?;
            transcript.update_u64(framebuffer.byte_length)?;
            transcript.update_u32(framebuffer.width)?;
            transcript.update_u32(framebuffer.height)?;
            transcript.update_u32(framebuffer.pitch)?;
            transcript.update_u8(framebuffer.bits_per_pixel)?;
            transcript.update_u32(framebuffer.format)?;
        }
        None => transcript.update_u8(0)?,
    }
    let transcript_root = transcript.finalize();
    let counter = boot_counter.to_le_bytes();
    let pseudo_random_key = hmac_sha256(
        GPU_BOOT_DOMAIN_SALT,
        &[&measured_image_digest, &counter, &transcript_root],
    )?;

    let mut schedule = [0_u8; 56];
    hkdf_expand(
        &pseudo_random_key,
        b"blacklab and drivernet domain schedule",
        &mut schedule,
    )?;
    let mut values = [0_u64; 7];
    for (index, chunk) in schedule.chunks_exact(8).enumerate() {
        let mut bytes = [0_u8; 8];
        bytes.copy_from_slice(chunk);
        values[index] = u64::from_le_bytes(bytes);
    }
    for left in 0..values.len() {
        if values[left] == 0 {
            return Err(GpuBootDomainError::InvalidSchedule);
        }
        for right in left + 1..values.len() {
            if values[left] == values[right] {
                return Err(GpuBootDomainError::InvalidSchedule);
            }
        }
    }

    Ok(GpuBootDomains {
        blacklab: BlackLabSeeds {
            ledger_secret: values[0],
            plan_secret: values[1],
            dma_secret: values[2],
            ledger_epoch: boot_counter.max(1),
        },
        drivernet: DriverNetSecrets {
            fingerprint: values[3],
            oracle: values[4],
            dispatch: values[5],
            telemetry: values[6],
        },
        transcript_root,
        schedule_root: pseudo_random_key,
    })
}

pub struct BoulderDriverClock;

impl DriverNetClock for BoulderDriverClock {
    fn now_tick(&self) -> u64 {
        <crate::arch::Active as crate::arch::Architecture>::counter_sample()
    }
}

pub struct BoulderIsolationBroker;

impl DeviceIsolationBroker for BoulderIsolationBroker {
    fn begin(
        &self,
        fingerprint: &GpuFingerprint,
        descriptor: &ShimDescriptor,
        _device: &Capability<'_, DeviceMemoryControl>,
        _dma: &Capability<'_, DmaControl>,
    ) -> Result<IsolationReceipt, BackendFault> {
        if !descriptor.vendor_gate.matches(fingerprint) {
            return Err(BackendFault::new(
                FaultCode::DescriptorRejected,
                false,
                fingerprint.evidence_root,
            ));
        }
        let discovery_root = if descriptor.strategy == DriverStrategy::HermesNvidia {
            HermesDiscovery::from_fingerprint(fingerprint)
                .map_err(|_| {
                    BackendFault::new(
                        FaultCode::DescriptorRejected,
                        false,
                        fingerprint.evidence_root,
                    )
                })?
                .portable_evidence()
                .evidence_root
        } else {
            fingerprint.evidence_root
        };
        // Topology evidence is not an IOMMU domain. Until a remapping backend
        // transfers a live domain into the selected native driver, activation
        // must stop here rather than manufacturing isolation from a group id.
        if descriptor.strategy.native() {
            return Err(BackendFault::new(
                FaultCode::TransactionRejected,
                false,
                mix(discovery_root, u64::from(fingerprint.iommu_group)),
            ));
        }

        Ok(IsolationReceipt {
            token: fingerprint.evidence_root,
            domain: fingerprint.iommu_group as u64,
            firmware_preserved: fingerprint.firmware_display_usable(),
            root: fingerprint.evidence_root,
        })
    }

    fn commit(
        &self,
        receipt: IsolationReceipt,
        _device: &Capability<'_, DeviceMemoryControl>,
        _dma: &Capability<'_, DmaControl>,
    ) -> Result<(), BackendFault> {
        if receipt.token == 0 || receipt.root == 0 {
            return Err(BackendFault::new(
                FaultCode::CommitFault,
                false,
                receipt.root,
            ));
        }
        Ok(())
    }

    fn rollback(
        &self,
        receipt: IsolationReceipt,
        stage: DispatchStage,
        fault: BackendFault,
        _device: &Capability<'_, DeviceMemoryControl>,
        _dma: &Capability<'_, DmaControl>,
        _fault_policy: &Capability<'_, FaultPolicyControl>,
    ) -> Result<(), BackendFault> {
        if receipt.token == 0 || receipt.root == 0 {
            return Err(BackendFault::new(
                FaultCode::RollbackFault,
                false,
                mix(fault.detail, stage as u8 as u64),
            ));
        }
        Ok(())
    }
}

pub struct BoulderNativeHost {
    portability_secret: u64,
}

impl BoulderNativeHost {
    pub const fn new(portability_secret: u64) -> Self {
        Self { portability_secret }
    }

    fn portability(
        &self,
        strategy: DriverStrategy,
        fingerprint: &GpuFingerprint,
    ) -> Result<crate::drivers::gpu_portability::PortabilityResolution, BackendFault> {
        let resolver = GpuPortabilityResolver::new(self.portability_secret)
            .map_err(|_| BackendFault::new(FaultCode::RegistryFault, false, 0))?;
        resolver.prove(strategy, fingerprint).map_err(|fault| {
            let detail = match fault {
                PortabilityFault::Rejected(proof) => proof.proof_root,
                PortabilityFault::Ambiguous => fingerprint.evidence_root,
                PortabilityFault::UnsupportedStrategy => strategy.index() as u64,
            };
            BackendFault::new(FaultCode::CandidateInadmissible, false, detail)
        })
    }

    fn sample_bar(
        &self,
        fingerprint: &GpuFingerprint,
        preferred_bar: usize,
        device: &Capability<'_, DeviceMemoryControl>,
    ) -> Result<(usize, u32), BackendFault> {
        let selection = fingerprint
            .bars
            .iter()
            .enumerate()
            .filter(|(_, bar)| bar.is_mmio() && bar.length >= 4)
            .find(|(index, _)| *index == preferred_bar)
            .or_else(|| {
                fingerprint
                    .bars
                    .iter()
                    .enumerate()
                    .find(|(_, bar)| bar.is_mmio() && bar.length >= 4)
            })
            .ok_or_else(|| {
                BackendFault::new(FaultCode::ProbeFault, false, fingerprint.evidence_root)
            })?;

        let physical = bar_address(selection.1.raw_low, selection.1.raw_high, selection.1.flags);
        if physical == 0 {
            return Err(BackendFault::new(
                FaultCode::ProbeFault,
                false,
                fingerprint.evidence_root,
            ));
        }

        let window = MmioWindow::map(physical, 4, device)
            .map_err(|_| BackendFault::new(FaultCode::ProbeFault, true, physical))?;
        let first = window.read_u32(0);
        let second = window.read_u32(0);
        let close_status = window.close(device);

        let first = first.map_err(|_| BackendFault::new(FaultCode::ProbeFault, true, physical))?;
        let second =
            second.map_err(|_| BackendFault::new(FaultCode::ProbeFault, true, physical))?;
        if close_status != sisyphus_driver_abi::STATUS_OK || first != second || first == u32::MAX {
            return Err(BackendFault::new(
                FaultCode::ProbeFault,
                true,
                u64::from(first) | (u64::from(second) << 32),
            ));
        }
        Ok((selection.0, first))
    }
}

impl NativeStrategyHost for BoulderNativeHost {
    fn begin(
        &self,
        strategy: DriverStrategy,
        fingerprint: &GpuFingerprint,
        _descriptor: &ShimDescriptor,
        isolation: IsolationReceipt,
        _policy: &Capability<'_, PolicyControl>,
    ) -> Result<BrokerTransaction, BackendFault> {
        let portability = self.portability(strategy, fingerprint)?;
        Ok(BrokerTransaction {
            token: isolation.token ^ portability.proof.proof_root,
            state: [
                portability.proof.proof_root,
                u64::from(portability.proof.score_q16),
                u64::MAX,
                0,
                0,
                0,
                0,
                0,
            ],
            root: portability.resolution_root,
        })
    }

    fn probe(
        &self,
        strategy: DriverStrategy,
        transaction: &mut BrokerTransaction,
        fingerprint: &GpuFingerprint,
        descriptor: &ShimDescriptor,
        step: ProbeStep,
        _attempt: u8,
        device: &Capability<'_, DeviceMemoryControl>,
        _dma: &Capability<'_, DmaControl>,
    ) -> Result<ProbeObservation, BackendFault> {
        self.portability(strategy, fingerprint)?;

        let (evidence_bit, value) = match step.semantic {
            ProbeSemantic::ValidateIdentity => {
                if !descriptor.vendor_gate.matches(fingerprint) || !fingerprint.is_display() {
                    return Err(BackendFault::new(
                        FaultCode::DescriptorRejected,
                        false,
                        fingerprint.evidence_root,
                    ));
                }
                (
                    PROBE_EVIDENCE_IDENTITY,
                    u64::from(fingerprint.vendor_id) | (u64::from(fingerprint.device_id) << 16),
                )
            }
            ProbeSemantic::VerifyIommuIsolation => {
                if !fingerprint.iommu_isolated() || fingerprint.iommu_group == u32::MAX {
                    return Err(BackendFault::new(
                        FaultCode::ProbeFault,
                        false,
                        fingerprint.evidence_root,
                    ));
                }
                (
                    PROBE_EVIDENCE_IOMMU_DOMAIN,
                    u64::from(fingerprint.iommu_group),
                )
            }
            ProbeSemantic::MapControlBarReadOnly => {
                let (bar, sample) =
                    self.sample_bar(fingerprint, usize::from(step.argument.min(5)), device)?;
                transaction.state[2] = bar as u64;
                transaction.state[3] = u64::from(sample);
                (PROBE_EVIDENCE_MMIO_DECODE, u64::from(sample))
            }
            ProbeSemantic::SampleVendorSignature => {
                let preferred = usize::try_from(transaction.state[2])
                    .unwrap_or(usize::from(step.argument.min(5)));
                let (_, sample) = self.sample_bar(fingerprint, preferred, device)?;
                if transaction.state[3] != 0 && transaction.state[3] != u64::from(sample) {
                    return Err(BackendFault::new(
                        FaultCode::ProbeFault,
                        true,
                        u64::from(sample),
                    ));
                }
                transaction.state[3] = u64::from(sample);
                (PROBE_EVIDENCE_MMIO_DECODE, u64::from(sample))
            }
            ProbeSemantic::VerifyParavirtualCapability => {
                if !matches!(
                    strategy,
                    DriverStrategy::VirtioGpu | DriverStrategy::VirtualSvga
                ) {
                    return Err(BackendFault::new(
                        FaultCode::ProbeFault,
                        false,
                        strategy.index() as u64,
                    ));
                }
                (step.evidence_bit, fingerprint.evidence_root)
            }
            ProbeSemantic::EstablishHealthBaseline => {
                let preferred = usize::try_from(transaction.state[2]).unwrap_or(0);
                let (_, sample) = self.sample_bar(fingerprint, preferred, device)?;
                (PROBE_EVIDENCE_HEALTH, u64::from(sample))
            }
            // Reset, interrupt, transport, display-engine and queue proofs need
            // a concrete generation-specific backend. Never manufacture them.
            _ => {
                return Err(BackendFault::new(
                    FaultCode::ProbeFault,
                    false,
                    step.semantic as u8 as u64,
                ));
            }
        };

        let tick = <crate::arch::Active as crate::arch::Architecture>::counter_sample();
        transaction.root = mix(transaction.root, value ^ u64::from(evidence_bit) ^ tick);
        Ok(ProbeObservation {
            semantic: step.semantic,
            evidence_bit,
            value,
            tick,
            root: transaction.root,
        })
    }

    fn activate(
        &self,
        strategy: DriverStrategy,
        _transaction: &mut BrokerTransaction,
        fingerprint: &GpuFingerprint,
        _descriptor: &ShimDescriptor,
        _isolation: IsolationReceipt,
        _device: &Capability<'_, DeviceMemoryControl>,
        _dma: &Capability<'_, DmaControl>,
    ) -> Result<ActivationReceipt, BackendFault> {
        Err(BackendFault::new(
            FaultCode::ActivationFault,
            false,
            mix(fingerprint.evidence_root, strategy.index() as u64),
        ))
    }

    fn health(
        &self,
        strategy: DriverStrategy,
        _transaction: &mut BrokerTransaction,
        _activation: &ActivationReceipt,
        _descriptor: &ShimDescriptor,
        _fault_policy: &Capability<'_, FaultPolicyControl>,
    ) -> Result<HealthReceipt, BackendFault> {
        Err(BackendFault::new(
            FaultCode::HealthFault,
            false,
            strategy.index() as u64,
        ))
    }

    fn commit(
        &self,
        strategy: DriverStrategy,
        _transaction: &mut BrokerTransaction,
        _activation: &ActivationReceipt,
        _health: &HealthReceipt,
        _decision: &OracleDecision,
        _policy: &Capability<'_, PolicyControl>,
    ) -> Result<(u64, u32), BackendFault> {
        Err(BackendFault::new(
            FaultCode::CommitFault,
            false,
            strategy.index() as u64,
        ))
    }

    fn rollback(
        &self,
        strategy: DriverStrategy,
        transaction: &mut BrokerTransaction,
        stage: DispatchStage,
        fault: BackendFault,
        _device: &Capability<'_, DeviceMemoryControl>,
        _dma: &Capability<'_, DmaControl>,
        _fault_policy: &Capability<'_, FaultPolicyControl>,
    ) -> Result<(), BackendFault> {
        if transaction.token == 0 || transaction.root == 0 {
            return Err(BackendFault::new(
                FaultCode::RollbackFault,
                false,
                mix(fault.detail, strategy.index() as u64),
            ));
        }
        transaction.state = [0; 8];
        transaction.root = mix(transaction.root, u64::from(stage as u8) ^ fault.detail);
        Ok(())
    }
}

pub struct BoulderFirmwareHost {
    secret: u64,
}

impl BoulderFirmwareHost {
    pub const fn new(secret: u64) -> Self {
        Self { secret }
    }
}

impl FirmwareFramebufferHost for BoulderFirmwareHost {
    fn now_tick(&self) -> u64 {
        <crate::arch::Active as crate::arch::Architecture>::counter_sample()
    }

    fn inspect(&self, evidence: FirmwareFramebufferEvidence) -> Result<(u64, u64), BackendFault> {
        let display = firmware_display::inspect(evidence, self.secret).map_err(|error| {
            BackendFault::new(
                FaultCode::TransactionRejected,
                false,
                firmware_fault_detail(error),
            )
        })?;
        Ok((display.object, display.state_root))
    }

    fn retain(
        &self,
        object: u64,
        _policy: &Capability<'_, PolicyControl>,
    ) -> Result<(u64, u32), BackendFault> {
        let display = firmware_display::retain(object, self.secret).map_err(|error| {
            BackendFault::new(FaultCode::CommitFault, false, firmware_fault_detail(error))
        })?;
        Ok((display.object, display.generation))
    }

    fn release(
        &self,
        object: u64,
        _fault_policy: &Capability<'_, FaultPolicyControl>,
    ) -> Result<(), BackendFault> {
        firmware_display::release(object, self.secret)
            .map(|_| ())
            .map_err(|error| {
                BackendFault::new(
                    FaultCode::RollbackFault,
                    false,
                    firmware_fault_detail(error),
                )
            })
    }
}

fn firmware_fault_detail(error: firmware_display::FirmwareDisplayError) -> u64 {
    match error {
        firmware_display::FirmwareDisplayError::InvalidEvidence => 1,
        firmware_display::FirmwareDisplayError::AddressOverflow => 2,
        firmware_display::FirmwareDisplayError::Capacity => 3,
        firmware_display::FirmwareDisplayError::NotFound => 4,
        firmware_display::FirmwareDisplayError::StaleObject => 5,
        firmware_display::FirmwareDisplayError::DomainMismatch => 6,
        firmware_display::FirmwareDisplayError::RefcountOverflow => 7,
        firmware_display::FirmwareDisplayError::PixelOutOfBounds => 8,
        firmware_display::FirmwareDisplayError::UnsupportedFormat => 9,
        firmware_display::FirmwareDisplayError::Mapping(_) => 10,
        firmware_display::FirmwareDisplayError::Unmap(_) => 11,
        firmware_display::FirmwareDisplayError::VerificationFault => 12,
    }
}

fn mix(mut state: u64, word: u64) -> u64 {
    state ^= word.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    state ^= state >> 30;
    state = state.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    state ^= state >> 27;
    state = state.wrapping_mul(0x94d0_49bb_1331_11eb);
    state ^ (state >> 31)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DriverNetHostError {
    DriverNet(DriverNetError),
    Inventory(InventoryError),
    Topology(TopologyTableError),
    Broker(BackendFault),
    Platform(PlatformError),
}

impl From<DriverNetError> for DriverNetHostError {
    fn from(error: DriverNetError) -> Self {
        Self::DriverNet(error)
    }
}

impl From<InventoryError> for DriverNetHostError {
    fn from(error: InventoryError) -> Self {
        Self::Inventory(error)
    }
}

impl From<TopologyTableError> for DriverNetHostError {
    fn from(error: TopologyTableError) -> Self {
        Self::Topology(error)
    }
}

impl From<BackendFault> for DriverNetHostError {
    fn from(error: BackendFault) -> Self {
        Self::Broker(error)
    }
}

impl From<PlatformError> for DriverNetHostError {
    fn from(error: PlatformError) -> Self {
        Self::Platform(error)
    }
}

pub fn resolve_drivernet<
    const SENSORS: usize,
    const RULES: usize,
    const LEDGER: usize,
    const APERTURES: usize,
    const MAPPINGS: usize,
>(
    pci_inventory: &PciInventory,
    dmar: Option<&DmarInfo>,
    boot_framebuffer: Option<BootFramebuffer>,
    secrets: DriverNetSecrets,
    authority: &Authority,
    blacklab: &mut BlackLabComplex<SENSORS, RULES, LEDGER, APERTURES, MAPPINGS>,
) -> Result<DriverNetSummary, DriverNetHostError> {
    let expected_model = ModelExpectation {
        schema: MODEL_SCHEMA_VERSION,
        corpus_root: MODEL_CORPUS_ROOT,
        model_root: MODEL_ROOT,
    };

    let mut drivernet = DriverNet::new_measured(secrets, OraclePolicy::BLACK_LAB, expected_model)?;

    let mut display_inventory = DisplayFunctionInventory::<64>::new()?;
    display_inventory.import_legacy(pci_inventory)?;

    let configuration_reader = LegacyConfigurationReader;
    let mut gpu_topology = BootTopologyTable::<64>::new()?;
    if let Some(dmar) = dmar {
        for function in display_inventory.functions() {
            let endpoint = DmarEndpoint {
                segment: function.address.segment,
                bus: function.address.bus,
                slot: function.address.slot,
                function: function.address.function,
            };
            if dmar.covers_endpoint(endpoint) {
                gpu_topology.insert(TopologyRecord {
                    address: function.address,
                    evidence: TopologyEvidence {
                        segment: function.address.segment,
                        topology_flags: TOPOLOGY_IOMMU_PRESENT,
                        ..TopologyEvidence::EMPTY
                    },
                })?;
            }
        }
    }
    if let Some(framebuffer) = boot_framebuffer {
        let evidence = FirmwareFramebufferEvidence {
            kind: FirmwareFramebufferKind::Vbe,
            physical_address: framebuffer.physical_address,
            width: framebuffer.width,
            height: framebuffer.height,
            pitch: framebuffer.pitch,
            format: framebuffer.format,
            byte_length: framebuffer.byte_length,
            owner: None,
            retained: true,
        };
        gpu_topology.set_firmware_framebuffer(evidence)?;
    }

    let device_memory = authority.grant::<DeviceMemoryControl>();
    let dma_control = authority.grant::<DmaControl>();
    let policy_control = authority.grant::<PolicyControl>();
    let fault_policy = authority.grant::<FaultPolicyControl>();

    let driver_clock = BoulderDriverClock;
    let device_isolation_broker = BoulderIsolationBroker;

    let native_strategy_host = BoulderNativeHost::new(secrets.dispatch ^ 0x4750_5550_4f52_5401);
    let firmware_framebuffer_host =
        BoulderFirmwareHost::new(secrets.dispatch ^ 0x4657_4449_5350_4c01);

    let nvidia = NativeBrokerAdapter::new(DriverStrategy::HermesNvidia, &native_strategy_host)?;
    let amd = NativeBrokerAdapter::new(DriverStrategy::AmdDisplay, &native_strategy_host)?;
    let intel = NativeBrokerAdapter::new(DriverStrategy::IntelDisplay, &native_strategy_host)?;
    let virtio = NativeBrokerAdapter::new(DriverStrategy::VirtioGpu, &native_strategy_host)?;
    let virtual_svga =
        NativeBrokerAdapter::new(DriverStrategy::VirtualSvga, &native_strategy_host)?;

    let firmware = FirmwareFramebufferBroker::new(&firmware_framebuffer_host);
    let quarantine = QuarantineBroker::new(secrets.dispatch)?;

    let brokers: [&dyn crate::drivers::drivernet::platform::StrategyBroker; 7] = [
        &nvidia,
        &amd,
        &intel,
        &virtio,
        &virtual_svga,
        &firmware,
        &quarantine,
    ];

    let mut platform = BlackLabDriverPlatform::new(
        &driver_clock,
        &device_isolation_broker,
        brokers,
        crate::drivers::drivernet::platform::DriverNetAuthority {
            device: &device_memory,
            dma: &dma_control,
            policy: &policy_control,
            fault: &fault_policy,
        },
        secrets.dispatch,
    )?;

    let mut observer =
        BlackLabDriverObserver::new(&blacklab.mnemosyne, &blacklab.oracular, &fault_policy);

    let mut scratch = DriverNetScratch::new();

    drivernet
        .resolve_all(
            &display_inventory,
            &configuration_reader,
            &gpu_topology,
            &mut platform,
            &mut observer,
            &mut scratch,
        )
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::drivernet::fingerprint::{
        BarEvidence, BAR_64BIT, BAR_PRESENT, TOPOLOGY_IOMMU_ISOLATED, VENDOR_NVIDIA,
    };

    #[test]
    fn hermes_handoff_rejects_incomplete_discovery_before_missing_iommu_backend() {
        let authority = unsafe { Authority::assume_root() };
        let device = authority.grant::<DeviceMemoryControl>();
        let dma = authority.grant::<DmaControl>();
        let descriptor = crate::drivers::drivernet::shims::hermes_nvidia::descriptor();
        let broker = BoulderIsolationBroker;

        let mut fingerprint = GpuFingerprint::EMPTY;
        fingerprint.vendor_id = VENDOR_NVIDIA;
        fingerprint.device_id = 0x2684;
        fingerprint.class_code = crate::drivers::drivernet::fingerprint::PCI_CLASS_DISPLAY;
        fingerprint.topology_flags = TOPOLOGY_IOMMU_ISOLATED;
        fingerprint.iommu_group = 7;
        fingerprint.bars[0] = BarEvidence {
            raw_low: 0x8000_0004,
            raw_high: 0,
            length: 0,
            flags: BAR_PRESENT | BAR_64BIT,
        };
        fingerprint.evidence_root = 0x1234;

        let incomplete = broker
            .begin(&fingerprint, &descriptor, &device, &dma)
            .unwrap_err();
        assert_eq!(incomplete.code, FaultCode::DescriptorRejected);

        fingerprint.bars[0].length = 16 * 1024 * 1024;
        let isolated_but_unbacked = broker
            .begin(&fingerprint, &descriptor, &device, &dma)
            .unwrap_err();
        assert_eq!(isolated_but_unbacked.code, FaultCode::TransactionRejected);
    }
}
