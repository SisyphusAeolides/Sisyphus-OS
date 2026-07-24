//! Retained xHCI discovery and bounded reset-ready activation.
//!
//! This module proves that a claimed PCI function exposes a structurally valid
//! xHCI register set, transfers firmware ownership, measures the exact BAR0
//! aperture, resets the controller, and decodes its protocol-to-port map. It
//! does not yet allocate DMA rings or enumerate USB children, so reset-ready
//! remains a deferred transport prerequisite rather than input-device support.

use sisyphus_driver_abi::STATUS_OK;

use crate::arch::Active;
use crate::capability::{Capability, DeviceMemoryRight, InterruptGuard, PciConfigurationControl};
use crate::drivers::device_census::{BindingAuthorization, DeviceAddress, PciFunctionEvidence};
use crate::drivers::drivernet::fingerprint::{
    FingerprintError, PciConfigReader, PciFunctionAddress,
};
use crate::drivers::xhci_protocol::{
    CheckedSupportedProtocolRead, SupportedProtocolError, SupportedProtocolEvidence,
    decode_supported_protocols,
};
use crate::drivers::xhci_takeover::{
    ApertureBindingError, ApicRelativeDeadline, ReadyHalted, RegisterIo, TakeoverConfig,
    TakeoverConfigError, TakeoverFaultClass, TakeoverMachine, TakeoverObservation, TakeoverPhase,
};
use crate::hw::pci::{Bar0ApertureLease, Bar0ApertureRange, BarApertureBoundsError, PciDevice};
use crate::hw::pci::{
    BarProbeError, BarProbeQuiescence, PciAddress, PciExpectedConfiguration, measure_bar0_aperture,
};
use crate::interrupts::LocalApicDeadlineClock;
use crate::mmio::{MmioAccessError, MmioWindow};
use crate::sync::SpinLock;

pub const XHCI_PROBE_DRIVER_ID: u64 = 0x5848_4349_5052_4f42;
pub const MAXIMUM_XHCI_CONTROLLERS: usize = 16;

const CAPABILITY_BYTES: usize = 0x20;
const MAXIMUM_BOOTSTRAP_EXTENDED_HEADERS: usize = 64;
const MAXIMUM_BOOTSTRAP_OFFSET: u32 = 0x000f_fffc;
pub const XHCI_BOOTSTRAP_CONTAINMENT_BYTES: u64 = 0x0010_0000;
const EXTENDED_CAPABILITY_LEGACY_SUPPORT: u8 = 1;
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
    InvalidBootstrapOffset(u32),
    BootstrapCapabilityCapacity,
    BootstrapCapabilityOverlap(u32),
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
    pub legacy_support_offset: u32,
    pub bootstrap_header_count: u8,
    pub bootstrap_range_root: u64,
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
        legacy_support_offset: 0,
        bootstrap_header_count: 0,
        bootstrap_range_root: 0,
        snapshot_root: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BootstrapRange {
    pub offset: u32,
    pub length: u8,
}

impl BootstrapRange {
    const EMPTY: Self = Self {
        offset: 0,
        length: 0,
    };
}

#[derive(Debug, Eq, PartialEq)]
pub struct BootstrapRangeJournal {
    ranges: [BootstrapRange; MAXIMUM_BOOTSTRAP_EXTENDED_HEADERS],
    length: usize,
    root: u64,
}

impl BootstrapRangeJournal {
    fn new(secret: u64, initial_offset: u32) -> Self {
        Self {
            ranges: [BootstrapRange::EMPTY; MAXIMUM_BOOTSTRAP_EXTENDED_HEADERS],
            length: 0,
            root: mix(secret, u64::from(initial_offset)),
        }
    }

    fn push(&mut self, range: BootstrapRange, header: u32) -> Result<(), XhciProbeError> {
        let destination = self
            .ranges
            .get_mut(self.length)
            .ok_or(XhciProbeError::BootstrapCapabilityCapacity)?;
        *destination = range;
        self.length += 1;
        self.root = mix(self.root, u64::from(range.offset));
        self.root = mix(self.root, u64::from(range.length));
        self.root = mix(self.root, u64::from(header));
        Ok(())
    }

    pub fn ranges(&self) -> &[BootstrapRange] {
        &self.ranges[..self.length]
    }

    pub const fn root(&self) -> u64 {
        self.root
    }
}

/// Non-cloneable bridge between a live census claim and mutable xHCI takeover.
/// The contained authorization is retained until the controller reaches a
/// terminal reset-ready or containment state.
pub struct XhciBootstrap {
    authorization: BindingAuthorization,
    evidence: PciFunctionEvidence,
    snapshot: XhciCapabilitySnapshot,
    journal: BootstrapRangeJournal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XhciRetentionError {
    AddressMismatch,
    AuthorizationMismatch,
    ApertureEvidenceMismatch,
    ProtocolEvidenceMismatch,
    ApertureBounds(BarApertureBoundsError),
    InvalidRoot,
}

pub struct XhciResetReadyController {
    bootstrap: XhciBootstrap,
    aperture: Bar0ApertureLease,
    ready: ReadyHalted,
    protocols: SupportedProtocolEvidence,
    reset_ready_root: u64,
}

impl XhciResetReadyController {
    pub const fn snapshot(&self) -> XhciCapabilitySnapshot {
        self.bootstrap.snapshot
    }

