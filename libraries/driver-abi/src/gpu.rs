pub const GPU_PORTABLE_ABI_MAJOR: u32 = 1;
pub const GPU_PORTABLE_ABI_MINOR: u32 = 0;
pub const GPU_PORTABLE_ABI_VERSION: u32 =
    (GPU_PORTABLE_ABI_MAJOR << 16) | GPU_PORTABLE_ABI_MINOR;

pub const GPU_BAR_COUNT: usize = 6;

pub const GPU_BAR_PRESENT: u32 = 1 << 0;
pub const GPU_BAR_IO: u32 = 1 << 1;
pub const GPU_BAR_64BIT: u32 = 1 << 2;
pub const GPU_BAR_PREFETCHABLE: u32 = 1 << 3;

pub const GPU_TOPOLOGY_BOOT_DISPLAY: u64 = 1 << 0;
pub const GPU_TOPOLOGY_IOMMU_PRESENT: u64 = 1 << 1;
pub const GPU_TOPOLOGY_IOMMU_ISOLATED: u64 = 1 << 2;
pub const GPU_TOPOLOGY_FIRMWARE_SURFACE: u64 = 1 << 3;
pub const GPU_TOPOLOGY_VIRTUAL_MACHINE: u64 = 1 << 4;
pub const GPU_TOPOLOGY_HOTPLUG: u64 = 1 << 5;
pub const GPU_TOPOLOGY_INVENTORY_COMPLETE: u64 = 1 << 6;

pub const GPU_FIRMWARE_REQUIRED: u32 = 1 << 0;
pub const GPU_FIRMWARE_FORBIDDEN: u32 = 1 << 1;
pub const GPU_FIRMWARE_PRESERVE_BOOT_SURFACE: u32 = 1 << 2;
pub const GPU_FIRMWARE_AUTHENTICATED: u32 = 1 << 3;

