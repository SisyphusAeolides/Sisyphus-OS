use core::ffi::c_void;
use core::ptr::NonNull;

use super::abi::NtStatus;

pub trait WindowsIrpBridge: Sync {
    /// Builds and dispatches a version-specific read IRP.
    ///
    /// # Safety
    ///
    /// `device_object` must be a live object belonging to the selected Windows
    /// personality. The implementation owns all layout, MDL, completion, IRQL,
    /// and pending-request semantics for that exact version.
    unsafe fn dispatch_read(
        &self,
        device_object: NonNull<c_void>,
        buffer: &mut [u8],
    ) -> Result<usize, NtStatus>;
}

pub struct WindowsDriverDevice<'a> {
    device_object: NonNull<c_void>,
    bridge: &'a dyn WindowsIrpBridge,
}

impl<'a> WindowsDriverDevice<'a> {
    /// Creates an opaque Windows device adapter.
    ///
    /// # Safety
    ///
    /// The device object must remain valid for this adapter's lifetime and
    /// must match the bridge's exact kernel version and architecture.
    pub unsafe fn new(device_object: NonNull<c_void>, bridge: &'a dyn WindowsIrpBridge) -> Self {
        Self {
            device_object,
            bridge,
        }
    }

    pub fn read(&self, buffer: &mut [u8]) -> Result<usize, NtStatus> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let completed = unsafe { self.bridge.dispatch_read(self.device_object, buffer)? };
        if completed > buffer.len() {
            Err(-1)
        } else {
            Ok(completed)
        }
    }
}

// SAFETY: Construction requires the version-specific bridge to provide all
// synchronization and lifetime guarantees for the opaque device object.
unsafe impl Send for WindowsDriverDevice<'_> {}
unsafe impl Sync for WindowsDriverDevice<'_> {}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestBridge;

    impl WindowsIrpBridge for TestBridge {
        unsafe fn dispatch_read(
            &self,
            _device_object: NonNull<c_void>,
            buffer: &mut [u8],
        ) -> Result<usize, NtStatus> {
            buffer[..2].copy_from_slice(&[1, 2]);
            Ok(2)
        }
    }

    #[test]
    fn keeps_version_specific_irp_layouts_behind_the_bridge() {
        let mut state = 0_u8;
        let pointer = NonNull::new(core::ptr::addr_of_mut!(state).cast()).unwrap();
        let device = unsafe { WindowsDriverDevice::new(pointer, &TestBridge) };
        let mut buffer = [0_u8; 4];
        assert_eq!(device.read(&mut buffer), Ok(2));
        assert_eq!(&buffer[..2], &[1, 2]);
    }
}
