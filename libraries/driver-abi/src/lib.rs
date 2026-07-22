#![no_std]

pub mod prometheus;
pub mod golem;
pub mod lazarus;

use core::ffi::c_void;

pub type Status = i32;
pub type Handle = u64;

pub const STATUS_OK: Status = 0;
pub const STATUS_INVALID_ARGUMENT: Status = -1;
pub const STATUS_UNSUPPORTED: Status = -2;
pub const STATUS_NO_MEMORY: Status = -3;
pub const STATUS_BUSY: Status = -4;
pub const STATUS_NOT_FOUND: Status = -5;
pub const STATUS_IO_ERROR: Status = -6;
pub const STATUS_ABI_MISMATCH: Status = -7;

pub const INVALID_HANDLE: Handle = 0;

pub const ABI_MAJOR: u32 = 1;
pub const ABI_MINOR: u32 = 0;
pub const ABI_VERSION: u32 = (ABI_MAJOR << 16) | ABI_MINOR;

pub const CAP_LOG: u64 = 1 << 0;
pub const CAP_ALLOC: u64 = 1 << 1;
pub const CAP_CLOCK: u64 = 1 << 2;
pub const CAP_SLEEP: u64 = 1 << 3;
pub const CAP_MMIO: u64 = 1 << 4;
pub const CAP_DMA: u64 = 1 << 5;
pub const CAP_IRQ: u64 = 1 << 6;
pub const CAP_DEVICE_PUBLISH: u64 = 1 << 7;

pub const LOG_ERROR: u32 = 1;
pub const LOG_WARN: u32 = 2;
pub const LOG_INFO: u32 = 3;
pub const LOG_DEBUG: u32 = 4;
pub const LOG_TRACE: u32 = 5;

pub const BUS_PLATFORM: u32 = 1;
pub const BUS_PCI: u32 = 2;
pub const BUS_USB: u32 = 3;
pub const BUS_VIRTIO: u32 = 4;

pub const fn abi_major(version: u32) -> u32 {
    version >> 16
}

pub const fn abi_minor(version: u32) -> u32 {
    version & 0xffff
}

pub type IrqHandler = unsafe extern "C" fn(driver_context: *mut c_void);

pub type LogFn = unsafe extern "C" fn(
    kernel_context: *mut c_void,
    level: u32,
    message: *const u8,
    message_len: usize,
) -> Status;

pub type AllocFn = unsafe extern "C" fn(
    kernel_context: *mut c_void,
    size: usize,
    alignment: usize,
    flags: u64,
    out_pointer: *mut *mut c_void,
) -> Status;

pub type DeallocFn = unsafe extern "C" fn(
    kernel_context: *mut c_void,
    pointer: *mut c_void,
    size: usize,
    alignment: usize,
) -> Status;

pub type MonotonicNsFn = unsafe extern "C" fn(kernel_context: *mut c_void) -> u64;
pub type SleepNsFn = unsafe extern "C" fn(kernel_context: *mut c_void, duration_ns: u64) -> Status;

pub type MmioMapFn = unsafe extern "C" fn(
    kernel_context: *mut c_void,
    physical_address: u64,
    length: usize,
    flags: u64,
    out_handle: *mut Handle,
    out_pointer: *mut *mut u8,
) -> Status;

pub type MmioUnmapFn = unsafe extern "C" fn(kernel_context: *mut c_void, mapping: Handle) -> Status;

pub type DmaAllocFn = unsafe extern "C" fn(
    kernel_context: *mut c_void,
    size: usize,
    alignment: usize,
    flags: u64,
    out_handle: *mut Handle,
    out_cpu_pointer: *mut *mut c_void,
    out_device_address: *mut u64,
) -> Status;

pub type DmaFreeFn =
    unsafe extern "C" fn(kernel_context: *mut c_void, allocation: Handle) -> Status;

pub type IrqRegisterFn = unsafe extern "C" fn(
    kernel_context: *mut c_void,
    irq: u32,
    flags: u64,
    handler: Option<IrqHandler>,
    driver_context: *mut c_void,
    out_handle: *mut Handle,
) -> Status;

pub type IrqSetEnabledFn =
    unsafe extern "C" fn(kernel_context: *mut c_void, registration: Handle, enabled: u8) -> Status;

