// libraries/driver-abi/src/membrane.rs

use super::{
    KernelApi, Status, Handle, DeviceInfo,
    CAP_LOG, CAP_ALLOC, CAP_CLOCK, CAP_SLEEP, CAP_MMIO, CAP_DMA, CAP_IRQ, CAP_DEVICE_PUBLISH,
    STATUS_UNSUPPORTED
};
use core::ffi::c_void;

pub struct DriverMembrane {
    pub allowed_capabilities: u64,
    pub inner_api: *const KernelApi,
}

impl DriverMembrane {
    pub const fn new(allowed_capabilities: u64, inner_api: *const KernelApi) -> Self {
        Self {
            allowed_capabilities,
            inner_api,
        }
    }

    pub fn wrap_api(&self) -> KernelApi {
        let mut api = unsafe { *self.inner_api };
        api.capabilities = self.allowed_capabilities;
        api.kernel_context = self as *const _ as *mut c_void;
        
        api.log = Some(membrane_log);
        api.alloc = Some(membrane_alloc);
        api.dealloc = Some(membrane_dealloc);
        api.monotonic_ns = Some(membrane_monotonic_ns);
        api.sleep_ns = Some(membrane_sleep_ns);
        api.mmio_map = Some(membrane_mmio_map);
        api.mmio_unmap = Some(membrane_mmio_unmap);
        api.dma_alloc = Some(membrane_dma_alloc);
        api.dma_free = Some(membrane_dma_free);
        api.irq_register = Some(membrane_irq_register);
        api.irq_set_enabled = Some(membrane_irq_set_enabled);
        api.irq_unregister = Some(membrane_irq_unregister);
        api.device_publish = Some(membrane_device_publish);
        api.device_remove = Some(membrane_device_remove);

        api
    }
}

#[inline(never)]
fn apoptosis(missing_cap: u64) -> ! {
    panic!("Apoptosis event: Driver attempted capability 0x{:x} without permission", missing_cap);
}

unsafe extern "C" fn membrane_log(
    kernel_context: *mut c_void,
    level: u32,
    message: *const u8,
    message_len: usize,
) -> Status {
    let membrane = unsafe { &*(kernel_context as *const DriverMembrane) };
    if (membrane.allowed_capabilities & CAP_LOG) == 0 {
        apoptosis(CAP_LOG);
    }
    let inner = unsafe { &*membrane.inner_api };
    if let Some(f) = inner.log {
        unsafe { f(inner.kernel_context, level, message, message_len) }
    } else {
        STATUS_UNSUPPORTED
    }
}

unsafe extern "C" fn membrane_alloc(
    kernel_context: *mut c_void,
    size: usize,
    alignment: usize,
    flags: u64,
    out_pointer: *mut *mut c_void,
) -> Status {
    let membrane = unsafe { &*(kernel_context as *const DriverMembrane) };
    if (membrane.allowed_capabilities & CAP_ALLOC) == 0 {
        apoptosis(CAP_ALLOC);
    }
    let inner = unsafe { &*membrane.inner_api };
    if let Some(f) = inner.alloc {
        unsafe { f(inner.kernel_context, size, alignment, flags, out_pointer) }
    } else {
        STATUS_UNSUPPORTED
    }
}

unsafe extern "C" fn membrane_dealloc(
    kernel_context: *mut c_void,
    pointer: *mut c_void,
    size: usize,
    alignment: usize,
) -> Status {
    let membrane = unsafe { &*(kernel_context as *const DriverMembrane) };
    if (membrane.allowed_capabilities & CAP_ALLOC) == 0 {
        apoptosis(CAP_ALLOC);
    }
    let inner = unsafe { &*membrane.inner_api };
    if let Some(f) = inner.dealloc {
        unsafe { f(inner.kernel_context, pointer, size, alignment) }
    } else {
        STATUS_UNSUPPORTED
    }
}

unsafe extern "C" fn membrane_monotonic_ns(kernel_context: *mut c_void) -> u64 {
    let membrane = unsafe { &*(kernel_context as *const DriverMembrane) };
    if (membrane.allowed_capabilities & CAP_CLOCK) == 0 {
        apoptosis(CAP_CLOCK);
    }
    let inner = unsafe { &*membrane.inner_api };
    if let Some(f) = inner.monotonic_ns {
        unsafe { f(inner.kernel_context) }
    } else {
        0
    }
}

unsafe extern "C" fn membrane_sleep_ns(kernel_context: *mut c_void, duration_ns: u64) -> Status {
    let membrane = unsafe { &*(kernel_context as *const DriverMembrane) };
    if (membrane.allowed_capabilities & CAP_SLEEP) == 0 {
        apoptosis(CAP_SLEEP);
    }
    let inner = unsafe { &*membrane.inner_api };
    if let Some(f) = inner.sleep_ns {
        unsafe { f(inner.kernel_context, duration_ns) }
    } else {
        STATUS_UNSUPPORTED
    }
}

unsafe extern "C" fn membrane_mmio_map(
    kernel_context: *mut c_void,
    physical_address: u64,
    length: usize,
    flags: u64,
    out_handle: *mut Handle,
    out_pointer: *mut *mut u8,
) -> Status {
    let membrane = unsafe { &*(kernel_context as *const DriverMembrane) };
    if (membrane.allowed_capabilities & CAP_MMIO) == 0 {
        apoptosis(CAP_MMIO);
    }
    let inner = unsafe { &*membrane.inner_api };
    if let Some(f) = inner.mmio_map {
        unsafe { f(inner.kernel_context, physical_address, length, flags, out_handle, out_pointer) }
    } else {
        STATUS_UNSUPPORTED
    }
}

