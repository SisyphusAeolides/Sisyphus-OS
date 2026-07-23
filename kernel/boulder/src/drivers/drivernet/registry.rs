use super::fingerprint::{
    GpuFingerprint, TOPOLOGY_FIRMWARE_FRAMEBUFFER, TOPOLOGY_IOMMU_ISOLATED,
    TOPOLOGY_VIRTUAL_MACHINE,
};
use super::model::DriverStrategy;

pub const MAXIMUM_PROBE_STEPS: usize = 12;
pub const MAXIMUM_SHIMS: usize = 7;

pub const PROBE_EVIDENCE_IDENTITY: u32 = 1 << 0;
pub const PROBE_EVIDENCE_MMIO_DECODE: u32 = 1 << 1;
pub const PROBE_EVIDENCE_IOMMU_DOMAIN: u32 = 1 << 2;
pub const PROBE_EVIDENCE_RESET_PATH: u32 = 1 << 3;
pub const PROBE_EVIDENCE_INTERRUPT_PATH: u32 = 1 << 4;
pub const PROBE_EVIDENCE_FIRMWARE_LEASE: u32 = 1 << 5;
pub const PROBE_EVIDENCE_TRANSPORT: u32 = 1 << 6;
pub const PROBE_EVIDENCE_HEALTH: u32 = 1 << 7;
pub const PROBE_EVIDENCE_DISPLAY_ENGINE: u32 = 1 << 8;
pub const PROBE_EVIDENCE_COMMAND_RING: u32 = 1 << 9;

