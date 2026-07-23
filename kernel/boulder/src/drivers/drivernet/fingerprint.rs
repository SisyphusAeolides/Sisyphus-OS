use crate::hw::pci::PciAddress;

use super::inventory::{DisplayFunctionInventory, PciFunctionRecord};

pub const PCI_CLASS_DISPLAY: u8 = 0x03;
pub const PCI_SUBCLASS_VGA: u8 = 0x00;
pub const PCI_SUBCLASS_XGA: u8 = 0x01;
pub const PCI_SUBCLASS_3D: u8 = 0x02;

pub const VENDOR_NVIDIA: u16 = 0x10de;
pub const VENDOR_AMD: u16 = 0x1002;
pub const VENDOR_INTEL: u16 = 0x8086;
pub const VENDOR_VIRTIO: u16 = 0x1af4;
pub const VENDOR_VMWARE: u16 = 0x15ad;
pub const VENDOR_REDHAT: u16 = 0x1b36;
pub const VENDOR_BOCHS: u16 = 0x1234;

pub const CAP_MSI: u32 = 1 << 0;
pub const CAP_MSIX: u32 = 1 << 1;
pub const CAP_PCIE: u32 = 1 << 2;
pub const CAP_POWER_MANAGEMENT: u32 = 1 << 3;
pub const CAP_AGP: u32 = 1 << 4;
pub const CAP_VENDOR_SPECIFIC: u32 = 1 << 5;
pub const CAP_ADVANCED_ERROR_REPORTING: u32 = 1 << 6;
pub const CAP_ACCESS_CONTROL_SERVICES: u32 = 1 << 7;
pub const CAP_SRIOV: u32 = 1 << 8;
pub const CAP_RESIZABLE_BAR: u32 = 1 << 9;
pub const CAP_ADDRESS_TRANSLATION: u32 = 1 << 10;

pub const BAR_PRESENT: u8 = 1 << 0;
pub const BAR_IO: u8 = 1 << 1;
pub const BAR_64BIT: u8 = 1 << 2;
pub const BAR_PREFETCHABLE: u8 = 1 << 3;

pub const TOPOLOGY_BOOT_DISPLAY: u32 = 1 << 0;
pub const TOPOLOGY_INTERNAL_PANEL: u32 = 1 << 1;
pub const TOPOLOGY_REMOVABLE: u32 = 1 << 2;
pub const TOPOLOGY_IOMMU_PRESENT: u32 = 1 << 3;
pub const TOPOLOGY_IOMMU_ISOLATED: u32 = 1 << 4;
pub const TOPOLOGY_FIRMWARE_FRAMEBUFFER: u32 = 1 << 5;
pub const TOPOLOGY_HOTPLUG_PORT: u32 = 1 << 6;
pub const TOPOLOGY_VIRTUAL_MACHINE: u32 = 1 << 7;
pub const TOPOLOGY_INVENTORY_OVERFLOW: u32 = 1 << 8;
pub const TOPOLOGY_CONFIG_INCOMPLETE: u32 = 1 << 9;