unsafe extern "C" fn membrane_mmio_unmap(kernel_context: *mut c_void, mapping: Handle) -> Status {
    let membrane = unsafe { &*(kernel_context as *const DriverMembrane) };
    if (membrane.allowed_capabilities & CAP_MMIO) == 0 {
        apoptosis(CAP_MMIO);
    }
    let inner = unsafe { &*membrane.inner_api };
    if let Some(f) = inner.mmio_unmap {
        unsafe { f(inner.kernel_context, mapping) }
    } else {
        STATUS_UNSUPPORTED
    }
}

unsafe extern "C" fn membrane_dma_alloc(
    kernel_context: *mut c_void,
    size: usize,
    alignment: usize,
    flags: u64,
    out_handle: *mut Handle,
    out_cpu_pointer: *mut *mut c_void,
    out_device_address: *mut u64,
) -> Status {
    let membrane = unsafe { &*(kernel_context as *const DriverMembrane) };
    if (membrane.allowed_capabilities & CAP_DMA) == 0 {
        apoptosis(CAP_DMA);
    }
    let inner = unsafe { &*membrane.inner_api };
    if let Some(f) = inner.dma_alloc {
        unsafe { f(inner.kernel_context, size, alignment, flags, out_handle, out_cpu_pointer, out_device_address) }
    } else {
        STATUS_UNSUPPORTED
    }
}

unsafe extern "C" fn membrane_dma_free(kernel_context: *mut c_void, allocation: Handle) -> Status {
    let membrane = unsafe { &*(kernel_context as *const DriverMembrane) };
    if (membrane.allowed_capabilities & CAP_DMA) == 0 {
        apoptosis(CAP_DMA);
    }
    let inner = unsafe { &*membrane.inner_api };
    if let Some(f) = inner.dma_free {
        unsafe { f(inner.kernel_context, allocation) }
    } else {
        STATUS_UNSUPPORTED
    }
}

unsafe extern "C" fn membrane_irq_register(
    kernel_context: *mut c_void,
    irq: u32,
    flags: u64,
    handler: Option<super::IrqHandler>,
    driver_context: *mut c_void,
    out_handle: *mut Handle,
) -> Status {
    let membrane = unsafe { &*(kernel_context as *const DriverMembrane) };
    if (membrane.allowed_capabilities & CAP_IRQ) == 0 {
        apoptosis(CAP_IRQ);
    }
    let inner = unsafe { &*membrane.inner_api };
    if let Some(f) = inner.irq_register {
        unsafe { f(inner.kernel_context, irq, flags, handler, driver_context, out_handle) }
    } else {
        STATUS_UNSUPPORTED
    }
}

unsafe extern "C" fn membrane_irq_set_enabled(
    kernel_context: *mut c_void,
    registration: Handle,
    enabled: u8,
) -> Status {
    let membrane = unsafe { &*(kernel_context as *const DriverMembrane) };
    if (membrane.allowed_capabilities & CAP_IRQ) == 0 {
        apoptosis(CAP_IRQ);
    }
    let inner = unsafe { &*membrane.inner_api };
    if let Some(f) = inner.irq_set_enabled {
        unsafe { f(inner.kernel_context, registration, enabled) }
    } else {
        STATUS_UNSUPPORTED
    }
}

unsafe extern "C" fn membrane_irq_unregister(kernel_context: *mut c_void, registration: Handle) -> Status {
    let membrane = unsafe { &*(kernel_context as *const DriverMembrane) };
    if (membrane.allowed_capabilities & CAP_IRQ) == 0 {
        apoptosis(CAP_IRQ);
    }
    let inner = unsafe { &*membrane.inner_api };
    if let Some(f) = inner.irq_unregister {
        unsafe { f(inner.kernel_context, registration) }
    } else {
        STATUS_UNSUPPORTED
    }
}

unsafe extern "C" fn membrane_device_publish(
    kernel_context: *mut c_void,
    parent: Handle,
    device: *const DeviceInfo,
    out_handle: *mut Handle,
) -> Status {
    let membrane = unsafe { &*(kernel_context as *const DriverMembrane) };
    if (membrane.allowed_capabilities & CAP_DEVICE_PUBLISH) == 0 {
        apoptosis(CAP_DEVICE_PUBLISH);
    }
    let inner = unsafe { &*membrane.inner_api };
    if let Some(f) = inner.device_publish {
        unsafe { f(inner.kernel_context, parent, device, out_handle) }
    } else {
        STATUS_UNSUPPORTED
    }
}

unsafe extern "C" fn membrane_device_remove(kernel_context: *mut c_void, device: Handle) -> Status {
    let membrane = unsafe { &*(kernel_context as *const DriverMembrane) };
    if (membrane.allowed_capabilities & CAP_DEVICE_PUBLISH) == 0 {
        apoptosis(CAP_DEVICE_PUBLISH);
    }
    let inner = unsafe { &*membrane.inner_api };
    if let Some(f) = inner.device_remove {
        unsafe { f(inner.kernel_context, device) }
    } else {
        STATUS_UNSUPPORTED
    }
}
