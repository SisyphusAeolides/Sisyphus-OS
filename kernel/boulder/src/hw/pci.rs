use core::marker::PhantomData;

use crate::arch::x86_64::{inb, inl, outb, outl};
use crate::capability::{Capability, InterruptsDisabled, PciConfigurationControl};
use crate::sync::SpinLock;

const CONFIG_ADDRESS: u16 = 0x0cf8;
const CONFIG_DATA: u16 = 0x0cfc;
const INVALID_VENDOR_ID: u16 = 0xffff;
const MULTIFUNCTION: u8 = 1 << 7;
const MAXIMUM_DEVICES: usize = 256;
const COMMAND_OFFSET: u8 = 0x04;
const HEADER_OFFSET: u8 = 0x0c;
const BAR0_OFFSET: u8 = 0x10;
const BAR_COUNT: usize = 6;
const COMMAND_IO_SPACE: u16 = 1 << 0;
const COMMAND_MEMORY_SPACE: u16 = 1 << 1;
const COMMAND_BUS_MASTER: u16 = 1 << 2;
const COMMAND_DECODE_AND_MASTER: u16 = 0x0007;
const ALL_BARS_RESTORED: u8 = (1 << BAR_COUNT) - 1;

static CONFIGURATION_ACCESS: SpinLock<()> = SpinLock::new(());

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciAddress {
    pub bus: u8,
    pub slot: u8,
    pub function: u8,
}

impl PciAddress {
    pub const fn new(bus: u8, slot: u8, function: u8) -> Option<Self> {
        if slot < 32 && function < 8 {
            Some(Self {
                bus,
                slot,
                function,
            })
        } else {
            None
        }
    }

    const fn configuration_address(self, offset: u8) -> u32 {
        0x8000_0000
            | ((self.bus as u32) << 16)
            | ((self.slot as u32) << 11)
            | ((self.function as u32) << 8)
            | (offset as u32 & 0xfc)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciDevice {
    pub address: PciAddress,
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
    pub raw_bars: [u32; BAR_COUNT],
}

impl PciDevice {
    const EMPTY: Self = Self {
        address: PciAddress {
            bus: 0,
            slot: 0,
            function: 0,
        },
        vendor_id: INVALID_VENDOR_ID,
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
        raw_bars: [0; BAR_COUNT],
    };
}

#[derive(Clone, Copy)]
pub struct PciInventory {
    devices: [PciDevice; MAXIMUM_DEVICES],
    length: usize,
    overflowed: bool,
}

impl PciInventory {
    pub const fn new() -> Self {
        Self {
            devices: [PciDevice::EMPTY; MAXIMUM_DEVICES],
            length: 0,
            overflowed: false,
        }
    }

    pub fn devices(&self) -> &[PciDevice] {
        &self.devices[..self.length]
    }

    pub const fn overflowed(&self) -> bool {
        self.overflowed
    }

    fn push(&mut self, device: PciDevice) {
        if let Some(slot) = self.devices.get_mut(self.length) {
            *slot = device;
            self.length += 1;
        } else {
            self.overflowed = true;
        }
    }
}

impl Default for PciInventory {
    fn default() -> Self {
        Self::new()
    }
}

/// Scans all legacy PCI configuration-mechanism-one buses.
///
/// Function zero is checked for every bus and slot. Additional functions are
/// read only when the multifunction bit is present.
///
/// # Safety
///
/// The platform must expose PCI configuration mechanism one at ports CF8/CFC.
/// No firmware or external agent may access those ports concurrently.
pub unsafe fn scan_buses() -> PciInventory {
    let mut inventory = PciInventory::new();
    for bus in 0..=u8::MAX {
        for slot in 0..32 {
            let address = PciAddress {
                bus,
                slot,
                function: 0,
            };
            let Some(device) = (unsafe { read_device(address) }) else {
                continue;
            };
            let multifunction = device.header_type & MULTIFUNCTION != 0;
            inventory.push(device);
            if multifunction {
                for function in 1..8 {
                    let address = PciAddress {
                        bus,
                        slot,
                        function,
                    };
                    if let Some(device) = unsafe { read_device(address) } {
                        inventory.push(device);
                    }
                }
            }
        }
    }
    inventory
}

unsafe fn read_device(address: PciAddress) -> Option<PciDevice> {
    let _access = CONFIGURATION_ACCESS.lock();
    let vendor_device = unsafe { read_configuration_u32_unlocked(address, 0) };
    let vendor_id = vendor_device as u16;
    if vendor_id == INVALID_VENDOR_ID {
        return None;
    }
    let command_status = unsafe { read_configuration_u32_unlocked(address, 0x04) };
    let class_revision = unsafe { read_configuration_u32_unlocked(address, 0x08) };
    let header = unsafe { read_configuration_u32_unlocked(address, 0x0c) };
    let interrupt = unsafe { read_configuration_u32_unlocked(address, 0x3c) };
    let bar_count = match ((header >> 16) as u8) & 0x7f {
        0 => 6,
        1 => 2,
        _ => 0,
    };
    let mut raw_bars = [0_u32; BAR_COUNT];
    for (index, bar) in raw_bars[..bar_count].iter_mut().enumerate() {
        *bar = unsafe { read_configuration_u32_unlocked(address, BAR0_OFFSET + (index as u8 * 4)) };
    }
    Some(decode_device(
        address,
        vendor_device,
        command_status,
        class_revision,
        header,
        interrupt,
        bar_count as u8,
        raw_bars,
    ))
}

unsafe fn read_configuration_u32(address: PciAddress, offset: u8) -> u32 {
    let _access = CONFIGURATION_ACCESS.lock();
    unsafe { read_configuration_u32_unlocked(address, offset) }
}

unsafe fn read_configuration_u32_unlocked(address: PciAddress, offset: u8) -> u32 {
    unsafe {
        outl(CONFIG_ADDRESS, address.configuration_address(offset));
        inl(CONFIG_DATA)
    }
}

unsafe fn write_configuration_u32_unlocked(address: PciAddress, offset: u8, value: u32) {
    unsafe {
        outl(CONFIG_ADDRESS, address.configuration_address(offset));
        outl(CONFIG_DATA, value);
    }
}

unsafe fn read_configuration_u16_unlocked(address: PciAddress, offset: u8) -> u16 {
    let lane = u16::from(offset & 0x03);
    unsafe {
        outl(CONFIG_ADDRESS, address.configuration_address(offset));
        u16::from(inb(CONFIG_DATA + lane)) | (u16::from(inb(CONFIG_DATA + lane + 1)) << 8)
    }
}

unsafe fn write_configuration_u16_unlocked(address: PciAddress, offset: u8, value: u16) {
    let lane = u16::from(offset & 0x03);
    unsafe {
        outl(CONFIG_ADDRESS, address.configuration_address(offset));
        outb(CONFIG_DATA + lane, value as u8);
        outb(CONFIG_DATA + lane + 1, (value >> 8) as u8);
    }
}

fn decode_device(
    address: PciAddress,
    vendor_device: u32,
    command_status: u32,
    class_revision: u32,
    header: u32,
    interrupt: u32,
    bar_count: u8,
    raw_bars: [u32; BAR_COUNT],
) -> PciDevice {
    PciDevice {
        address,
        vendor_id: vendor_device as u16,
        device_id: (vendor_device >> 16) as u16,
        class_code: (class_revision >> 24) as u8,
        subclass: (class_revision >> 16) as u8,
        programming_interface: (class_revision >> 8) as u8,
        revision: class_revision as u8,
        header_type: (header >> 16) as u8,
        interrupt_line: interrupt as u8,
        interrupt_pin: (interrupt >> 8) as u8,
        command: command_status as u16,
        bar_count,
        raw_bars,
    }
}
/// Read a 32-bit PCI config dword. Safe wrapper over the locked CF8/CFC path.
pub unsafe fn read_config_u32(address: PciAddress, offset: u8) -> u32 {
    unsafe { read_configuration_u32(address, offset) }
}

/// Write a 32-bit PCI config dword under the global configuration lock.
pub unsafe fn write_config_u32(address: PciAddress, offset: u8, value: u32) {
    let _access = CONFIGURATION_ACCESS.lock();
    unsafe { write_configuration_u32_unlocked(address, offset, value) };
}

/// Exact retained type-0 configuration evidence checked before BAR mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciExpectedConfiguration {
    address: PciAddress,
    vendor_id: u16,
    device_id: u16,
    class_code: u8,
    subclass: u8,
    programming_interface: u8,
    revision: u8,
    header_type: u8,
    command: u16,
    raw_bars: [u32; BAR_COUNT],
}

impl PciExpectedConfiguration {
    pub const fn from_device(device: PciDevice) -> Option<Self> {
        if device.vendor_id == 0
            || device.vendor_id == INVALID_VENDOR_ID
            || device.header_type & 0x7f != 0
            || device.bar_count as usize != BAR_COUNT
        {
            return None;
        }
        Some(Self {
            address: device.address,
            vendor_id: device.vendor_id,
            device_id: device.device_id,
            class_code: device.class_code,
            subclass: device.subclass,
            programming_interface: device.programming_interface,
            revision: device.revision,
            header_type: device.header_type,
            command: device.command,
            raw_bars: device.raw_bars,
        })
    }

