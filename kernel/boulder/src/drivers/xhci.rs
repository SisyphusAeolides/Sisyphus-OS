//! Read-only xHCI capability discovery.
//!
//! This module proves that a claimed PCI function exposes a structurally valid
//! xHCI capability header. It does not reset the controller, allocate rings, or
//! enumerate USB children, so a successful probe remains a deferred transport
//! prerequisite rather than operational keyboard, camera, or network support.

use sisyphus_driver_abi::STATUS_OK;

use crate::capability::{Capability, DeviceMemoryRight};
use crate::drivers::device_census::{BindingAuthorization, DeviceAddress, PciFunctionEvidence};
use crate::drivers::drivernet::fingerprint::{
    FingerprintError, PciConfigReader, PciFunctionAddress,
};
use crate::mmio::{MmioAccessError, MmioWindow};
use crate::sync::SpinLock;

pub const XHCI_PROBE_DRIVER_ID: u64 = 0x5848_4349_5052_4f42;
pub const MAXIMUM_XHCI_CONTROLLERS: usize = 16;

const CAPABILITY_BYTES: usize = 0x20;
const PCI_BAR0: u16 = 0x10;
const PCI_CLASS_SERIAL_BUS: u8 = 0x0c;
const PCI_SUBCLASS_USB: u8 = 0x03;
const PCI_INTERFACE_XHCI: u8 = 0x30;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XhciProbeError {
    InvalidSecret,
    InvalidAuthorizationRoot,
    WrongClass,
    EvidenceMismatch,
    Configuration(FingerprintError),
    ConfigurationChanged,
    IoBar,
    UnsupportedBarEncoding,
    MissingBar,
    AddressOverflow,
    Mmio(MmioAccessError),
    InvalidCapabilityLength(u8),
    UnsupportedInterfaceVersion(u16),
    InvalidGeometry,
    InvalidRegisterOffset,
    Unmap(i32),
    Capacity,
    DuplicateAddress,
    AlreadyPublished,
}