    pub const fn aperture(&self) -> &Bar0ApertureLease {
        &self.aperture
    }

    pub const fn ready(&self) -> &ReadyHalted {
        &self.ready
    }

    pub const fn protocols(&self) -> &SupportedProtocolEvidence {
        &self.protocols
    }

    pub const fn reset_ready_root(&self) -> u64 {
        self.reset_ready_root
    }
}

fn validate_retained_controller(
    bootstrap: &XhciBootstrap,
    aperture: &Bar0ApertureLease,
    ready: &ReadyHalted,
    protocols: &SupportedProtocolEvidence,
    secret: u64,
) -> Result<u64, XhciRetentionError> {
    if secret == 0 {
        return Err(XhciRetentionError::InvalidRoot);
    }
    let snapshot = bootstrap.snapshot;
    let authorization_root = retained_authorization_root(bootstrap)?;
    if snapshot.address.segment != 0
        || aperture.address().bus != snapshot.address.bus
        || aperture.address().slot != snapshot.address.slot
        || aperture.address().function != snapshot.address.function
    {
        return Err(XhciRetentionError::AddressMismatch);
    }
    if aperture.physical_base() != snapshot.mmio_base
        || aperture.length() != ready.measured_aperture_bytes()
    {
        return Err(XhciRetentionError::ApertureEvidenceMismatch);
    }
    if protocols.aperture_base != aperture.physical_base()
        || protocols.aperture_bytes != aperture.length()
        || protocols.initial_offset != snapshot.extended_capabilities_offset
        || protocols.maximum_ports != snapshot.maximum_ports
        || protocols.root == 0
    {
        return Err(XhciRetentionError::ProtocolEvidenceMismatch);
    }
    aperture
        .checked_range(0, CAPABILITY_BYTES)
        .map_err(XhciRetentionError::ApertureBounds)?;
    aperture
        .checked_range(u64::from(snapshot.doorbell_offset), 4)
        .map_err(XhciRetentionError::ApertureBounds)?;
    aperture
        .checked_range(u64::from(snapshot.runtime_offset), 0x20)
        .map_err(XhciRetentionError::ApertureBounds)?;
    for range in bootstrap.journal.ranges() {
        aperture
            .checked_range(u64::from(range.offset), usize::from(range.length))
            .map_err(XhciRetentionError::ApertureBounds)?;
    }
    let mut root = mix(secret, snapshot.snapshot_root);
    root = mix(root, authorization_root);
    root = mix(root, aperture.physical_base());
    root = mix(root, aperture.length());
    root = mix(root, u64::from(ready.command()));
    root = mix(root, u64::from(ready.status()));
    root = mix(root, ready.legacy_handoff_performed() as u64);
    root = mix(root, u64::from(ready.ports_observed()));
    root = mix(root, protocols.root);
    if root == 0 {
        return Err(XhciRetentionError::InvalidRoot);
    }
    Ok(root)
}

fn retained_authorization_root(bootstrap: &XhciBootstrap) -> Result<u64, XhciRetentionError> {
    let authorization = &bootstrap.authorization;
    if authorization.address() != bootstrap.snapshot.address {
        return Err(XhciRetentionError::AddressMismatch);
    }
    if authorization.evidence_root() != bootstrap.snapshot.evidence_root
        || authorization.authorization_root() != bootstrap.snapshot.binding_root
    {
        return Err(XhciRetentionError::AuthorizationMismatch);
    }
    Ok(authorization.authorization_root())
}

fn mutation_debt_root(
    bootstrap: &XhciBootstrap,
    aperture: Option<&Bar0ApertureLease>,
    phase: TakeoverPhase,
    fault_class: XhciDebtClass,
    secret: u64,
) -> Result<u64, XhciRetentionError> {
    if secret == 0 {
        return Err(XhciRetentionError::InvalidRoot);
    }
    let mut root = mix(secret, bootstrap.snapshot.snapshot_root);
    root = mix(root, retained_authorization_root(bootstrap)?);
    root = mix(root, phase as u8 as u64);
    root = mix(root, fault_class.code());
    if let Some(aperture) = aperture {
        root = mix(root, aperture.physical_base());
        root = mix(root, aperture.length());
    }
    if root == 0 {
        return Err(XhciRetentionError::InvalidRoot);
    }
    Ok(root)
}

pub struct XhciMutationDebt {
    bootstrap: XhciBootstrap,
    aperture: Option<Bar0ApertureLease>,
    phase: TakeoverPhase,
    fault_class: XhciDebtClass,
    debt_root: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XhciDebtClass {
    RegisterAccess,
    Deadline,
    Timeout,
    HostControllerError,
    IllegalControllerState,
    ReadyHaltedInvariant,
    ApertureRequired,
    TerminalState,
    PciTransaction,
    ApertureBinding,
    Retention,
    StepCapacity,
    ProtocolEvidence,
}

impl XhciDebtClass {
    const fn from_machine(class: TakeoverFaultClass) -> Self {
        match class {
            TakeoverFaultClass::RegisterAccess => Self::RegisterAccess,
            TakeoverFaultClass::Deadline => Self::Deadline,
            TakeoverFaultClass::Timeout => Self::Timeout,
            TakeoverFaultClass::HostControllerError => Self::HostControllerError,
            TakeoverFaultClass::IllegalControllerState => Self::IllegalControllerState,
            TakeoverFaultClass::ReadyHaltedInvariant => Self::ReadyHaltedInvariant,
            TakeoverFaultClass::MeasuredApertureRequired => Self::ApertureRequired,
            TakeoverFaultClass::TerminalState => Self::TerminalState,
        }
    }