    pub const fn address(&self) -> PciAddress {
        self.address
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BarCommandState {
    DecodeDisabled,
    Observed(u16),
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BarRestoreFault {
    WriteFailed {
        offset: u8,
    },
    ReadFailed {
        offset: u8,
    },
    Mismatch {
        offset: u8,
        expected: u32,
        observed: u32,
    },
}

/// Evidence that a BAR-sizing transaction could not prove its rollback.
///
/// Decode is never deliberately re-enabled when this value is produced. The
/// command state records whether that fail-closed condition was observed or
/// whether configuration access itself made the state unknowable. The bitmap
/// names BAR dwords whose original values were read back exactly.
#[derive(Debug, Eq, PartialEq)]
pub struct BarRestorationDebt {
    address: PciAddress,
    fault: BarRestoreFault,
    restored_bar_mask: u8,
    command_state: BarCommandState,
}

impl BarRestorationDebt {
    pub const fn address(&self) -> PciAddress {
        self.address
    }

    pub const fn fault(&self) -> BarRestoreFault {
        self.fault
    }

    pub const fn restored_bar_mask(&self) -> u8 {
        self.restored_bar_mask
    }

    pub const fn command_state(&self) -> BarCommandState {
        self.command_state
    }

    pub const fn all_bars_restored(&self) -> bool {
        self.restored_bar_mask == ALL_BARS_RESTORED
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum BarProbeError {
    ConfigurationAccessFailed { offset: u8 },
    ExpectedAddressMismatch,
    ConfigurationChanged { offset: u8 },
    DeviceAbsent,
    UnsupportedHeader { header_type: u8 },
    QuiescenceFailed { command: u16 },
    InvalidBarEncoding { index: u8 },
    Truncated64BitBar { index: u8 },
    InvalidSizeMask { index: u8 },
    Bar0IsIo,
    Bar0Unassigned,
    Bar0Misaligned { base: u64, length: u64 },
    Bar0AddressOverflow,
    RestorationDebt(BarRestorationDebt),
}

/// A linear proof obligation supplied by the device-specific state machine.
///
/// It is intentionally neither `Copy` nor `Clone`: each sizing transaction
/// consumes one assertion that the device cannot issue DMA, interrupts, MMIO,
/// or I/O transactions while its BARs temporarily contain sizing masks.
pub struct BarProbeQuiescence<'authority, 'guard> {
    address: PciAddress,
    _configuration: Capability<'authority, PciConfigurationControl>,
    _interrupts: InterruptsDisabled<'guard>,
}

impl<'authority, 'guard> BarProbeQuiescence<'authority, 'guard> {
    /// Binds device quiescence to PCI-configuration authority and a local
    /// interrupt-masking epoch.
    ///
    /// # Safety
    ///
    /// The caller must exclusively own `address`, have stopped every device DMA
    /// engine and interrupt source, and prevent firmware or another CPU from
    /// touching the function's decoded windows until the sizing call returns.
    pub const unsafe fn asserted(
        address: PciAddress,
        configuration: Capability<'authority, PciConfigurationControl>,
        interrupts: InterruptsDisabled<'guard>,
    ) -> Self {
        Self {
            address,
            _configuration: configuration,
            _interrupts: interrupts,
        }
    }

    pub const fn address(&self) -> PciAddress {
        self.address
    }
}

/// An exact, restored BAR0 MMIO aperture.
///
/// Construction is private and occurs only after the complete BAR image was
/// restored and read back with decode disabled, followed by an exact command
/// readback with I/O and bus mastering still disabled.
#[derive(Debug, Eq, PartialEq)]
pub struct Bar0ApertureLease {
    address: PciAddress,
    physical_base: u64,
    length: u64,
}

impl Bar0ApertureLease {
    pub const fn address(&self) -> PciAddress {
        self.address
    }

    pub const fn physical_base(&self) -> u64 {
        self.physical_base
    }

    pub const fn length(&self) -> u64 {
        self.length
    }

    /// Derives a range that is both non-empty and wholly contained in BAR0.
    pub fn checked_range(
        &self,
        offset: u64,
        length: usize,
    ) -> Result<Bar0ApertureRange<'_>, BarApertureBoundsError> {
        if length == 0 {
            return Err(BarApertureBoundsError::Empty);
        }
        let length_u64 = u64::try_from(length).map_err(|_| BarApertureBoundsError::Overflow)?;
        let end = offset
            .checked_add(length_u64)
            .ok_or(BarApertureBoundsError::Overflow)?;
        if end > self.length {
            return Err(BarApertureBoundsError::OutsideAperture);
        }
        let physical_address = self
            .physical_base
            .checked_add(offset)
            .ok_or(BarApertureBoundsError::Overflow)?;
        Ok(Bar0ApertureRange {
            physical_address,
            length,
            _lease: PhantomData,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BarApertureBoundsError {
    Empty,
    Overflow,
    OutsideAperture,
}

/// A range whose lifetime is tied to the non-copy aperture lease that checked it.
#[derive(Debug, Eq, PartialEq)]
pub struct Bar0ApertureRange<'lease> {
    physical_address: u64,
    length: usize,
    _lease: PhantomData<&'lease Bar0ApertureLease>,
}

impl Bar0ApertureRange<'_> {
    pub const fn physical_address(&self) -> u64 {
        self.physical_address
    }

    pub const fn length(&self) -> usize {
        self.length
    }
}

trait ConfigurationSpace {
    fn read_u32(&mut self, offset: u8) -> Result<u32, ()>;
    fn write_u32(&mut self, offset: u8, value: u32) -> Result<(), ()>;
    fn read_command(&mut self) -> Result<u16, ()>;
    fn write_command(&mut self, value: u16) -> Result<(), ()>;
}

struct LegacyConfigurationSpace {
    address: PciAddress,
}

impl ConfigurationSpace for LegacyConfigurationSpace {
    fn read_u32(&mut self, offset: u8) -> Result<u32, ()> {
        Ok(unsafe { read_configuration_u32_unlocked(self.address, offset) })
    }

    fn write_u32(&mut self, offset: u8, value: u32) -> Result<(), ()> {
        unsafe { write_configuration_u32_unlocked(self.address, offset, value) };
        Ok(())
    }

    fn read_command(&mut self) -> Result<u16, ()> {
        Ok(unsafe { read_configuration_u16_unlocked(self.address, COMMAND_OFFSET) })
    }

    fn write_command(&mut self, value: u16) -> Result<(), ()> {
        unsafe { write_configuration_u16_unlocked(self.address, COMMAND_OFFSET, value) };
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BarSnapshot {
    bars: [u32; BAR_COUNT],
}

#[derive(Debug, Eq, PartialEq)]
struct BarMeasurement {
    snapshot: BarSnapshot,
    lengths: [u64; BAR_COUNT],
}

/// Sizes BAR0..BAR5 as one serialized, fail-closed type-0 transaction.
/// A 64-bit BAR reports its length in the low slot and zero in its consumed high slot.
/// On success the original BAR image and memory-decode bit are restored; I/O
/// decode and bus mastering deliberately remain disabled. Prefer
/// [`measure_bar0_aperture`] when the caller needs an MMIO mapping authority.
///
/// # Safety
///
/// The caller must exclusively own the device, stop its DMA engines and interrupt
/// handlers, and ensure that no CPU, firmware, or external agent can access its
/// MMIO or I/O windows until this function returns. The configuration lock only
/// serializes Boulder configuration-port users; it cannot establish device
/// quiescence. Violating this contract can redirect live transactions while BARs
/// temporarily contain sizing masks.
pub unsafe fn bar_lengths(address: PciAddress) -> Result<[u64; BAR_COUNT], BarProbeError> {
    let _access = CONFIGURATION_ACCESS.lock();
    let mut configuration = LegacyConfigurationSpace { address };
    probe_bar_lengths(&mut configuration, address, false, None)
        .map(|(measurement, _)| measurement.lengths)
}

/// Consumes one caller-supplied quiescence proof and returns an exact BAR0
/// aperture only after the sizing transaction reaches its fail-closed commit
/// point.
pub fn measure_bar0_aperture(
    quiescence: BarProbeQuiescence<'_, '_>,
    expected: PciExpectedConfiguration,
) -> Result<Bar0ApertureLease, BarProbeError> {
    let address = quiescence.address;
    if expected.address != address {
        return Err(BarProbeError::ExpectedAddressMismatch);
    }
    let _access = CONFIGURATION_ACCESS.lock();
    let mut configuration = LegacyConfigurationSpace { address };
    let (_, lease) = probe_bar_lengths(&mut configuration, address, true, Some(&expected))?;
    lease.ok_or(BarProbeError::Bar0Unassigned)
}

fn probe_bar_lengths(
    configuration: &mut impl ConfigurationSpace,
    address: PciAddress,
    require_bar0_lease: bool,
    expected: Option<&PciExpectedConfiguration>,
) -> Result<(BarMeasurement, Option<Bar0ApertureLease>), BarProbeError> {
    if expected.is_some_and(|expected| expected.address != address) {
        return Err(BarProbeError::ExpectedAddressMismatch);
    }
    let vendor_device = read(configuration, 0x00)?;
    let vendor = vendor_device as u16;
    if vendor == INVALID_VENDOR_ID {
        return Err(BarProbeError::DeviceAbsent);
    }
    let class_revision = read(configuration, 0x08)?;
    let header = read(configuration, HEADER_OFFSET)?;
    let header_type = (header >> 16) as u8;
    if header_type & 0x7f != 0 {
        return Err(BarProbeError::UnsupportedHeader { header_type });
    }
    let command =
        configuration
            .read_command()
            .map_err(|()| BarProbeError::ConfigurationAccessFailed {
                offset: COMMAND_OFFSET,
            })?;
    let mut bars = [0_u32; BAR_COUNT];
    for (index, bar) in bars.iter_mut().enumerate() {
        *bar = read(configuration, bar_offset(index))?;
    }
    if let Some(expected) = expected {
        validate_expected_configuration(
            expected,
            vendor_device,
            class_revision,
            header_type,
            command,
            &bars,
        )?;
    }
    let snapshot = BarSnapshot { bars };
    let disabled_command = command & !COMMAND_DECODE_AND_MASTER;
    if configuration.write_command(disabled_command).is_err() {
        return Err(restoration_debt(
            address,
            BarRestoreFault::WriteFailed {
                offset: COMMAND_OFFSET,
            },
            ALL_BARS_RESTORED,
            BarCommandState::Unknown,
        ));
    }
    let disabled = match configuration.read_command() {
        Ok(command) => command,
        Err(()) => {
            return Err(restoration_debt(
                address,
                BarRestoreFault::ReadFailed {
                    offset: COMMAND_OFFSET,
                },
                ALL_BARS_RESTORED,
                BarCommandState::Unknown,
            ));
        }
    };
    if disabled != disabled_command || disabled & COMMAND_DECODE_AND_MASTER != 0 {
        return Err(BarProbeError::QuiescenceFailed { command: disabled });
    }

    // Conservatively treat every BAR as potentially modified once decode has
    // been disabled. Even an encoding error or failed write takes the complete
    // high-to-low restore and readback path.
    let probe_result = size_bars(configuration, &snapshot);
    let restored_bar_mask =
        restore_and_verify_bars(configuration, address, &snapshot, disabled_command)?;
    debug_assert_eq!(restored_bar_mask, ALL_BARS_RESTORED);

    let lengths = match probe_result {
        Ok(lengths) => lengths,
        Err(error) => return Err(error),
    };
    let measurement = BarMeasurement { snapshot, lengths };
    let bar0_lease = if require_bar0_lease {
        Some(build_bar0_lease(address, &measurement)?)
    } else {
        None
    };

    // Among the three transaction-producing bits, only the original memory
    // decode state may be restored. I/O decode and bus mastering remain off.
    let committed_command = command & !(COMMAND_IO_SPACE | COMMAND_BUS_MASTER);
    debug_assert_eq!(
        committed_command & COMMAND_DECODE_AND_MASTER,
        command & COMMAND_MEMORY_SPACE
    );
    if configuration.write_command(committed_command).is_err() {
        return Err(restoration_debt(
            address,
            BarRestoreFault::WriteFailed {
                offset: COMMAND_OFFSET,
            },
            restored_bar_mask,
            BarCommandState::Unknown,
        ));
    }
    match configuration.read_command() {
        Ok(observed) if observed == committed_command => Ok((measurement, bar0_lease)),
        Ok(observed) => Err(restoration_debt(
            address,
            BarRestoreFault::Mismatch {
                offset: COMMAND_OFFSET,
                expected: u32::from(committed_command),
                observed: u32::from(observed),
            },
            restored_bar_mask,
            command_state(observed),
        )),
        Err(()) => Err(restoration_debt(
            address,
            BarRestoreFault::ReadFailed {
                offset: COMMAND_OFFSET,
            },
            restored_bar_mask,
            BarCommandState::Unknown,
        )),
    }
}

fn validate_expected_configuration(
    expected: &PciExpectedConfiguration,
    vendor_device: u32,
    class_revision: u32,
    header_type: u8,
    command: u16,
    bars: &[u32; BAR_COUNT],
) -> Result<(), BarProbeError> {
    let expected_vendor_device =
        (u32::from(expected.device_id) << 16) | u32::from(expected.vendor_id);
    if vendor_device != expected_vendor_device {
        return Err(BarProbeError::ConfigurationChanged { offset: 0x00 });
    }
    let expected_class_revision = (u32::from(expected.class_code) << 24)
        | (u32::from(expected.subclass) << 16)
        | (u32::from(expected.programming_interface) << 8)
        | u32::from(expected.revision);
    if class_revision != expected_class_revision {
        return Err(BarProbeError::ConfigurationChanged { offset: 0x08 });
    }
    if header_type != expected.header_type {
        return Err(BarProbeError::ConfigurationChanged {
            offset: HEADER_OFFSET,
        });
    }
    if command != expected.command {
        return Err(BarProbeError::ConfigurationChanged {
            offset: COMMAND_OFFSET,
        });
    }
    for (index, (&observed, &retained)) in bars.iter().zip(expected.raw_bars.iter()).enumerate() {
        if observed != retained {
            return Err(BarProbeError::ConfigurationChanged {
                offset: bar_offset(index),
            });
        }
    }
    Ok(())
}

fn size_bars(
    configuration: &mut impl ConfigurationSpace,
    snapshot: &BarSnapshot,
) -> Result<[u64; BAR_COUNT], BarProbeError> {
    let mut lengths = [0_u64; BAR_COUNT];
    let mut index = 0;
    while index < BAR_COUNT {
        let original = snapshot.bars[index];
        let offset = bar_offset(index);
        if original & 1 != 0 {
            if original & 0x2 != 0 {
                return Err(BarProbeError::InvalidBarEncoding { index: index as u8 });
            }
            write(configuration, offset, u32::MAX)?;
            let probed = read(configuration, offset)?;
            if probed == 0 {
                index += 1;
                continue;
            }
            if probed & 0x3 != original & 0x3 {
                return Err(BarProbeError::InvalidBarEncoding { index: index as u8 });
            }
            lengths[index] = decode_size(
                index,
                u64::from(probed & 0xffff_fffc),
                u32::MAX as u64,
                false,
            )?;
            index += 1;
            continue;
        }

        match (original >> 1) & 0x3 {
            memory_type @ (0 | 1) => {
                write(configuration, offset, u32::MAX)?;
                let probed = read(configuration, offset)?;
                if probed == 0 {
                    index += 1;
                    continue;
                }
                if probed & 0xf != original & 0xf {
                    return Err(BarProbeError::InvalidBarEncoding { index: index as u8 });
                }
                let (mask, width_mask, allow_zero_mask) = if memory_type == 1 {
                    if original & 0xfff0_0000 != 0 || probed & 0xfff0_0000 != 0 {
                        return Err(BarProbeError::InvalidBarEncoding { index: index as u8 });
                    }
                    (u64::from(probed & 0x000f_fff0), 0x000f_ffff, true)
                } else {
                    (u64::from(probed & 0xffff_fff0), u32::MAX as u64, false)
                };
                let size = decode_size(index, mask, width_mask, allow_zero_mask)?;
                if memory_type == 1 && size > 0x10_0000 {
                    return Err(BarProbeError::InvalidBarEncoding { index: index as u8 });
                }
                lengths[index] = size;
                index += 1;
            }
            2 => {
                if index + 1 == BAR_COUNT {
                    return Err(BarProbeError::Truncated64BitBar { index: index as u8 });
                }
                write(configuration, offset, u32::MAX)?;
                write(configuration, bar_offset(index + 1), u32::MAX)?;
                let probed_low = read(configuration, offset)?;
                let probed_high = read(configuration, bar_offset(index + 1))?;
                if probed_low == 0 && probed_high == 0 {
                    index += 2;
                    continue;
                }
                if probed_low & 0xf != original & 0xf {
                    return Err(BarProbeError::InvalidBarEncoding { index: index as u8 });
                }
                let mask = (u64::from(probed_high) << 32) | u64::from(probed_low & 0xffff_fff0);
                lengths[index] = decode_size(index, mask, u64::MAX, false)?;
                index += 2;
            }
            _ => return Err(BarProbeError::InvalidBarEncoding { index: index as u8 }),
        }
    }
    Ok(lengths)
}

fn decode_size(
    index: usize,
    mask: u64,
    width_mask: u64,
    allow_zero_mask: bool,
) -> Result<u64, BarProbeError> {
    if mask == 0 && !allow_zero_mask {
        return Err(BarProbeError::InvalidSizeMask { index: index as u8 });
    }
    let size = (!mask & width_mask).wrapping_add(1);
    if size == 0 || !size.is_power_of_two() {
        Err(BarProbeError::InvalidSizeMask { index: index as u8 })
    } else {
        Ok(size)
    }
}

fn restore_and_verify_bars(
    configuration: &mut impl ConfigurationSpace,
    address: PciAddress,
    snapshot: &BarSnapshot,
    disabled_command: u16,
) -> Result<u8, BarProbeError> {
    let mut first_fault = None;

    // Reverse order restores the high half of every 64-bit BAR before its low
    // half can make the pair observable again. Decode remains disabled.
    for index in (0..BAR_COUNT).rev() {
        let offset = bar_offset(index);
        if configuration
            .write_u32(offset, snapshot.bars[index])
            .is_err()
            && first_fault.is_none()
        {
            first_fault = Some(BarRestoreFault::WriteFailed { offset });
        }
    }

    match configuration.read_command() {
        Ok(observed) if observed == disabled_command => {}
        Ok(observed) => {
            if first_fault.is_none() {
                first_fault = Some(BarRestoreFault::Mismatch {
                    offset: COMMAND_OFFSET,
                    expected: u32::from(disabled_command),
                    observed: u32::from(observed),
                });
            }
        }
        Err(()) => {
            if first_fault.is_none() {
                first_fault = Some(BarRestoreFault::ReadFailed {
                    offset: COMMAND_OFFSET,
                });
            }
        }
    }

    let mut restored_bar_mask = 0_u8;
    for (index, original) in snapshot.bars.iter().copied().enumerate() {
        let offset = bar_offset(index);
        match configuration.read_u32(offset) {
            Ok(observed) if observed == original => restored_bar_mask |= 1 << index,
            Ok(observed) if first_fault.is_none() => {
                first_fault = Some(BarRestoreFault::Mismatch {
                    offset,
                    expected: original,
                    observed,
                });
            }
            Err(()) if first_fault.is_none() => {
                first_fault = Some(BarRestoreFault::ReadFailed { offset });
            }
            _ => {}
        }
    }

    // This second observation brackets every restoration readback. No memory
    // decode is enabled until all BARs and both command observations are exact.
    let observed_command_state = match configuration.read_command() {
        Ok(observed) if observed == disabled_command => BarCommandState::DecodeDisabled,
        Ok(observed) => {
            if first_fault.is_none() {
                first_fault = Some(BarRestoreFault::Mismatch {
                    offset: COMMAND_OFFSET,
                    expected: u32::from(disabled_command),
                    observed: u32::from(observed),
                });
            }
            command_state(observed)
        }
        Err(()) => {
            if first_fault.is_none() {
                first_fault = Some(BarRestoreFault::ReadFailed {
                    offset: COMMAND_OFFSET,
                });
            }
            BarCommandState::Unknown
        }
    };

    match first_fault {
        Some(fault) => Err(restoration_debt(
            address,
            fault,
            restored_bar_mask,
            observed_command_state,
        )),
        None => Ok(restored_bar_mask),
    }
}

fn restoration_debt(
    address: PciAddress,
    fault: BarRestoreFault,
    restored_bar_mask: u8,
    command_state: BarCommandState,
) -> BarProbeError {
    BarProbeError::RestorationDebt(BarRestorationDebt {
        address,
        fault,
        restored_bar_mask,
        command_state,
    })
}

const fn command_state(command: u16) -> BarCommandState {
    if command & COMMAND_DECODE_AND_MASTER == 0 {
        BarCommandState::DecodeDisabled
    } else {
        BarCommandState::Observed(command)
    }
}

fn build_bar0_lease(
    address: PciAddress,
    measurement: &BarMeasurement,
) -> Result<Bar0ApertureLease, BarProbeError> {
    let low = measurement.snapshot.bars[0];
    if low & 1 != 0 {
        return Err(BarProbeError::Bar0IsIo);
    }
    let physical_base = match (low >> 1) & 0x3 {
        0 => u64::from(low & 0xffff_fff0),
        1 => u64::from(low & 0x000f_fff0),
        2 => (u64::from(measurement.snapshot.bars[1]) << 32) | u64::from(low & 0xffff_fff0),
        _ => return Err(BarProbeError::InvalidBarEncoding { index: 0 }),
    };
    let length = measurement.lengths[0];
    if physical_base == 0 || length == 0 {
        return Err(BarProbeError::Bar0Unassigned);
    }
    if physical_base & (length - 1) != 0 {
        return Err(BarProbeError::Bar0Misaligned {
            base: physical_base,
            length,
        });
    }
    if physical_base.checked_add(length).is_none() {
        return Err(BarProbeError::Bar0AddressOverflow);
    }
    Ok(Bar0ApertureLease {
        address,
        physical_base,
        length,
    })
}

fn read(configuration: &mut impl ConfigurationSpace, offset: u8) -> Result<u32, BarProbeError> {
    configuration
        .read_u32(offset)
        .map_err(|()| BarProbeError::ConfigurationAccessFailed { offset })
}

fn write(
    configuration: &mut impl ConfigurationSpace,
    offset: u8,
    value: u32,
) -> Result<(), BarProbeError> {
    configuration
        .write_u32(offset, value)
        .map_err(|()| BarProbeError::ConfigurationAccessFailed { offset })
}

const fn bar_offset(index: usize) -> u8 {
    BAR0_OFFSET + index as u8 * 4
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_ADDRESS: PciAddress = PciAddress {
        bus: 0,
        slot: 20,
        function: 0,
    };

    fn probe_bar_lengths(
        configuration: &mut impl ConfigurationSpace,
    ) -> Result<[u64; BAR_COUNT], BarProbeError> {
        super::probe_bar_lengths(configuration, TEST_ADDRESS, false, None)
            .map(|(measurement, _)| measurement.lengths)
    }

    fn expected_configuration(bars: [u32; BAR_COUNT]) -> PciExpectedConfiguration {
        PciExpectedConfiguration::from_device(PciDevice {
            address: TEST_ADDRESS,
            vendor_id: 0x1234,
            device_id: 0x5678,
            class_code: 0,
            subclass: 0,
            programming_interface: 0,
            revision: 0,
            header_type: 0,
            interrupt_line: 0xff,
            interrupt_pin: 0,
            command: COMMAND_DECODE_AND_MASTER,
            bar_count: BAR_COUNT as u8,
            raw_bars: bars,
        })
        .unwrap()
    }

    struct ModelConfiguration {
        values: [u32; 16],
        original: [u32; 16],
        masks: [u32; BAR_COUNT],
        fail_probe_read: Option<usize>,
        fail_restore_write: Option<usize>,
        fail_command_write: Option<usize>,
        command_write_count: usize,
        bar_write_while_active: bool,
        bar_verify_while_active: bool,
        sizing_started: bool,
        restore_order: [u8; BAR_COUNT],
        restore_count: usize,
    }

    impl ModelConfiguration {
        fn with_bars(bars: [u32; BAR_COUNT], masks: [u32; BAR_COUNT]) -> Self {
            let mut values = [0_u32; 16];
            values[0] = 0x5678_1234;
            values[1] = u32::from(COMMAND_DECODE_AND_MASTER);
            values[3] = 0;
            values[4..10].copy_from_slice(&bars);
            Self {
                values,
                original: values,
                masks,
                fail_probe_read: None,
                fail_restore_write: None,
                fail_command_write: None,
                command_write_count: 0,
                bar_write_while_active: false,
                bar_verify_while_active: false,
                sizing_started: false,
                restore_order: [u8::MAX; BAR_COUNT],
                restore_count: 0,
            }
        }

        fn bars_restored(&self) -> bool {
            self.values[4..10] == self.original[4..10]
        }

        fn committed_command(&self) -> u16 {
            self.values[1] as u16
        }
    }

    impl ConfigurationSpace for ModelConfiguration {
        fn read_u32(&mut self, offset: u8) -> Result<u32, ()> {
            let slot = usize::from(offset / 4);
            if (BAR0_OFFSET..BAR0_OFFSET + BAR_COUNT as u8 * 4).contains(&offset) {
                let bar = usize::from((offset - BAR0_OFFSET) / 4);
                if self.values[slot] == u32::MAX {
                    if self.fail_probe_read == Some(bar) {
                        self.fail_probe_read = None;
                        return Err(());
                    }
                    return Ok(self.masks[bar]);
                }
                if self.sizing_started && self.read_command()? & COMMAND_DECODE_AND_MASTER != 0 {
                    self.bar_verify_while_active = true;
                }
            }
            Ok(self.values[slot])
        }

        fn write_u32(&mut self, offset: u8, value: u32) -> Result<(), ()> {
            if (BAR0_OFFSET..BAR0_OFFSET + BAR_COUNT as u8 * 4).contains(&offset) {
                let bar = usize::from((offset - BAR0_OFFSET) / 4);
                if self.read_command()? & COMMAND_DECODE_AND_MASTER != 0 {
                    self.bar_write_while_active = true;
                }
                let slot = usize::from(offset / 4);
                if value == u32::MAX {
                    self.sizing_started = true;
                }
                if value == self.original[slot] {
                    self.restore_order[self.restore_count] = bar as u8;
                    self.restore_count += 1;
                    if self.fail_restore_write == Some(bar) {
                        self.fail_restore_write = None;
                        return Err(());
                    }
                }
            }
            self.values[usize::from(offset / 4)] = value;
            Ok(())
        }

        fn read_command(&mut self) -> Result<u16, ()> {
            Ok(self.values[1] as u16)
        }

        fn write_command(&mut self, value: u16) -> Result<(), ()> {
            let write_number = self.command_write_count;
            self.command_write_count += 1;
            if self.fail_command_write == Some(write_number) {
                return Err(());
            }
            self.values[1] = (self.values[1] & 0xffff_0000) | u32::from(value);
            Ok(())
        }
    }

    #[test]
    fn builds_configuration_addresses() {
        let address = PciAddress::new(0xab, 0x1d, 6).unwrap();
        assert_eq!(address.configuration_address(0x13), 0x80ab_ee10);
        assert!(PciAddress::new(0, 32, 0).is_none());
        assert!(PciAddress::new(0, 0, 8).is_none());
    }

    #[test]
    fn decodes_standard_configuration_fields() {
        let address = PciAddress::new(2, 3, 1).unwrap();
        let bars = [0xfebf_0004, 0, 0, 0, 0, 0];
        let device = decode_device(
            address,
            0x5678_1234,
            0x0010_0007,
            0x0200_01a2,
            0x0080_0000,
            0x0000_010b,
            6,
            bars,
        );

        assert_eq!(device.address, address);
        assert_eq!(device.vendor_id, 0x1234);
        assert_eq!(device.device_id, 0x5678);
        assert_eq!(device.class_code, 0x02);
        assert_eq!(device.subclass, 0x00);
        assert_eq!(device.programming_interface, 0x01);
        assert_eq!(device.revision, 0xa2);
        assert_eq!(device.header_type, 0x80);
        assert_eq!(device.interrupt_line, 0x0b);
        assert_eq!(device.interrupt_pin, 0x01);
        assert_eq!(device.command, 0x0007);
        assert_eq!(device.bar_count, 6);
        assert_eq!(device.raw_bars, bars);
    }

    #[test]
    fn inventory_reports_capacity_overflow() {
        let mut inventory = PciInventory::new();
        for _ in 0..=MAXIMUM_DEVICES {
            inventory.push(PciDevice::EMPTY);
        }
        assert_eq!(inventory.devices().len(), MAXIMUM_DEVICES);
        assert!(inventory.overflowed());
    }

    #[test]
    fn bar_probe_sizes_exact_pairs_and_restores_configuration() {
        let bars = [
            0x8000_0004,
            0x0000_0001,
            0x0000_c001,
            0xd000_0000,
            0,
            0x0008_0002,
        ];
        let masks = [
            0xffff_f004,
            0xffff_ffff,
            0xffff_ff01,
            0xfffe_0000,
            0,
            0x0000_0002,
        ];
        let mut configuration = ModelConfiguration::with_bars(bars, masks);

        let lengths = probe_bar_lengths(&mut configuration).expect("valid BAR model probes");

        assert_eq!(lengths, [0x1000, 0, 0x100, 0x2_0000, 0, 0x10_0000]);
        assert!(configuration.bars_restored());
        assert_eq!(configuration.committed_command(), COMMAND_MEMORY_SPACE);
        assert!(!configuration.bar_write_while_active);
        assert!(!configuration.bar_verify_while_active);
        assert_eq!(configuration.restore_order, [5, 4, 3, 2, 1, 0]);
    }

    #[test]
    fn probe_fault_restores_every_bar_and_command() {
        let bars = [0x8000_0004, 0, 0x0000_c001, 0, 0, 0];
        let masks = [0xffff_f004, 0xffff_ffff, 0xffff_ff01, 0, 0, 0];
        let mut configuration = ModelConfiguration::with_bars(bars, masks);
        configuration.fail_probe_read = Some(2);

        assert_eq!(
            probe_bar_lengths(&mut configuration),
            Err(BarProbeError::ConfigurationAccessFailed {
                offset: BAR0_OFFSET + 8,
            })
        );
        assert!(configuration.bars_restored());
        assert_eq!(configuration.committed_command(), 0);
        assert!(!configuration.bar_write_while_active);
    }

    #[test]
    fn mid_pair_fault_restores_high_before_low() {
        let bars = [0x8000_0004, 1, 0, 0, 0, 0];
        let masks = [0xffff_f004, 0xffff_ffff, 0, 0, 0, 0];
        let mut configuration = ModelConfiguration::with_bars(bars, masks);
        configuration.fail_probe_read = Some(1);

        assert_eq!(
            probe_bar_lengths(&mut configuration),
            Err(BarProbeError::ConfigurationAccessFailed {
                offset: BAR0_OFFSET + 4,
            })
        );
        assert!(configuration.bars_restored());
        assert_eq!(configuration.committed_command(), 0);
        assert_eq!(configuration.restore_order, [5, 4, 3, 2, 1, 0]);
    }

    #[test]
    fn failed_high_restore_still_restores_low_and_command() {
        let bars = [0x8000_0004, 1, 0, 0, 0, 0];
        let masks = [0xffff_f004, 0xffff_ffff, 0, 0, 0, 0];
        let mut configuration = ModelConfiguration::with_bars(bars, masks);
        configuration.fail_restore_write = Some(1);

        assert_eq!(
            probe_bar_lengths(&mut configuration),
            Err(BarProbeError::RestorationDebt(BarRestorationDebt {
                address: TEST_ADDRESS,
                fault: BarRestoreFault::WriteFailed {
                    offset: BAR0_OFFSET + 4,
                },
                restored_bar_mask: ALL_BARS_RESTORED & !(1 << 1),
                command_state: BarCommandState::DecodeDisabled,
            }))
        );
        assert_eq!(configuration.values[1] as u16, 0);
        assert_eq!(configuration.values[4], bars[0]);
        assert_eq!(configuration.restore_order, [5, 4, 3, 2, 1, 0]);
        assert!(!configuration.bar_write_while_active);
    }

    #[test]
    fn rejects_a_64_bit_bar_without_a_high_dword() {
        let mut bars = [0_u32; BAR_COUNT];
        bars[5] = 0x8000_0004;
        let mut configuration = ModelConfiguration::with_bars(bars, [0; BAR_COUNT]);

        assert_eq!(
            probe_bar_lengths(&mut configuration),
            Err(BarProbeError::Truncated64BitBar { index: 5 })
        );
        assert!(configuration.bars_restored());
        assert_eq!(configuration.committed_command(), 0);
    }

    #[test]
    fn rejects_attribute_only_masks_instead_of_fabricating_a_length() {
        let bars = [0x0000_c001, 0, 0, 0, 0, 0];
        let masks = [1, 0, 0, 0, 0, 0];
        let mut configuration = ModelConfiguration::with_bars(bars, masks);

        assert_eq!(
            probe_bar_lengths(&mut configuration),
            Err(BarProbeError::InvalidSizeMask { index: 0 })
        );
        assert!(configuration.bars_restored());
        assert_eq!(configuration.committed_command(), 0);
    }

    #[test]
    fn bar0_lease_derives_only_contained_nonempty_ranges() {
        let bars = [0x8000_0004, 1, 0, 0, 0, 0];
        let masks = [0xffff_f004, 0xffff_ffff, 0, 0, 0, 0];
        let mut configuration = ModelConfiguration::with_bars(bars, masks);

        let (_, lease) =
            super::probe_bar_lengths(&mut configuration, TEST_ADDRESS, true, None).unwrap();
        let lease = lease.expect("BAR0 lease requested");

        assert_eq!(lease.address(), TEST_ADDRESS);
        assert_eq!(lease.physical_base(), 0x0000_0001_8000_0000);
        assert_eq!(lease.length(), 0x1000);
        let range = lease.checked_range(0x180, 0x280).unwrap();
        assert_eq!(range.physical_address(), 0x0000_0001_8000_0180);
        assert_eq!(range.length(), 0x280);
        assert_eq!(
            lease.checked_range(0, 0),
            Err(BarApertureBoundsError::Empty)
        );
        assert_eq!(
            lease.checked_range(0xf00, 0x101),
            Err(BarApertureBoundsError::OutsideAperture)
        );
        assert_eq!(configuration.committed_command(), COMMAND_MEMORY_SPACE);
        assert!(!configuration.bar_verify_while_active);
    }

    #[test]
    fn invalid_bar0_never_reenables_memory_decode() {
        let bars = [0x8000_0800, 0, 0, 0, 0, 0];
        let masks = [0xffff_f000, 0, 0, 0, 0, 0];
        let mut configuration = ModelConfiguration::with_bars(bars, masks);

        assert_eq!(
            super::probe_bar_lengths(&mut configuration, TEST_ADDRESS, true, None),
            Err(BarProbeError::Bar0Misaligned {
                base: 0x8000_0800,
                length: 0x1000,
            })
        );
        assert!(configuration.bars_restored());
        assert_eq!(configuration.committed_command(), 0);
    }

    #[test]
    fn final_command_write_failure_returns_unknown_restoration_debt() {
        let bars = [0x8000_0000, 0, 0, 0, 0, 0];
        let masks = [0xffff_f000, 0, 0, 0, 0, 0];
        let mut configuration = ModelConfiguration::with_bars(bars, masks);
        configuration.fail_command_write = Some(1);

        assert_eq!(
            probe_bar_lengths(&mut configuration),
            Err(BarProbeError::RestorationDebt(BarRestorationDebt {
                address: TEST_ADDRESS,
                fault: BarRestoreFault::WriteFailed {
                    offset: COMMAND_OFFSET,
                },
                restored_bar_mask: ALL_BARS_RESTORED,
                command_state: BarCommandState::Unknown,
            }))
        );
        assert!(configuration.bars_restored());
        assert_eq!(configuration.committed_command(), 0);
    }

    #[test]
    fn successful_commit_preserves_nontransaction_command_controls() {
        let bars = [0x8000_0000, 0, 0, 0, 0, 0];
        let masks = [0xffff_f000, 0, 0, 0, 0, 0];
        let mut configuration = ModelConfiguration::with_bars(bars, masks);
        configuration.values[1] = 0x0407;
        configuration.original[1] = 0x0407;

        probe_bar_lengths(&mut configuration).unwrap();

        assert_eq!(configuration.committed_command(), 0x0402);
        assert!(configuration.bars_restored());
    }

    #[test]
    fn retained_configuration_drift_causes_zero_writes() {
        let bars = [0x8000_0000, 0, 0, 0, 0, 0];
        let masks = [0xffff_f000, 0, 0, 0, 0, 0];
        let expected = expected_configuration(bars);

        let mut identity_drift = ModelConfiguration::with_bars(bars, masks);
        identity_drift.values[0] = 0x5678_4321;
        assert_eq!(
            super::probe_bar_lengths(&mut identity_drift, TEST_ADDRESS, true, Some(&expected),),
            Err(BarProbeError::ConfigurationChanged { offset: 0x00 })
        );
        assert_eq!(identity_drift.command_write_count, 0);
        assert_eq!(identity_drift.restore_count, 0);

        let mut class_drift = ModelConfiguration::with_bars(bars, masks);
        class_drift.values[2] = 0x0c03_3001;
        assert_eq!(
            super::probe_bar_lengths(&mut class_drift, TEST_ADDRESS, true, Some(&expected),),
            Err(BarProbeError::ConfigurationChanged { offset: 0x08 })
        );
        assert_eq!(class_drift.command_write_count, 0);
        assert_eq!(class_drift.restore_count, 0);

        let mut header_drift = ModelConfiguration::with_bars(bars, masks);
        header_drift.values[3] = 0x0080_0000;
        assert_eq!(
            super::probe_bar_lengths(&mut header_drift, TEST_ADDRESS, true, Some(&expected),),
            Err(BarProbeError::ConfigurationChanged {
                offset: HEADER_OFFSET,
            })
        );
        assert_eq!(header_drift.command_write_count, 0);
        assert_eq!(header_drift.restore_count, 0);

        let mut command_drift = ModelConfiguration::with_bars(bars, masks);
        command_drift.values[1] = u32::from(COMMAND_MEMORY_SPACE);
        assert_eq!(
            super::probe_bar_lengths(&mut command_drift, TEST_ADDRESS, true, Some(&expected),),
            Err(BarProbeError::ConfigurationChanged {
                offset: COMMAND_OFFSET,
            })
        );
        assert_eq!(command_drift.command_write_count, 0);
        assert_eq!(command_drift.restore_count, 0);

        let mut bar_drift = ModelConfiguration::with_bars(bars, masks);
        bar_drift.values[9] = 0x1000;
        assert_eq!(
            super::probe_bar_lengths(&mut bar_drift, TEST_ADDRESS, true, Some(&expected)),
            Err(BarProbeError::ConfigurationChanged {
                offset: bar_offset(5),
            })
        );
        assert_eq!(bar_drift.command_write_count, 0);
        assert_eq!(bar_drift.restore_count, 0);
        assert!(!bar_drift.sizing_started);
    }
}
