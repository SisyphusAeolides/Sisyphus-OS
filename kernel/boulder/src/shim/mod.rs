mod abyss_allocator;
mod host;
pub mod linux_kpi;
mod module;
pub mod polymorphic_vtable;
mod services;

pub use abyss_allocator::AbyssAllocator;
pub use host::{DriverHost, kernel_api};
pub use module::{DriverInstance, DriverLoadError, DriverModule};
pub use services::{
    AllocationService, ClockService, DeviceService, DmaAllocation, DmaService, DriverServices,
    IrqService, LogService, MmioMapping, MmioService, SleepService,
};

#[cfg(feature = "reference-driver")]
pub fn linked_reference_driver() -> Result<DriverModule, DriverLoadError> {
    unsafe extern "C" {
        fn sisyphus_driver_entry(
            api: *const sisyphus_driver_abi::KernelApi,
            out_driver: *mut sisyphus_driver_abi::DriverDescriptor,
            out_driver_size: usize,
        ) -> sisyphus_driver_abi::Status;
    }

    // SAFETY: The symbol is compiled from the bundled driver against the
    // canonical header and remains linked for the lifetime of the kernel.
    unsafe { DriverModule::load(sisyphus_driver_entry) }
}

#[cfg(all(test, feature = "reference-driver"))]
mod tests {
    use abyss::allocator::BumpAllocator;
    use core::cell::UnsafeCell;
    use core::ffi::c_void;
    use core::ptr::NonNull;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use sisyphus_driver_abi::{BUS_PLATFORM, DeviceInfo, DriverDescriptor, KernelApi, STATUS_OK};

    use super::{
        AbyssAllocator, ClockService, DeviceService, DmaAllocation, DmaService, DriverHost,
        DriverServices, IrqService, LogService, MmioMapping, MmioService, SleepService,
    };

    static LOG_CALLS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn count_log(
        _context: *mut c_void,
        _level: u32,
        message: *const u8,
        message_len: usize,
    ) -> i32 {
        if message.is_null() || message_len == 0 {
            return sisyphus_driver_abi::STATUS_INVALID_ARGUMENT;
        }
        LOG_CALLS.fetch_add(1, Ordering::Relaxed);
        STATUS_OK
    }

    unsafe extern "C" {
        fn sisyphus_reference_sizeof_kernel_api() -> usize;
        fn sisyphus_reference_sizeof_device_info() -> usize;
        fn sisyphus_reference_sizeof_driver_descriptor() -> usize;
        fn sisyphus_reference_exercise_api(api: *const KernelApi) -> i32;
    }

    const CALL_LOG: usize = 1 << 0;
    const CALL_CLOCK: usize = 1 << 1;
    const CALL_SLEEP: usize = 1 << 2;
    const CALL_MMIO_MAP: usize = 1 << 3;
    const CALL_MMIO_UNMAP: usize = 1 << 4;
    const CALL_DMA_ALLOC: usize = 1 << 5;
    const CALL_DMA_FREE: usize = 1 << 6;
    const CALL_IRQ_REGISTER: usize = 1 << 7;
    const CALL_IRQ_ENABLE: usize = 1 << 8;
    const CALL_IRQ_DISABLE: usize = 1 << 9;
    const CALL_IRQ_UNREGISTER: usize = 1 << 10;
    const CALL_DEVICE_PUBLISH: usize = 1 << 11;
    const CALL_DEVICE_REMOVE: usize = 1 << 12;
    const ALL_HARDWARE_CALLS: usize = (1 << 13) - 1;

    struct TestHardware {
        calls: AtomicUsize,
        mmio: UnsafeCell<[u8; 64]>,
        dma: UnsafeCell<[u8; 64]>,
    }

    // SAFETY: Tests access the backing buffers only through serialized service
    // calls and never retain or dereference them concurrently.
    unsafe impl Sync for TestHardware {}