    const fn code(self) -> u64 {
        match self {
            Self::RegisterAccess => 1,
            Self::Deadline => 2,
            Self::Timeout => 3,
            Self::HostControllerError => 4,
            Self::IllegalControllerState => 5,
            Self::ReadyHaltedInvariant => 6,
            Self::ApertureRequired => 7,
            Self::TerminalState => 8,
            Self::PciTransaction => 9,
            Self::ApertureBinding => 10,
            Self::Retention => 11,
            Self::StepCapacity => 12,
            Self::ProtocolEvidence => 13,
        }
    }
}

impl XhciMutationDebt {
    pub fn retain(
        bootstrap: XhciBootstrap,
        aperture: Option<Bar0ApertureLease>,
        phase: TakeoverPhase,
        fault_class: XhciDebtClass,
        secret: u64,
    ) -> Result<Self, XhciRetentionError> {
        let root = mutation_debt_root(&bootstrap, aperture.as_ref(), phase, fault_class, secret)?;
        Ok(Self {
            bootstrap,
            aperture,
            phase,
            fault_class,
            debt_root: root,
        })
    }

    pub const fn snapshot(&self) -> XhciCapabilitySnapshot {
        self.bootstrap.snapshot
    }

    pub fn debt_root(&self, secret: u64) -> u64 {
        let Ok(root) = mutation_debt_root(
            &self.bootstrap,
            self.aperture.as_ref(),
            self.phase,
            self.fault_class,
            secret,
        ) else {
            return 0;
        };
        if root != self.debt_root {
            return 0;
        }
        root
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XhciRegisterError {
    AddressOverflow,
    InvalidRangeLength(usize),
    OutsideBootstrapContainment,
    OutsideMeasuredAperture(BarApertureBoundsError),
    Mmio(MmioAccessError),
    Unmap(i32),
}

#[derive(Clone, Copy)]
enum RegisterBoundary<'lease> {
    Bootstrap {
        physical_base: u64,
        containment_bytes: u64,
    },
    Measured(&'lease Bar0ApertureLease),
}

/// Maps exactly one register access at a time and closes it before returning.
/// Pre-halt accesses are constrained by the explicit bootstrap trust boundary;
/// after BAR sizing, every access is derived from the measured aperture lease.
pub struct XhciRegisterTransport<'capability, 'authority, 'lease> {
    boundary: RegisterBoundary<'lease>,
    authority: &'capability Capability<'authority, DeviceMemoryRight>,
}

impl<'capability, 'authority> XhciRegisterTransport<'capability, 'authority, 'static> {
    pub fn bootstrap(
        controller: &XhciBootstrap,
        authority: &'capability Capability<'authority, DeviceMemoryRight>,
    ) -> Self {
        Self {
            boundary: RegisterBoundary::Bootstrap {
                physical_base: controller.snapshot.mmio_base,
                containment_bytes: XHCI_BOOTSTRAP_CONTAINMENT_BYTES,
            },
            authority,
        }
    }
}

impl<'capability, 'authority, 'lease> XhciRegisterTransport<'capability, 'authority, 'lease> {
    pub fn measured(
        aperture: &'lease Bar0ApertureLease,
        authority: &'capability Capability<'authority, DeviceMemoryRight>,
    ) -> Self {
        Self {
            boundary: RegisterBoundary::Measured(aperture),
            authority,
        }
    }

    fn physical_range(&self, offset: u32, length: usize) -> Result<u64, XhciRegisterError> {
        match self.boundary {
            RegisterBoundary::Bootstrap {
                physical_base,
                containment_bytes,
            } => {
                let end = u64::from(offset)
                    .checked_add(
                        u64::try_from(length).map_err(|_| XhciRegisterError::AddressOverflow)?,
                    )
                    .ok_or(XhciRegisterError::AddressOverflow)?;
                if end > containment_bytes {
                    return Err(XhciRegisterError::OutsideBootstrapContainment);
                }
                physical_base
                    .checked_add(u64::from(offset))
                    .ok_or(XhciRegisterError::AddressOverflow)
            }
            RegisterBoundary::Measured(aperture) => aperture
                .checked_range(u64::from(offset), length)
                .map(|range| range.physical_address())
                .map_err(XhciRegisterError::OutsideMeasuredAperture),
        }
    }

