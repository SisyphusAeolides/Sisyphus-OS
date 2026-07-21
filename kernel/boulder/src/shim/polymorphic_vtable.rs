use core::ffi::c_void;
use core::ptr::NonNull;

use sisyphus_driver_abi::{
    STATUS_ABI_MISMATCH, STATUS_INVALID_ARGUMENT, STATUS_IO_ERROR, STATUS_OK, Status,
};

use crate::sync::SpinLock;

pub const DEVICE_OPERATIONS_ABI_VERSION: u32 = 1;

pub type InitializeFn = unsafe extern "C" fn(device_state: *mut c_void) -> Status;
pub type ReadFn =
    unsafe extern "C" fn(device_state: *mut c_void, buffer: *mut u8, buffer_length: usize) -> isize;
pub type WriteFn =
    unsafe extern "C" fn(device_state: *mut c_void, data: *const u8, data_length: usize) -> isize;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CDeviceOperations {
    pub abi_version: u32,
    pub struct_size: u32,
    pub initialize: Option<InitializeFn>,
    pub read: Option<ReadFn>,
    pub write: Option<WriteFn>,
}

pub trait SisyphusDevice: Send + Sync {
    fn read_data(&self, buffer: &mut [u8]) -> Result<usize, Status>;
    fn write_data(&self, data: &[u8]) -> Result<usize, Status>;
}

pub struct TransmutedDevice {
    device_state: NonNull<c_void>,
    operations: CDeviceOperations,
    callback_lock: SpinLock<()>,
}

impl TransmutedDevice {
    /// Copies, validates, and initializes a C device vtable.
    ///
    /// # Safety
    ///
    /// `device_state` and callback code must remain valid for this wrapper's
    /// lifetime. The callbacks must support serialized invocation from any CPU.
    pub unsafe fn absorb(
        device_state: *mut c_void,
        operations: *const CDeviceOperations,
    ) -> Result<Self, Status> {
        let device_state = NonNull::new(device_state).ok_or(STATUS_INVALID_ARGUMENT)?;
        let operations = NonNull::new(operations.cast_mut()).ok_or(STATUS_INVALID_ARGUMENT)?;
        // SAFETY: The caller guarantees a readable CDeviceOperations value.
        let operations = unsafe { operations.as_ptr().read() };
        if operations.abi_version != DEVICE_OPERATIONS_ABI_VERSION
            || operations.struct_size < core::mem::size_of::<CDeviceOperations>() as u32
        {
            return Err(STATUS_ABI_MISMATCH);
        }
        let initialize = operations.initialize.ok_or(STATUS_INVALID_ARGUMENT)?;
        if operations.read.is_none() || operations.write.is_none() {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        let status = unsafe { initialize(device_state.as_ptr()) };
        if status != STATUS_OK {
            return Err(status);
        }
        Ok(Self {
            device_state,
            operations,
            callback_lock: SpinLock::new(()),
        })
    }
}

// SAFETY: absorb requires callbacks valid on any CPU, and callback_lock
// serializes all access to the opaque C state.
unsafe impl Send for TransmutedDevice {}
unsafe impl Sync for TransmutedDevice {}

impl SisyphusDevice for TransmutedDevice {
    fn read_data(&self, buffer: &mut [u8]) -> Result<usize, Status> {
        if buffer.is_empty() {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        let _lock = self.callback_lock.lock();
        let read = self.operations.read.expect("validated C read callback");
        let result = unsafe {
            read(
                self.device_state.as_ptr(),
                buffer.as_mut_ptr(),
                buffer.len(),
            )
        };
        checked_length(result, buffer.len())
    }

    fn write_data(&self, data: &[u8]) -> Result<usize, Status> {
        if data.is_empty() {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        let _lock = self.callback_lock.lock();
        let write = self.operations.write.expect("validated C write callback");
        let result = unsafe { write(self.device_state.as_ptr(), data.as_ptr(), data.len()) };
        checked_length(result, data.len())
    }
}

fn checked_length(result: isize, capacity: usize) -> Result<usize, Status> {
    let length = usize::try_from(result).map_err(|_| STATUS_IO_ERROR)?;
    if length > capacity {
        Err(STATUS_IO_ERROR)
    } else {
        Ok(length)
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    unsafe extern "C" fn initialize(state: *mut c_void) -> Status {
        unsafe { &*state.cast::<AtomicUsize>() }.store(1, Ordering::Release);
        STATUS_OK
    }

    unsafe extern "C" fn read(_state: *mut c_void, buffer: *mut u8, length: usize) -> isize {
        if length < 2 {
            return -1;
        }
        unsafe { buffer.copy_from_nonoverlapping([4_u8, 2].as_ptr(), 2) };
        2
    }

    unsafe extern "C" fn write(state: *mut c_void, _data: *const u8, length: usize) -> isize {
        unsafe { &*state.cast::<AtomicUsize>() }.store(length, Ordering::Release);
        length as isize
    }

    #[test]
    fn validates_and_adapts_a_c_vtable() {
        let state = AtomicUsize::new(0);
        let operations = CDeviceOperations {
            abi_version: DEVICE_OPERATIONS_ABI_VERSION,
            struct_size: core::mem::size_of::<CDeviceOperations>() as u32,
            initialize: Some(initialize),
            read: Some(read),
            write: Some(write),
        };
        let device = unsafe {
            TransmutedDevice::absorb(
                core::ptr::addr_of!(state) as *mut c_void,
                core::ptr::addr_of!(operations),
            )
        }
        .unwrap();
        let mut buffer = [0_u8; 8];
        assert_eq!(device.read_data(&mut buffer), Ok(2));
        assert_eq!(&buffer[..2], &[4, 2]);
        assert_eq!(device.write_data(&[0; 5]), Ok(5));
        assert_eq!(state.load(Ordering::Acquire), 5);
    }
}