    impl TestHardware {
        const fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                mmio: UnsafeCell::new([0; 64]),
                dma: UnsafeCell::new([0; 64]),
            }
        }

        fn record(&self, call: usize) {
            self.calls.fetch_or(call, Ordering::Relaxed);
        }
    }

    impl LogService for TestHardware {
        fn log(&self, _level: u32, message: &[u8]) -> i32 {
            if message.is_empty() {
                return sisyphus_driver_abi::STATUS_INVALID_ARGUMENT;
            }
            self.record(CALL_LOG);
            STATUS_OK
        }
    }

    impl ClockService for TestHardware {
        fn monotonic_ns(&self) -> u64 {
            self.record(CALL_CLOCK);
            42
        }
    }

    impl SleepService for TestHardware {
        fn sleep_ns(&self, _duration_ns: u64) -> i32 {
            self.record(CALL_SLEEP);
            STATUS_OK
        }
    }

    impl MmioService for TestHardware {
        fn map(
            &self,
            _physical_address: u64,
            _length: usize,
            _flags: u64,
        ) -> Result<MmioMapping, i32> {
            self.record(CALL_MMIO_MAP);
            let pointer = NonNull::new(self.mmio.get().cast::<u8>()).expect("buffer is non-null");
            Ok(MmioMapping { handle: 1, pointer })
        }

        fn unmap(&self, _mapping: u64) -> i32 {
            self.record(CALL_MMIO_UNMAP);
            STATUS_OK
        }
    }

    impl DmaService for TestHardware {
        fn allocate(
            &self,
            _size: usize,
            _alignment: usize,
            _flags: u64,
        ) -> Result<DmaAllocation, i32> {
            self.record(CALL_DMA_ALLOC);
            let cpu_pointer =
                NonNull::new(self.dma.get().cast::<u8>()).expect("buffer is non-null");
            Ok(DmaAllocation {
                handle: 2,
                cpu_pointer,
                device_address: 0x2000,
            })
        }

        fn free(&self, _allocation: u64) -> i32 {
            self.record(CALL_DMA_FREE);
            STATUS_OK
        }
    }

    impl IrqService for TestHardware {
        fn register(
            &self,
            _irq: u32,
            _flags: u64,
            handler: sisyphus_driver_abi::IrqHandler,
            driver_context: *mut c_void,
        ) -> Result<u64, i32> {
            self.record(CALL_IRQ_REGISTER);
            // SAFETY: The C test driver supplied this callback and context for
            // immediate invocation under the IRQ callback contract.
            unsafe { handler(driver_context) };
            Ok(3)
        }

        fn set_enabled(&self, _registration: u64, enabled: bool) -> i32 {
            self.record(if enabled {
                CALL_IRQ_ENABLE
            } else {
                CALL_IRQ_DISABLE
            });
            STATUS_OK
        }

        fn unregister(&self, _registration: u64) -> i32 {
            self.record(CALL_IRQ_UNREGISTER);
            STATUS_OK
        }
    }

    impl DeviceService for TestHardware {
        fn publish(&self, _parent: u64, _device: &DeviceInfo) -> Result<u64, i32> {
            self.record(CALL_DEVICE_PUBLISH);
            Ok(4)
        }

        fn remove(&self, _device: u64) -> i32 {
            self.record(CALL_DEVICE_REMOVE);
            STATUS_OK
        }
    }

    #[test]
    fn c_and_rust_layouts_match() {
        unsafe {
            assert_eq!(
                sisyphus_reference_sizeof_kernel_api(),
                core::mem::size_of::<KernelApi>()
            );
            assert_eq!(
                sisyphus_reference_sizeof_device_info(),
                core::mem::size_of::<DeviceInfo>()
            );
            assert_eq!(
                sisyphus_reference_sizeof_driver_descriptor(),
                core::mem::size_of::<DriverDescriptor>()
            );
        }
    }

    #[test]
    fn linked_c_driver_loads_probes_and_removes() {
        LOG_CALLS.store(0, Ordering::Relaxed);
        let mut api = *super::kernel_api();
        api.log = Some(count_log);

        unsafe extern "C" {
            fn sisyphus_driver_entry(
                api: *const KernelApi,
                out_driver: *mut DriverDescriptor,
                out_driver_size: usize,
            ) -> i32;
        }

        let module = unsafe { super::DriverModule::load_with_api(sisyphus_driver_entry, &api) }
            .expect("reference driver should load");
        assert_eq!(module.name(), b"sisyphus-reference");

        let address = b"platform:reference0";
        let device = DeviceInfo {
            struct_size: core::mem::size_of::<DeviceInfo>() as u32,
            bus_type: BUS_PLATFORM,
            kernel_handle: 1,
            vendor_id: 0,
            device_id: 0,
            subsystem_vendor_id: 0,
            subsystem_device_id: 0,
            class_code: 0,
            revision: 0,
            address: address.as_ptr(),
            address_len: address.len(),
        };

        let instance = module
            .probe_with_api(&api, &device)
            .expect("probe should succeed");
        assert_eq!(LOG_CALLS.load(Ordering::Relaxed), 1);
        module
            .remove_with_api(&api, &device, instance)
            .expect("remove should succeed");
    }

    #[test]
    fn c_driver_reaches_every_installed_service() {
        let mut heap = [0_u8; 1024];
        let allocator = BumpAllocator::empty();
        // SAFETY: The stack buffer is writable, exclusively held for this test,
        // and outlives the allocator and every service call.
        unsafe {
            allocator
                .initialize(heap.as_mut_ptr() as usize, heap.len())
                .expect("test heap should initialize");
        }
        let initial_remaining = allocator.remaining();
        let abyss = AbyssAllocator::new(&allocator);
        let hardware = TestHardware::new();
        let services = DriverServices::new()
            .with_logger(&hardware)
            .with_allocator(&abyss)
            .with_clock(&hardware)
            .with_sleep(&hardware)
            .with_mmio(&hardware)
            .with_dma(&hardware)
            .with_irq(&hardware)
            .with_devices(&hardware);
        let host = DriverHost::new(&services);

        let status = unsafe { sisyphus_reference_exercise_api(host.api()) };
        assert_eq!(status, STATUS_OK);
        assert_eq!(hardware.calls.load(Ordering::Relaxed), ALL_HARDWARE_CALLS);
        assert!(allocator.remaining() < initial_remaining);
    }

    #[test]
    fn absent_services_are_not_advertised() {
        let services = DriverServices::new();
        let host = DriverHost::new(&services);
        let api = host.api();

        assert_eq!(api.capabilities, 0);
        assert!(api.log.is_none());
        assert!(api.alloc.is_none());
        assert!(api.dealloc.is_none());
        assert!(api.monotonic_ns.is_none());
        assert!(api.sleep_ns.is_none());
        assert!(api.mmio_map.is_none());
        assert!(api.mmio_unmap.is_none());
        assert!(api.dma_alloc.is_none());
        assert!(api.dma_free.is_none());
        assert!(api.irq_register.is_none());
        assert!(api.irq_set_enabled.is_none());
        assert!(api.irq_unregister.is_none());
        assert!(api.device_publish.is_none());
        assert!(api.device_remove.is_none());
    }
}
