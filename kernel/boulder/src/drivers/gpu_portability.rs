use sisyphus_driver_abi::gpu::{
    GPU_BAR_64BIT, GPU_BAR_COUNT, GPU_BAR_IO, GPU_BAR_PREFETCHABLE, GPU_BAR_PRESENT,
    GPU_FIRMWARE_PRESERVE_BOOT_SURFACE, GPU_FIRMWARE_REQUIRED, GPU_PORTABLE_ABI_VERSION,
    GPU_TOPOLOGY_BOOT_DISPLAY, GPU_TOPOLOGY_FIRMWARE_SURFACE, GPU_TOPOLOGY_HOTPLUG,
    GPU_TOPOLOGY_INVENTORY_COMPLETE, GPU_TOPOLOGY_IOMMU_ISOLATED,
    GPU_TOPOLOGY_IOMMU_PRESENT, GPU_TOPOLOGY_VIRTUAL_MACHINE, GpuBarEvidence,
    GpuCompatibilityManifest, GpuCompatibilityProof, GpuDeviceEvidence, GpuDriverClass,
    GpuFirmwareSurface, GpuPciIdentity, evaluate_compatibility,
};

use super::drivernet::fingerprint::{
    BAR_64BIT, BAR_IO, BAR_PREFETCHABLE, BAR_PRESENT, GpuFingerprint,
    TOPOLOGY_BOOT_DISPLAY, TOPOLOGY_CONFIG_INCOMPLETE, TOPOLOGY_FIRMWARE_FRAMEBUFFER,
    TOPOLOGY_HOTPLUG_PORT, TOPOLOGY_INVENTORY_OVERFLOW, TOPOLOGY_IOMMU_ISOLATED,
    TOPOLOGY_IOMMU_PRESENT, TOPOLOGY_VIRTUAL_MACHINE, VENDOR_AMD, VENDOR_INTEL,
    VENDOR_NVIDIA, VENDOR_VIRTIO, VENDOR_VMWARE,
};
use super::drivernet::model::DriverStrategy;

