use crate::arch::x86_64::{inl, outl};
use crate::sync::SpinLock;

const CONFIG_ADDRESS: u16 = 0x0cf8;
const CONFIG_DATA: u16 = 0x0cfc;
const INVALID_VENDOR_ID: u16 = 0xffff;
const MULTIFUNCTION: u8 = 1 << 7;
const MAXIMUM_DEVICES: usize = 256;

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
    let vendor_device = unsafe { read_configuration_u32(address, 0) };
    let vendor_id = vendor_device as u16;
    if vendor_id == INVALID_VENDOR_ID {
        return None;
    }
    let class_revision = unsafe { read_configuration_u32(address, 0x08) };
    let header = unsafe { read_configuration_u32(address, 0x0c) };
    let interrupt = unsafe { read_configuration_u32(address, 0x3c) };
    Some(decode_device(
        address,
        vendor_device,
        class_revision,
        header,
        interrupt,
    ))
}

unsafe fn read_configuration_u32(address: PciAddress, offset: u8) -> u32 {
    let _access = CONFIGURATION_ACCESS.lock();
    unsafe {
        outl(CONFIG_ADDRESS, address.configuration_address(offset));
        inl(CONFIG_DATA)
    }
}

fn decode_device(
    address: PciAddress,
    vendor_device: u32,
    class_revision: u32,
    header: u32,
    interrupt: u32,
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
    }
}
/// Read a 32-bit PCI config dword. Safe wrapper over the locked CF8/CFC path.
pub unsafe fn read_config_u32(address: PciAddress, offset: u8) -> u32 {
    unsafe { read_configuration_u32(address, offset) }
}

/// Write a 32-bit PCI config dword under the global configuration lock.
pub unsafe fn write_config_u32(address: PciAddress, offset: u8, value: u32) {
    let _access = CONFIGURATION_ACCESS.lock();
    unsafe {
        outl(CONFIG_ADDRESS, address.configuration_address(offset));
        outl(CONFIG_DATA, value);
    }
}

/// Probe BAR sizes for a type-0 header without permanently destroying BARs.
///
/// Returns lengths in bytes for BAR0..BAR5. 64-bit BARs report size in the
/// low slot and 0 in the high slot. I/O BARs report their I/O window size.
///
/// # Safety
/// Device must be quiescent. No concurrent MMIO through these BARs.
pub unsafe fn bar_lengths(address: PciAddress) -> [u64; 6] {
    let mut lengths = [0_u64; 6];
    let mut index = 0_usize;
    while index < 6 {
        let offset = 0x10 + (index as u8) * 4;
        let original = unsafe { read_configuration_u32(address, offset) };
        unsafe { write_config_u32(address, offset, 0xffff_ffff) };
        let probed = unsafe { read_configuration_u32(address, offset) };
        unsafe { write_config_u32(address, offset, original) };

        if probed == 0 || probed == 0xffff_ffff {
            index += 1;
            continue;
        }

        let is_io = original & 1 != 0;
        if is_io {
            let mask = probed & 0xffff_fffc;
            lengths[index] = (!mask).wrapping_add(1) as u64 & 0xffff;
            index += 1;
            continue;
        }

        let mem_type = (original >> 1) & 0b11;
        if mem_type == 0b10 && index + 1 < 6 {
            // 64-bit memory BAR
            let offset_hi = offset + 4;
            let original_hi = unsafe { read_configuration_u32(address, offset_hi) };
            unsafe { write_config_u32(address, offset_hi, 0xffff_ffff) };
            let probed_hi = unsafe { read_configuration_u32(address, offset_hi) };
            unsafe { write_config_u32(address, offset_hi, original_hi) };

            let low = (probed & 0xffff_fff0) as u64;
            let high = (probed_hi as u64) << 32;
            let mask = low | high;
            lengths[index] = (!mask).wrapping_add(1);
            lengths[index + 1] = 0;
            index += 2;
        } else {
            let mask = probed & 0xffff_fff0;
            lengths[index] = (!mask).wrapping_add(1) as u64;
            index += 1;
        }
    }
    lengths
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let device = decode_device(address, 0x5678_1234, 0x0200_01a2, 0x0080_0000, 0x0000_010b);

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
}