impl From<MmioAccessError> for XhciProbeError {
    fn from(error: MmioAccessError) -> Self {
        Self::Mmio(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XhciCapabilitySnapshot {
    pub address: DeviceAddress,
    pub evidence_root: u64,
    pub binding_root: u64,
    pub mmio_base: u64,
    pub capability_length: u8,
    pub interface_version: u16,
    pub maximum_device_slots: u8,
    pub maximum_interrupters: u16,
    pub maximum_ports: u8,
    pub maximum_scratchpad_buffers: u16,
    pub supports_64_bit_addresses: bool,
    pub uses_64_byte_contexts: bool,
    pub doorbell_offset: u32,
    pub runtime_offset: u32,
    pub extended_capabilities_offset: u32,
    pub snapshot_root: u64,
}

impl XhciCapabilitySnapshot {
    const EMPTY: Self = Self {
        address: DeviceAddress {
            segment: 0,
            bus: 0,
            slot: 0,
            function: 0,
        },
        evidence_root: 0,
        binding_root: 0,
        mmio_base: 0,
        capability_length: 0,
        interface_version: 0,
        maximum_device_slots: 0,
        maximum_interrupters: 0,
        maximum_ports: 0,
        maximum_scratchpad_buffers: 0,
        supports_64_bit_addresses: false,
        uses_64_byte_contexts: false,
        doorbell_offset: 0,
        runtime_offset: 0,
        extended_capabilities_offset: 0,
        snapshot_root: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XhciProbeSummary {
    pub controllers: usize,
    pub total_ports: usize,
    pub total_slots: usize,
    pub root: u64,
}

pub struct XhciProbeCensus {
    snapshots: [XhciCapabilitySnapshot; MAXIMUM_XHCI_CONTROLLERS],
    length: usize,
    secret: u64,
}

impl XhciProbeCensus {
    pub fn new(secret: u64) -> Result<Self, XhciProbeError> {
        if secret == 0 {
            return Err(XhciProbeError::InvalidSecret);
        }
        Ok(Self {
            snapshots: [XhciCapabilitySnapshot::EMPTY; MAXIMUM_XHCI_CONTROLLERS],
            length: 0,
            secret,
        })
    }

    pub fn insert(&mut self, snapshot: XhciCapabilitySnapshot) -> Result<(), XhciProbeError> {
        if self.snapshots[..self.length]
            .iter()
            .any(|known| known.address == snapshot.address)
        {
            return Err(XhciProbeError::DuplicateAddress);
        }
        let destination = self
            .snapshots
            .get_mut(self.length)
            .ok_or(XhciProbeError::Capacity)?;
        *destination = snapshot;
        self.length += 1;
        Ok(())
    }

    pub fn snapshots(&self) -> &[XhciCapabilitySnapshot] {
        &self.snapshots[..self.length]
    }

    pub fn summary(&self) -> XhciProbeSummary {
        let mut summary = XhciProbeSummary {
            controllers: self.length,
            total_ports: 0,
            total_slots: 0,
            root: mix(self.secret, self.length as u64),
        };
        for snapshot in self.snapshots() {
            summary.total_ports = summary
                .total_ports
                .saturating_add(usize::from(snapshot.maximum_ports));
            summary.total_slots = summary
                .total_slots
                .saturating_add(usize::from(snapshot.maximum_device_slots));
            summary.root = mix(summary.root, snapshot.snapshot_root);
        }
        summary
    }
}

static BOOT_XHCI_CENSUS: SpinLock<Option<XhciProbeCensus>> = SpinLock::new(None);

pub fn publish_boot_xhci(census: XhciProbeCensus) -> Result<XhciProbeSummary, XhciProbeError> {
    let summary = census.summary();
    let mut published = BOOT_XHCI_CENSUS.lock();
    if published.is_some() {
        return Err(XhciProbeError::AlreadyPublished);
    }
    *published = Some(census);
    Ok(summary)
}

pub fn boot_xhci_summary() -> Option<XhciProbeSummary> {
    BOOT_XHCI_CENSUS
        .lock()
        .as_ref()
        .map(XhciProbeCensus::summary)
}

pub fn boot_xhci_snapshot(address: DeviceAddress) -> Option<XhciCapabilitySnapshot> {
    BOOT_XHCI_CENSUS
        .lock()
        .as_ref()?
        .snapshots()
        .iter()
        .find(|snapshot| snapshot.address == address)
        .copied()
}

pub fn containment_root(
    secret: u64,
    evidence_root: u64,
    address: DeviceAddress,
    error: XhciProbeError,
) -> Option<u64> {
    if secret == 0 || evidence_root == 0 {
        return None;
    }
    let (code, detail) = match error {
        XhciProbeError::InvalidSecret => (1, 0),
        XhciProbeError::InvalidAuthorizationRoot => (2, 0),
        XhciProbeError::WrongClass => (3, 0),
        XhciProbeError::EvidenceMismatch => (4, 0),
        XhciProbeError::Configuration(_) => (5, 0),
        XhciProbeError::ConfigurationChanged => (6, 0),
        XhciProbeError::IoBar => (7, 0),
        XhciProbeError::UnsupportedBarEncoding => (8, 0),
        XhciProbeError::MissingBar => (9, 0),
        XhciProbeError::AddressOverflow => (10, 0),
        XhciProbeError::Mmio(_) => (11, 0),
        XhciProbeError::InvalidCapabilityLength(length) => (12, u64::from(length)),
        XhciProbeError::UnsupportedInterfaceVersion(version) => (13, u64::from(version)),
        XhciProbeError::InvalidGeometry => (14, 0),
        XhciProbeError::InvalidRegisterOffset => (15, 0),
        XhciProbeError::Unmap(status) => (16, status as u32 as u64),
        XhciProbeError::Capacity => (17, 0),
        XhciProbeError::DuplicateAddress => (18, 0),
        XhciProbeError::AlreadyPublished => (19, 0),
    };
    let mut state = mix(secret, evidence_root);
    state = mix(state, u64::from(address.segment));
    state = mix(state, u64::from(address.bus));
    state = mix(state, u64::from(address.slot));
    state = mix(state, u64::from(address.function));
    state = mix(state, code);
    Some(mix(state, detail))
}

pub fn probe_read_only(
    authorization: BindingAuthorization,
    evidence: PciFunctionEvidence,
    configuration: &dyn PciConfigReader,
    authority: &Capability<'_, DeviceMemoryRight>,
    secret: u64,
) -> Result<XhciCapabilitySnapshot, XhciProbeError> {
    if secret == 0 {
        return Err(XhciProbeError::InvalidSecret);
    }
    let address = authorization.address();
    let evidence_root = authorization.evidence_root();
    let binding_root = authorization.authorization_root();
    if evidence.address != address || evidence.evidence_root != evidence_root {
        return Err(XhciProbeError::EvidenceMismatch);
    }
    if evidence.class_code != PCI_CLASS_SERIAL_BUS
        || evidence.subclass != PCI_SUBCLASS_USB
        || evidence.programming_interface != PCI_INTERFACE_XHCI
    {
        return Err(XhciProbeError::WrongClass);
    }

    let function = PciFunctionAddress {
        segment: address.segment,
        bus: address.bus,
        slot: address.slot,
        function: address.function,
    };
    validate_configuration(configuration, function, evidence)?;
    let mmio_base = decode_bar0(evidence)?;
    mmio_base
        .checked_add(CAPABILITY_BYTES as u64)
        .ok_or(XhciProbeError::AddressOverflow)?;

    let window = MmioWindow::map(mmio_base, CAPABILITY_BYTES, authority)?;
    let result = read_snapshot(
        &window,
        address,
        evidence_root,
        binding_root,
        mmio_base,
        secret,
    );
    let close = window.close(authority);
    if close != STATUS_OK {
        return Err(XhciProbeError::Unmap(close));
    }
    result
}

fn decode_bar0(evidence: PciFunctionEvidence) -> Result<u64, XhciProbeError> {
    if evidence.bar_count == 0 {
        return Err(XhciProbeError::MissingBar);
    }
    let low = evidence.raw_bars[0];
    if low == 0 || low == u32::MAX {
        return Err(XhciProbeError::MissingBar);
    }
    if low & 1 != 0 {
        return Err(XhciProbeError::IoBar);
    }
    let base_low = u64::from(low & !0x0f);
    let memory_type = (low >> 1) & 0x03;
    let base = match memory_type {
        0 => base_low,
        2 => {
            if evidence.bar_count < 2 {
                return Err(XhciProbeError::UnsupportedBarEncoding);
            }
            let high = evidence.raw_bars[1];
            (u64::from(high) << 32) | base_low
        }
        _ => return Err(XhciProbeError::UnsupportedBarEncoding),
    };
    if base == 0 {
        Err(XhciProbeError::MissingBar)
    } else {
        Ok(base)
    }
}

fn validate_configuration(
    configuration: &dyn PciConfigReader,
    function: PciFunctionAddress,
    evidence: PciFunctionEvidence,
) -> Result<(), XhciProbeError> {
    let vendor_device = configuration
        .read_u32(function, 0x00)
        .map_err(XhciProbeError::Configuration)?;
    let command_status = configuration
        .read_u32(function, 0x04)
        .map_err(XhciProbeError::Configuration)?;
    let class_revision = configuration
        .read_u32(function, 0x08)
        .map_err(XhciProbeError::Configuration)?;
    let header = configuration
        .read_u32(function, 0x0c)
        .map_err(XhciProbeError::Configuration)?;
    if vendor_device as u16 != evidence.vendor_id
        || (vendor_device >> 16) as u16 != evidence.device_id
        || command_status as u16 != evidence.command
        || class_revision as u8 != evidence.revision
        || (class_revision >> 8) as u8 != evidence.programming_interface
        || (class_revision >> 16) as u8 != evidence.subclass
        || (class_revision >> 24) as u8 != evidence.class_code
        || (header >> 16) as u8 != evidence.header_type
    {
        return Err(XhciProbeError::ConfigurationChanged);
    }
    for index in 0..usize::from(evidence.bar_count) {
        let offset = PCI_BAR0 + (index as u16 * 4);
        let current = configuration
            .read_u32(function, offset)
            .map_err(XhciProbeError::Configuration)?;
        if current != evidence.raw_bars[index] {
            return Err(XhciProbeError::ConfigurationChanged);
        }
    }
    Ok(())
}

fn read_snapshot(
    window: &MmioWindow,
    address: DeviceAddress,
    evidence_root: u64,
    binding_root: u64,
    mmio_base: u64,
    secret: u64,
) -> Result<XhciCapabilitySnapshot, XhciProbeError> {
    let registers = CapabilityRegisters {
        cap_version: window.read_u32(0x00)?,
        hcsparams1: window.read_u32(0x04)?,
        hcsparams2: window.read_u32(0x08)?,
        hccparams1: window.read_u32(0x10)?,
        doorbell_offset: window.read_u32(0x14)? & !0x03,
        runtime_offset: window.read_u32(0x18)? & !0x1f,
    };

    decode_snapshot(
        address,
        evidence_root,
        binding_root,
        mmio_base,
        registers,
        secret,
    )
}

#[derive(Clone, Copy)]
struct CapabilityRegisters {
    cap_version: u32,
    hcsparams1: u32,
    hcsparams2: u32,
    hccparams1: u32,
    doorbell_offset: u32,
    runtime_offset: u32,
}

fn decode_snapshot(
    address: DeviceAddress,
    evidence_root: u64,
    binding_root: u64,
    mmio_base: u64,
    registers: CapabilityRegisters,
    secret: u64,
) -> Result<XhciCapabilitySnapshot, XhciProbeError> {
    if evidence_root == 0 || binding_root == 0 {
        return Err(XhciProbeError::InvalidAuthorizationRoot);
    }
    let CapabilityRegisters {
        cap_version,
        hcsparams1,
        hcsparams2,
        hccparams1,
        doorbell_offset,
        runtime_offset,
    } = registers;
    let capability_length = cap_version as u8;
    if usize::from(capability_length) < CAPABILITY_BYTES || capability_length & 0x03 != 0 {
        return Err(XhciProbeError::InvalidCapabilityLength(capability_length));
    }
    let interface_version = (cap_version >> 16) as u16;
    if interface_version < 0x0096 || interface_version > 0x0120 || !is_packed_bcd(interface_version)
    {
        return Err(XhciProbeError::UnsupportedInterfaceVersion(
            interface_version,
        ));
    }
    let maximum_device_slots = hcsparams1 as u8;
    let maximum_interrupters = ((hcsparams1 >> 8) & 0x7ff) as u16;
    let maximum_ports = (hcsparams1 >> 24) as u8;
    if maximum_device_slots == 0
        || maximum_interrupters == 0
        || maximum_interrupters > 1024
        || maximum_ports == 0
    {
        return Err(XhciProbeError::InvalidGeometry);
    }
    if doorbell_offset < u32::from(capability_length)
        || runtime_offset < u32::from(capability_length)
    {
        return Err(XhciProbeError::InvalidRegisterOffset);
    }
    let scratchpad_low = ((hcsparams2 >> 27) & 0x1f) as u16;
    let scratchpad_high = ((hcsparams2 >> 21) & 0x1f) as u16;
    let maximum_scratchpad_buffers = (scratchpad_high << 5) | scratchpad_low;
    let extended_capabilities_offset = ((hccparams1 >> 16) & 0xffff) * 4;

    let mut snapshot = XhciCapabilitySnapshot {
        address,
        evidence_root,
        binding_root,
        mmio_base,
        capability_length,
        interface_version,
        maximum_device_slots,
        maximum_interrupters,
        maximum_ports,
        maximum_scratchpad_buffers,
        supports_64_bit_addresses: hccparams1 & 1 != 0,
        uses_64_byte_contexts: hccparams1 & (1 << 2) != 0,
        doorbell_offset,
        runtime_offset,
        extended_capabilities_offset,
        snapshot_root: 0,
    };
    snapshot.snapshot_root = snapshot_root(secret, snapshot);
    Ok(snapshot)
}

const fn is_packed_bcd(value: u16) -> bool {
    value & 0x000f <= 9
        && (value >> 4) & 0x000f <= 9
        && (value >> 8) & 0x000f <= 9
        && (value >> 12) & 0x000f <= 9
}

fn snapshot_root(secret: u64, snapshot: XhciCapabilitySnapshot) -> u64 {
    let mut state = mix(secret, u64::from(snapshot.address.segment));
    state = mix(state, u64::from(snapshot.address.bus));
    state = mix(state, u64::from(snapshot.address.slot));
    state = mix(state, u64::from(snapshot.address.function));
    state = mix(state, snapshot.evidence_root);
    state = mix(state, snapshot.binding_root);
    state = mix(state, snapshot.mmio_base);
    state = mix(state, u64::from(snapshot.capability_length));
    state = mix(state, u64::from(snapshot.interface_version));
    state = mix(state, u64::from(snapshot.maximum_device_slots));
    state = mix(state, u64::from(snapshot.maximum_interrupters));
    state = mix(state, u64::from(snapshot.maximum_ports));
    state = mix(state, u64::from(snapshot.maximum_scratchpad_buffers));
    state = mix(state, snapshot.supports_64_bit_addresses as u64);
    state = mix(state, snapshot.uses_64_byte_contexts as u64);
    state = mix(state, u64::from(snapshot.doorbell_offset));
    state = mix(state, u64::from(snapshot.runtime_offset));
    mix(state, u64::from(snapshot.extended_capabilities_offset))
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
    use crate::drivers::device_census::{
        DeviceFamily, EVIDENCE_CLASS_TUPLE, EVIDENCE_IDENTITY, EVIDENCE_PCI_CONFIGURATION,
    };

    struct Configuration {
        values: [u32; 16],
    }

    impl PciConfigReader for Configuration {
        fn maximum_offset(&self) -> u16 {
            0x3c
        }

        fn read_u32(
            &self,
            _address: PciFunctionAddress,
            offset: u16,
        ) -> Result<u32, FingerprintError> {
            if offset & 3 != 0 || offset > self.maximum_offset() {
                return Err(FingerprintError::UnsupportedConfigurationOffset);
            }
            Ok(self.values[usize::from(offset / 4)])
        }
    }

    fn address() -> DeviceAddress {
        DeviceAddress {
            segment: 0,
            bus: 0,
            slot: 5,
            function: 0,
        }
    }

    fn registers(
        cap_version: u32,
        hcsparams1: u32,
        hcsparams2: u32,
        hccparams1: u32,
        doorbell_offset: u32,
        runtime_offset: u32,
    ) -> CapabilityRegisters {
        CapabilityRegisters {
            cap_version,
            hcsparams1,
            hcsparams2,
            hccparams1,
            doorbell_offset,
            runtime_offset,
        }
    }

    fn evidence() -> PciFunctionEvidence {
        PciFunctionEvidence {
            address: address(),
            vendor_id: 0x1234,
            device_id: 0x5678,
            class_code: 0x0c,
            subclass: 0x03,
            programming_interface: 0x30,
            revision: 1,
            header_type: 0,
            interrupt_line: 11,
            interrupt_pin: 1,
            command: 0x0007,
            bar_count: 6,
            raw_bars: [0xfebf_0004, 1, 0, 0, 0, 0],
            family: DeviceFamily::UsbHostController,
            evidence_mask: EVIDENCE_IDENTITY | EVIDENCE_CLASS_TUPLE | EVIDENCE_PCI_CONFIGURATION,
            dmar_covered: false,
            evidence_root: 0x1234,
        }
    }

    fn configuration() -> Configuration {
        let evidence = evidence();
        let mut values = [0_u32; 16];
        values[0] = u32::from(evidence.vendor_id) | (u32::from(evidence.device_id) << 16);
        values[1] = u32::from(evidence.command);
        values[2] = u32::from(evidence.revision)
            | (u32::from(evidence.programming_interface) << 8)
            | (u32::from(evidence.subclass) << 16)
            | (u32::from(evidence.class_code) << 24);
        values[3] = u32::from(evidence.header_type) << 16;
        values[4..10].copy_from_slice(&evidence.raw_bars);
        Configuration { values }
    }

    #[test]
    fn decodes_bounded_capability_geometry() {
        let snapshot = decode_snapshot(
            address(),
            0x1234,
            0xabcd,
            0xfebf_0000,
            registers(
                0x0100_0040,
                (8 << 24) | (4 << 8) | 32,
                (2 << 27) | (3 << 21),
                1 | (1 << 2) | (0x80 << 16),
                0x1000,
                0x2000,
            ),
            7,
        )
        .unwrap();

        assert_eq!(snapshot.capability_length, 0x40);
        assert_eq!(snapshot.interface_version, 0x0100);
        assert_eq!(snapshot.maximum_device_slots, 32);
        assert_eq!(snapshot.maximum_interrupters, 4);
        assert_eq!(snapshot.maximum_ports, 8);
        assert_eq!(snapshot.maximum_scratchpad_buffers, 98);
        assert_eq!(snapshot.extended_capabilities_offset, 0x200);
        assert_ne!(snapshot.snapshot_root, 0);
    }

    #[test]
    fn configuration_snapshot_rejects_identity_or_bar_drift() {
        let function = PciFunctionAddress {
            segment: 0,
            bus: 0,
            slot: 5,
            function: 0,
        };
        let measured = evidence();
        let current = configuration();
        assert_eq!(validate_configuration(&current, function, measured), Ok(()));
        assert_eq!(decode_bar0(measured), Ok(0x0000_0001_febf_0000));

        let mut changed = configuration();
        changed.values[4] = 0xdeaf_0004;
        assert_eq!(
            validate_configuration(&changed, function, measured),
            Err(XhciProbeError::ConfigurationChanged)
        );
    }

    #[test]
    fn rejects_empty_geometry_and_register_aliases() {
        assert_eq!(
            decode_snapshot(
                address(),
                0x1234,
                0xabcd,
                1,
                registers(0x0100_0020, 0, 0, 0, 0x100, 0x200),
                3,
            ),
            Err(XhciProbeError::InvalidGeometry)
        );
        assert_eq!(
            decode_snapshot(
                address(),
                0x1234,
                0xabcd,
                1,
                registers(0x0100_0040, (1 << 24) | (1 << 8) | 1, 0, 0, 0x20, 0x80,),
                3,
            ),
            Err(XhciProbeError::InvalidRegisterOffset)
        );
    }

    #[test]
    fn rejects_misaligned_lengths_non_bcd_versions_and_excess_interrupters() {
        let geometry = (1 << 24) | (1 << 8) | 1;
        assert_eq!(
            decode_snapshot(
                address(),
                0x1234,
                0xabcd,
                1,
                registers(0x0100_0021, geometry, 0, 0, 0x100, 0x200),
                3,
            ),
            Err(XhciProbeError::InvalidCapabilityLength(0x21))
        );
        assert_eq!(
            decode_snapshot(
                address(),
                0x1234,
                0xabcd,
                1,
                registers(0x01a0_0040, geometry, 0, 0, 0x100, 0x200),
                3,
            ),
            Err(XhciProbeError::UnsupportedInterfaceVersion(0x01a0))
        );
        assert_eq!(
            decode_snapshot(
                address(),
                0x1234,
                0xabcd,
                1,
                registers(0x0100_0040, (1 << 24) | (1025 << 8) | 1, 0, 0, 0x100, 0x200,),
                3,
            ),
            Err(XhciProbeError::InvalidGeometry)
        );
    }

    #[test]
    fn snapshot_root_binds_geometry() {
        let first = decode_snapshot(
            address(),
            0x1234,
            0xabcd,
            0x1000,
            registers(0x0100_0040, (4 << 24) | (1 << 8) | 8, 0, 1, 0x100, 0x200),
            9,
        )
        .unwrap();
        let second = decode_snapshot(
            address(),
            0x1234,
            0xabcd,
            0x1000,
            registers(0x0100_0040, (5 << 24) | (1 << 8) | 8, 0, 1, 0x100, 0x200),
            9,
        )
        .unwrap();
        assert_ne!(first.snapshot_root, second.snapshot_root);
    }

    #[test]
    fn snapshot_and_containment_roots_bind_authority_and_fault_class() {
        let first = decode_snapshot(
            address(),
            0x1234,
            0xabcd,
            0x1000,
            registers(0x0100_0040, (4 << 24) | (1 << 8) | 8, 0, 1, 0x100, 0x200),
            9,
        )
        .unwrap();
        let second = decode_snapshot(
            address(),
            0x1234,
            0xabce,
            0x1000,
            registers(0x0100_0040, (4 << 24) | (1 << 8) | 8, 0, 1, 0x100, 0x200),
            9,
        )
        .unwrap();
        assert_ne!(first.snapshot_root, second.snapshot_root);

        let malformed = containment_root(
            9,
            0x1234,
            address(),
            XhciProbeError::InvalidCapabilityLength(0x10),
        )
        .unwrap();
        let missing = containment_root(9, 0x1234, address(), XhciProbeError::MissingBar).unwrap();
        assert_ne!(malformed, missing);
    }
}
