use core::ffi::c_void;
use sisyphus_driver_abi::{
    ABI_MAJOR, DeviceInfo, DriverDescriptor, DriverEntryFn, KernelApi, STATUS_OK, Status, abi_major,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DriverLoadError {
    EntryFailed(Status),
    AbiMismatch { driver: u32, kernel: u32 },
    DescriptorTooSmall,
    MissingCapability(u64),
    InvalidName,
    MissingProbe,
    ProbeFailed(Status),
    RemoveFailed(Status),
}

pub struct DriverModule {
    descriptor: DriverDescriptor,
}

pub struct DriverInstance {
    pointer: *mut c_void,
}

impl DriverModule {
    /// Loads and validates a driver entry point against Boulder's kernel API.
    ///
    /// # Safety
    ///
    /// `entry` must implement the Sisyphus driver contract. Every pointer
    /// published through its descriptor must remain valid until unload.
    pub unsafe fn load(entry: DriverEntryFn) -> Result<Self, DriverLoadError> {
        unsafe { Self::load_with_api(entry, super::kernel_api()) }
    }

    /// Loads and validates a driver against a specific kernel API table.
    ///
    /// # Safety
    ///
    /// `entry` and every pointer it publishes must obey the Sisyphus driver
    /// contract. `api` and its context must remain valid for every later call.
    pub unsafe fn load_with_api(
        entry: DriverEntryFn,
        api: &KernelApi,
    ) -> Result<Self, DriverLoadError> {
        let mut descriptor = DriverDescriptor::empty();
        let status = unsafe {
            entry(
                api,
                &mut descriptor,
                core::mem::size_of::<DriverDescriptor>(),
            )
        };
        if status != STATUS_OK {
            return Err(DriverLoadError::EntryFailed(status));
        }
        if abi_major(descriptor.abi_version) != ABI_MAJOR {
            return Err(DriverLoadError::AbiMismatch {
                driver: descriptor.abi_version,
                kernel: api.abi_version,
            });
        }
        if descriptor.struct_size < core::mem::size_of::<DriverDescriptor>() as u32 {
            return Err(DriverLoadError::DescriptorTooSmall);
        }

        let missing = descriptor.required_capabilities & !api.capabilities;
        if missing != 0 {
            return Err(DriverLoadError::MissingCapability(missing));
        }
        if descriptor.name.is_null() || descriptor.name_len == 0 || descriptor.name_len > 255 {
            return Err(DriverLoadError::InvalidName);
        }
        if descriptor.probe.is_none() {
            return Err(DriverLoadError::MissingProbe);
        }

        Ok(Self { descriptor })
    }

    pub fn name(&self) -> &[u8] {
        // SAFETY: A loaded driver promises that descriptor strings remain valid
        // until unload. The loader rejected null and unreasonable lengths.
        unsafe { core::slice::from_raw_parts(self.descriptor.name, self.descriptor.name_len) }
    }

    pub fn driver_version(&self) -> u64 {
        self.descriptor.driver_version
    }

    pub fn probe(&self, device: &DeviceInfo) -> Result<DriverInstance, DriverLoadError> {
        self.probe_with_api(super::kernel_api(), device)
    }

    pub fn probe_with_api(
        &self,
        api: &KernelApi,
        device: &DeviceInfo,
    ) -> Result<DriverInstance, DriverLoadError> {
        let probe = self.descriptor.probe.ok_or(DriverLoadError::MissingProbe)?;
        let mut instance = core::ptr::null_mut();
        // SAFETY: The descriptor was validated at load and all pointers refer to
        // live values for the duration of this call.
        let status = unsafe { probe(self.descriptor.driver_context, api, device, &mut instance) };
        if status == STATUS_OK {
            Ok(DriverInstance { pointer: instance })
        } else {
            Err(DriverLoadError::ProbeFailed(status))
        }
    }

    pub fn remove(
        &self,
        device: &DeviceInfo,
        instance: DriverInstance,
    ) -> Result<(), DriverLoadError> {
        self.remove_with_api(super::kernel_api(), device, instance)
    }

    pub fn remove_with_api(
        &self,
        api: &KernelApi,
        device: &DeviceInfo,
        instance: DriverInstance,
    ) -> Result<(), DriverLoadError> {
        let Some(remove) = self.descriptor.remove else {
            return Ok(());
        };
        // SAFETY: The instance came from this driver's probe callback and the
        // device/API values remain valid throughout the call.
        let status = unsafe { remove(instance.pointer, api, device) };
        if status == STATUS_OK {
            Ok(())
        } else {
            Err(DriverLoadError::RemoveFailed(status))
        }
    }
}