    fn with_window<T>(
        &self,
        offset: u32,
        length: usize,
        operation: impl FnOnce(&MmioWindow) -> Result<T, MmioAccessError>,
    ) -> Result<T, XhciRegisterError> {
        let physical = self.physical_range(offset, length)?;
        let window =
            MmioWindow::map(physical, length, self.authority).map_err(XhciRegisterError::Mmio)?;
        let result = operation(&window).map_err(XhciRegisterError::Mmio);
        let close = window.close(self.authority);
        if close != STATUS_OK {
            return Err(XhciRegisterError::Unmap(close));
        }
        result
    }
}

impl RegisterIo for XhciRegisterTransport<'_, '_, '_> {
    type Error = XhciRegisterError;

    fn read8(&mut self, offset: u32) -> Result<u8, Self::Error> {
        self.with_window(offset, 1, |window| window.read_u8(0))
    }

    fn write8(&mut self, offset: u32, value: u8) -> Result<(), Self::Error> {
        self.with_window(offset, 1, |window| window.write_u8(0, value))
    }

    fn read32(&mut self, offset: u32) -> Result<u32, Self::Error> {
        self.with_window(offset, core::mem::size_of::<u32>(), |window| {
            window.read_u32(0)
        })
    }

    fn write32(&mut self, offset: u32, value: u32) -> Result<(), Self::Error> {
        self.with_window(offset, core::mem::size_of::<u32>(), |window| {
            window.write_u32(0, value)
        })
    }
}

impl CheckedSupportedProtocolRead for XhciRegisterTransport<'_, '_, '_> {
    type Error = XhciRegisterError;