pub type IrqUnregisterFn =
    unsafe extern "C" fn(kernel_context: *mut c_void, registration: Handle) -> Status;

pub type DevicePublishFn = unsafe extern "C" fn(
    kernel_context: *mut c_void,
    parent: Handle,
    device: *const DeviceInfo,
    out_handle: *mut Handle,
) -> Status;

pub type DeviceRemoveFn =
    unsafe extern "C" fn(kernel_context: *mut c_void, device: Handle) -> Status;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KernelApi {
    pub abi_version: u32,
    pub struct_size: u32,
    pub capabilities: u64,
    pub kernel_context: *mut c_void,
    pub log: Option<LogFn>,
    pub alloc: Option<AllocFn>,
    pub dealloc: Option<DeallocFn>,
    pub monotonic_ns: Option<MonotonicNsFn>,
    pub sleep_ns: Option<SleepNsFn>,
    pub mmio_map: Option<MmioMapFn>,
    pub mmio_unmap: Option<MmioUnmapFn>,
    pub dma_alloc: Option<DmaAllocFn>,
    pub dma_free: Option<DmaFreeFn>,
    pub irq_register: Option<IrqRegisterFn>,
    pub irq_set_enabled: Option<IrqSetEnabledFn>,
    pub irq_unregister: Option<IrqUnregisterFn>,
    pub device_publish: Option<DevicePublishFn>,
    pub device_remove: Option<DeviceRemoveFn>,
}

unsafe impl Sync for KernelApi {}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DeviceInfo {
    pub struct_size: u32,
    pub bus_type: u32,
    pub kernel_handle: Handle,
    pub vendor_id: u32,
    pub device_id: u32,
    pub subsystem_vendor_id: u32,
    pub subsystem_device_id: u32,
    pub class_code: u32,
    pub revision: u32,
    pub address: *const u8,
    pub address_len: usize,
}

pub type ProbeFn = unsafe extern "C" fn(
    driver_context: *mut c_void,
    api: *const KernelApi,
    device: *const DeviceInfo,
    out_instance: *mut *mut c_void,
) -> Status;

pub type RemoveFn = unsafe extern "C" fn(
    instance: *mut c_void,
    api: *const KernelApi,
    device: *const DeviceInfo,
) -> Status;

pub type PowerFn = unsafe extern "C" fn(instance: *mut c_void, api: *const KernelApi) -> Status;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DriverDescriptor {
    pub abi_version: u32,
    pub struct_size: u32,
    pub driver_version: u64,
    pub required_capabilities: u64,
    pub name: *const u8,
    pub name_len: usize,
    pub driver_context: *mut c_void,
    pub probe: Option<ProbeFn>,
    pub remove: Option<RemoveFn>,
    pub suspend: Option<PowerFn>,
    pub resume: Option<PowerFn>,
}

impl DriverDescriptor {
    pub const fn empty() -> Self {
        Self {
            abi_version: ABI_VERSION,
            struct_size: core::mem::size_of::<Self>() as u32,
            driver_version: 0,
            required_capabilities: 0,
            name: core::ptr::null(),
            name_len: 0,
            driver_context: core::ptr::null_mut(),
            probe: None,
            remove: None,
            suspend: None,
            resume: None,
        }
    }
}

pub type DriverEntryFn = unsafe extern "C" fn(
    api: *const KernelApi,
    out_driver: *mut DriverDescriptor,
    out_driver_size: usize,
) -> Status;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_fields_round_trip() {
        assert_eq!(abi_major(ABI_VERSION), ABI_MAJOR);
        assert_eq!(abi_minor(ABI_VERSION), ABI_MINOR);
    }

    #[test]
    fn public_structs_have_stable_alignment() {
        assert_eq!(
            core::mem::align_of::<KernelApi>(),
            core::mem::align_of::<usize>()
        );
        assert_eq!(
            core::mem::align_of::<DriverDescriptor>(),
            core::mem::align_of::<usize>()
        );
        assert!(core::mem::size_of::<KernelApi>() <= u32::MAX as usize);
        assert!(core::mem::size_of::<DriverDescriptor>() <= u32::MAX as usize);
    }
}
