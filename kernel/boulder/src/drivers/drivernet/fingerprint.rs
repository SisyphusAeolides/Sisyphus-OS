use crate::hw::pci::{self, PciDevice, PciInventory};
use sisyphus_driver_abi::hermes::HermesPciIdentity;

pub const PCI_CLASS_DISPLAY: u8 = 0x03;
pub const VENDOR_NVIDIA: u16 = 0x10de;
pub const VENDOR_AMD: u16 = 0x1002;
pub const VENDOR_INTEL: u16 = 0x8086;
pub const VENDOR_QUALCOMM: u16 = 0x17cb;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GpuFingerprint {
    pub bus: u8,
    pub slot: u8,
    pub function: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub revision: u8,
    pub class_code: u8,
    pub subclass: u8,
    pub programming_interface: u8,
    pub interrupt_line: u8,
    pub interrupt_pin: u8,
    /// BAR lengths in bytes (from `pci::bar_lengths`)
    pub bar_lengths: [u64; 6],
    pub mmio_64bit: bool,
}

impl GpuFingerprint {
    pub const fn empty() -> Self {
        Self {
            bus: 0,
            slot: 0,
            function: 0,
            vendor_id: 0xffff,
            device_id: 0xffff,
            revision: 0,
            class_code: 0,
            subclass: 0,
            programming_interface: 0,
            interrupt_line: 0xff,
            interrupt_pin: 0,
            bar_lengths: [0; 6],
            mmio_64bit: false,
        }
    }

    pub const fn bar0_mb(self) -> u32 {
        (self.bar_lengths[0] / (1024 * 1024)) as u32
    }

    pub const fn is_display(self) -> bool {
        self.class_code == PCI_CLASS_DISPLAY
    }

    /// Bridge into the Hermes ABI identity already used by hermes_gsp.
    pub const fn to_hermes_identity(self) -> HermesPciIdentity {
        HermesPciIdentity {
            segment: 0,
            bus: self.bus,
            slot: self.slot,
            function: self.function,
            revision: self.revision,
            vendor_id: self.vendor_id,
            device_id: self.device_id,
            subsystem_vendor_id: 0,
            subsystem_device_id: 0,
            class_code: self.class_code,
            subclass: self.subclass,
            programming_interface: self.programming_interface,
            reserved: 0,
        }
    }
}

pub fn fingerprint_display_devices(inventory: &PciInventory, out: &mut [GpuFingerprint]) -> usize {
    let mut count = 0_usize;
    for device in inventory.devices() {
        if device.class_code != PCI_CLASS_DISPLAY {
            continue;
        }
        if count >= out.len() {
            break;
        }
        out[count] = fingerprint_device(device);
        count += 1;
    }
    count
}

fn fingerprint_device(device: &PciDevice) -> GpuFingerprint {
    // SAFETY: Early boot, interrupts disabled, device not yet claimed by a driver.
    let bars = unsafe { pci::bar_lengths(device.address) };
    let mmio_64bit =
        bars[0] > 0x1_0000_0000 || (bars[0] != 0 && bars[1] == 0 && bars[0] > u32::MAX as u64);

    GpuFingerprint {
        bus: device.address.bus,
        slot: device.address.slot,
        function: device.address.function,
        vendor_id: device.vendor_id,
        device_id: device.device_id,
        revision: device.revision,
        class_code: device.class_code,
        subclass: device.subclass,
        programming_interface: device.programming_interface,
        interrupt_line: device.interrupt_line,
        interrupt_pin: device.interrupt_pin,
        bar_lengths: bars,
        mmio_64bit,
    }
}
