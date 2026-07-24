//! Measured, class-neutral PCI discovery and binding authority.
//!
//! The census records what the bus proves, not what a device might be used for.
//! A multimedia-video function is not automatically a camera, an other-network
//! function is not automatically Wi-Fi, and an xHCI controller is not a HID
//! device. Drivers must add transport-specific evidence before publishing those
//! higher-level identities.

use crate::boot::acpi::{DmarEndpoint, DmarInfo};
use crate::hw::pci::{PciDevice, PciInventory};
use crate::sync::SpinLock;

pub const MAXIMUM_BOOT_DEVICES: usize = 256;
pub const MAXIMUM_DISPLAY_CLAIMS: usize = 64;

pub const EVIDENCE_IDENTITY: u32 = 1 << 0;
pub const EVIDENCE_CLASS_TUPLE: u32 = 1 << 1;
pub const EVIDENCE_LEGACY_IRQ: u32 = 1 << 2;
pub const EVIDENCE_DMAR_COVERAGE: u32 = 1 << 3;
pub const EVIDENCE_PCI_CONFIGURATION: u32 = 1 << 4;

pub const AUTHORITY_DELEGATE: u64 = 1 << 0;
pub const AUTHORITY_MMIO: u64 = 1 << 1;
pub const AUTHORITY_DMA: u64 = 1 << 2;
pub const AUTHORITY_IRQ: u64 = 1 << 3;
pub const AUTHORITY_CLOCK: u64 = 1 << 4;
pub const AUTHORITY_PUBLISH: u64 = 1 << 5;
pub const AUTHORITY_PCI_CONFIG: u64 = 1 << 6;

const PCI_CLASS_NETWORK: u8 = 0x02;
const PCI_CLASS_DISPLAY: u8 = 0x03;
const PCI_CLASS_MULTIMEDIA: u8 = 0x04;
const PCI_CLASS_INPUT: u8 = 0x09;
const PCI_CLASS_SERIAL_BUS: u8 = 0x0c;
const PCI_CLASS_WIRELESS: u8 = 0x0d;
const PCI_SUBCLASS_MULTIMEDIA_VIDEO: u8 = 0x00;
const PCI_SUBCLASS_MULTIMEDIA_AUDIO: u8 = 0x01;
const PCI_SUBCLASS_HD_AUDIO: u8 = 0x03;
const PCI_SUBCLASS_USB: u8 = 0x03;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum DeviceFamily {
    DisplayAdapter = 1,
    AudioController = 2,
    MultimediaVideoController = 3,
    NetworkController = 4,
    WirelessController = 5,
    UsbHostController = 6,
    InputController = 7,
    Other = 8,
}

