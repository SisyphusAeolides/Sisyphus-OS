use core::alloc::Layout;
use core::ffi::c_void;
use core::ptr::NonNull;
use sisyphus_driver_abi::{DeviceInfo, Handle, IrqHandler, Status};

pub trait LogService: Sync {
    fn log(&self, level: u32, message: &[u8]) -> Status;
}

pub trait AllocationService: Sync {
    fn allocate(&self, layout: Layout, flags: u64) -> Result<NonNull<u8>, Status>;

    /// Releases an allocation previously returned by `allocate`.
    ///
    /// # Safety
    ///
    /// `pointer` and `layout` must identify a live allocation produced by this
    /// service, and that allocation must not be used after this call.
    unsafe fn deallocate(&self, pointer: NonNull<u8>, layout: Layout);
}

pub trait ClockService: Sync {
    fn monotonic_ns(&self) -> u64;
}

pub trait SleepService: Sync {
    fn sleep_ns(&self, duration_ns: u64) -> Status;
}

#[derive(Clone, Copy)]
pub struct MmioMapping {
    pub handle: Handle,
    pub pointer: NonNull<u8>,
}

pub trait MmioService: Sync {
    fn map(&self, physical_address: u64, length: usize, flags: u64) -> Result<MmioMapping, Status>;

    fn unmap(&self, mapping: Handle) -> Status;
}

#[derive(Clone, Copy)]
pub struct DmaAllocation {
    pub handle: Handle,
    pub cpu_pointer: NonNull<u8>,
    pub device_address: u64,
}

pub trait DmaService: Sync {
    fn allocate(&self, size: usize, alignment: usize, flags: u64) -> Result<DmaAllocation, Status>;

    fn free(&self, allocation: Handle) -> Status;
}

pub trait IrqService: Sync {
    fn register(
        &self,
        irq: u32,
        flags: u64,
        handler: IrqHandler,
        driver_context: *mut c_void,
    ) -> Result<Handle, Status>;

    fn set_enabled(&self, registration: Handle, enabled: bool) -> Status;
    fn unregister(&self, registration: Handle) -> Status;
}

pub trait DeviceService: Sync {
    fn publish(&self, parent: Handle, device: &DeviceInfo) -> Result<Handle, Status>;
    fn remove(&self, device: Handle) -> Status;
}

pub struct DriverServices<'a> {
    pub(crate) logger: Option<&'a dyn LogService>,
    pub(crate) allocator: Option<&'a dyn AllocationService>,
    pub(crate) clock: Option<&'a dyn ClockService>,
    pub(crate) sleeper: Option<&'a dyn SleepService>,
    pub(crate) mmio: Option<&'a dyn MmioService>,
    pub(crate) dma: Option<&'a dyn DmaService>,
    pub(crate) irq: Option<&'a dyn IrqService>,
    pub(crate) devices: Option<&'a dyn DeviceService>,
}

impl<'a> DriverServices<'a> {
    pub const fn new() -> Self {
        Self {
            logger: None,
            allocator: None,
            clock: None,
            sleeper: None,
            mmio: None,
            dma: None,
            irq: None,
            devices: None,
        }
    }

    pub const fn with_logger(mut self, service: &'a dyn LogService) -> Self {
        self.logger = Some(service);
        self
    }

    pub const fn with_allocator(mut self, service: &'a dyn AllocationService) -> Self {
        self.allocator = Some(service);
        self
    }

    pub const fn with_clock(mut self, service: &'a dyn ClockService) -> Self {
        self.clock = Some(service);
        self
    }

    pub const fn with_sleep(mut self, service: &'a dyn SleepService) -> Self {
        self.sleeper = Some(service);
        self
    }

    pub const fn with_mmio(mut self, service: &'a dyn MmioService) -> Self {
        self.mmio = Some(service);
        self
    }

    pub const fn with_dma(mut self, service: &'a dyn DmaService) -> Self {
        self.dma = Some(service);
        self
    }

    pub const fn with_irq(mut self, service: &'a dyn IrqService) -> Self {
        self.irq = Some(service);
        self
    }

    pub const fn with_devices(mut self, service: &'a dyn DeviceService) -> Self {
        self.devices = Some(service);
        self
    }
}

impl Default for DriverServices<'_> {
    fn default() -> Self {
        Self::new()
    }
}
