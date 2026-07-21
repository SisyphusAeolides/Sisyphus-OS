use core::alloc::Layout;
use core::ffi::c_void;
use core::ptr::NonNull;
use sisyphus_driver_abi::{
    ABI_VERSION, CAP_ALLOC, CAP_CLOCK, CAP_DEVICE_PUBLISH, CAP_DMA, CAP_IRQ, CAP_LOG, CAP_MMIO,
    CAP_SLEEP, DeviceInfo, Handle, IrqHandler, KernelApi, STATUS_INVALID_ARGUMENT, STATUS_OK,
    STATUS_UNSUPPORTED, Status,
};

use super::services::{DriverServices, LogService};

pub struct DriverHost<'a> {
    api: KernelApi,
    _services: &'a DriverServices<'a>,
}

impl<'a> DriverHost<'a> {
    pub const fn new(services: &'a DriverServices<'a>) -> Self {
        let mut capabilities = 0;
        let mut api = KernelApi {
            abi_version: ABI_VERSION,
            struct_size: core::mem::size_of::<KernelApi>() as u32,
            capabilities: 0,
            kernel_context: services as *const DriverServices<'a> as *mut c_void,
            log: None,
            alloc: None,
            dealloc: None,
            monotonic_ns: None,
            sleep_ns: None,
            mmio_map: None,
            mmio_unmap: None,
            dma_alloc: None,
            dma_free: None,
            irq_register: None,
            irq_set_enabled: None,
            irq_unregister: None,
            device_publish: None,
            device_remove: None,
        };

        if services.logger.is_some() {
            capabilities |= CAP_LOG;
            api.log = Some(log);
        }
        if services.allocator.is_some() {
            capabilities |= CAP_ALLOC;
            api.alloc = Some(allocate);
            api.dealloc = Some(deallocate);
        }
        if services.clock.is_some() {
            capabilities |= CAP_CLOCK;
            api.monotonic_ns = Some(monotonic_ns);
        }
        if services.sleeper.is_some() {
            capabilities |= CAP_SLEEP;
            api.sleep_ns = Some(sleep_ns);
        }
        if services.mmio.is_some() {
            capabilities |= CAP_MMIO;
            api.mmio_map = Some(mmio_map);
            api.mmio_unmap = Some(mmio_unmap);
        }
        if services.dma.is_some() {
            capabilities |= CAP_DMA;
            api.dma_alloc = Some(dma_allocate);
            api.dma_free = Some(dma_free);
        }
        if services.irq.is_some() {
            capabilities |= CAP_IRQ;
            api.irq_register = Some(irq_register);
            api.irq_set_enabled = Some(irq_set_enabled);
            api.irq_unregister = Some(irq_unregister);
        }
        if services.devices.is_some() {
            capabilities |= CAP_DEVICE_PUBLISH;
            api.device_publish = Some(device_publish);
            api.device_remove = Some(device_remove);
        }
        api.capabilities = capabilities;

        Self {
            api,
            _services: services,
        }
    }

    pub const fn api(&self) -> &KernelApi {
        &self.api
    }
}

