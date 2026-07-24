use core::ffi::c_void;
use sisyphus_driver_abi::{
    ABI_MAJOR, ABI_VERSION, CAP_ALLOC, CAP_CLOCK, CAP_DEVICE_PUBLISH, CAP_DMA, CAP_IRQ, CAP_LOG,
    CAP_MMIO, CAP_SLEEP, DeviceInfo, DriverDescriptor, DriverEntryFn, KernelApi, STATUS_OK, Status,
    abi_major,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DriverLoadError {
    EntryFailed(Status),
    KernelAbiMismatch { kernel: u32, supported: u32 },
    KernelApiTooSmall,
    InconsistentKernelApi(u64),
    AbiMismatch { driver: u32, kernel: u32 },
    DescriptorTooSmall,
    MissingCapability(u64),
    InvalidName,
    MissingProbe,
    InstanceNotLive,
    ProbeFailed(Status),
    RemoveFailed(Status),
}

pub struct DriverModule {
    descriptor: DriverDescriptor,
}

pub struct DriverInstance {
    pointer: *mut c_void,
    live: bool,
}

impl DriverModule {
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
        validate_kernel_api(api)?;
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

    pub fn probe_with_api(
        &self,
        api: &KernelApi,
        device: &DeviceInfo,
    ) -> Result<DriverInstance, DriverLoadError> {
        validate_kernel_api(api)?;
        let probe = self.descriptor.probe.ok_or(DriverLoadError::MissingProbe)?;
        let mut instance = core::ptr::null_mut();
        // SAFETY: The descriptor was validated at load and all pointers refer to
        // live values for the duration of this call.
        let status = unsafe { probe(self.descriptor.driver_context, api, device, &mut instance) };
        if status == STATUS_OK {
            Ok(DriverInstance {
                pointer: instance,
                live: true,
            })
        } else {
            Err(DriverLoadError::ProbeFailed(status))
        }
    }

    pub fn remove_with_api(
        &self,
        api: &KernelApi,
        device: &DeviceInfo,
        instance: &mut DriverInstance,
    ) -> Result<(), DriverLoadError> {
        if !instance.live {
            return Err(DriverLoadError::InstanceNotLive);
        }
        validate_kernel_api(api)?;
        let Some(remove) = self.descriptor.remove else {
            instance.live = false;
            return Ok(());
        };
        // SAFETY: The instance came from this driver's probe callback and the
        // device/API values remain valid throughout the call.
        let status = unsafe { remove(instance.pointer, api, device) };
        if status == STATUS_OK {
            instance.live = false;
            Ok(())
        } else {
            Err(DriverLoadError::RemoveFailed(status))
        }
    }
}

fn validate_kernel_api(api: &KernelApi) -> Result<(), DriverLoadError> {
    if abi_major(api.abi_version) != ABI_MAJOR {
        return Err(DriverLoadError::KernelAbiMismatch {
            kernel: api.abi_version,
            supported: ABI_VERSION,
        });
    }
    if api.struct_size < core::mem::size_of::<KernelApi>() as u32 {
        return Err(DriverLoadError::KernelApiTooSmall);
    }

    let mut inconsistent = 0;
    inconsistent |= mismatch(api, CAP_LOG, api.log.is_some(), api.log.is_some());
    inconsistent |= mismatch(
        api,
        CAP_ALLOC,
        api.alloc.is_some() || api.dealloc.is_some(),
        api.alloc.is_some() && api.dealloc.is_some(),
    );
    inconsistent |= mismatch(
        api,
        CAP_CLOCK,
        api.monotonic_ns.is_some(),
        api.monotonic_ns.is_some(),
    );
    inconsistent |= mismatch(
        api,
        CAP_SLEEP,
        api.sleep_ns.is_some(),
        api.sleep_ns.is_some(),
    );
    inconsistent |= mismatch(
        api,
        CAP_MMIO,
        api.mmio_map.is_some() || api.mmio_unmap.is_some(),
        api.mmio_map.is_some() && api.mmio_unmap.is_some(),
    );
    inconsistent |= mismatch(
        api,
        CAP_DMA,
        api.dma_alloc.is_some() || api.dma_free.is_some(),
        api.dma_alloc.is_some() && api.dma_free.is_some(),
    );
    inconsistent |= mismatch(
        api,
        CAP_IRQ,
        api.irq_register.is_some() || api.irq_set_enabled.is_some() || api.irq_unregister.is_some(),
        api.irq_register.is_some() && api.irq_set_enabled.is_some() && api.irq_unregister.is_some(),
    );
    inconsistent |= mismatch(
        api,
        CAP_DEVICE_PUBLISH,
        api.device_publish.is_some() || api.device_remove.is_some(),
        api.device_publish.is_some() && api.device_remove.is_some(),
    );
    if inconsistent == 0 {
        Ok(())
    } else {
        Err(DriverLoadError::InconsistentKernelApi(inconsistent))
    }
}

fn mismatch(api: &KernelApi, capability: u64, any_callback: bool, all_callbacks: bool) -> u64 {
    let advertised = api.capabilities & capability != 0;
    if (advertised && !all_callbacks) || (!advertised && any_callback) {
        capability
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use sisyphus_driver_abi::{
        ABI_VERSION, BUS_PLATFORM, CAP_IRQ, CAP_LOG, DeviceInfo, DriverDescriptor, KernelApi,
        STATUS_BUSY, STATUS_OK,
    };

    use super::{DriverLoadError, DriverModule};

    static REMOVE_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
    static REJECTED_ENTRY_CALLS: AtomicUsize = AtomicUsize::new(0);
    static BOUNDARY_PROBE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static BOUNDARY_REMOVE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static INSTANCE_TOKEN: u8 = 0;
    static DRIVER_NAME: &[u8] = b"retrying-remove";

    unsafe extern "C" fn entry(
        _api: *const KernelApi,
        out_driver: *mut DriverDescriptor,
        out_driver_size: usize,
    ) -> i32 {
        if out_driver.is_null() || out_driver_size < core::mem::size_of::<DriverDescriptor>() {
            return sisyphus_driver_abi::STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: The caller supplied and sized the writable descriptor above.
        unsafe {
            out_driver.write(DriverDescriptor {
                abi_version: ABI_VERSION,
                struct_size: core::mem::size_of::<DriverDescriptor>() as u32,
                driver_version: 1,
                required_capabilities: 0,
                name: DRIVER_NAME.as_ptr(),
                name_len: DRIVER_NAME.len(),
                driver_context: core::ptr::null_mut(),
                probe: Some(probe),
                remove: Some(remove),
                suspend: None,
                resume: None,
            });
        }
        STATUS_OK
    }

    unsafe extern "C" fn rejected_entry(
        _api: *const KernelApi,
        _out_driver: *mut DriverDescriptor,
        _out_driver_size: usize,
    ) -> i32 {
        REJECTED_ENTRY_CALLS.fetch_add(1, Ordering::Relaxed);
        STATUS_OK
    }

    unsafe extern "C" fn boundary_entry(
        _api: *const KernelApi,
        out_driver: *mut DriverDescriptor,
        out_driver_size: usize,
    ) -> i32 {
        if out_driver.is_null() || out_driver_size < core::mem::size_of::<DriverDescriptor>() {
            return sisyphus_driver_abi::STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: The caller supplied and sized the writable descriptor above.
        unsafe {
            out_driver.write(DriverDescriptor {
                abi_version: ABI_VERSION,
                struct_size: core::mem::size_of::<DriverDescriptor>() as u32,
                driver_version: 1,
                required_capabilities: 0,
                name: DRIVER_NAME.as_ptr(),
                name_len: DRIVER_NAME.len(),
                driver_context: core::ptr::null_mut(),
                probe: Some(boundary_probe),
                remove: Some(boundary_remove),
                suspend: None,
                resume: None,
            });
        }
        STATUS_OK
    }

    unsafe extern "C" fn boundary_probe(
        _driver_context: *mut c_void,
        _api: *const KernelApi,
        _device: *const DeviceInfo,
        out_instance: *mut *mut c_void,
    ) -> i32 {
        BOUNDARY_PROBE_CALLS.fetch_add(1, Ordering::Relaxed);
        if out_instance.is_null() {
            return sisyphus_driver_abi::STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: The loader supplies a writable output slot for the callback.
        unsafe {
            out_instance.write(core::ptr::addr_of!(INSTANCE_TOKEN).cast_mut().cast());
        }
        STATUS_OK
    }

    unsafe extern "C" fn boundary_remove(
        _instance: *mut c_void,
        _api: *const KernelApi,
        _device: *const DeviceInfo,
    ) -> i32 {
        BOUNDARY_REMOVE_CALLS.fetch_add(1, Ordering::Relaxed);
        STATUS_OK
    }

    unsafe extern "C" fn test_log(
        _kernel_context: *mut c_void,
        _level: u32,
        _message: *const u8,
        _message_len: usize,
    ) -> i32 {
        STATUS_OK
    }

    unsafe extern "C" fn probe(
        _driver_context: *mut c_void,
        _api: *const KernelApi,
        _device: *const DeviceInfo,
        out_instance: *mut *mut c_void,
    ) -> i32 {
        if out_instance.is_null() {
            return sisyphus_driver_abi::STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: The loader supplies a writable output slot for the callback.
        unsafe {
            out_instance.write(core::ptr::addr_of!(INSTANCE_TOKEN).cast_mut().cast());
        }
        STATUS_OK
    }

    unsafe extern "C" fn remove(
        _instance: *mut c_void,
        _api: *const KernelApi,
        _device: *const DeviceInfo,
    ) -> i32 {
        if REMOVE_ATTEMPTS.fetch_add(1, Ordering::Relaxed) == 0 {
            STATUS_BUSY
        } else {
            STATUS_OK
        }
    }

    fn device() -> DeviceInfo {
        DeviceInfo {
            struct_size: core::mem::size_of::<DeviceInfo>() as u32,
            bus_type: BUS_PLATFORM,
            kernel_handle: 1,
            vendor_id: 0,
            device_id: 0,
            subsystem_vendor_id: 0,
            subsystem_device_id: 0,
            class_code: 0,
            revision: 0,
            address: core::ptr::null(),
            address_len: 0,
        }
    }

    #[test]
    fn failed_remove_preserves_instance_for_retry() {
        REMOVE_ATTEMPTS.store(0, Ordering::Relaxed);
        let services = super::super::DriverServices::new();
        let host = super::super::DriverHost::new(&services);
        let api = host.api();
        // SAFETY: The test entry and all descriptor storage are static.
        let module = unsafe { DriverModule::load_with_api(entry, api) }.expect("driver loads");
        let device = device();
        let mut instance = module.probe_with_api(api, &device).expect("driver probes");

        assert_eq!(
            module.remove_with_api(api, &device, &mut instance),
            Err(DriverLoadError::RemoveFailed(STATUS_BUSY))
        );
        assert_eq!(REMOVE_ATTEMPTS.load(Ordering::Relaxed), 1);

        module
            .remove_with_api(api, &device, &mut instance)
            .expect("live instance remains retryable");
        assert_eq!(REMOVE_ATTEMPTS.load(Ordering::Relaxed), 2);
        assert_eq!(
            module.remove_with_api(api, &device, &mut instance),
            Err(DriverLoadError::InstanceNotLive)
        );
        assert_eq!(REMOVE_ATTEMPTS.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn inconsistent_capabilities_are_rejected_before_driver_entry() {
        REJECTED_ENTRY_CALLS.store(0, Ordering::Relaxed);
        let services = super::super::DriverServices::new();
        let host = super::super::DriverHost::new(&services);

        let mut api = *host.api();
        api.capabilities = CAP_IRQ;
        assert_eq!(
            unsafe { DriverModule::load_with_api(rejected_entry, &api) }.err(),
            Some(DriverLoadError::InconsistentKernelApi(CAP_IRQ))
        );

        api = *host.api();
        api.log = Some(test_log);
        assert_eq!(
            unsafe { DriverModule::load_with_api(rejected_entry, &api) }.err(),
            Some(DriverLoadError::InconsistentKernelApi(CAP_LOG))
        );
        assert_eq!(REJECTED_ENTRY_CALLS.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn invalid_kernel_table_header_is_rejected_before_driver_entry() {
        REJECTED_ENTRY_CALLS.store(0, Ordering::Relaxed);
        let services = super::super::DriverServices::new();
        let host = super::super::DriverHost::new(&services);

        let mut api = *host.api();
        api.abi_version = ABI_VERSION + (1 << 16);
        assert_eq!(
            unsafe { DriverModule::load_with_api(rejected_entry, &api) }.err(),
            Some(DriverLoadError::KernelAbiMismatch {
                kernel: api.abi_version,
                supported: ABI_VERSION,
            })
        );

        api = *host.api();
        api.struct_size = (core::mem::size_of::<KernelApi>() - 1) as u32;
        assert_eq!(
            unsafe { DriverModule::load_with_api(rejected_entry, &api) }.err(),
            Some(DriverLoadError::KernelApiTooSmall)
        );
        assert_eq!(REJECTED_ENTRY_CALLS.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn inconsistent_api_never_reaches_lifecycle_callbacks() {
        BOUNDARY_PROBE_CALLS.store(0, Ordering::Relaxed);
        BOUNDARY_REMOVE_CALLS.store(0, Ordering::Relaxed);
        let services = super::super::DriverServices::new();
        let host = super::super::DriverHost::new(&services);
        let module = unsafe { DriverModule::load_with_api(boundary_entry, host.api()) }
            .expect("valid host loads driver");
        let device = device();
        let mut inconsistent_api = *host.api();
        inconsistent_api.capabilities = CAP_LOG;

        assert_eq!(
            module.probe_with_api(&inconsistent_api, &device).err(),
            Some(DriverLoadError::InconsistentKernelApi(CAP_LOG))
        );
        assert_eq!(BOUNDARY_PROBE_CALLS.load(Ordering::Relaxed), 0);

        let mut instance = module
            .probe_with_api(host.api(), &device)
            .expect("valid host probes driver");
        assert_eq!(BOUNDARY_PROBE_CALLS.load(Ordering::Relaxed), 1);
        assert_eq!(
            module.remove_with_api(&inconsistent_api, &device, &mut instance),
            Err(DriverLoadError::InconsistentKernelApi(CAP_LOG))
        );
        assert_eq!(BOUNDARY_REMOVE_CALLS.load(Ordering::Relaxed), 0);
        module
            .remove_with_api(host.api(), &device, &mut instance)
            .expect("failed boundary check preserves live instance");
        assert_eq!(BOUNDARY_REMOVE_CALLS.load(Ordering::Relaxed), 1);
    }
}