    fn read_u32(&mut self, range: Bar0ApertureRange<'_>) -> Result<u32, Self::Error> {
        if range.length() != core::mem::size_of::<u32>() {
            return Err(XhciRegisterError::InvalidRangeLength(range.length()));
        }
        let window = MmioWindow::map(range.physical_address(), range.length(), self.authority)
            .map_err(XhciRegisterError::Mmio)?;
        let result = window.read_u32(0).map_err(XhciRegisterError::Mmio);
        let close = window.close(self.authority);
        if close != STATUS_OK {
            return Err(XhciRegisterError::Unmap(close));
        }
        result
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum XhciActivationError {
    InvalidTakeoverConfiguration(TakeoverConfigError),
    Machine {
        phase: TakeoverPhase,
        class: TakeoverFaultClass,
    },
    InvalidPciEvidence,
    Pci(BarProbeError),
    ApertureBinding(ApertureBindingError),
    Protocol(SupportedProtocolError<XhciRegisterError>),
    Retention(XhciRetentionError),
    StepCapacity,
}

pub struct XhciActivationFailure {
    bootstrap: XhciBootstrap,
    aperture: Option<Bar0ApertureLease>,
    phase: TakeoverPhase,
    error: XhciActivationError,
    mutated: bool,
}

impl XhciActivationFailure {
    pub const fn snapshot(&self) -> XhciCapabilitySnapshot {
        self.bootstrap.snapshot
    }

    pub const fn phase(&self) -> TakeoverPhase {
        self.phase
    }

    pub const fn error(&self) -> &XhciActivationError {
        &self.error
    }

    pub const fn mutated(&self) -> bool {
        self.mutated
    }

    pub const fn debt_class(&self) -> XhciDebtClass {
        match self.error {
            XhciActivationError::Machine { class, .. } => XhciDebtClass::from_machine(class),
            XhciActivationError::Pci(_) | XhciActivationError::InvalidPciEvidence => {
                XhciDebtClass::PciTransaction
            }
            XhciActivationError::ApertureBinding(_) => XhciDebtClass::ApertureBinding,
            XhciActivationError::Protocol(_) => XhciDebtClass::ProtocolEvidence,
            XhciActivationError::Retention(_) => XhciDebtClass::Retention,
            XhciActivationError::StepCapacity => XhciDebtClass::StepCapacity,
            XhciActivationError::InvalidTakeoverConfiguration(_) => {
                XhciDebtClass::IllegalControllerState
            }
        }
    }

    pub fn into_parts(self) -> (XhciBootstrap, Option<Bar0ApertureLease>) {
        (self.bootstrap, self.aperture)
    }
}

const MAXIMUM_TAKEOVER_STEPS: usize = 5_000_000;

struct ActivationAbort {
    phase: TakeoverPhase,
    error: XhciActivationError,
    mutated: bool,
}

/// Runs the complete bounded xHCI bootstrap transaction through reset-ready.
/// BAR sizing occurs only after the machine proves HCHalted, and reset cannot
/// occur until the resulting exact aperture lease is bound back into it.
pub fn activate_reset_ready(
    bootstrap: XhciBootstrap,
    deadline_clock: &mut LocalApicDeadlineClock,
    mmio_authority: &Capability<'_, DeviceMemoryRight>,
    pci_authority: &Capability<'_, PciConfigurationControl>,
    secret: u64,
) -> Result<XhciResetReadyController, XhciActivationFailure> {
    let snapshot = bootstrap.snapshot;
    let config = TakeoverConfig {
        bootstrap_containment_bytes: XHCI_BOOTSTRAP_CONTAINMENT_BYTES,
        operational_offset: u32::from(snapshot.capability_length),
        legacy_support_offset: (snapshot.legacy_support_offset != 0)
            .then_some(snapshot.legacy_support_offset),
        maximum_ports: snapshot.maximum_ports,
        usb2_protocols: &[],
    };
    let mut machine = match TakeoverMachine::new(config) {
        Ok(machine) => machine,
        Err(error) => {
            return Err(XhciActivationFailure {
                bootstrap,
                aperture: None,
                phase: TakeoverPhase::Faulted,
                error: XhciActivationError::InvalidTakeoverConfiguration(error),
                mutated: false,
            });
        }
    };
    let mut deadline = ApicRelativeDeadline::new(deadline_clock);
    let mut aperture = None;
    let mut mutated = false;
    let transaction = 'transaction: {
        for _ in 0..MAXIMUM_TAKEOVER_STEPS {
            if machine.phase() == TakeoverPhase::AwaitMeasuredAperture {
                mutated = true;
                let evidence = bootstrap.evidence;
                let Some(address) = PciAddress::new(
                    snapshot.address.bus,
                    snapshot.address.slot,
                    snapshot.address.function,
                ) else {
                    break 'transaction Err(ActivationAbort {
                        phase: machine.phase(),
                        error: XhciActivationError::InvalidPciEvidence,
                        mutated,
                    });
                };
                if snapshot.address.segment != 0 {
                    break 'transaction Err(ActivationAbort {
                        phase: machine.phase(),
                        error: XhciActivationError::InvalidPciEvidence,
                        mutated,
                    });
                }
                let Some(expected) = PciExpectedConfiguration::from_device(PciDevice {
                    address,
                    vendor_id: evidence.vendor_id,
                    device_id: evidence.device_id,
                    class_code: evidence.class_code,
                    subclass: evidence.subclass,
                    programming_interface: evidence.programming_interface,
                    revision: evidence.revision,
                    header_type: evidence.header_type,
                    interrupt_line: evidence.interrupt_line,
                    interrupt_pin: evidence.interrupt_pin,
                    command: evidence.command,
                    bar_count: evidence.bar_count,
                    raw_bars: evidence.raw_bars,
                }) else {
                    break 'transaction Err(ActivationAbort {
                        phase: machine.phase(),
                        error: XhciActivationError::InvalidPciEvidence,
                        mutated,
                    });
                };
                let interrupt_guard = InterruptGuard::<Active>::enter();
                // SAFETY: AwaitMeasuredAperture is reachable only after the
                // controller is OS-owned, every observed port reset is clear,
                // Run/Stop is clear, and USBSTS.HCH is set. The register
                // transport maps per access and no mapping is live here.
                let quiescence = unsafe {
                    BarProbeQuiescence::asserted(
                        address,
                        pci_authority.reborrow(),
                        interrupt_guard.proof(),
                    )
                };
                let measured = match measure_bar0_aperture(quiescence, expected) {
                    Ok(aperture) => aperture,
                    Err(error) => {
                        break 'transaction Err(ActivationAbort {
                            phase: machine.phase(),
                            error: XhciActivationError::Pci(error),
                            mutated,
                        });
                    }
                };
                drop(interrupt_guard);
                if let Err(error) = machine.bind_measured_aperture(measured.length()) {
                    aperture = Some(measured);
                    break 'transaction Err(ActivationAbort {
                        phase: machine.phase(),
                        error: XhciActivationError::ApertureBinding(error),
                        mutated,
                    });
                }
                aperture = Some(measured);
                continue;
            }

            let phase = machine.phase();
            if matches!(
                phase,
                TakeoverPhase::ClaimLegacyOwnership
                    | TakeoverPhase::RequestHalt
                    | TakeoverPhase::RequestReset
            ) {
                mutated = true;
            }
            let observation = if let Some(measured) = aperture.as_ref() {
                let mut registers = XhciRegisterTransport::measured(measured, mmio_authority);
                machine.step(&mut registers, &mut deadline)
            } else {
                let mut registers = XhciRegisterTransport::bootstrap(&bootstrap, mmio_authority);
                machine.step(&mut registers, &mut deadline)
            };
            match observation {
                Ok(TakeoverObservation::Ready(ready)) => break 'transaction Ok(ready),
                Ok(_) => {}
                Err(error) => {
                    break 'transaction Err(ActivationAbort {
                        phase,
                        error: XhciActivationError::Machine {
                            phase,
                            class: error.class(),
                        },
                        mutated,
                    });
                }
            }
        }
        Err(ActivationAbort {
            phase: machine.phase(),
            error: XhciActivationError::StepCapacity,
            mutated,
        })
    };
    drop(deadline);
    let ready = match transaction {
        Ok(ready) => ready,
        Err(abort) => {
            return Err(XhciActivationFailure {
                bootstrap,
                aperture,
                phase: abort.phase,
                error: abort.error,
                mutated: abort.mutated,
            });
        }
    };
    let Some(aperture) = aperture else {
        return Err(XhciActivationFailure {
            bootstrap,
            aperture: None,
            phase: TakeoverPhase::ReadyHalted,
            error: XhciActivationError::StepCapacity,
            mutated: true,
        });
    };
    let protocols = {
        let mut registers = XhciRegisterTransport::measured(&aperture, mmio_authority);
        match decode_supported_protocols(
            &aperture,
            snapshot.extended_capabilities_offset,
            snapshot.maximum_ports,
            secret.rotate_left(17) | 1,
            &mut registers,
        ) {
            Ok(protocols) => protocols,
            Err(error) => {
                return Err(XhciActivationFailure {
                    bootstrap,
                    aperture: Some(aperture),
                    phase: TakeoverPhase::ReadyHalted,
                    error: XhciActivationError::Protocol(error),
                    mutated: true,
                });
            }
        }
    };
    let reset_ready_root =
        match validate_retained_controller(&bootstrap, &aperture, &ready, &protocols, secret) {
            Ok(root) => root,
            Err(error) => {
                return Err(XhciActivationFailure {
                    bootstrap,
                    aperture: Some(aperture),
                    phase: TakeoverPhase::ReadyHalted,
                    error: XhciActivationError::Retention(error),
                    mutated: true,
                });
            }
        };
    Ok(XhciResetReadyController {
        bootstrap,
        aperture,
        ready,
        protocols,
        reset_ready_root,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XhciProbeSummary {
    pub controllers: usize,
    pub total_ports: usize,
    pub total_slots: usize,
    pub bootstrap_headers: usize,
    pub legacy_capable_controllers: usize,
    pub reset_ready_controllers: usize,
    pub mutation_debts: usize,
    pub measured_aperture_bytes: u64,
    pub supported_protocols: usize,
    pub usb2_ports: usize,
    pub usb3_ports: usize,
    pub root: u64,
}

enum RetainedController {
    ResetReady(XhciResetReadyController),
    MutationDebt(XhciMutationDebt),
}

pub struct XhciProbeCensus {
    snapshots: [XhciCapabilitySnapshot; MAXIMUM_XHCI_CONTROLLERS],
    retained: [Option<RetainedController>; MAXIMUM_XHCI_CONTROLLERS],
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
            retained: [const { None }; MAXIMUM_XHCI_CONTROLLERS],
            length: 0,
            secret,
        })
    }

    pub fn insert(&mut self, snapshot: XhciCapabilitySnapshot) -> Result<(), XhciProbeError> {
        self.insert_snapshot(snapshot, None)
    }

    pub fn insert_reset_ready(
        &mut self,
        controller: XhciResetReadyController,
    ) -> Result<(), XhciProbeError> {
        let snapshot = controller.snapshot();
        self.insert_snapshot(snapshot, Some(RetainedController::ResetReady(controller)))
    }

    pub fn insert_mutation_debt(&mut self, debt: XhciMutationDebt) -> Result<(), XhciProbeError> {
        let snapshot = debt.snapshot();
        self.insert_snapshot(snapshot, Some(RetainedController::MutationDebt(debt)))
    }

    fn insert_snapshot(
        &mut self,
        snapshot: XhciCapabilitySnapshot,
        retained: Option<RetainedController>,
    ) -> Result<(), XhciProbeError> {
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
        self.retained[self.length] = retained;
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
            bootstrap_headers: 0,
            legacy_capable_controllers: 0,
            reset_ready_controllers: 0,
            mutation_debts: 0,
            measured_aperture_bytes: 0,
            supported_protocols: 0,
            usb2_ports: 0,
            usb3_ports: 0,
            root: mix(self.secret, self.length as u64),
        };
        for (index, snapshot) in self.snapshots().iter().enumerate() {
            summary.total_ports = summary
                .total_ports
                .saturating_add(usize::from(snapshot.maximum_ports));
            summary.total_slots = summary
                .total_slots
                .saturating_add(usize::from(snapshot.maximum_device_slots));
            summary.bootstrap_headers = summary
                .bootstrap_headers
                .saturating_add(usize::from(snapshot.bootstrap_header_count));
            summary.legacy_capable_controllers = summary
                .legacy_capable_controllers
                .saturating_add(usize::from(snapshot.legacy_support_offset != 0));
            match self.retained[index].as_ref() {
                Some(RetainedController::ResetReady(controller)) => {
                    summary.reset_ready_controllers += 1;
                    summary.measured_aperture_bytes = summary
                        .measured_aperture_bytes
                        .saturating_add(controller.aperture().length());
                    summary.supported_protocols = summary
                        .supported_protocols
                        .saturating_add(usize::from(controller.protocols().protocol_count()));
                    summary.usb2_ports = summary.usb2_ports.saturating_add(
                        controller
                            .protocols()
                            .usb2_protocols()
                            .map(|protocol| usize::from(protocol.port_count))
                            .sum::<usize>(),
                    );
                    summary.usb3_ports = summary.usb3_ports.saturating_add(
                        controller
                            .protocols()
                            .usb3_protocols()
                            .map(|protocol| usize::from(protocol.port_count))
                            .sum::<usize>(),
                    );
                    summary.root = mix(summary.root, controller.reset_ready_root());
                }
                Some(RetainedController::MutationDebt(debt)) => {
                    summary.mutation_debts += 1;
                    summary.root = mix(summary.root, debt.debt_root(self.secret));
                }
                None => {}
            }
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

pub fn boot_xhci_terminal_root(address: DeviceAddress) -> Option<u64> {
    let census = BOOT_XHCI_CENSUS.lock();
    let census = census.as_ref()?;
    let index = census
        .snapshots()
        .iter()
        .position(|snapshot| snapshot.address == address)?;
    Some(match census.retained[index].as_ref() {
        Some(RetainedController::ResetReady(controller)) => controller.reset_ready_root(),
        Some(RetainedController::MutationDebt(debt)) => {
            let root = debt.debt_root(census.secret);
            (root != 0).then_some(root)?
        }
        None => census.snapshots[index].snapshot_root,
    })
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
        XhciProbeError::InvalidBootstrapOffset(offset) => (16, u64::from(offset)),
        XhciProbeError::BootstrapCapabilityCapacity => (17, 0),
        XhciProbeError::BootstrapCapabilityOverlap(offset) => (18, u64::from(offset)),
        XhciProbeError::Unmap(status) => (19, status as u32 as u64),
        XhciProbeError::Capacity => (20, 0),
        XhciProbeError::DuplicateAddress => (21, 0),
        XhciProbeError::AlreadyPublished => (22, 0),
    };
    let mut state = mix(secret, evidence_root);
    state = mix(state, u64::from(address.segment));
    state = mix(state, u64::from(address.bus));
    state = mix(state, u64::from(address.slot));
    state = mix(state, u64::from(address.function));
    state = mix(state, code);
    Some(mix(state, detail))
}

