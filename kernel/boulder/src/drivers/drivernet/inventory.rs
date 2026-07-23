use crate::hw::pci::PciInventory;

use super::fingerprint::{
    FingerprintError, PCI_CLASS_DISPLAY, PciConfigReader, PciFunctionAddress,
};

pub const MAXIMUM_DISPLAY_FUNCTIONS: usize = 64;
pub const MULTIFUNCTION_HEADER: u8 = 1 << 7;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciBusRange {
    pub segment: u16,
    pub bus_start: u8,
    pub bus_end: u8,
}

impl PciBusRange {
    pub const fn valid(self) -> bool {
        self.bus_start <= self.bus_end
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciFunctionRecord {
    pub address: PciFunctionAddress,
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

impl PciFunctionRecord {
    pub const EMPTY: Self = Self {
        address: PciFunctionAddress {
            segment: 0,
            bus: 0,
            slot: 0,
            function: 0,
        },
        vendor_id: 0xffff,
        device_id: 0xffff,
        class_code: 0,
        subclass: 0,
        programming_interface: 0,
        revision: 0,
        header_type: 0,
        interrupt_line: 0xff,
        interrupt_pin: 0,
    };

    pub const fn is_display(self) -> bool {
        self.class_code == PCI_CLASS_DISPLAY
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InventoryError {
    ZeroCapacity,
    Capacity,
    DuplicateAddress,
    InvalidRange,
}

pub struct DisplayFunctionInventory<const N: usize> {
    functions: [PciFunctionRecord; N],
    length: usize,
    overflowed: bool,
    configuration_faults: u32,
}

impl<const N: usize> DisplayFunctionInventory<N> {
    pub fn new() -> Result<Self, InventoryError> {
        if N == 0 {
            return Err(InventoryError::ZeroCapacity);
        }

        Ok(Self {
            functions: [PciFunctionRecord::EMPTY; N],
            length: 0,
            overflowed: false,
            configuration_faults: 0,
        })
    }

    pub fn functions(&self) -> &[PciFunctionRecord] {
        &self.functions[..self.length]
    }

    pub const fn len(&self) -> usize {
        self.length
    }

    pub const fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub const fn overflowed(&self) -> bool {
        self.overflowed
    }

    pub const fn configuration_faults(&self) -> u32 {
        self.configuration_faults
    }

    pub fn insert(&mut self, function: PciFunctionRecord) -> Result<(), InventoryError> {
        if self.functions[..self.length]
            .iter()
            .any(|existing| existing.address == function.address)
        {
            return Err(InventoryError::DuplicateAddress);
        }

        let Some(destination) = self.functions.get_mut(self.length) else {
            self.overflowed = true;
            return Err(InventoryError::Capacity);
        };

        *destination = function;
        self.length += 1;
        Ok(())
    }

    pub fn import_legacy(&mut self, inventory: &PciInventory) -> Result<(), InventoryError> {
        for device in inventory.devices() {
            if device.class_code != PCI_CLASS_DISPLAY {
                continue;
            }

            let function = PciFunctionRecord {
                address: PciFunctionAddress {
                    segment: 0,
                    bus: device.address.bus,
                    slot: device.address.slot,
                    function: device.address.function,
                },
                vendor_id: device.vendor_id,
                device_id: device.device_id,
                class_code: device.class_code,
                subclass: device.subclass,
                programming_interface: device.programming_interface,
                revision: device.revision,
                header_type: device.header_type,
                interrupt_line: device.interrupt_line,
                interrupt_pin: device.interrupt_pin,
            };

            match self.insert(function) {
                Ok(()) | Err(InventoryError::DuplicateAddress) | Err(InventoryError::Capacity) => {}
                Err(error) => return Err(error),
            }
        }

        self.overflowed |= inventory.overflowed();
        Ok(())
    }

    pub fn discover(
        &mut self,
        ranges: &[PciBusRange],
        configuration: &dyn PciConfigReader,
    ) -> Result<(), InventoryError> {
        for range in ranges.iter().copied() {
            if !range.valid() {
                return Err(InventoryError::InvalidRange);
            }

            for bus in range.bus_start..=range.bus_end {
                for slot in 0_u8..32 {
                    let address = PciFunctionAddress {
                        segment: range.segment,
                        bus,
                        slot,
                        function: 0,
                    };

                    let Some(function_zero) = self.read_function(address, configuration) else {
                        continue;
                    };

                    let multifunction = function_zero.header_type & MULTIFUNCTION_HEADER != 0;
                    self.insert_if_display(function_zero)?;

                    if multifunction {
                        for function in 1_u8..8 {
                            let address = PciFunctionAddress {
                                function,
                                ..address
                            };
                            if let Some(record) = self.read_function(address, configuration) {
                                self.insert_if_display(record)?;
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn read_function(
        &mut self,
        address: PciFunctionAddress,
        configuration: &dyn PciConfigReader,
    ) -> Option<PciFunctionRecord> {
        let vendor_device = match configuration.read_u32(address, 0x00) {
            Ok(value) => value,
            Err(_) => {
                self.configuration_faults = self.configuration_faults.saturating_add(1);
                return None;
            }
        };

        let vendor_id = vendor_device as u16;
        if vendor_id == 0xffff || vendor_id == 0 {
            return None;
        }

        let class_revision = match configuration.read_u32(address, 0x08) {
            Ok(value) => value,
            Err(_) => {
                self.configuration_faults = self.configuration_faults.saturating_add(1);
                return None;
            }
        };
        let header = match configuration.read_u32(address, 0x0c) {
            Ok(value) => value,
            Err(_) => {
                self.configuration_faults = self.configuration_faults.saturating_add(1);
                return None;
            }
        };
        let interrupt = configuration.read_u32(address, 0x3c).unwrap_or_else(|_| {
            self.configuration_faults = self.configuration_faults.saturating_add(1);
            u32::from(u8::MAX)
        });

        Some(PciFunctionRecord {
            address,
            vendor_id,
            device_id: (vendor_device >> 16) as u16,
            class_code: (class_revision >> 24) as u8,
            subclass: (class_revision >> 16) as u8,
            programming_interface: (class_revision >> 8) as u8,
            revision: class_revision as u8,
            header_type: (header >> 16) as u8,
            interrupt_line: interrupt as u8,
            interrupt_pin: (interrupt >> 8) as u8,
        })
    }

    fn insert_if_display(&mut self, function: PciFunctionRecord) -> Result<(), InventoryError> {
        if !function.is_display() {
            return Ok(());
        }

        match self.insert(function) {
            Ok(()) | Err(InventoryError::DuplicateAddress) | Err(InventoryError::Capacity) => {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
}

pub fn fingerprint_error_is_absence(error: FingerprintError) -> bool {
    matches!(
        error,
        FingerprintError::UnsupportedPciSegment | FingerprintError::UnsupportedConfigurationOffset
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Configuration {
        vendor_device: u32,
        class_revision: u32,
        header: u32,
    }

    impl PciConfigReader for Configuration {
        fn maximum_offset(&self) -> u16 {
            0x0fff
        }

        fn read_u32(
            &self,
            _address: PciFunctionAddress,
            offset: u16,
        ) -> Result<u32, FingerprintError> {
            match offset {
                0x00 if _address.slot == 0 => Ok(self.vendor_device),
                0x00 => Ok(u32::MAX),
                0x08 => Ok(self.class_revision),
                0x0c => Ok(self.header),
                0x3c => Ok(0x0000_010b),
                _ => Ok(0),
            }
        }
    }

    #[test]
    fn discovers_a_segmented_display_function() {
        let mut inventory = DisplayFunctionInventory::<4>::new().unwrap();
        let configuration = Configuration {
            vendor_device: 0x2206_10de,
            class_revision: 0x0300_0001,
            header: 0,
        };

        inventory
            .discover(
                &[PciBusRange {
                    segment: 4,
                    bus_start: 0x40,
                    bus_end: 0x40,
                }],
                &configuration,
            )
            .unwrap();

        assert_eq!(inventory.len(), 1);
        assert_eq!(inventory.functions()[0].address.segment, 4);
    }
}