pub const SHIM_FLAG_PRESERVE_FIRMWARE_DISPLAY: u32 = 1 << 0;
pub const SHIM_FLAG_REQUIRES_EXCLUSIVE_DEVICE: u32 = 1 << 1;
pub const SHIM_FLAG_SUPPORTS_ROLLBACK: u32 = 1 << 2;
pub const SHIM_FLAG_VIRTUAL_DEVICE: u32 = 1 << 3;
pub const SHIM_FLAG_READ_ONLY_PROBE: u32 = 1 << 4;
pub const SHIM_FLAG_TERMINAL_FALLBACK: u32 = 1 << 5;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ProbeSemantic {
    ValidateIdentity = 1,
    VerifyIommuIsolation = 2,
    MapControlBarReadOnly = 3,
    SampleVendorSignature = 4,
    VerifyResetMechanism = 5,
    VerifyInterruptRoute = 6,
    VerifyFirmwareLease = 7,
    VerifyTransportPersonality = 8,
    VerifyDisplayEngine = 9,
    VerifyCommandRingGeometry = 10,
    VerifyParavirtualCapability = 11,
    EstablishHealthBaseline = 12,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProbeStep {
    pub semantic: ProbeSemantic,
    pub evidence_bit: u32,
    pub maximum_ticks: u32,
    pub retries: u8,
    pub flags: u8,
    pub argument: u16,
}

impl ProbeStep {
    pub const EMPTY: Self = Self {
        semantic: ProbeSemantic::ValidateIdentity,
        evidence_bit: 0,
        maximum_ticks: 0,
        retries: 0,
        flags: 0,
        argument: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProbeProgram {
    pub steps: [ProbeStep; MAXIMUM_PROBE_STEPS],
    pub length: usize,
    pub required_evidence: u32,
    pub maximum_total_ticks: u64,
}

impl ProbeProgram {
    pub const fn new(
        steps: [ProbeStep; MAXIMUM_PROBE_STEPS],
        length: usize,
        required_evidence: u32,
        maximum_total_ticks: u64,
    ) -> Self {
        Self {
            steps,
            length,
            required_evidence,
            maximum_total_ticks,
        }
    }

    pub fn steps(&self) -> &[ProbeStep] {
        &self.steps[..self.length.min(MAXIMUM_PROBE_STEPS)]
    }

    pub const fn valid(self) -> bool {
        self.length != 0
            && self.length <= MAXIMUM_PROBE_STEPS
            && self.required_evidence != 0
            && self.maximum_total_ticks != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VendorGate {
    pub vendor_id: u16,
    pub device_id_mask: u16,
    pub device_id_value: u16,
}

impl VendorGate {
    pub const ANY: Self = Self {
        vendor_id: 0xffff,
        device_id_mask: 0,
        device_id_value: 0,
    };

    pub const fn matches(self, fingerprint: &GpuFingerprint) -> bool {
        (self.vendor_id == 0xffff || self.vendor_id == fingerprint.vendor_id)
            && (fingerprint.device_id & self.device_id_mask)
                == (self.device_id_value & self.device_id_mask)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShimDescriptor {
    pub strategy: DriverStrategy,
    pub name: &'static str,
    pub abi_version: u32,
    pub vendor_gate: VendorGate,
    pub required_topology: u32,
    pub forbidden_topology: u32,
    pub minimum_confidence_q16: u16,
    pub flags: u32,
    pub activation_budget_ticks: u64,
    pub health_budget_ticks: u64,
    pub program: ProbeProgram,
}

impl ShimDescriptor {
    pub const fn valid(self) -> bool {
        !self.name.is_empty()
            && self.abi_version != 0
            && self.activation_budget_ticks != 0
            && self.health_budget_ticks != 0
            && self.program.valid()
    }

    pub const fn accepts(self, fingerprint: &GpuFingerprint, confidence_q16: u16) -> bool {
        self.valid()
            && self.vendor_gate.matches(fingerprint)
            && fingerprint.topology_flags & self.required_topology == self.required_topology
            && fingerprint.topology_flags & self.forbidden_topology == 0
            && confidence_q16 >= self.minimum_confidence_q16
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistryError {
    DuplicateStrategy,
    InvalidDescriptor,
    MissingStrategy,
}

pub struct ShimRegistry {
    descriptors: [ShimDescriptor; MAXIMUM_SHIMS],
}

impl ShimRegistry {
    pub fn new(descriptors: [ShimDescriptor; MAXIMUM_SHIMS]) -> Result<Self, RegistryError> {
        for descriptor in descriptors {
            if !descriptor.valid() {
                return Err(RegistryError::InvalidDescriptor);
            }
        }

        for left in 0..descriptors.len() {
            for right in left + 1..descriptors.len() {
                if descriptors[left].strategy == descriptors[right].strategy {
                    return Err(RegistryError::DuplicateStrategy);
                }
            }
        }

        Ok(Self { descriptors })
    }

    pub fn black_lab() -> Result<Self, RegistryError> {
        Self::new([
            super::shims::hermes_nvidia::descriptor(),
            super::shims::amd_display::descriptor(),
            super::shims::intel_display::descriptor(),
            super::shims::virtio_gpu::descriptor(),
            super::shims::virtual_svga::descriptor(),
            super::shims::firmware_fb::descriptor(),
            super::shims::quarantine::descriptor(),
        ])
    }

    pub fn descriptor(&self, strategy: DriverStrategy) -> Result<&ShimDescriptor, RegistryError> {
        self.descriptors
            .iter()
            .find(|descriptor| descriptor.strategy == strategy)
            .ok_or(RegistryError::MissingStrategy)
    }

    pub fn descriptors(&self) -> &[ShimDescriptor] {
        &self.descriptors
    }
}

pub const fn native_required_topology() -> u32 {
    TOPOLOGY_IOMMU_ISOLATED
}

pub const fn firmware_required_topology() -> u32 {
    TOPOLOGY_FIRMWARE_FRAMEBUFFER
}

pub const fn virtual_required_topology() -> u32 {
    TOPOLOGY_VIRTUAL_MACHINE
}

pub const fn step(
    semantic: ProbeSemantic,
    evidence_bit: u32,
    maximum_ticks: u32,
    retries: u8,
    argument: u16,
) -> ProbeStep {
    ProbeStep {
        semantic,
        evidence_bit,
        maximum_ticks,
        retries,
        flags: 0,
        argument,
    }
}

pub const fn empty_steps() -> [ProbeStep; MAXIMUM_PROBE_STEPS] {
    [ProbeStep::EMPTY; MAXIMUM_PROBE_STEPS]
}