impl DeviceFamily {
    const fn authority_ceiling(self) -> u64 {
        match self {
            Self::DisplayAdapter
            | Self::AudioController
            | Self::MultimediaVideoController
            | Self::NetworkController
            | Self::WirelessController => {
                AUTHORITY_DELEGATE
                    | AUTHORITY_MMIO
                    | AUTHORITY_DMA
                    | AUTHORITY_IRQ
                    | AUTHORITY_CLOCK
                    | AUTHORITY_PUBLISH
                    | AUTHORITY_PCI_CONFIG
            }
            Self::UsbHostController => {
                AUTHORITY_DELEGATE
                    | AUTHORITY_MMIO
                    | AUTHORITY_DMA
                    | AUTHORITY_IRQ
                    | AUTHORITY_CLOCK
                    | AUTHORITY_PUBLISH
                    | AUTHORITY_PCI_CONFIG
            }
            Self::InputController => {
                AUTHORITY_DELEGATE
                    | AUTHORITY_MMIO
                    | AUTHORITY_IRQ
                    | AUTHORITY_CLOCK
                    | AUTHORITY_PCI_CONFIG
            }
            Self::Other => AUTHORITY_DELEGATE,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceAddress {
    pub segment: u16,
    pub bus: u8,
    pub slot: u8,
    pub function: u8,
}

impl DeviceAddress {
    const EMPTY: Self = Self {
        segment: 0,
        bus: 0,
        slot: 0,
        function: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum DeviceState {
    Detected = 1,
    Claimed = 2,
    Operational = 3,
    Quarantined = 4,
    Deferred = 5,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciFunctionEvidence {
    pub address: DeviceAddress,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class_code: u8,
    pub subclass: u8,
    pub programming_interface: u8,
    pub revision: u8,
    pub header_type: u8,
    pub interrupt_line: u8,
    pub interrupt_pin: u8,
    pub command: u16,
    pub bar_count: u8,
    pub raw_bars: [u32; 6],
    pub family: DeviceFamily,
    pub evidence_mask: u32,
    pub dmar_covered: bool,
    pub evidence_root: u64,
}

impl PciFunctionEvidence {
    const EMPTY: Self = Self {
        address: DeviceAddress::EMPTY,
        vendor_id: 0xffff,
        device_id: 0xffff,
        class_code: 0,
        subclass: 0,
        programming_interface: 0,
        revision: 0,
        header_type: 0,
        interrupt_line: 0xff,
        interrupt_pin: 0,
        command: 0,
        bar_count: 0,
        raw_bars: [0; 6],
        family: DeviceFamily::Other,
        evidence_mask: 0,
        dmar_covered: false,
        evidence_root: 0,
    };

    pub const fn valid(self) -> bool {
        self.vendor_id != 0
            && self.vendor_id != 0xffff
            && self.address.slot < 32
            && self.address.function < 8
            && self.evidence_mask & (EVIDENCE_IDENTITY | EVIDENCE_CLASS_TUPLE)
                == EVIDENCE_IDENTITY | EVIDENCE_CLASS_TUPLE
            && self.evidence_mask & EVIDENCE_PCI_CONFIGURATION != 0
            && self.bar_count
                == match self.header_type & 0x7f {
                    0 => 6,
                    1 => 2,
                    _ => 0,
                }
            && self.dmar_covered == (self.evidence_mask & EVIDENCE_DMAR_COVERAGE != 0)
            && self.evidence_root != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DeviceSlot {
    evidence: PciFunctionEvidence,
    state: DeviceState,
    generation: u32,
    driver_id: u64,
    authority: u64,
    terminal_root: u64,
}

impl DeviceSlot {
    const EMPTY: Self = Self {
        evidence: PciFunctionEvidence::EMPTY,
        state: DeviceState::Detected,
        generation: 0,
        driver_id: 0,
        authority: 0,
        terminal_root: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceCensusSummary {
    pub total: usize,
    pub display: usize,
    pub audio: usize,
    pub multimedia_video: usize,
    pub network: usize,
    pub wireless: usize,
    pub usb_hosts: usize,
    pub input: usize,
    pub other: usize,
    pub detected: usize,
    pub claimed: usize,
    pub operational: usize,
    pub quarantined: usize,
    pub deferred: usize,
    pub root: u64,
}

impl DeviceCensusSummary {
    const EMPTY: Self = Self {
        total: 0,
        display: 0,
        audio: 0,
        multimedia_video: 0,
        network: 0,
        wireless: 0,
        usb_hosts: 0,
        input: 0,
        other: 0,
        detected: 0,
        claimed: 0,
        operational: 0,
        quarantined: 0,
        deferred: 0,
        root: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CensusError {
    InvalidSecret,
    InventoryOverflow,
    Capacity,
    DuplicateAddress,
    InvalidEvidence,
    InvalidManifest,
    NoMatch,
    EvidenceMissing(u32),
    AuthorityUnavailable(u64),
    AuthorityExceedsClass(u64),
    InvalidSlot,
    StaleClaim,
    AuthorizationMismatch,
    InvalidTerminalRoot,
    AlreadyPublished,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DriverBindingManifest {
    pub driver_id: u64,
    pub family: DeviceFamily,
    pub vendor_id: u16,
    pub device_id_mask: u16,
    pub device_id_value: u16,
    pub class_code_mask: u8,
    pub class_code_value: u8,
    pub subclass_mask: u8,
    pub subclass_value: u8,
    pub programming_interface_mask: u8,
    pub programming_interface_value: u8,
    pub revision_minimum: u8,
    pub revision_maximum: u8,
    pub required_evidence: u32,
    pub requested_authority: u64,
}

impl DriverBindingManifest {
    pub const fn valid(self) -> bool {
        self.driver_id != 0
            && self.revision_minimum <= self.revision_maximum
            && self.device_id_value & !self.device_id_mask == 0
            && self.class_code_value & !self.class_code_mask == 0
            && self.subclass_value & !self.subclass_mask == 0
            && self.programming_interface_value & !self.programming_interface_mask == 0
            && self.required_evidence
                & !(EVIDENCE_IDENTITY
                    | EVIDENCE_CLASS_TUPLE
                    | EVIDENCE_LEGACY_IRQ
                    | EVIDENCE_DMAR_COVERAGE
                    | EVIDENCE_PCI_CONFIGURATION)
                == 0
            && self.requested_authority != 0
            && self.requested_authority & !self.family.authority_ceiling() == 0
    }

    fn matches(self, evidence: PciFunctionEvidence) -> bool {
        self.valid()
            && self.family == evidence.family
            && (self.vendor_id == 0xffff || self.vendor_id == evidence.vendor_id)
            && evidence.device_id & self.device_id_mask
                == self.device_id_value & self.device_id_mask
            && evidence.class_code & self.class_code_mask == self.class_code_value
            && evidence.subclass & self.subclass_mask == self.subclass_value
            && evidence.programming_interface & self.programming_interface_mask
                == self.programming_interface_value
            && evidence.revision >= self.revision_minimum
            && evidence.revision <= self.revision_maximum
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BindingClaim {
    slot: u16,
    generation: u32,
    address: DeviceAddress,
    evidence_root: u64,
    driver_id: u64,
    authority: u64,
    claim_root: u64,
}

impl BindingClaim {
    const EMPTY: Self = Self {
        slot: u16::MAX,
        generation: 0,
        address: DeviceAddress::EMPTY,
        evidence_root: 0,
        driver_id: 0,
        authority: 0,
        claim_root: 0,
    };

    pub const fn address(self) -> DeviceAddress {
        self.address
    }

    pub const fn evidence_root(self) -> u64 {
        self.evidence_root
    }
}

/// Ephemeral proof that a claim was checked against its still-live census slot.
///
/// The fields are intentionally private and the value is not cloneable: driver
/// code can receive one authorization for one operation, but cannot construct
/// or duplicate authority from the detached fields in a `BindingClaim`.
#[derive(Debug, Eq, PartialEq)]
pub struct BindingAuthorization {
    address: DeviceAddress,
    evidence_root: u64,
    generation: u32,
    driver_id: u64,
    authority: u64,
    authorization_root: u64,
}

impl BindingAuthorization {
    pub const fn address(&self) -> DeviceAddress {
        self.address
    }

    pub const fn evidence_root(&self) -> u64 {
        self.evidence_root
    }

    pub const fn generation(&self) -> u32 {
        self.generation
    }

    pub const fn authorization_root(&self) -> u64 {
        self.authorization_root
    }
}

pub struct BindingClaimSet<const N: usize> {
    claims: [BindingClaim; N],
    length: usize,
}

impl<const N: usize> BindingClaimSet<N> {
    const fn new() -> Self {
        Self {
            claims: [BindingClaim::EMPTY; N],
            length: 0,
        }
    }

    pub fn claims(&self) -> &[BindingClaim] {
        &self.claims[..self.length]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BindingLease {
    pub address: DeviceAddress,
    pub generation: u32,
    pub driver_id: u64,
    pub authority: u64,
    pub evidence_root: u64,
    pub operational_root: u64,
    pub lease_root: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetainedDeviceRecord {
    pub evidence: PciFunctionEvidence,
    pub state: DeviceState,
    pub generation: u32,
    pub driver_id: u64,
    pub authority: u64,
    pub terminal_root: u64,
}

pub struct DeviceCensus<const N: usize> {
    slots: [DeviceSlot; N],
    length: usize,
    secret: u64,
}

impl<const N: usize> DeviceCensus<N> {
    pub fn measure_pci(
        inventory: &PciInventory,
        dmar: Option<&DmarInfo>,
        secret: u64,
    ) -> Result<Self, CensusError> {
        if inventory.overflowed() {
            return Err(CensusError::InventoryOverflow);
        }
        Self::measure_functions(inventory.devices(), secret, |device| {
            dmar.is_some_and(|table| {
                table.covers_endpoint(DmarEndpoint {
                    segment: 0,
                    bus: device.address.bus,
                    slot: device.address.slot,
                    function: device.address.function,
                })
            })
        })
    }

    fn measure_functions(
        devices: &[PciDevice],
        secret: u64,
        mut dmar_covered: impl FnMut(PciDevice) -> bool,
    ) -> Result<Self, CensusError> {
        if secret == 0 {
            return Err(CensusError::InvalidSecret);
        }
        if devices.len() > N {
            return Err(CensusError::Capacity);
        }

        let mut census = Self {
            slots: [DeviceSlot::EMPTY; N],
            length: 0,
            secret,
        };
        for device in devices.iter().copied() {
            census.insert(device, dmar_covered(device))?;
        }
        Ok(census)
    }

    fn insert(&mut self, device: PciDevice, dmar_covered: bool) -> Result<(), CensusError> {
        let address = DeviceAddress {
            segment: 0,
            bus: device.address.bus,
            slot: device.address.slot,
            function: device.address.function,
        };
        if self.slots[..self.length]
            .iter()
            .any(|slot| slot.evidence.address == address)
        {
            return Err(CensusError::DuplicateAddress);
        }

        let mut evidence_mask =
            EVIDENCE_IDENTITY | EVIDENCE_CLASS_TUPLE | EVIDENCE_PCI_CONFIGURATION;
        if device.interrupt_pin != 0 {
            evidence_mask |= EVIDENCE_LEGACY_IRQ;
        }
        if dmar_covered {
            evidence_mask |= EVIDENCE_DMAR_COVERAGE;
        }
        let mut evidence = PciFunctionEvidence {
            address,
            vendor_id: device.vendor_id,
            device_id: device.device_id,
            class_code: device.class_code,
            subclass: device.subclass,
            programming_interface: device.programming_interface,
            revision: device.revision,
            header_type: device.header_type,
            interrupt_line: device.interrupt_line,
            interrupt_pin: device.interrupt_pin,
            command: device.command,
            bar_count: device.bar_count,
            raw_bars: device.raw_bars,
            family: classify(device),
            evidence_mask,
            dmar_covered,
            evidence_root: 0,
        };
        evidence.evidence_root = evidence_root(self.secret, evidence);
        if !evidence.valid() {
            return Err(CensusError::InvalidEvidence);
        }
        let destination = self
            .slots
            .get_mut(self.length)
            .ok_or(CensusError::Capacity)?;
        *destination = DeviceSlot {
            evidence,
            state: DeviceState::Detected,
            generation: 1,
            driver_id: 0,
            authority: 0,
            terminal_root: 0,
        };
        self.length += 1;
        Ok(())
    }

    pub fn evidence(&self) -> impl Iterator<Item = &PciFunctionEvidence> {
        self.slots[..self.length].iter().map(|slot| &slot.evidence)
    }

    pub fn record(&self, address: DeviceAddress) -> Option<RetainedDeviceRecord> {
        self.slots[..self.length]
            .iter()
            .find(|slot| slot.evidence.address == address)
            .map(|slot| RetainedDeviceRecord {
                evidence: slot.evidence,
                state: slot.state,
                generation: slot.generation,
                driver_id: slot.driver_id,
                authority: slot.authority,
                terminal_root: slot.terminal_root,
            })
    }

    pub fn claim_family<const M: usize>(
        &mut self,
        manifest: DriverBindingManifest,
        available_authority: u64,
    ) -> Result<BindingClaimSet<M>, CensusError> {
        if !manifest.valid() {
            return Err(CensusError::InvalidManifest);
        }
        let matching = self.slots[..self.length]
            .iter()
            .filter(|slot| manifest.matches(slot.evidence))
            .count();
        if matching > M {
            return Err(CensusError::Capacity);
        }

        for slot in self.slots[..self.length]
            .iter()
            .filter(|slot| manifest.matches(slot.evidence))
        {
            validate_claim(slot, manifest, available_authority)?;
        }

        let mut claims = BindingClaimSet::new();
        for (index, slot) in self.slots[..self.length].iter_mut().enumerate() {
            if !manifest.matches(slot.evidence) {
                continue;
            }
            let mut claim = BindingClaim {
                slot: index as u16,
                generation: slot.generation,
                address: slot.evidence.address,
                evidence_root: slot.evidence.evidence_root,
                driver_id: manifest.driver_id,
                authority: manifest.requested_authority,
                claim_root: 0,
            };
            claim.claim_root = claim_root(self.secret, claim);
            slot.state = DeviceState::Claimed;
            slot.driver_id = manifest.driver_id;
            slot.authority = manifest.requested_authority;
            claims.claims[claims.length] = claim;
            claims.length += 1;
        }
        Ok(claims)
    }

    pub fn authorize(
        &self,
        claim: BindingClaim,
        driver_id: u64,
        required_authority: u64,
    ) -> Result<BindingAuthorization, CensusError> {
        let slot = self.validate_live_claim_ref(claim)?;
        if driver_id == 0
            || required_authority == 0
            || claim.driver_id != driver_id
            || claim.authority & required_authority != required_authority
        {
            return Err(CensusError::AuthorizationMismatch);
        }
        let mut authorization = BindingAuthorization {
            address: claim.address,
            evidence_root: claim.evidence_root,
            generation: claim.generation,
            driver_id,
            authority: required_authority,
            authorization_root: 0,
        };
        authorization.authorization_root = authorization_root(self.secret, &authorization);
        if slot.evidence.evidence_root != authorization.evidence_root {
            return Err(CensusError::StaleClaim);
        }
        Ok(authorization)
    }

    pub fn commit(
        &mut self,
        claim: BindingClaim,
        operational_root: u64,
    ) -> Result<BindingLease, CensusError> {
        if operational_root == 0 {
            return Err(CensusError::InvalidTerminalRoot);
        }
        let secret = self.secret;
        let slot = self.validate_live_claim(claim)?;
        slot.state = DeviceState::Operational;
        slot.terminal_root = operational_root;
        let mut lease = BindingLease {
            address: claim.address,
            generation: claim.generation,
            driver_id: claim.driver_id,
            authority: claim.authority,
            evidence_root: claim.evidence_root,
            operational_root,
            lease_root: 0,
        };
        lease.lease_root = lease_root(secret, lease);
        Ok(lease)
    }

    pub fn quarantine(
        &mut self,
        claim: BindingClaim,
        containment_root: u64,
    ) -> Result<(), CensusError> {
        if containment_root == 0 {
            return Err(CensusError::InvalidTerminalRoot);
        }
        let slot = self.validate_live_claim(claim)?;
        slot.state = DeviceState::Quarantined;
        slot.terminal_root = containment_root;
        Ok(())
    }

    pub fn defer(
        &mut self,
        claim: BindingClaim,
        prerequisite_root: u64,
    ) -> Result<(), CensusError> {
        if prerequisite_root == 0 {
            return Err(CensusError::InvalidTerminalRoot);
        }
        let slot = self.validate_live_claim(claim)?;
        slot.state = DeviceState::Deferred;
        slot.terminal_root = prerequisite_root;
        Ok(())
    }

    fn validate_live_claim(&mut self, claim: BindingClaim) -> Result<&mut DeviceSlot, CensusError> {
        if claim.claim_root == 0 || claim.claim_root != claim_root(self.secret, claim) {
            return Err(CensusError::StaleClaim);
        }
        let slot = self
            .slots
            .get_mut(usize::from(claim.slot))
            .filter(|_| usize::from(claim.slot) < self.length)
            .ok_or(CensusError::InvalidSlot)?;
        if slot.state != DeviceState::Claimed
            || slot.generation != claim.generation
            || slot.evidence.address != claim.address
            || slot.evidence.evidence_root != claim.evidence_root
            || slot.driver_id != claim.driver_id
            || slot.authority != claim.authority
        {
            return Err(CensusError::StaleClaim);
        }
        Ok(slot)
    }

    fn validate_live_claim_ref(&self, claim: BindingClaim) -> Result<&DeviceSlot, CensusError> {
        if claim.claim_root == 0 || claim.claim_root != claim_root(self.secret, claim) {
            return Err(CensusError::StaleClaim);
        }
        let slot = self
            .slots
            .get(usize::from(claim.slot))
            .filter(|_| usize::from(claim.slot) < self.length)
            .ok_or(CensusError::InvalidSlot)?;
        if slot.state != DeviceState::Claimed
            || slot.generation != claim.generation
            || slot.evidence.address != claim.address
            || slot.evidence.evidence_root != claim.evidence_root
            || slot.driver_id != claim.driver_id
            || slot.authority != claim.authority
        {
            return Err(CensusError::StaleClaim);
        }
        Ok(slot)
    }

    pub fn summary(&self) -> DeviceCensusSummary {
        let mut summary = DeviceCensusSummary::EMPTY;
        summary.total = self.length;
        for slot in &self.slots[..self.length] {
            match slot.evidence.family {
                DeviceFamily::DisplayAdapter => summary.display += 1,
                DeviceFamily::AudioController => summary.audio += 1,
                DeviceFamily::MultimediaVideoController => summary.multimedia_video += 1,
                DeviceFamily::NetworkController => summary.network += 1,
                DeviceFamily::WirelessController => summary.wireless += 1,
                DeviceFamily::UsbHostController => summary.usb_hosts += 1,
                DeviceFamily::InputController => summary.input += 1,
                DeviceFamily::Other => summary.other += 1,
            }
            match slot.state {
                DeviceState::Detected => summary.detected += 1,
                DeviceState::Claimed => summary.claimed += 1,
                DeviceState::Operational => summary.operational += 1,
                DeviceState::Quarantined => summary.quarantined += 1,
                DeviceState::Deferred => summary.deferred += 1,
            }
        }
        summary.root = summary_root(self.secret, &self.slots[..self.length], summary);
        summary
    }
}

fn validate_claim(
    slot: &DeviceSlot,
    manifest: DriverBindingManifest,
    available_authority: u64,
) -> Result<(), CensusError> {
    if slot.state != DeviceState::Detected || !manifest.matches(slot.evidence) {
        return Err(CensusError::NoMatch);
    }
    let missing = manifest.required_evidence & !slot.evidence.evidence_mask;
    if missing != 0 {
        return Err(CensusError::EvidenceMissing(missing));
    }
    let unavailable = manifest.requested_authority & !available_authority;
    if unavailable != 0 {
        return Err(CensusError::AuthorityUnavailable(unavailable));
    }
    let excessive = manifest.requested_authority & !slot.evidence.family.authority_ceiling();
    if excessive != 0 {
        return Err(CensusError::AuthorityExceedsClass(excessive));
    }
    Ok(())
}

const fn classify(device: PciDevice) -> DeviceFamily {
    match (device.class_code, device.subclass) {
        (PCI_CLASS_DISPLAY, _) => DeviceFamily::DisplayAdapter,
        (PCI_CLASS_MULTIMEDIA, PCI_SUBCLASS_MULTIMEDIA_AUDIO | PCI_SUBCLASS_HD_AUDIO) => {
            DeviceFamily::AudioController
        }
        (PCI_CLASS_MULTIMEDIA, PCI_SUBCLASS_MULTIMEDIA_VIDEO) => {
            DeviceFamily::MultimediaVideoController
        }
        (PCI_CLASS_NETWORK, _) => DeviceFamily::NetworkController,
        (PCI_CLASS_WIRELESS, _) => DeviceFamily::WirelessController,
        (PCI_CLASS_SERIAL_BUS, PCI_SUBCLASS_USB) => DeviceFamily::UsbHostController,
        (PCI_CLASS_INPUT, _) => DeviceFamily::InputController,
        _ => DeviceFamily::Other,
    }
}

fn evidence_root(secret: u64, evidence: PciFunctionEvidence) -> u64 {
    let mut state = mix(secret, u64::from(evidence.address.segment));
    state = mix(state, u64::from(evidence.address.bus));
    state = mix(state, u64::from(evidence.address.slot));
    state = mix(state, u64::from(evidence.address.function));
    state = mix(state, u64::from(evidence.vendor_id));
    state = mix(state, u64::from(evidence.device_id));
    state = mix(state, u64::from(evidence.class_code));
    state = mix(state, u64::from(evidence.subclass));
    state = mix(state, u64::from(evidence.programming_interface));
    state = mix(state, u64::from(evidence.revision));
    state = mix(state, u64::from(evidence.header_type));
    state = mix(state, u64::from(evidence.interrupt_line));
    state = mix(state, u64::from(evidence.interrupt_pin));
    state = mix(state, u64::from(evidence.command));
    state = mix(state, u64::from(evidence.bar_count));
    for bar in evidence.raw_bars {
        state = mix(state, u64::from(bar));
    }
    state = mix(state, evidence.family as u8 as u64);
    state = mix(state, u64::from(evidence.evidence_mask));
    mix(state, evidence.dmar_covered as u64)
}

fn claim_root(secret: u64, claim: BindingClaim) -> u64 {
    let mut state = mix(secret, u64::from(claim.slot));
    state = mix(state, u64::from(claim.generation));
    state = mix(state, u64::from(claim.address.segment));
    state = mix(state, u64::from(claim.address.bus));
    state = mix(state, u64::from(claim.address.slot));
    state = mix(state, u64::from(claim.address.function));
    state = mix(state, claim.evidence_root);
    state = mix(state, claim.driver_id);
    mix(state, claim.authority)
}

fn lease_root(secret: u64, lease: BindingLease) -> u64 {
    let mut state = mix(secret, lease.evidence_root);
    state = mix(state, lease.driver_id);
    state = mix(state, u64::from(lease.generation));
    state = mix(state, lease.authority);
    mix(state, lease.operational_root)
}

fn authorization_root(secret: u64, authorization: &BindingAuthorization) -> u64 {
    let mut state = mix(secret, u64::from(authorization.address.segment));
    state = mix(state, u64::from(authorization.address.bus));
    state = mix(state, u64::from(authorization.address.slot));
    state = mix(state, u64::from(authorization.address.function));
    state = mix(state, authorization.evidence_root);
    state = mix(state, u64::from(authorization.generation));
    state = mix(state, authorization.driver_id);
    mix(state, authorization.authority)
}

fn summary_root(secret: u64, slots: &[DeviceSlot], summary: DeviceCensusSummary) -> u64 {
    let mut state = mix(secret, summary.total as u64);
    state = mix(state, summary.display as u64);
    state = mix(state, summary.audio as u64);
    state = mix(state, summary.multimedia_video as u64);
    state = mix(state, summary.network as u64);
    state = mix(state, summary.wireless as u64);
    state = mix(state, summary.usb_hosts as u64);
    state = mix(state, summary.input as u64);
    state = mix(state, summary.other as u64);
    state = mix(state, summary.detected as u64);
    state = mix(state, summary.claimed as u64);
    state = mix(state, summary.operational as u64);
    state = mix(state, summary.quarantined as u64);
    state = mix(state, summary.deferred as u64);
    for slot in slots {
        state = mix(state, slot.evidence.evidence_root);
        state = mix(state, slot.state as u8 as u64);
        state = mix(state, u64::from(slot.generation));
        state = mix(state, slot.driver_id);
        state = mix(state, slot.authority);
        state = mix(state, slot.terminal_root);
    }
    state
}

fn mix(mut state: u64, word: u64) -> u64 {
    state ^= word.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    state ^= state >> 30;
    state = state.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    state ^= state >> 27;
    state = state.wrapping_mul(0x94d0_49bb_1331_11eb);
    state ^ (state >> 31)
}

pub type BootDeviceCensus = DeviceCensus<MAXIMUM_BOOT_DEVICES>;

static BOOT_DEVICE_CENSUS: SpinLock<Option<BootDeviceCensus>> = SpinLock::new(None);

pub fn publish_boot_census(census: BootDeviceCensus) -> Result<DeviceCensusSummary, CensusError> {
    let summary = census.summary();
    let mut published = BOOT_DEVICE_CENSUS.lock();
    if published.is_some() {
        return Err(CensusError::AlreadyPublished);
    }
    *published = Some(census);
    Ok(summary)
}

pub fn boot_census_summary() -> Option<DeviceCensusSummary> {
    BOOT_DEVICE_CENSUS
        .lock()
        .as_ref()
        .map(DeviceCensus::summary)
}

pub fn boot_device_record(address: DeviceAddress) -> Option<RetainedDeviceRecord> {
    BOOT_DEVICE_CENSUS.lock().as_ref()?.record(address)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hw::pci::PciAddress;

    fn device(address: PciAddress, class_code: u8, subclass: u8, interface: u8) -> PciDevice {
        PciDevice {
            address,
            vendor_id: 0x1234,
            device_id: 0x5678,
            class_code,
            subclass,
            programming_interface: interface,
            revision: 1,
            header_type: 0,
            interrupt_line: 11,
            interrupt_pin: 1,
            command: 0x0007,
            bar_count: 6,
            raw_bars: [0x8000_0000, 0, 0, 0, 0, 0],
        }
    }

    fn display_manifest() -> DriverBindingManifest {
        DriverBindingManifest {
            driver_id: 0x4452_4956_4552_4e45,
            family: DeviceFamily::DisplayAdapter,
            vendor_id: 0xffff,
            device_id_mask: 0,
            device_id_value: 0,
            class_code_mask: u8::MAX,
            class_code_value: PCI_CLASS_DISPLAY,
            subclass_mask: 0,
            subclass_value: 0,
            programming_interface_mask: 0,
            programming_interface_value: 0,
            revision_minimum: 0,
            revision_maximum: u8::MAX,
            required_evidence: EVIDENCE_IDENTITY
                | EVIDENCE_CLASS_TUPLE
                | EVIDENCE_PCI_CONFIGURATION,
            requested_authority: AUTHORITY_DELEGATE,
        }
    }

    #[test]
    fn classifies_bus_evidence_without_inventing_child_devices() {
        let devices = [
            device(PciAddress::new(0, 1, 0).unwrap(), 0x03, 0x00, 0),
            device(PciAddress::new(0, 1, 1).unwrap(), 0x04, 0x03, 0),
            device(PciAddress::new(0, 2, 0).unwrap(), 0x04, 0x00, 0),
            device(PciAddress::new(0, 3, 0).unwrap(), 0x02, 0x80, 0),
            device(PciAddress::new(0, 4, 0).unwrap(), 0x0c, 0x03, 0x30),
            device(PciAddress::new(0, 5, 0).unwrap(), 0x09, 0x02, 0),
            device(PciAddress::new(0, 6, 0).unwrap(), 0x0d, 0x20, 0),
        ];
        let census = DeviceCensus::<8>::measure_functions(&devices, 7, |_| false).unwrap();
        let families: [DeviceFamily; 7] =
            core::array::from_fn(|index| census.evidence().nth(index).unwrap().family);

        assert_eq!(families[0], DeviceFamily::DisplayAdapter);
        assert_eq!(families[1], DeviceFamily::AudioController);
        assert_eq!(families[2], DeviceFamily::MultimediaVideoController);
        assert_eq!(families[3], DeviceFamily::NetworkController);
        assert_eq!(families[4], DeviceFamily::UsbHostController);
        assert_eq!(families[5], DeviceFamily::InputController);
        assert_eq!(families[6], DeviceFamily::WirelessController);
    }

    #[test]
    fn multifunction_display_and_audio_are_both_retained() {
        let devices = [
            device(PciAddress::new(1, 2, 0).unwrap(), 0x03, 0x00, 0),
            device(PciAddress::new(1, 2, 1).unwrap(), 0x04, 0x03, 0),
        ];
        let census = DeviceCensus::<4>::measure_functions(&devices, 11, |_| true).unwrap();
        let summary = census.summary();
        assert_eq!(summary.total, 2);
        assert_eq!(summary.display, 1);
        assert_eq!(summary.audio, 1);
        assert!(census.evidence().all(|evidence| evidence.dmar_covered));
    }

    #[test]
    fn duplicate_addresses_and_capacity_fail_closed() {
        let repeated = device(PciAddress::new(0, 1, 0).unwrap(), 0x03, 0, 0);
        assert_eq!(
            DeviceCensus::<2>::measure_functions(&[repeated, repeated], 5, |_| false)
                .err()
                .unwrap(),
            CensusError::DuplicateAddress
        );
        assert_eq!(
            DeviceCensus::<1>::measure_functions(
                &[
                    repeated,
                    device(PciAddress::new(0, 2, 0).unwrap(), 0x04, 1, 0),
                ],
                5,
                |_| false,
            )
            .err()
            .unwrap(),
            CensusError::Capacity
        );
    }

    #[test]
    fn dmar_coverage_is_evidence_not_dma_authority() {
        let display = device(PciAddress::new(0, 1, 0).unwrap(), 0x03, 0, 0);
        let mut census = DeviceCensus::<1>::measure_functions(&[display], 13, |_| true).unwrap();
        let mut manifest = display_manifest();
        manifest.requested_authority = AUTHORITY_DMA;

        assert_eq!(
            census
                .claim_family::<1>(manifest, AUTHORITY_DELEGATE)
                .err()
                .unwrap(),
            CensusError::AuthorityUnavailable(AUTHORITY_DMA)
        );
        assert_eq!(census.summary().detected, 1);
    }

    #[test]
    fn claims_commit_once_and_stale_replay_is_rejected() {
        let display = device(PciAddress::new(0, 1, 0).unwrap(), 0x03, 0, 0);
        let mut census = DeviceCensus::<1>::measure_functions(&[display], 17, |_| false).unwrap();
        let claims = census
            .claim_family::<1>(display_manifest(), AUTHORITY_DELEGATE)
            .unwrap();
        let claim = claims.claims()[0];
        let lease = census.commit(claim, 0xcafe).unwrap();

        assert_ne!(lease.lease_root, 0);
        assert_eq!(census.summary().operational, 1);
        assert_eq!(census.commit(claim, 0xbeef), Err(CensusError::StaleClaim));
    }

    #[test]
    fn exact_class_tuple_leaves_other_usb_generations_unclaimed() {
        let devices = [
            device(PciAddress::new(0, 4, 0).unwrap(), 0x0c, 0x03, 0x30),
            device(PciAddress::new(0, 5, 0).unwrap(), 0x0c, 0x03, 0x20),
        ];
        let mut census = DeviceCensus::<2>::measure_functions(&devices, 29, |_| false).unwrap();
        let manifest = DriverBindingManifest {
            driver_id: 0x5848_4349,
            family: DeviceFamily::UsbHostController,
            vendor_id: 0xffff,
            device_id_mask: 0,
            device_id_value: 0,
            class_code_mask: u8::MAX,
            class_code_value: 0x0c,
            subclass_mask: u8::MAX,
            subclass_value: 0x03,
            programming_interface_mask: u8::MAX,
            programming_interface_value: 0x30,
            revision_minimum: 0,
            revision_maximum: u8::MAX,
            required_evidence: EVIDENCE_IDENTITY
                | EVIDENCE_CLASS_TUPLE
                | EVIDENCE_PCI_CONFIGURATION,
            requested_authority: AUTHORITY_MMIO,
        };

        let claims = census.claim_family::<1>(manifest, AUTHORITY_MMIO).unwrap();
        assert_eq!(claims.claims().len(), 1);
        assert_eq!(claims.claims()[0].address().slot, 4);
        assert_eq!(census.summary().claimed, 1);
        assert_eq!(census.summary().detected, 1);
    }

    #[test]
    fn authorization_is_minted_only_for_a_live_exact_claim() {
        let display = device(PciAddress::new(0, 1, 0).unwrap(), 0x03, 0, 0);
        let mut census = DeviceCensus::<1>::measure_functions(&[display], 31, |_| false).unwrap();
        let manifest = display_manifest();
        let claim = census
            .claim_family::<1>(manifest, AUTHORITY_DELEGATE)
            .unwrap()
            .claims()[0];

        assert_eq!(
            census.authorize(claim, manifest.driver_id + 1, AUTHORITY_DELEGATE),
            Err(CensusError::AuthorizationMismatch)
        );
        let authorization = census
            .authorize(claim, manifest.driver_id, AUTHORITY_DELEGATE)
            .unwrap();
        assert_eq!(authorization.address(), claim.address());
        assert_ne!(authorization.authorization_root(), 0);

        census.defer(claim, 0x1234).unwrap();
        assert_eq!(
            census.authorize(claim, manifest.driver_id, AUTHORITY_DELEGATE),
            Err(CensusError::StaleClaim)
        );
    }

    #[test]
    fn every_observed_tuple_change_reseals_the_evidence() {
        let original = device(PciAddress::new(0, 1, 0).unwrap(), 0x03, 0, 0);
        let first = DeviceCensus::<1>::measure_functions(&[original], 19, |_| false)
            .unwrap()
            .evidence()
            .next()
            .unwrap()
            .evidence_root;
        let mut changed = original;
        changed.interrupt_line = 12;
        let second = DeviceCensus::<1>::measure_functions(&[changed], 19, |_| false)
            .unwrap()
            .evidence()
            .next()
            .unwrap()
            .evidence_root;
        let third = DeviceCensus::<1>::measure_functions(&[original], 19, |_| true)
            .unwrap()
            .evidence()
            .next()
            .unwrap()
            .evidence_root;
        let mut remapped = original;
        remapped.raw_bars[0] = 0x9000_0000;
        let fourth = DeviceCensus::<1>::measure_functions(&[remapped], 19, |_| false)
            .unwrap()
            .evidence()
            .next()
            .unwrap()
            .evidence_root;

        assert_ne!(first, second);
        assert_ne!(first, third);
        assert_ne!(first, fourth);
    }
}