pub fn activation_containment_root(
    secret: u64,
    snapshot: XhciCapabilitySnapshot,
    phase: TakeoverPhase,
    class: XhciDebtClass,
    mutated: bool,
) -> Option<u64> {
    if secret == 0 || snapshot.snapshot_root == 0 {
        return None;
    }
    let mut root = mix(secret, snapshot.snapshot_root);
    root = mix(root, phase as u8 as u64);
    root = mix(root, class.code());
    root = mix(root, mutated as u64);
    Some(root)
}

pub fn probe_bootstrap(
    authorization: BindingAuthorization,
    evidence: PciFunctionEvidence,
    configuration: &dyn PciConfigReader,
    authority: &Capability<'_, DeviceMemoryRight>,
    secret: u64,
) -> Result<XhciBootstrap, XhciProbeError> {
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
    let mut snapshot = result?;
    let mut reader = BootstrapMmioReader {
        mmio_base,
        authority,
    };
    let discovery = discover_bootstrap_capabilities(
        &mut reader,
        snapshot.extended_capabilities_offset,
        secret,
    )?;
    snapshot.legacy_support_offset = discovery.legacy_support_offset;
    snapshot.bootstrap_header_count = u8::try_from(discovery.journal.ranges().len())
        .map_err(|_| XhciProbeError::BootstrapCapabilityCapacity)?;
    snapshot.bootstrap_range_root = discovery.journal.root();
    snapshot.snapshot_root = snapshot_root(secret, snapshot);
    Ok(XhciBootstrap {
        authorization,
        evidence,
        snapshot,
        journal: discovery.journal,
    })
}