pub const HERMES_DRIVER_ID: u64 = 0x4845_524d_4553_0001;
pub const AMD_DISPLAY_DRIVER_ID: u64 = 0x414d_445f_4453_5001;
pub const INTEL_DISPLAY_DRIVER_ID: u64 = 0x494e_5445_4c44_5001;
pub const VIRTIO_GPU_DRIVER_ID: u64 = 0x5649_5254_4750_5501;
pub const VIRTUAL_SVGA_DRIVER_ID: u64 = 0x5356_4741_4750_5501;
pub const FIRMWARE_SURFACE_DRIVER_ID: u64 = 0x4657_5355_5246_0001;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PortabilityFault {
    UnsupportedStrategy,
    Rejected(GpuCompatibilityProof),
    Ambiguous,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PortabilityResolution {
    pub strategy: DriverStrategy,
    pub proof: GpuCompatibilityProof,
    pub resolution_root: u64,
}

impl PortabilityResolution {
    pub const fn accepted(self) -> bool {
        self.proof.accepted() && self.resolution_root != 0
    }
}

pub struct GpuPortabilityResolver {
    secret: u64,
}

impl GpuPortabilityResolver {
    pub fn new(secret: u64) -> Result<Self, PortabilityFault> {
        if secret == 0 {
            return Err(PortabilityFault::UnsupportedStrategy);
        }
        Ok(Self { secret })
    }

    pub fn prove(
        &self,
        strategy: DriverStrategy,
        fingerprint: &GpuFingerprint,
    ) -> Result<PortabilityResolution, PortabilityFault> {
        let manifest = manifest_for(strategy).ok_or(PortabilityFault::UnsupportedStrategy)?;
        let evidence = portable_evidence(fingerprint);
        let proof = evaluate_compatibility(&manifest, &evidence, self.secret);
        if !proof.accepted() {
            return Err(PortabilityFault::Rejected(proof));
        }

        Ok(PortabilityResolution {
            strategy,
            resolution_root: mix(
                self.secret,
                proof.proof_root ^ fingerprint.evidence_root ^ strategy.index() as u64,
            ),
            proof,
        })
    }

    pub fn choose(
        &self,
        candidates: &[DriverStrategy],
        fingerprint: &GpuFingerprint,
    ) -> Result<PortabilityResolution, PortabilityFault> {
        let mut winner: Option<PortabilityResolution> = None;
        let mut tied = false;

        for strategy in candidates.iter().copied() {
            let Ok(candidate) = self.prove(strategy, fingerprint) else {
                continue;
            };
            match winner {
                None => {
                    winner = Some(candidate);
                    tied = false;
                }
                Some(current) if candidate.proof.score_q16 > current.proof.score_q16 => {
                    winner = Some(candidate);
                    tied = false;
                }
                Some(current) if candidate.proof.score_q16 == current.proof.score_q16 => {
                    tied = true;
                }
                Some(_) => {}
            }
        }

        if tied {
            return Err(PortabilityFault::Ambiguous);
        }
        winner.ok_or(PortabilityFault::UnsupportedStrategy)
    }
}

pub fn manifest_for(strategy: DriverStrategy) -> Option<GpuCompatibilityManifest> {
    match strategy {
        DriverStrategy::HermesNvidia => Some(native_manifest(
            HERMES_DRIVER_ID,
            VENDOR_NVIDIA,
            2_000,
        )),
        DriverStrategy::AmdDisplay => Some(native_manifest(
            AMD_DISPLAY_DRIVER_ID,
            VENDOR_AMD,
            1_800,
        )),
        DriverStrategy::IntelDisplay => Some(native_manifest(
            INTEL_DISPLAY_DRIVER_ID,
            VENDOR_INTEL,
            1_800,
        )),
        DriverStrategy::VirtioGpu => Some(paravirtual_manifest(
            VIRTIO_GPU_DRIVER_ID,
            VENDOR_VIRTIO,
            2_500,
        )),
        DriverStrategy::VirtualSvga => Some(paravirtual_manifest(
            VIRTUAL_SVGA_DRIVER_ID,
            VENDOR_VMWARE,
            2_000,
        )),
        DriverStrategy::FirmwareFramebuffer => Some(firmware_manifest()),
        DriverStrategy::Quarantine => None,
    }
}

fn native_manifest(
    driver_id: u64,
    vendor_id: u16,
    priority: u16,
) -> GpuCompatibilityManifest {
    let mut minimum_bar_lengths = [0_u64; GPU_BAR_COUNT];
    minimum_bar_lengths[0] = 4096;

    GpuCompatibilityManifest {
        abi_version: GPU_PORTABLE_ABI_VERSION,
        struct_size: core::mem::size_of::<GpuCompatibilityManifest>() as u32,
        driver_id,
        driver_class: GpuDriverClass::Native,
        reserved0: [0; 7],
        vendor_id,
        device_id_mask: 0,
        device_id_value: 0,
        class_mask: 0xff,
        class_value: 0x03,
        subclass_mask: 0,
        subclass_value: 0,
        revision_minimum: 0,
        revision_maximum: u8::MAX,
        reserved1: 0,
        architecture_mask: 0,
        architecture_value: 0,
        required_topology: GPU_TOPOLOGY_IOMMU_ISOLATED,
        forbidden_topology: 0,
        required_features: 0,
        optional_features: 0,
        required_bar_mask: 1,
        reserved2: [0; 7],
        minimum_bar_lengths,
        firmware_policy: GPU_FIRMWARE_PRESERVE_BOOT_SURFACE,
        priority,
        reserved3: 0,
    }
}

fn paravirtual_manifest(
    driver_id: u64,
    vendor_id: u16,
    priority: u16,
) -> GpuCompatibilityManifest {
    let mut manifest = native_manifest(driver_id, vendor_id, priority);
    manifest.driver_class = GpuDriverClass::Paravirtual;
    manifest.required_topology = GPU_TOPOLOGY_VIRTUAL_MACHINE;
    manifest
}

fn firmware_manifest() -> GpuCompatibilityManifest {
    GpuCompatibilityManifest {
        abi_version: GPU_PORTABLE_ABI_VERSION,
        struct_size: core::mem::size_of::<GpuCompatibilityManifest>() as u32,
        driver_id: FIRMWARE_SURFACE_DRIVER_ID,
        driver_class: GpuDriverClass::FirmwareSurface,
        reserved0: [0; 7],
        vendor_id: 0xffff,
        device_id_mask: 0,
        device_id_value: 0,
        class_mask: 0,
        class_value: 0,
        subclass_mask: 0,
        subclass_value: 0,
        revision_minimum: 0,
        revision_maximum: u8::MAX,
        reserved1: 0,
        architecture_mask: 0,
        architecture_value: 0,
        required_topology: GPU_TOPOLOGY_FIRMWARE_SURFACE,
        forbidden_topology: 0,
        required_features: 0,
        optional_features: 0,
        required_bar_mask: 0,
        reserved2: [0; 7],
        minimum_bar_lengths: [0; GPU_BAR_COUNT],
        firmware_policy: GPU_FIRMWARE_REQUIRED | GPU_FIRMWARE_PRESERVE_BOOT_SURFACE,
        priority: 1_000,
        reserved3: 0,
    }
}

pub fn portable_evidence(fingerprint: &GpuFingerprint) -> GpuDeviceEvidence {
    let mut bars = [GpuBarEvidence::EMPTY; GPU_BAR_COUNT];
    for (output, source) in bars.iter_mut().zip(fingerprint.bars.iter().copied()) {
        let mut flags = 0_u32;
        if source.flags & BAR_PRESENT != 0 {
            flags |= GPU_BAR_PRESENT;
        }
        if source.flags & BAR_IO != 0 {
            flags |= GPU_BAR_IO;
        }
        if source.flags & BAR_64BIT != 0 {
            flags |= GPU_BAR_64BIT;
        }
        if source.flags & BAR_PREFETCHABLE != 0 {
            flags |= GPU_BAR_PREFETCHABLE;
        }
        *output = GpuBarEvidence {
            physical_address: bar_address(source.raw_low, source.raw_high, source.flags),
            length: source.length,
            flags,
            reserved: 0,
        };
    }

    let mut topology = 0_u64;
    if fingerprint.topology_flags & TOPOLOGY_BOOT_DISPLAY != 0 {
        topology |= GPU_TOPOLOGY_BOOT_DISPLAY;
    }
    if fingerprint.topology_flags & TOPOLOGY_IOMMU_PRESENT != 0 {
        topology |= GPU_TOPOLOGY_IOMMU_PRESENT;
    }
    if fingerprint.topology_flags & TOPOLOGY_IOMMU_ISOLATED != 0 {
        topology |= GPU_TOPOLOGY_IOMMU_ISOLATED;
    }
    if fingerprint.topology_flags & TOPOLOGY_FIRMWARE_FRAMEBUFFER != 0 {
        topology |= GPU_TOPOLOGY_FIRMWARE_SURFACE;
    }
    if fingerprint.topology_flags & TOPOLOGY_VIRTUAL_MACHINE != 0 {
        topology |= GPU_TOPOLOGY_VIRTUAL_MACHINE;
    }
    if fingerprint.topology_flags & TOPOLOGY_HOTPLUG_PORT != 0 {
        topology |= GPU_TOPOLOGY_HOTPLUG;
    }
    if fingerprint.topology_flags
        & (TOPOLOGY_INVENTORY_OVERFLOW | TOPOLOGY_CONFIG_INCOMPLETE)
        == 0
    {
        topology |= GPU_TOPOLOGY_INVENTORY_COMPLETE;
    }

    let firmware = fingerprint.firmware_framebuffer;
    GpuDeviceEvidence {
        abi_version: GPU_PORTABLE_ABI_VERSION,
        struct_size: core::mem::size_of::<GpuDeviceEvidence>() as u32,
        identity: GpuPciIdentity {
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
        },
        bars,
        capability_flags: u64::from(fingerprint.capability_flags),
        topology_flags: topology,
        observed_features: u64::from(fingerprint.capability_flags),
        architecture_hint: 0,
        bootrom_revision: 0,
        firmware_surface: if firmware.usable() {
            GpuFirmwareSurface {
                physical_address: firmware.physical_address,
                byte_length: firmware.byte_length,
                width: firmware.width,
                height: firmware.height,
                pitch: firmware.pitch,
                format: firmware.format,
                flags: 0,
                reserved: 0,
            }
        } else {
            GpuFirmwareSurface::NONE
        },
        evidence_root: fingerprint.evidence_root,
    }
}

pub const fn bar_address(raw_low: u32, raw_high: u32, flags: u8) -> u64 {
    if flags & BAR_PRESENT == 0 {
        0
    } else if flags & BAR_IO != 0 {
        (raw_low & !0x3) as u64
    } else if flags & BAR_64BIT != 0 {
        ((raw_high as u64) << 32) | ((raw_low & !0x0f) as u64)
    } else {
        (raw_low & !0x0f) as u64
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::drivernet::fingerprint::{
        BAR_64BIT, BAR_PRESENT, BarEvidence, FirmwareFramebufferEvidence,
        FirmwareFramebufferKind,
    };

    #[test]
    fn native_resolution_requires_isolation_and_real_bar_length() {
        let mut fingerprint = GpuFingerprint::EMPTY;
        fingerprint.vendor_id = VENDOR_NVIDIA;
        fingerprint.class_code = 0x03;
        fingerprint.topology_flags = TOPOLOGY_IOMMU_ISOLATED;
        fingerprint.bars[0] = BarEvidence {
            raw_low: 0x8000_0004,
            raw_high: 0,
            length: 16 * 1024 * 1024,
            flags: BAR_PRESENT | BAR_64BIT,
        };
        fingerprint.evidence_root = 9;

        let resolver = GpuPortabilityResolver::new(17).unwrap();
        assert!(resolver
            .prove(DriverStrategy::HermesNvidia, &fingerprint)
            .unwrap()
            .accepted());

        fingerprint.topology_flags = 0;
        assert!(matches!(
            resolver.prove(DriverStrategy::HermesNvidia, &fingerprint),
            Err(PortabilityFault::Rejected(_))
        ));
    }

    #[test]
    fn firmware_surface_accepts_synthetic_fingerprint() {
        let mut fingerprint = GpuFingerprint::EMPTY;
        fingerprint.firmware_framebuffer = FirmwareFramebufferEvidence {
            kind: FirmwareFramebufferKind::Vbe,
            physical_address: 0xe000_0000,
            width: 1024,
            height: 768,
            pitch: 4096,
            format: 1,
            byte_length: 4096 * 768,
            owner: None,
            retained: true,
        };
        fingerprint.topology_flags = TOPOLOGY_FIRMWARE_FRAMEBUFFER;
        fingerprint.evidence_root = 11;

        let resolver = GpuPortabilityResolver::new(19).unwrap();
        assert!(resolver
            .prove(DriverStrategy::FirmwareFramebuffer, &fingerprint)
            .unwrap()
            .accepted());
    }
}