pub const MAXIMUM_GPU_FINGERPRINTS: usize = 16;
pub const MAXIMUM_CAPABILITY_HOPS: usize = 48;
pub const PCI_CONFIGURATION_DWORDS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum FirmwareFramebufferKind {
    None = 0,
    UefiGop = 1,
    Vbe = 2,
    SimpleFramebuffer = 3,
    Hypervisor = 4,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FirmwareFramebufferEvidence {
    pub kind: FirmwareFramebufferKind,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub format: u32,
    pub byte_length: u64,
    pub owner: Option<PciFunctionAddress>,
    pub retained: bool,
}

impl FirmwareFramebufferEvidence {
    pub const NONE: Self = Self {
        kind: FirmwareFramebufferKind::None,
        width: 0,
        height: 0,
        pitch: 0,
        format: 0,
        byte_length: 0,
        owner: None,
        retained: false,
    };

    pub const fn usable(self) -> bool {
        !matches!(self.kind, FirmwareFramebufferKind::None)
            && self.width != 0
            && self.height != 0
            && self.pitch != 0
            && self.byte_length != 0
            && self.retained
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TopologyEvidence {
    pub segment: u16,
    pub acpi_path_hash: u64,
    pub iommu_group: u32,
    pub upstream_vendor_id: u16,
    pub upstream_device_id: u16,
    pub root_port_depth: u8,
    pub sibling_display_functions: u8,
    pub topology_flags: u32,
    pub resource_lengths: [u64; 6],
    pub firmware_framebuffer: FirmwareFramebufferEvidence,
}

impl TopologyEvidence {
    pub const EMPTY: Self = Self {
        segment: 0,
        acpi_path_hash: 0,
        iommu_group: u32::MAX,
        upstream_vendor_id: 0xffff,
        upstream_device_id: 0xffff,
        root_port_depth: 0,
        sibling_display_functions: 0,
        topology_flags: 0,
        resource_lengths: [0; 6],
        firmware_framebuffer: FirmwareFramebufferEvidence::NONE,
    };
}

pub trait TopologyEvidenceProvider {
    fn evidence_for(&self, function: &PciFunctionRecord) -> TopologyEvidence;

    fn firmware_framebuffer(&self) -> FirmwareFramebufferEvidence {
        FirmwareFramebufferEvidence::NONE
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciFunctionAddress {
    pub segment: u16,
    pub bus: u8,
    pub slot: u8,
    pub function: u8,
}

impl PciFunctionAddress {
    pub const fn legacy(self) -> Option<PciAddress> {
        if self.segment != 0 {
            return None;
        }
        PciAddress::new(self.bus, self.slot, self.function)
    }
}

pub trait PciConfigReader {
    fn maximum_offset(&self) -> u16;

    fn read_u32(&self, address: PciFunctionAddress, offset: u16) -> Result<u32, FingerprintError>;
}

pub struct LegacyConfigurationReader;

impl PciConfigReader for LegacyConfigurationReader {
    fn maximum_offset(&self) -> u16 {
        0x00ff
    }

    fn read_u32(&self, address: PciFunctionAddress, offset: u16) -> Result<u32, FingerprintError> {
        if offset & 3 != 0 {
            return Err(FingerprintError::UnalignedConfigurationOffset);
        }
        if offset > self.maximum_offset() {
            return Err(FingerprintError::UnsupportedConfigurationOffset);
        }
        let legacy = address
            .legacy()
            .ok_or(FingerprintError::UnsupportedPciSegment)?;

        // SAFETY: this adapter performs read-only configuration access through
        // the globally serialized PCI mechanism owned by Boulder.
        Ok(unsafe { crate::hw::pci::read_config_u32(legacy, offset as u8) })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BarEvidence {
    pub raw_low: u32,
    pub raw_high: u32,
    pub length: u64,
    pub flags: u8,
}

impl BarEvidence {
    pub const EMPTY: Self = Self {
        raw_low: 0,
        raw_high: 0,
        length: 0,
        flags: 0,
    };

    pub const fn present(self) -> bool {
        self.flags & BAR_PRESENT != 0
    }

    pub const fn is_mmio(self) -> bool {
        self.present() && self.flags & BAR_IO == 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GpuFingerprint {
    pub segment: u16,
    pub bus: u8,
    pub slot: u8,
    pub function: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub subsystem_vendor_id: u16,
    pub subsystem_device_id: u16,
    pub revision: u8,
    pub class_code: u8,
    pub subclass: u8,
    pub programming_interface: u8,
    pub header_type: u8,
    pub interrupt_line: u8,
    pub interrupt_pin: u8,
    pub command: u16,
    pub status: u16,
    pub capability_flags: u32,
    pub topology_flags: u32,
    pub acpi_path_hash: u64,
    pub iommu_group: u32,
    pub upstream_vendor_id: u16,
    pub upstream_device_id: u16,
    pub root_port_depth: u8,
    pub sibling_display_functions: u8,
    pub bars: [BarEvidence; 6],
    pub firmware_framebuffer: FirmwareFramebufferEvidence,
    pub evidence_root: u64,
}

impl GpuFingerprint {
    pub const EMPTY: Self = Self {
        segment: 0,
        bus: 0,
        slot: 0,
        function: 0,
        vendor_id: 0xffff,
        device_id: 0xffff,
        subsystem_vendor_id: 0xffff,
        subsystem_device_id: 0xffff,
        revision: 0,
        class_code: 0,
        subclass: 0,
        programming_interface: 0,
        header_type: 0,
        interrupt_line: 0xff,
        interrupt_pin: 0,
        command: 0,
        status: 0,
        capability_flags: 0,
        topology_flags: 0,
        acpi_path_hash: 0,
        iommu_group: u32::MAX,
        upstream_vendor_id: 0xffff,
        upstream_device_id: 0xffff,
        root_port_depth: 0,
        sibling_display_functions: 0,
        bars: [BarEvidence::EMPTY; 6],
        firmware_framebuffer: FirmwareFramebufferEvidence::NONE,
        evidence_root: 0,
    };

    pub const fn legacy_address(self) -> Option<PciAddress> {
        self.function_address().legacy()
    }

    pub const fn function_address(self) -> PciFunctionAddress {
        PciFunctionAddress {
            segment: self.segment,
            bus: self.bus,
            slot: self.slot,
            function: self.function,
        }
    }

    pub const fn is_display(self) -> bool {
        self.class_code == PCI_CLASS_DISPLAY
    }

    pub const fn has_mmio(self) -> bool {
        let mut index = 0;
        while index < self.bars.len() {
            if self.bars[index].is_mmio() {
                return true;
            }
            index += 1;
        }
        false
    }

    pub const fn total_declared_resources(self) -> u64 {
        let mut total = 0_u64;
        let mut index = 0;
        while index < self.bars.len() {
            total = total.saturating_add(self.bars[index].length);
            index += 1;
        }
        total
    }

    pub const fn firmware_display_usable(self) -> bool {
        self.firmware_framebuffer.usable()
    }

    pub const fn iommu_isolated(self) -> bool {
        self.topology_flags & TOPOLOGY_IOMMU_ISOLATED != 0
    }

    pub const fn boot_display(self) -> bool {
        self.topology_flags & TOPOLOGY_BOOT_DISPLAY != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FingerprintSummary {
    pub length: usize,
    pub display_functions: usize,
    pub inventory_overflowed: bool,
    pub configuration_faults: u32,
    pub synthetic_firmware_entry: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FingerprintError {
    OutputCapacity,
    ConfigurationRead,
    UnalignedConfigurationOffset,
    UnsupportedConfigurationOffset,
    UnsupportedPciSegment,
    CapabilityCycle,
    CapabilityOutOfRange,
    InvalidHeader,
    MissingFirmwareEvidence,
}

pub fn fingerprint_inventory<const N: usize>(
    inventory: &DisplayFunctionInventory<N>,
    config: &dyn PciConfigReader,
    topology: &dyn TopologyEvidenceProvider,
    secret: u64,
    output: &mut [GpuFingerprint],
) -> Result<FingerprintSummary, FingerprintError> {
    if output.is_empty() {
        return Err(FingerprintError::OutputCapacity);
    }

    let firmware = topology.firmware_framebuffer();
    let owner_index = firmware.owner.and_then(|owner| {
        inventory
            .functions()
            .iter()
            .position(|function| function.address == owner)
    });
    let reserve_synthetic = firmware.usable() && owner_index.is_none();
    let function_capacity = output.len().saturating_sub(reserve_synthetic as usize);
    let display_functions = inventory.functions().len();
    let output_overflowed = display_functions > function_capacity;
    let discovery_incomplete = inventory.overflowed() || output_overflowed;
    let mut length = 0_usize;

    if let Some(index) = owner_index {
        let function = &inventory.functions()[index];
        let mut fingerprint = fingerprint_device(
            function,
            config,
            topology.evidence_for(function),
            discovery_incomplete,
            secret,
        )?;
        if firmware.usable() && !fingerprint.firmware_display_usable() {
            fingerprint.firmware_framebuffer = firmware;
            fingerprint.topology_flags |= TOPOLOGY_FIRMWARE_FRAMEBUFFER;
            fingerprint.evidence_root = fingerprint_root(secret, &fingerprint);
        }
        output[length] = fingerprint;
        length += 1;
    }

    for (index, function) in inventory.functions().iter().enumerate() {
        if Some(index) == owner_index || length >= function_capacity {
            continue;
        }

        output[length] = fingerprint_device(
            function,
            config,
            topology.evidence_for(function),
            discovery_incomplete,
            secret,
        )?;
        length += 1;
    }

    let mut synthetic_firmware_entry = false;
    if reserve_synthetic {
        let mut fingerprint = GpuFingerprint::EMPTY;
        fingerprint.firmware_framebuffer = firmware;
        fingerprint.topology_flags = TOPOLOGY_FIRMWARE_FRAMEBUFFER | TOPOLOGY_BOOT_DISPLAY;
        if discovery_incomplete {
            fingerprint.topology_flags |= TOPOLOGY_INVENTORY_OVERFLOW;
        }
        fingerprint.evidence_root = fingerprint_root(secret, &fingerprint);
        output[length] = fingerprint;
        length += 1;
        synthetic_firmware_entry = true;
    }

    Ok(FingerprintSummary {
        length,
        display_functions,
        inventory_overflowed: discovery_incomplete,
        configuration_faults: inventory.configuration_faults(),
        synthetic_firmware_entry,
    })
}

fn fingerprint_device(
    function: &PciFunctionRecord,
    config: &dyn PciConfigReader,
    topology: TopologyEvidence,
    inventory_overflowed: bool,
    secret: u64,
) -> Result<GpuFingerprint, FingerprintError> {
    let function_address = function.address;
    let effective_segment = if topology.segment == 0 {
        function.address.segment
    } else {
        topology.segment
    };
    if effective_segment != function.address.segment {
        return Err(FingerprintError::InvalidHeader);
    }

    let mut configuration_incomplete = false;
    let command_status = match config.read_u32(function_address, 0x04) {
        Ok(value) => value,
        Err(_) => {
            configuration_incomplete = true;
            0
        }
    };
    let subsystem = match config.read_u32(function_address, 0x2c) {
        Ok(value) => value,
        Err(_) => {
            configuration_incomplete = true;
            u32::MAX
        }
    };

    let mut bars = [BarEvidence::EMPTY; 6];
    if read_bar_evidence(
        function_address,
        config,
        topology.resource_lengths,
        &mut bars,
    )
    .is_err()
    {
        configuration_incomplete = true;
        bars = [BarEvidence::EMPTY; 6];
    }

    let capability_flags =
        match read_capability_flags(function_address, (command_status >> 16) as u16, config) {
            Ok(flags) => flags,
            Err(_) => {
                configuration_incomplete = true;
                0
            }
        };

    let mut topology_flags = topology.topology_flags;
    if topology.firmware_framebuffer.usable() {
        topology_flags |= TOPOLOGY_FIRMWARE_FRAMEBUFFER;
    }
    if inventory_overflowed {
        topology_flags |= TOPOLOGY_INVENTORY_OVERFLOW;
    }
    if configuration_incomplete {
        topology_flags |= TOPOLOGY_CONFIG_INCOMPLETE;
    }

    let mut fingerprint = GpuFingerprint {
        segment: effective_segment,
        bus: function.address.bus,
        slot: function.address.slot,
        function: function.address.function,
        vendor_id: function.vendor_id,
        device_id: function.device_id,
        subsystem_vendor_id: subsystem as u16,
        subsystem_device_id: (subsystem >> 16) as u16,
        revision: function.revision,
        class_code: function.class_code,
        subclass: function.subclass,
        programming_interface: function.programming_interface,
        header_type: function.header_type,
        interrupt_line: function.interrupt_line,
        interrupt_pin: function.interrupt_pin,
        command: command_status as u16,
        status: (command_status >> 16) as u16,
        capability_flags,
        topology_flags,
        acpi_path_hash: topology.acpi_path_hash,
        iommu_group: topology.iommu_group,
        upstream_vendor_id: topology.upstream_vendor_id,
        upstream_device_id: topology.upstream_device_id,
        root_port_depth: topology.root_port_depth,
        sibling_display_functions: topology.sibling_display_functions,
        bars,
        firmware_framebuffer: topology.firmware_framebuffer,
        evidence_root: 0,
    };
    fingerprint.evidence_root = fingerprint_root(secret, &fingerprint);
    Ok(fingerprint)
}

fn read_bar_evidence(
    address: PciFunctionAddress,
    config: &dyn PciConfigReader,
    lengths: [u64; 6],
    output: &mut [BarEvidence; 6],
) -> Result<(), FingerprintError> {
    let mut index = 0_usize;

    while index < output.len() {
        let offset = 0x10_u16.saturating_add((index as u16).saturating_mul(4));
        let low = config
            .read_u32(address, offset)
            .map_err(|_| FingerprintError::ConfigurationRead)?;

        if low == 0 || low == u32::MAX {
            index += 1;
            continue;
        }

        let mut flags = BAR_PRESENT;
        let mut high = 0_u32;

        if low & 1 != 0 {
            flags |= BAR_IO;
        } else {
            if low & (1 << 3) != 0 {
                flags |= BAR_PREFETCHABLE;
            }

            let memory_type = (low >> 1) & 0b11;
            if memory_type == 0b10 && index + 1 < output.len() {
                flags |= BAR_64BIT;
                high = config
                    .read_u32(address, offset.saturating_add(4))
                    .map_err(|_| FingerprintError::ConfigurationRead)?;
            }
        }

        output[index] = BarEvidence {
            raw_low: low,
            raw_high: high,
            length: lengths[index],
            flags,
        };

        if flags & BAR_64BIT != 0 {
            index += 2;
        } else {
            index += 1;
        }
    }

    Ok(())
}

fn read_capability_flags(
    address: PciFunctionAddress,
    status: u16,
    config: &dyn PciConfigReader,
) -> Result<u32, FingerprintError> {
    const STATUS_CAPABILITIES_LIST: u16 = 1 << 4;
    let mut flags = 0_u32;

    if status & STATUS_CAPABILITIES_LIST != 0 {
        let header = config
            .read_u32(address, 0x34)
            .map_err(|_| FingerprintError::ConfigurationRead)?;
        let mut pointer = u16::from((header as u8) & 0xfc);
        let mut visited = [0_u64; PCI_CONFIGURATION_DWORDS / 64];
        let mut hops = 0_usize;

        while pointer != 0 {
            if pointer < 0x40 || pointer & 3 != 0 {
                return Err(FingerprintError::CapabilityOutOfRange);
            }
            if hops >= MAXIMUM_CAPABILITY_HOPS {
                return Err(FingerprintError::CapabilityCycle);
            }

            let dword = usize::from(pointer / 4);
            let word = dword / 64;
            let bit = dword % 64;
            let mask = 1_u64 << bit;
            if visited[word] & mask != 0 {
                return Err(FingerprintError::CapabilityCycle);
            }
            visited[word] |= mask;

            let capability = config
                .read_u32(address, pointer)
                .map_err(|_| FingerprintError::ConfigurationRead)?;
            let capability_id = capability as u8;
            let next = u16::from(((capability >> 8) as u8) & 0xfc);

            flags |= match capability_id {
                0x01 => CAP_POWER_MANAGEMENT,
                0x02 => CAP_AGP,
                0x05 => CAP_MSI,
                0x09 => CAP_VENDOR_SPECIFIC,
                0x10 => CAP_PCIE,
                0x11 => CAP_MSIX,
                _ => 0,
            };

            pointer = next;
            hops += 1;
        }
    }

    if config.maximum_offset() >= 0x0100 {
        flags |= read_extended_capability_flags(address, config)?;
    }

    Ok(flags)
}

fn read_extended_capability_flags(
    address: PciFunctionAddress,
    config: &dyn PciConfigReader,
) -> Result<u32, FingerprintError> {
    const MAXIMUM_EXTENDED_HOPS: usize = 64;
    let mut pointer = 0x0100_u16;
    let mut visited = [0_u64; 16];
    let mut flags = 0_u32;
    let mut hops = 0_usize;

    while pointer != 0 {
        if pointer < 0x0100 || pointer > config.maximum_offset() || pointer & 3 != 0 {
            return Err(FingerprintError::CapabilityOutOfRange);
        }
        if hops >= MAXIMUM_EXTENDED_HOPS {
            return Err(FingerprintError::CapabilityCycle);
        }

        let dword = usize::from(pointer / 4);
        let word = dword / 64;
        let bit = dword % 64;
        let visited_word = visited
            .get_mut(word)
            .ok_or(FingerprintError::CapabilityOutOfRange)?;
        let mask = 1_u64 << bit;
        if *visited_word & mask != 0 {
            return Err(FingerprintError::CapabilityCycle);
        }
        *visited_word |= mask;

        let header = config
            .read_u32(address, pointer)
            .map_err(|_| FingerprintError::ConfigurationRead)?;
        if header == 0 || header == u32::MAX {
            break;
        }

        let capability_id = header as u16;
        flags |= match capability_id {
            0x0001 => CAP_ADVANCED_ERROR_REPORTING,
            0x000d => CAP_ACCESS_CONTROL_SERVICES,
            0x000f => CAP_ADDRESS_TRANSLATION,
            0x0010 => CAP_SRIOV,
            0x0015 => CAP_RESIZABLE_BAR,
            _ => 0,
        };

        pointer = ((header >> 20) as u16) & 0x0ffc;
        hops += 1;
    }

    Ok(flags)
}

pub fn fingerprint_root(secret: u64, fingerprint: &GpuFingerprint) -> u64 {
    let mut state = mix(secret, 0x4452_4956_4552_4e45);
    state = mix(state, u64::from(fingerprint.segment));
    state = mix(
        state,
        u64::from(fingerprint.bus)
            | (u64::from(fingerprint.slot) << 8)
            | (u64::from(fingerprint.function) << 16),
    );
    state = mix(
        state,
        u64::from(fingerprint.vendor_id)
            | (u64::from(fingerprint.device_id) << 16)
            | (u64::from(fingerprint.subsystem_vendor_id) << 32)
            | (u64::from(fingerprint.subsystem_device_id) << 48),
    );
    state = mix(
        state,
        u64::from(fingerprint.revision)
            | (u64::from(fingerprint.class_code) << 8)
            | (u64::from(fingerprint.subclass) << 16)
            | (u64::from(fingerprint.programming_interface) << 24)
            | (u64::from(fingerprint.header_type) << 32),
    );
    state = mix(
        state,
        u64::from(fingerprint.command)
            | (u64::from(fingerprint.status) << 16)
            | (u64::from(fingerprint.capability_flags) << 32),
    );
    state = mix(state, u64::from(fingerprint.topology_flags));
    state = mix(state, fingerprint.acpi_path_hash);
    state = mix(
        state,
        u64::from(fingerprint.iommu_group)
            | (u64::from(fingerprint.root_port_depth) << 32)
            | (u64::from(fingerprint.sibling_display_functions) << 40),
    );
    state = mix(
        state,
        u64::from(fingerprint.upstream_vendor_id)
            | (u64::from(fingerprint.upstream_device_id) << 16),
    );

    for bar in fingerprint.bars {
        state = mix(
            state,
            u64::from(bar.raw_low) | (u64::from(bar.raw_high) << 32),
        );
        state = mix(state, bar.length);
        state = mix(state, u64::from(bar.flags));
    }

    let firmware = fingerprint.firmware_framebuffer;
    state = mix(
        state,
        firmware.kind as u8 as u64
            | (u64::from(firmware.width) << 8)
            | (u64::from(firmware.height) << 32),
    );
    state = mix(
        state,
        u64::from(firmware.pitch) | (u64::from(firmware.format) << 32),
    );
    state = mix(state, firmware.byte_length);
    state = mix(state, firmware.retained as u64);
    if let Some(owner) = firmware.owner {
        state = mix(
            state,
            u64::from(owner.segment)
                | (u64::from(owner.bus) << 16)
                | (u64::from(owner.slot) << 24)
                | (u64::from(owner.function) << 29),
        );
    }

    avalanche(state)
}

fn mix(state: u64, word: u64) -> u64 {
    avalanche(state ^ word.wrapping_mul(0x9e37_79b9_7f4a_7c15))
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

    struct MockConfig {
        words: [u32; PCI_CONFIGURATION_DWORDS],
    }

    impl PciConfigReader for MockConfig {
        fn maximum_offset(&self) -> u16 {
            0x00ff
        }

        fn read_u32(
            &self,
            _address: PciFunctionAddress,
            offset: u16,
        ) -> Result<u32, FingerprintError> {
            Ok(self.words[usize::from(offset / 4)])
        }
    }

    #[test]
    fn capability_walk_is_bounded_and_detects_msi() {
        let mut words = [0_u32; PCI_CONFIGURATION_DWORDS];
        words[0x34 / 4] = 0x40;
        words[0x40 / 4] = 0x0000_0005;
        let config = MockConfig { words };
        let address = PciFunctionAddress {
            segment: 0,
            bus: 0,
            slot: 1,
            function: 0,
        };
        let flags = read_capability_flags(address, 1 << 4, &config).unwrap();
        assert_eq!(flags & CAP_MSI, CAP_MSI);
    }

    #[test]
    fn capability_cycle_is_rejected() {
        let mut words = [0_u32; PCI_CONFIGURATION_DWORDS];
        words[0x34 / 4] = 0x40;
        words[0x40 / 4] = 0x0000_4005;
        let config = MockConfig { words };
        let address = PciFunctionAddress {
            segment: 0,
            bus: 0,
            slot: 1,
            function: 0,
        };
        assert_eq!(
            read_capability_flags(address, 1 << 4, &config),
            Err(FingerprintError::CapabilityCycle)
        );
    }

    #[test]
    fn firmware_evidence_requires_retention() {
        let evidence = FirmwareFramebufferEvidence {
            kind: FirmwareFramebufferKind::UefiGop,
            width: 1920,
            height: 1080,
            pitch: 7680,
            format: 1,
            byte_length: 8_294_400,
            owner: None,
            retained: false,
        };
        assert!(!evidence.usable());
    }
}