trait BootstrapCapabilityReader {
    fn read_header(&mut self, offset: u32) -> Result<u32, XhciProbeError>;
}

struct BootstrapMmioReader<'authority, 'capability> {
    mmio_base: u64,
    authority: &'capability Capability<'authority, DeviceMemoryRight>,
}

impl BootstrapCapabilityReader for BootstrapMmioReader<'_, '_> {
    fn read_header(&mut self, offset: u32) -> Result<u32, XhciProbeError> {
        let physical = self
            .mmio_base
            .checked_add(u64::from(offset))
            .ok_or(XhciProbeError::AddressOverflow)?;
        let window = MmioWindow::map(physical, core::mem::size_of::<u32>(), self.authority)?;
        let result = window.read_u32(0).map_err(XhciProbeError::Mmio);
        let close = window.close(self.authority);
        if close != STATUS_OK {
            return Err(XhciProbeError::Unmap(close));
        }
        result
    }
}

struct BootstrapDiscovery {
    legacy_support_offset: u32,
    journal: BootstrapRangeJournal,
}

fn discover_bootstrap_capabilities(
    reader: &mut impl BootstrapCapabilityReader,
    initial_offset: u32,
    secret: u64,
) -> Result<BootstrapDiscovery, XhciProbeError> {
    let mut offset = initial_offset;
    let mut journal = BootstrapRangeJournal::new(secret, initial_offset);
    let mut legacy_support_offset = 0;
    while offset != 0 {
        validate_bootstrap_offset(offset)?;
        if journal.ranges().len() == MAXIMUM_BOOTSTRAP_EXTENDED_HEADERS {
            return Err(XhciProbeError::BootstrapCapabilityCapacity);
        }
        let header = reader.read_header(offset)?;
        let capability_id = header as u8;
        let next_dwords = ((header >> 8) & 0xff) as u32;
        let next_offset = if next_dwords == 0 {
            0
        } else {
            let displacement = next_dwords
                .checked_mul(4)
                .ok_or(XhciProbeError::InvalidBootstrapOffset(offset))?;
            let next = offset
                .checked_add(displacement)
                .ok_or(XhciProbeError::InvalidBootstrapOffset(offset))?;
            validate_bootstrap_offset(next)?;
            next
        };
        let length = if capability_id == EXTENDED_CAPABILITY_LEGACY_SUPPORT {
            if next_offset != 0 && next_offset - offset < 8 {
                return Err(XhciProbeError::BootstrapCapabilityOverlap(offset));
            }
            8
        } else {
            4
        };
        journal.push(BootstrapRange { offset, length }, header)?;
        if capability_id == EXTENDED_CAPABILITY_LEGACY_SUPPORT {
            legacy_support_offset = offset;
            break;
        }
        offset = next_offset;
    }
    Ok(BootstrapDiscovery {
        legacy_support_offset,
        journal,
    })
}

