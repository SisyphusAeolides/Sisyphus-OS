use crate::arch::x86_64::{inb, inl, outb, outl};
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
const COMMAND_DECODE_AND_MASTER: u16 = 0x0007;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BarProbeError {
    ConfigurationAccessFailed { offset: u8 },
    DeviceAbsent,
    UnsupportedHeader { header_type: u8 },
    QuiescenceFailed { command: u16 },
    InvalidBarEncoding { index: u8 },
    Truncated64BitBar { index: u8 },
    InvalidSizeMask { index: u8 },
    RestoreFailed { offset: u8 },
    RestoreMismatch { offset: u8 },
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

#[derive(Clone, Copy)]
struct BarSnapshot {
    command: u16,
    bars: [u32; BAR_COUNT],
}

/// Sizes BAR0..BAR5 as one serialized, reversible type-0 configuration transaction.
/// A 64-bit BAR reports its length in the low slot and zero in its consumed high slot.
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
    probe_bar_lengths(&mut configuration)
}

fn probe_bar_lengths(
    configuration: &mut impl ConfigurationSpace,
) -> Result<[u64; BAR_COUNT], BarProbeError> {
    let vendor = read(configuration, 0x00)? as u16;
    if vendor == INVALID_VENDOR_ID {
        return Err(BarProbeError::DeviceAbsent);
    }
    let header_type = (read(configuration, HEADER_OFFSET)? >> 16) as u8;
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
    let snapshot = BarSnapshot { command, bars };
    let mut bars_may_be_modified = false;

    let probe_result = (|| {
        configuration
            .write_command(command & !COMMAND_DECODE_AND_MASTER)
            .map_err(|()| BarProbeError::ConfigurationAccessFailed {
                offset: COMMAND_OFFSET,
            })?;
        let disabled = configuration.read_command().map_err(|()| {
            BarProbeError::ConfigurationAccessFailed {
                offset: COMMAND_OFFSET,
            }
        })?;
        if disabled & COMMAND_DECODE_AND_MASTER != 0 {
            return Err(BarProbeError::QuiescenceFailed { command: disabled });
        }
        bars_may_be_modified = true;
        size_bars(configuration, &snapshot)
    })();

    match restore_snapshot(configuration, &snapshot, bars_may_be_modified) {
        Ok(()) => probe_result,
        Err(error) => Err(error),
    }
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

fn restore_snapshot(
    configuration: &mut impl ConfigurationSpace,
    snapshot: &BarSnapshot,
    bars_may_be_modified: bool,
) -> Result<(), BarProbeError> {
    let mut failure = None;
    if bars_may_be_modified {
        // Reverse order restores the high half of every 64-bit BAR before its
        // low half can make the pair observable again.
        for index in (0..BAR_COUNT).rev() {
            let offset = bar_offset(index);
            if configuration
                .write_u32(offset, snapshot.bars[index])
                .is_err()
                && failure.is_none()
            {
                failure = Some(BarProbeError::RestoreFailed { offset });
            }
        }
    }
    if configuration.write_command(snapshot.command).is_err() && failure.is_none() {
        failure = Some(BarProbeError::RestoreFailed {
            offset: COMMAND_OFFSET,
        });
    }
    for (index, original) in snapshot.bars.iter().copied().enumerate() {
        let offset = bar_offset(index);
        match configuration.read_u32(offset) {
            Ok(observed) if observed == original => {}
            Ok(_) if failure.is_none() => {
                failure = Some(BarProbeError::RestoreMismatch { offset });
            }
            Err(()) if failure.is_none() => {
                failure = Some(BarProbeError::RestoreFailed { offset });
            }
            _ => {}
        }
    }
    match configuration.read_command() {
        Ok(observed) if observed == snapshot.command => {}
        Ok(_) if failure.is_none() => {
            failure = Some(BarProbeError::RestoreMismatch {
                offset: COMMAND_OFFSET,
            });
        }
        Err(()) if failure.is_none() => {
            failure = Some(BarProbeError::RestoreFailed {
                offset: COMMAND_OFFSET,
            });
        }
        _ => {}
    }
    failure.map_or(Ok(()), Err)
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

    struct ModelConfiguration {
        values: [u32; 16],
        original: [u32; 16],
        masks: [u32; BAR_COUNT],
        fail_probe_read: Option<usize>,
        fail_restore_write: Option<usize>,
        bar_write_while_active: bool,
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
                bar_write_while_active: false,
                restore_order: [u8::MAX; BAR_COUNT],
                restore_count: 0,
            }
        }

        fn restored(&self) -> bool {
            self.values == self.original
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
        assert!(configuration.restored());
        assert!(!configuration.bar_write_while_active);
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
        assert!(configuration.restored());
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
        assert!(configuration.restored());
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
            Err(BarProbeError::RestoreFailed {
                offset: BAR0_OFFSET + 4,
            })
        );
        assert_eq!(configuration.values[1] as u16, COMMAND_DECODE_AND_MASTER);
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
        assert!(configuration.restored());
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
        assert!(configuration.restored());
    }
}