fn services<'a>(context: *mut c_void) -> Option<&'a DriverServices<'a>> {
    if context.is_null() {
        None
    } else {
        // SAFETY: DriverHost always installs a pointer to its borrowed,
        // immovable DriverServices value as the kernel context.
        Some(unsafe { &*context.cast::<DriverServices<'a>>() })
    }
}

unsafe extern "C" fn log(
    context: *mut c_void,
    level: u32,
    message: *const u8,
    message_len: usize,
) -> Status {
    let Some(logger) = services(context).and_then(|value| value.logger) else {
        return STATUS_UNSUPPORTED;
    };
    let message = if message_len == 0 {
        &[]
    } else if message.is_null() {
        return STATUS_INVALID_ARGUMENT;
    } else {
        // SAFETY: The driver supplies a readable buffer for the duration of the
        // call, as required by the C driver contract.
        unsafe { core::slice::from_raw_parts(message, message_len) }
    };
    logger.log(level, message)
}

unsafe extern "C" fn allocate(
    context: *mut c_void,
    size: usize,
    alignment: usize,
    flags: u64,
    out_pointer: *mut *mut c_void,
) -> Status {
    let Some(allocator) = services(context).and_then(|value| value.allocator) else {
        return STATUS_UNSUPPORTED;
    };
    if size == 0 || out_pointer.is_null() {
        return STATUS_INVALID_ARGUMENT;
    }
    let Ok(layout) = Layout::from_size_align(size, alignment) else {
        return STATUS_INVALID_ARGUMENT;
    };
    match allocator.allocate(layout, flags) {
        Ok(pointer) => {
            // SAFETY: The non-null output pointer was validated above.
            unsafe { out_pointer.write(pointer.as_ptr().cast()) };
            STATUS_OK
        }
        Err(status) => status,
    }
}

unsafe extern "C" fn deallocate(
    context: *mut c_void,
    pointer: *mut c_void,
    size: usize,
    alignment: usize,
) -> Status {
    let Some(allocator) = services(context).and_then(|value| value.allocator) else {
        return STATUS_UNSUPPORTED;
    };
    let Some(pointer) = NonNull::new(pointer.cast::<u8>()) else {
        return STATUS_INVALID_ARGUMENT;
    };
    let Ok(layout) = Layout::from_size_align(size, alignment) else {
        return STATUS_INVALID_ARGUMENT;
    };
    // SAFETY: The C contract requires this allocation to have come from the
    // same API table with the same size and alignment.
    unsafe { allocator.deallocate(pointer, layout) };
    STATUS_OK
}

unsafe extern "C" fn monotonic_ns(context: *mut c_void) -> u64 {
    services(context)
        .and_then(|value| value.clock)
        .map_or(0, |clock| clock.monotonic_ns())
}

unsafe extern "C" fn sleep_ns(context: *mut c_void, duration_ns: u64) -> Status {
    services(context)
        .and_then(|value| value.sleeper)
        .map_or(STATUS_UNSUPPORTED, |sleeper| sleeper.sleep_ns(duration_ns))
}

unsafe extern "C" fn mmio_map(
    context: *mut c_void,
    physical_address: u64,
    length: usize,
    flags: u64,
    out_handle: *mut Handle,
    out_pointer: *mut *mut u8,
) -> Status {
    let Some(mmio) = services(context).and_then(|value| value.mmio) else {
        return STATUS_UNSUPPORTED;
    };
    if length == 0 || out_handle.is_null() || out_pointer.is_null() {
        return STATUS_INVALID_ARGUMENT;
    }
    match mmio.map(physical_address, length, flags) {
        Ok(mapping) => {
            // SAFETY: Both output pointers were validated above.
            unsafe {
                out_handle.write(mapping.handle);
                out_pointer.write(mapping.pointer.as_ptr());
            }
            STATUS_OK
        }
        Err(status) => status,
    }
}

unsafe extern "C" fn mmio_unmap(context: *mut c_void, mapping: Handle) -> Status {
    services(context)
        .and_then(|value| value.mmio)
        .map_or(STATUS_UNSUPPORTED, |mmio| mmio.unmap(mapping))
}

unsafe extern "C" fn dma_allocate(
    context: *mut c_void,
    size: usize,
    alignment: usize,
    flags: u64,
    out_handle: *mut Handle,
    out_cpu_pointer: *mut *mut c_void,
    out_device_address: *mut u64,
) -> Status {
    let Some(dma) = services(context).and_then(|value| value.dma) else {
        return STATUS_UNSUPPORTED;
    };
    if size == 0
        || !alignment.is_power_of_two()
        || out_handle.is_null()
        || out_cpu_pointer.is_null()
        || out_device_address.is_null()
    {
        return STATUS_INVALID_ARGUMENT;
    }
    match dma.allocate(size, alignment, flags) {
        Ok(allocation) => {
            // SAFETY: All output pointers were validated above.
            unsafe {
                out_handle.write(allocation.handle);
                out_cpu_pointer.write(allocation.cpu_pointer.as_ptr().cast());
                out_device_address.write(allocation.device_address);
            }
            STATUS_OK
        }
        Err(status) => status,
    }
}

unsafe extern "C" fn dma_free(context: *mut c_void, allocation: Handle) -> Status {
    services(context)
        .and_then(|value| value.dma)
        .map_or(STATUS_UNSUPPORTED, |dma| dma.free(allocation))
}

unsafe extern "C" fn irq_register(
    context: *mut c_void,
    irq: u32,
    flags: u64,
    handler: Option<IrqHandler>,
    driver_context: *mut c_void,
    out_handle: *mut Handle,
) -> Status {
    let Some(service) = services(context).and_then(|value| value.irq) else {
        return STATUS_UNSUPPORTED;
    };
    let Some(handler) = handler else {
        return STATUS_INVALID_ARGUMENT;
    };
    if out_handle.is_null() {
        return STATUS_INVALID_ARGUMENT;
    }
    match service.register(irq, flags, handler, driver_context) {
        Ok(handle) => {
            // SAFETY: The output pointer was validated above.
            unsafe { out_handle.write(handle) };
            STATUS_OK
        }
        Err(status) => status,
    }
}

unsafe extern "C" fn irq_set_enabled(
    context: *mut c_void,
    registration: Handle,
    enabled: u8,
) -> Status {
    let Some(service) = services(context).and_then(|value| value.irq) else {
        return STATUS_UNSUPPORTED;
    };
    let enabled = match enabled {
        0 => false,
        1 => true,
        _ => return STATUS_INVALID_ARGUMENT,
    };
    service.set_enabled(registration, enabled)
}

unsafe extern "C" fn irq_unregister(context: *mut c_void, registration: Handle) -> Status {
    services(context)
        .and_then(|value| value.irq)
        .map_or(STATUS_UNSUPPORTED, |irq| irq.unregister(registration))
}

unsafe extern "C" fn device_publish(
    context: *mut c_void,
    parent: Handle,
    device: *const DeviceInfo,
    out_handle: *mut Handle,
) -> Status {
    let Some(service) = services(context).and_then(|value| value.devices) else {
        return STATUS_UNSUPPORTED;
    };
    if device.is_null() || out_handle.is_null() {
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: The driver contract requires a readable DeviceInfo for this call.
    let device = unsafe { &*device };
    if device.struct_size < core::mem::size_of::<DeviceInfo>() as u32
        || (device.address.is_null() && device.address_len != 0)
    {
        return STATUS_INVALID_ARGUMENT;
    }
    match service.publish(parent, device) {
        Ok(handle) => {
            // SAFETY: The output pointer was validated above.
            unsafe { out_handle.write(handle) };
            STATUS_OK
        }
        Err(status) => status,
    }
}

unsafe extern "C" fn device_remove(context: *mut c_void, device: Handle) -> Status {
    services(context)
        .and_then(|value| value.devices)
        .map_or(STATUS_UNSUPPORTED, |devices| devices.remove(device))
}

struct SilentLogger;

impl LogService for SilentLogger {
    fn log(&self, _level: u32, _message: &[u8]) -> Status {
        STATUS_OK
    }
}

static DEFAULT_LOGGER: SilentLogger = SilentLogger;
static DEFAULT_SERVICES: DriverServices<'static> =
    DriverServices::new().with_logger(&DEFAULT_LOGGER);
static DEFAULT_HOST: DriverHost<'static> = DriverHost::new(&DEFAULT_SERVICES);

pub fn kernel_api() -> &'static KernelApi {
    DEFAULT_HOST.api()
}