fn validate_bootstrap_offset(offset: u32) -> Result<(), XhciProbeError> {
    if offset < CAPABILITY_BYTES as u32 || offset & 0x03 != 0 || offset > MAXIMUM_BOOTSTRAP_OFFSET {
        Err(XhciProbeError::InvalidBootstrapOffset(offset))
    } else {
        Ok(())
    }
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
        legacy_support_offset: 0,
        bootstrap_header_count: 0,
        bootstrap_range_root: 0,
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
    state = mix(state, u64::from(snapshot.extended_capabilities_offset));
    state = mix(state, u64::from(snapshot.legacy_support_offset));
    state = mix(state, u64::from(snapshot.bootstrap_header_count));
    mix(state, snapshot.bootstrap_range_root)
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

    struct ExtendedHeaders<'a> {
        entries: &'a [(u32, u32)],
        reads: usize,
    }

    impl BootstrapCapabilityReader for ExtendedHeaders<'_> {
        fn read_header(&mut self, offset: u32) -> Result<u32, XhciProbeError> {
            self.reads += 1;
            self.entries
                .iter()
                .find_map(|(known_offset, header)| (*known_offset == offset).then_some(*header))
                .ok_or(XhciProbeError::InvalidBootstrapOffset(offset))
        }
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

    #[test]
    fn bootstrap_walk_uses_current_relative_next_offsets() {
        let mut reader = ExtendedHeaders {
            entries: &[(0x100, 0x0000_0202), (0x108, 0x0000_0001)],
            reads: 0,
        };
        let discovery = discover_bootstrap_capabilities(&mut reader, 0x100, 9).unwrap();
        assert_eq!(discovery.legacy_support_offset, 0x108);
        assert_eq!(reader.reads, 2);
        assert_eq!(
            discovery.journal.ranges(),
            &[
                BootstrapRange {
                    offset: 0x100,
                    length: 4,
                },
                BootstrapRange {
                    offset: 0x108,
                    length: 8,
                },
            ]
        );
        assert_ne!(discovery.journal.root(), 0);
    }

    #[test]
    fn qemu_protocol_headers_prove_legacy_handoff_is_not_advertised() {
        let mut reader = ExtendedHeaders {
            entries: &[(0x20, 0x0200_0402), (0x30, 0x0300_0002)],
            reads: 0,
        };
        let discovery = discover_bootstrap_capabilities(&mut reader, 0x20, 11).unwrap();
        assert_eq!(discovery.legacy_support_offset, 0);
        assert_eq!(discovery.journal.ranges().len(), 2);
        assert_eq!(reader.reads, 2);
    }

    #[test]
    fn bootstrap_walk_rejects_overlapping_legacy_payload_and_policy_escape() {
        let mut overlap = ExtendedHeaders {
            entries: &[(0x20, 0x0000_0101)],
            reads: 0,
        };
        assert_eq!(
            discover_bootstrap_capabilities(&mut overlap, 0x20, 13).map(|_| ()),
            Err(XhciProbeError::BootstrapCapabilityOverlap(0x20))
        );

        let mut invalid = ExtendedHeaders {
            entries: &[],
            reads: 0,
        };
        assert_eq!(
            discover_bootstrap_capabilities(&mut invalid, MAXIMUM_BOOTSTRAP_OFFSET + 4, 13)
                .map(|_| ()),
            Err(XhciProbeError::InvalidBootstrapOffset(
                MAXIMUM_BOOTSTRAP_OFFSET + 4
            ))
        );
        assert_eq!(invalid.reads, 0);
    }
}