pub const GPU_OBLIGATION_ABI: u64 = 1 << 0;
pub const GPU_OBLIGATION_IDENTITY: u64 = 1 << 1;
pub const GPU_OBLIGATION_CLASS: u64 = 1 << 2;
pub const GPU_OBLIGATION_REVISION: u64 = 1 << 3;
pub const GPU_OBLIGATION_ARCHITECTURE: u64 = 1 << 4;
pub const GPU_OBLIGATION_TOPOLOGY: u64 = 1 << 5;
pub const GPU_OBLIGATION_FEATURES: u64 = 1 << 6;
pub const GPU_OBLIGATION_BARS: u64 = 1 << 7;
pub const GPU_OBLIGATION_FIRMWARE: u64 = 1 << 8;
pub const GPU_REQUIRED_OBLIGATIONS: u64 = GPU_OBLIGATION_ABI
    | GPU_OBLIGATION_IDENTITY
    | GPU_OBLIGATION_CLASS
    | GPU_OBLIGATION_REVISION
    | GPU_OBLIGATION_ARCHITECTURE
    | GPU_OBLIGATION_TOPOLOGY
    | GPU_OBLIGATION_FEATURES
    | GPU_OBLIGATION_BARS
    | GPU_OBLIGATION_FIRMWARE;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum GpuDriverClass {
    Native = 1,
    Paravirtual = 2,
    FirmwareSurface = 3,
    ForeignPersonality = 4,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum GpuCompatibilityVerdict {
    Rejected = 0,
    Accepted = 1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct GpuPciIdentity {
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

impl GpuPciIdentity {
    pub const EMPTY: Self = Self {
        segment: 0,
        bus: 0,
        slot: 0,
        function: 0,
        revision: 0,
        vendor_id: 0xffff,
        device_id: 0xffff,
        subsystem_vendor_id: 0xffff,
        subsystem_device_id: 0xffff,
        class_code: 0,
        subclass: 0,
        programming_interface: 0,
        reserved: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct GpuBarEvidence {
    pub physical_address: u64,
    pub length: u64,
    pub flags: u32,
    pub reserved: u32,
}

impl GpuBarEvidence {
    pub const EMPTY: Self = Self {
        physical_address: 0,
        length: 0,
        flags: 0,
        reserved: 0,
    };

    pub const fn usable_mmio(self) -> bool {
        self.flags & GPU_BAR_PRESENT != 0
            && self.flags & GPU_BAR_IO == 0
            && self.physical_address != 0
            && self.length != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct GpuFirmwareSurface {
    pub physical_address: u64,
    pub byte_length: u64,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub format: u32,
    pub flags: u32,
    pub reserved: u32,
}

impl GpuFirmwareSurface {
    pub const NONE: Self = Self {
        physical_address: 0,
        byte_length: 0,
        width: 0,
        height: 0,
        pitch: 0,
        format: 0,
        flags: 0,
        reserved: 0,
    };

    pub const fn usable(self) -> bool {
        self.physical_address != 0
            && self.byte_length != 0
            && self.width != 0
            && self.height != 0
            && self.pitch != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct GpuDeviceEvidence {
    pub abi_version: u32,
    pub struct_size: u32,
    pub identity: GpuPciIdentity,
    pub bars: [GpuBarEvidence; GPU_BAR_COUNT],
    pub capability_flags: u64,
    pub topology_flags: u64,
    pub observed_features: u64,
    pub architecture_hint: u32,
    pub bootrom_revision: u32,
    pub firmware_surface: GpuFirmwareSurface,
    pub evidence_root: u64,
}

impl GpuDeviceEvidence {
    pub const EMPTY: Self = Self {
        abi_version: GPU_PORTABLE_ABI_VERSION,
        struct_size: core::mem::size_of::<Self>() as u32,
        identity: GpuPciIdentity::EMPTY,
        bars: [GpuBarEvidence::EMPTY; GPU_BAR_COUNT],
        capability_flags: 0,
        topology_flags: 0,
        observed_features: 0,
        architecture_hint: 0,
        bootrom_revision: 0,
        firmware_surface: GpuFirmwareSurface::NONE,
        evidence_root: 0,
    };

    pub fn valid(&self) -> bool {
        if self.abi_version >> 16 != GPU_PORTABLE_ABI_MAJOR
            || self.struct_size as usize < core::mem::size_of::<Self>()
            || self.evidence_root == 0
        {
            return false;
        }

        let firmware_flag =
            self.topology_flags & GPU_TOPOLOGY_FIRMWARE_SURFACE != 0;
        if firmware_flag != self.firmware_surface.usable() {
            return false;
        }

        for bar in self.bars {
            let present = bar.flags & GPU_BAR_PRESENT != 0;
            if present {
                if bar.physical_address == 0 || bar.length == 0 {
                    return false;
                }
            } else if bar.physical_address != 0
                || bar.length != 0
                || bar.flags != 0
            {
                return false;
            }
        }

        self.identity.vendor_id != 0
            && (self.identity.vendor_id != 0xffff
                || self.firmware_surface.usable())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct GpuCompatibilityManifest {
    pub abi_version: u32,
    pub struct_size: u32,
    pub driver_id: u64,
    pub driver_class: GpuDriverClass,
    pub reserved0: [u8; 7],
    pub vendor_id: u16,
    pub device_id_mask: u16,
    pub device_id_value: u16,
    pub class_mask: u8,
    pub class_value: u8,
    pub subclass_mask: u8,
    pub subclass_value: u8,
    pub revision_minimum: u8,
    pub revision_maximum: u8,
    pub reserved1: u16,
    pub architecture_mask: u32,
    pub architecture_value: u32,
    pub required_topology: u64,
    pub forbidden_topology: u64,
    pub required_features: u64,
    pub optional_features: u64,
    pub required_bar_mask: u8,
    pub reserved2: [u8; 7],
    pub minimum_bar_lengths: [u64; GPU_BAR_COUNT],
    pub firmware_policy: u32,
    pub priority: u16,
    pub reserved3: u16,
}

impl GpuCompatibilityManifest {
    pub const EMPTY: Self = Self {
        abi_version: GPU_PORTABLE_ABI_VERSION,
        struct_size: core::mem::size_of::<Self>() as u32,
        driver_id: 0,
        driver_class: GpuDriverClass::ForeignPersonality,
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
        required_topology: 0,
        forbidden_topology: 0,
        required_features: 0,
        optional_features: 0,
        required_bar_mask: 0,
        reserved2: [0; 7],
        minimum_bar_lengths: [0; GPU_BAR_COUNT],
        firmware_policy: 0,
        priority: 0,
        reserved3: 0,
    };

    pub const fn valid(self) -> bool {
        let firmware_mode = self.firmware_policy
            & (GPU_FIRMWARE_REQUIRED | GPU_FIRMWARE_FORBIDDEN);

        self.abi_version >> 16 == GPU_PORTABLE_ABI_MAJOR
            && self.struct_size as usize >= core::mem::size_of::<Self>()
            && self.driver_id != 0
            && self.revision_minimum <= self.revision_maximum
            && self.required_topology & self.forbidden_topology == 0
            && self.required_features & self.optional_features == 0
            && self.required_bar_mask & !0x3f == 0
            && firmware_mode
                != (GPU_FIRMWARE_REQUIRED | GPU_FIRMWARE_FORBIDDEN)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct GpuCompatibilityProof {
    pub driver_id: u64,
    pub evidence_root: u64,
    pub satisfied_obligations: u64,
    pub missing_obligations: u64,
    pub violated_obligations: u64,
    pub matched_optional_features: u64,
    pub score_q16: u32,
    pub verdict: GpuCompatibilityVerdict,
    pub reserved: [u8; 3],
    pub proof_root: u64,
}

impl GpuCompatibilityProof {
    pub const EMPTY: Self = Self {
        driver_id: 0,
        evidence_root: 0,
        satisfied_obligations: 0,
        missing_obligations: GPU_REQUIRED_OBLIGATIONS,
        violated_obligations: 0,
        matched_optional_features: 0,
        score_q16: 0,
        verdict: GpuCompatibilityVerdict::Rejected,
        reserved: [0; 3],
        proof_root: 0,
    };

    pub const fn accepted(self) -> bool {
        matches!(self.verdict, GpuCompatibilityVerdict::Accepted)
            && self.missing_obligations == 0
            && self.violated_obligations == 0
            && self.driver_id != 0
            && self.evidence_root != 0
            && self.proof_root != 0
    }
}

pub fn evaluate_compatibility(
    manifest: &GpuCompatibilityManifest,
    evidence: &GpuDeviceEvidence,
    secret: u64,
) -> GpuCompatibilityProof {
    if secret == 0 || !manifest.valid() || !evidence.valid() {
        return GpuCompatibilityProof::EMPTY;
    }

    let mut satisfied = 0_u64;
    let mut missing = 0_u64;
    let mut violated = 0_u64;

    obligation(
        evidence.abi_version >> 16 == GPU_PORTABLE_ABI_MAJOR,
        GPU_OBLIGATION_ABI,
        &mut satisfied,
        &mut missing,
    );

    let identity_matches = (manifest.vendor_id == 0xffff
        || manifest.vendor_id == evidence.identity.vendor_id)
        && (evidence.identity.device_id & manifest.device_id_mask)
            == (manifest.device_id_value & manifest.device_id_mask);
    obligation(
        identity_matches,
        GPU_OBLIGATION_IDENTITY,
        &mut satisfied,
        &mut missing,
    );

    let class_matches = evidence.identity.class_code & manifest.class_mask
        == manifest.class_value & manifest.class_mask
        && evidence.identity.subclass & manifest.subclass_mask
            == manifest.subclass_value & manifest.subclass_mask;
    obligation(
        class_matches,
        GPU_OBLIGATION_CLASS,
        &mut satisfied,
        &mut missing,
    );

    obligation(
        (manifest.revision_minimum..=manifest.revision_maximum)
            .contains(&evidence.identity.revision),
        GPU_OBLIGATION_REVISION,
        &mut satisfied,
        &mut missing,
    );

    obligation(
        evidence.architecture_hint & manifest.architecture_mask
            == manifest.architecture_value & manifest.architecture_mask,
        GPU_OBLIGATION_ARCHITECTURE,
        &mut satisfied,
        &mut missing,
    );

    let required_topology = evidence.topology_flags & manifest.required_topology
        == manifest.required_topology;
    obligation(
        required_topology,
        GPU_OBLIGATION_TOPOLOGY,
        &mut satisfied,
        &mut missing,
    );
    if evidence.topology_flags & manifest.forbidden_topology != 0 {
        violated |= GPU_OBLIGATION_TOPOLOGY;
    }

    obligation(
        evidence.observed_features & manifest.required_features
            == manifest.required_features,
        GPU_OBLIGATION_FEATURES,
        &mut satisfied,
        &mut missing,
    );

    let mut bars_match = true;
    for index in 0..GPU_BAR_COUNT {
        let required = manifest.required_bar_mask & (1 << index) != 0;
        let minimum = manifest.minimum_bar_lengths[index];
        if required
            && (!evidence.bars[index].usable_mmio()
                || evidence.bars[index].length < minimum)
        {
            bars_match = false;
        }
    }
    obligation(
        bars_match,
        GPU_OBLIGATION_BARS,
        &mut satisfied,
        &mut missing,
    );

    let firmware_present = evidence.firmware_surface.usable();
    let firmware_ok = if manifest.firmware_policy & GPU_FIRMWARE_REQUIRED != 0 {
        firmware_present
    } else if manifest.firmware_policy & GPU_FIRMWARE_FORBIDDEN != 0 {
        !firmware_present
    } else {
        true
    };
    obligation(
        firmware_ok,
        GPU_OBLIGATION_FIRMWARE,
        &mut satisfied,
        &mut missing,
    );

    let optional = evidence.observed_features & manifest.optional_features;
    let required_score = satisfied.count_ones() as u64 * 6_000;
    let optional_score = optional.count_ones() as u64 * 1_000;
    let priority_score = u64::from(manifest.priority).min(10_000);
    let score_q16 = required_score
        .saturating_add(optional_score)
        .saturating_add(priority_score)
        .min(u64::from(u32::MAX)) as u32;

    let verdict = if missing == 0 && violated == 0 {
        GpuCompatibilityVerdict::Accepted
    } else {
        GpuCompatibilityVerdict::Rejected
    };

    let mut proof = GpuCompatibilityProof {
        driver_id: manifest.driver_id,
        evidence_root: evidence.evidence_root,
        satisfied_obligations: satisfied,
        missing_obligations: missing,
        violated_obligations: violated,
        matched_optional_features: optional,
        score_q16,
        verdict,
        reserved: [0; 3],
        proof_root: 0,
    };
    proof.proof_root = compatibility_root(secret, &proof);
    proof
}

fn obligation(
    condition: bool,
    bit: u64,
    satisfied: &mut u64,
    missing: &mut u64,
) {
    if condition {
        *satisfied |= bit;
    } else {
        *missing |= bit;
    }
}

fn compatibility_root(secret: u64, proof: &GpuCompatibilityProof) -> u64 {
    let mut state = mix(secret, proof.driver_id);
    state = mix(state, proof.evidence_root);
    state = mix(state, proof.satisfied_obligations);
    state = mix(state, proof.missing_obligations);
    state = mix(state, proof.violated_obligations);
    state = mix(state, proof.matched_optional_features);
    state = mix(state, u64::from(proof.score_q16));
    mix(state, proof.verdict as u8 as u64)
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

    fn evidence() -> GpuDeviceEvidence {
        let mut value = GpuDeviceEvidence::EMPTY;
        value.identity.vendor_id = 0x10de;
        value.identity.device_id = 0x2684;
        value.identity.class_code = 0x03;
        value.identity.revision = 1;
        value.bars[0] = GpuBarEvidence {
            physical_address: 0x8000_0000,
            length: 16 * 1024 * 1024,
            flags: GPU_BAR_PRESENT | GPU_BAR_64BIT,
            reserved: 0,
        };
        value.topology_flags = GPU_TOPOLOGY_IOMMU_ISOLATED;
        value.observed_features = 0b111;
        value.evidence_root = 7;
        value
    }

    fn manifest() -> GpuCompatibilityManifest {
        GpuCompatibilityManifest {
            abi_version: GPU_PORTABLE_ABI_VERSION,
            struct_size: core::mem::size_of::<GpuCompatibilityManifest>() as u32,
            driver_id: 11,
            driver_class: GpuDriverClass::Native,
            reserved0: [0; 7],
            vendor_id: 0x10de,
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
            required_features: 0b011,
            optional_features: 0b100,
            required_bar_mask: 1,
            reserved2: [0; 7],
            minimum_bar_lengths: [4096, 0, 0, 0, 0, 0],
            firmware_policy: 0,
            priority: 1_000,
            reserved3: 0,
        }
    }

    #[test]
    fn accepted_proof_binds_manifest_and_evidence() {
        let proof = evaluate_compatibility(&manifest(), &evidence(), 19);
        assert!(proof.accepted());
        assert_eq!(proof.matched_optional_features, 0b100);
    }

    #[test]
    fn missing_isolation_rejects_native_driver() {
        let mut value = evidence();
        value.topology_flags = 0;
        let proof = evaluate_compatibility(&manifest(), &value, 19);
        assert!(!proof.accepted());
        assert_ne!(proof.missing_obligations & GPU_OBLIGATION_TOPOLOGY, 0);
    }

    #[test]
    fn undersized_bar_rejects_driver() {
        let mut value = evidence();
        value.bars[0].length = 1024;
        let proof = evaluate_compatibility(&manifest(), &value, 19);
        assert!(!proof.accepted());
        assert_ne!(proof.missing_obligations & GPU_OBLIGATION_BARS, 0);
    }

    #[test]
    fn internally_inconsistent_evidence_is_rejected() {
        let mut value = evidence();
        value.topology_flags |= GPU_TOPOLOGY_FIRMWARE_SURFACE;
        assert!(!value.valid());
        assert_eq!(
            evaluate_compatibility(&manifest(), &value, 19),
            GpuCompatibilityProof::EMPTY,
        );
    }
}
