use core::ffi::c_void;
use core::ptr::NonNull;

use sisyphus_driver_abi::{
    STATUS_ABI_MISMATCH, STATUS_INVALID_ARGUMENT, STATUS_NOT_FOUND, STATUS_OK, Status,
};

use crate::sync::SpinLock;

pub const NETWORK_ABI_VERSION: u32 = 1;

pub type TransmitFn = unsafe extern "C" fn(
    device_context: *mut c_void,
    packet: *const u8,
    packet_length: usize,
) -> Status;

pub type ReceiveFn = unsafe extern "C" fn(
    device_context: *mut c_void,
    packet: *mut u8,
    packet_capacity: usize,
    out_packet_length: *mut usize,
) -> Status;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CNetworkDevice {
    pub abi_version: u32,
    pub struct_size: u32,
    pub device_context: *mut c_void,
    pub mac_address: [u8; 6],
    pub reserved: [u8; 2],
    pub maximum_transmission_unit: u32,
    pub transmit: Option<TransmitFn>,
    pub receive: Option<ReceiveFn>,
}

pub trait NetworkDevice: Send + Sync {
    fn mac_address(&self) -> [u8; 6];
    fn maximum_transmission_unit(&self) -> usize;
    fn transmit(&self, packet: &[u8]) -> Status;
    fn receive(&self, packet: &mut [u8]) -> Result<Option<usize>, Status>;
}

pub struct DynamicCDriverShim {
    descriptor: CNetworkDevice,
    callback_lock: SpinLock<()>,
}

impl DynamicCDriverShim {
    /// Validates and wraps a C network-device function table.
    ///
    /// # Safety
    ///
    /// The device context and callback code must remain valid until this shim
    /// is dropped. Each callback must tolerate serialized calls from any CPU.
    pub unsafe fn new(descriptor: CNetworkDevice) -> Result<Self, Status> {
        if descriptor.abi_version != NETWORK_ABI_VERSION
            || descriptor.struct_size < core::mem::size_of::<CNetworkDevice>() as u32
        {
            return Err(STATUS_ABI_MISMATCH);
        }
        if NonNull::new(descriptor.device_context).is_none()
            || descriptor.transmit.is_none()
            || descriptor.receive.is_none()
            || descriptor.maximum_transmission_unit == 0
        {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        Ok(Self {
            descriptor,
            callback_lock: SpinLock::new(()),
        })
    }
}

// SAFETY: Construction requires a stable context and callbacks that may be
// called from any CPU. callback_lock serializes every C callback invocation.
unsafe impl Send for DynamicCDriverShim {}
unsafe impl Sync for DynamicCDriverShim {}

impl NetworkDevice for DynamicCDriverShim {
    fn mac_address(&self) -> [u8; 6] {
        self.descriptor.mac_address
    }

    fn maximum_transmission_unit(&self) -> usize {
        self.descriptor.maximum_transmission_unit as usize
    }

    fn transmit(&self, packet: &[u8]) -> Status {
        if packet.is_empty() || packet.len() > self.maximum_transmission_unit() {
            return STATUS_INVALID_ARGUMENT;
        }
        let _lock = self.callback_lock.lock();
        let transmit = self
            .descriptor
            .transmit
            .expect("validated network transmit callback");
        unsafe {
            transmit(
                self.descriptor.device_context,
                packet.as_ptr(),
                packet.len(),
            )
        }
    }

    fn receive(&self, packet: &mut [u8]) -> Result<Option<usize>, Status> {
        if packet.is_empty() {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        let _lock = self.callback_lock.lock();
        let receive = self
            .descriptor
            .receive
            .expect("validated network receive callback");
        let mut packet_length = 0;
        let status = unsafe {
            receive(
                self.descriptor.device_context,
                packet.as_mut_ptr(),
                packet.len(),
                &mut packet_length,
            )
        };
        if status == STATUS_NOT_FOUND {
            return Ok(None);
        }
        if status != STATUS_OK {
            return Err(status);
        }
        if packet_length > packet.len() {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        Ok(Some(packet_length))
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    unsafe extern "C" fn transmit(
        context: *mut c_void,
        _packet: *const u8,
        packet_length: usize,
    ) -> Status {
        let counter = unsafe { &*context.cast::<AtomicUsize>() };
        counter.store(packet_length, Ordering::Release);
        STATUS_OK
    }

    unsafe extern "C" fn receive(
        _context: *mut c_void,
        packet: *mut u8,
        packet_capacity: usize,
        out_packet_length: *mut usize,
    ) -> Status {
        if packet_capacity < 3 {
            return STATUS_INVALID_ARGUMENT;
        }
        unsafe {
            packet.copy_from_nonoverlapping([1_u8, 2, 3].as_ptr(), 3);
            out_packet_length.write(3);
        }
        STATUS_OK
    }

    #[test]
    fn validates_and_serializes_c_network_callbacks() {
        let counter = AtomicUsize::new(0);
        let descriptor = CNetworkDevice {
            abi_version: NETWORK_ABI_VERSION,
            struct_size: core::mem::size_of::<CNetworkDevice>() as u32,
            device_context: core::ptr::addr_of!(counter) as *mut c_void,
            mac_address: [2, 0, 0, 0, 0, 1],
            reserved: [0; 2],
            maximum_transmission_unit: 1500,
            transmit: Some(transmit),
            receive: Some(receive),
        };
        let device = unsafe { DynamicCDriverShim::new(descriptor) }.unwrap();

        assert_eq!(device.transmit(&[0; 64]), STATUS_OK);
        assert_eq!(counter.load(Ordering::Acquire), 64);
        let mut packet = [0_u8; 8];
        assert_eq!(device.receive(&mut packet), Ok(Some(3)));
        assert_eq!(&packet[..3], &[1, 2, 3]);
    }
}
