//! Kernel resource coordinator for the Hermes GSP transport.
//!
//! This module deliberately does not infer DMA isolation from a generic
//! allocation. A production DMA service must bind each allocation to the
//! supplied IOMMU domain and implement its publication/acquisition barriers.

use core::ffi::c_void;

use sisyphus_driver_abi::hermes::HermesPciIdentity;
use sisyphus_driver_abi::{Handle, IrqHandler, Status, STATUS_OK};

use super::drivernet::fingerprint::{GpuFingerprint, BAR_64BIT, BAR_IO, BAR_PRESENT};
use super::hermes_gsp::{
    DmaPurpose, DmaRegion as HermesDmaRegion, HermesFault, HermesPlatform,
    MmioWindow as HermesMmioWindow,
};
use crate::hw::iommu::DmaRemappingBackend;
use crate::shim::{ClockService, DmaAllocation, IrqService, MmioService};
use crate::sync::SpinLock;

const BAR_COUNT: usize = 6;
const MAXIMUM_DMA_LEASES: usize = 8;

/// Domain-aware DMA integration required by Hermes.
///
/// The existing generic `DmaService` is intentionally insufficient: it does
/// not bind allocations to an IOMMU domain or define device/CPU visibility.
/// Successful allocations must return unique nonzero handles. A failed
/// `release` must leave the allocation live and retryable.
pub trait HermesDomainDmaService: Sync {
    fn supports(&self, purpose: DmaPurpose) -> bool;

    fn allocate(
        &self,
        domain: Handle,
        length: usize,
        alignment: usize,
        purpose: DmaPurpose,
    ) -> Result<DmaAllocation, Status>;

    fn release(&self, domain: Handle, allocation: Handle) -> Status;

    fn write(&self, allocation: Handle, offset: usize, bytes: &[u8]) -> Status;
    fn read(&self, allocation: Handle, offset: usize, bytes: &mut [u8]) -> Status;
    fn publish(&self, allocation: Handle, offset: usize, length: usize) -> Status;
    fn acquire(&self, allocation: Handle, offset: usize, length: usize) -> Status;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HermesKernelServices {
    pub isolation: bool,
    pub mmio: bool,
    pub dma: bool,
    pub irq: bool,
    pub firmware_dma: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HermesPlatformError {
    MissingIsolationService,
    MissingMmioService,
    MissingDmaService,
    MissingIrqService,
    IdentityMismatch,
    InvalidBar,
    BarEvidenceMismatch,
    InvalidRequest,
    LeaseCapacity,
    StaleLease,
    ResourcesStillLive,
    Backend(Status),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DomainLease {
    generation: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MmioLease {
    slot: u8,
    generation: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmaLease {
    slot: u8,
    generation: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IrqLease {
    generation: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MappedBar {
    pub lease: MmioLease,
    pub bar: u8,
    pub length: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AllocatedDma {
    pub lease: DmaLease,
    pub device_address: u64,
    pub length: usize,
    pub alignment: usize,
    pub purpose: DmaPurpose,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CleanupReport {
    pub pending_bars: u8,
    pub pending_dma: u16,
    pub pending_irq: bool,
    pub pending_domain: bool,
    pub first_status: Status,
}

impl CleanupReport {
    pub const fn complete(self) -> bool {
        self.pending_bars == 0 && self.pending_dma == 0 && !self.pending_irq && !self.pending_domain
    }
}

#[derive(Clone, Copy)]
struct DomainRecord {
    backend: Handle,
    generation: u32,
    active: bool,
}

impl DomainRecord {
    const EMPTY: Self = Self {
        backend: 0,
        generation: 0,
        active: false,
    };
}

#[derive(Clone, Copy)]
struct MmioRecord {
    backend: Handle,
    pointer: usize,
    length: u64,
    generation: u32,
    active: bool,
}

impl MmioRecord {
    const EMPTY: Self = Self {
        backend: 0,
        pointer: 0,
        length: 0,
        generation: 0,
        active: false,
    };
}

#[derive(Clone, Copy)]
struct DmaRecord {
    backend: Handle,
    device_address: u64,
    length: usize,
    alignment: usize,
    purpose: DmaPurpose,
    generation: u32,
    active: bool,
}

impl DmaRecord {
    const EMPTY: Self = Self {
        backend: 0,
        device_address: 0,
        length: 0,
        alignment: 0,
        purpose: DmaPurpose::CommandRing,
        generation: 0,
        active: false,
    };
}

#[derive(Clone, Copy)]
struct IrqRecord {
    backend: Handle,
    generation: u32,
    enabled: bool,
    active: bool,
}

impl IrqRecord {
    const EMPTY: Self = Self {
        backend: 0,
        generation: 0,
        enabled: false,
        active: false,
    };
}

struct LeaseState {
    domain: DomainRecord,
    bars: [MmioRecord; BAR_COUNT],
    dma: [DmaRecord; MAXIMUM_DMA_LEASES],
    irq: IrqRecord,
}

impl LeaseState {
    const fn new() -> Self {
        Self {
            domain: DomainRecord::EMPTY,
            bars: [MmioRecord::EMPTY; BAR_COUNT],
            dma: [DmaRecord::EMPTY; MAXIMUM_DMA_LEASES],
            irq: IrqRecord::EMPTY,
        }
    }

    fn domain(&self, lease: DomainLease) -> Result<Handle, HermesPlatformError> {
        if self.domain.active && self.domain.generation == lease.generation {
            Ok(self.domain.backend)
        } else {
            Err(HermesPlatformError::StaleLease)
        }
    }
}

#[must_use = "the platform owns kernel resource leases until cleanup succeeds"]
pub struct KernelHermesPlatform<'a> {
    fingerprint: GpuFingerprint,
    isolation: Option<&'a dyn DmaRemappingBackend>,
    mmio: Option<&'a dyn MmioService>,
    dma: Option<&'a dyn HermesDomainDmaService>,
    irq: Option<&'a dyn IrqService>,
    clock: &'a dyn ClockService,
    state: SpinLock<LeaseState>,
}

impl<'a> KernelHermesPlatform<'a> {
    pub const fn new(
        fingerprint: GpuFingerprint,
        isolation: Option<&'a dyn DmaRemappingBackend>,
        mmio: Option<&'a dyn MmioService>,
        dma: Option<&'a dyn HermesDomainDmaService>,
        irq: Option<&'a dyn IrqService>,
        clock: &'a dyn ClockService,
    ) -> Self {
        Self {
            fingerprint,
            isolation,
            mmio,
            dma,
            irq,
            clock,
            state: SpinLock::new(LeaseState::new()),
        }
    }

    pub fn available_services(&self) -> HermesKernelServices {
        let isolated = self.isolation.is_some();
        let dma = isolated
            && self.dma.is_some_and(|service| {
                service.supports(DmaPurpose::CommandRing) && service.supports(DmaPurpose::EventRing)
            });
        let mmio_evidence = self.fingerprint.bars.iter().copied().any(|bar| {
            bar.length != 0
                && bar.length.is_power_of_two()
                && bar_physical_address(bar.raw_low, bar.raw_high, bar.flags)
                    .is_ok_and(|address| address % bar.length == 0)
        });
        let irq_evidence =
            self.fingerprint.interrupt_pin != 0 && self.fingerprint.interrupt_line != u8::MAX;
        HermesKernelServices {
            isolation: isolated,
            mmio: isolated && self.mmio.is_some() && mmio_evidence,
            dma,
            irq: isolated && self.irq.is_some() && irq_evidence,
            firmware_dma: dma
                && self
                    .dma
                    .is_some_and(|service| service.supports(DmaPurpose::Firmware)),
        }
    }

    pub fn acquire_domain(
        &self,
        identity: HermesPciIdentity,
    ) -> Result<DomainLease, HermesPlatformError> {
        if !identity_matches(&self.fingerprint, identity) {
            return Err(HermesPlatformError::IdentityMismatch);
        }
        let isolation = self
            .isolation
            .ok_or(HermesPlatformError::MissingIsolationService)?;
        let address = self
            .fingerprint
            .legacy_address()
            .ok_or(HermesPlatformError::IdentityMismatch)?;
        let mut state = self.state.lock();
        if state.domain.active {
            return Err(HermesPlatformError::ResourcesStillLive);
        }
        let generation = next_generation(state.domain.generation)?;
        let backend = isolation
            .isolate_device(address)
            .map_err(HermesPlatformError::Backend)?;
        state.domain = DomainRecord {
            backend,
            generation,
            active: true,
        };
        if backend == 0 {
            return Err(HermesPlatformError::Backend(
                sisyphus_driver_abi::STATUS_UNSUPPORTED,
            ));
        }
        Ok(DomainLease { generation })
    }

    pub fn map_bar(
        &self,
        domain: DomainLease,
        bar: u8,
        minimum_length: u64,
    ) -> Result<MappedBar, HermesPlatformError> {
        let service = self.mmio.ok_or(HermesPlatformError::MissingMmioService)?;
        let evidence = *self
            .fingerprint
            .bars
            .get(usize::from(bar))
            .ok_or(HermesPlatformError::InvalidBar)?;
        if evidence.flags & BAR_PRESENT == 0
            || evidence.flags & BAR_IO != 0
            || evidence.length == 0
            || !evidence.length.is_power_of_two()
            || minimum_length == 0
            || minimum_length > evidence.length
        {
            return Err(HermesPlatformError::BarEvidenceMismatch);
        }
        let physical = bar_physical_address(evidence.raw_low, evidence.raw_high, evidence.flags)?;
        if physical % evidence.length != 0 {
            return Err(HermesPlatformError::BarEvidenceMismatch);
        }
        let length = usize::try_from(evidence.length)
            .map_err(|_| HermesPlatformError::BarEvidenceMismatch)?;

        let mut state = self.state.lock();
        state.domain(domain)?;
        let record = &mut state.bars[usize::from(bar)];
        if record.active {
            return Err(HermesPlatformError::ResourcesStillLive);
        }
        let generation = next_generation(record.generation)?;
        let mapping = service
            .map(physical, length, 0)
            .map_err(HermesPlatformError::Backend)?;
        *record = MmioRecord {
            backend: mapping.handle,
            pointer: mapping.pointer.as_ptr() as usize,
            length: evidence.length,
            generation,
            active: true,
        };
        if mapping.handle == 0 {
            return Err(HermesPlatformError::Backend(
                sisyphus_driver_abi::STATUS_UNSUPPORTED,
            ));
        }
        Ok(MappedBar {
            lease: MmioLease {
                slot: bar,
                generation,
            },
            bar,
            length: evidence.length,
        })
    }

    pub fn read32(&self, lease: MmioLease, offset: u32) -> Result<u32, HermesPlatformError> {
        let state = self.state.lock();
        let record = mmio_record(&state, lease)?;
        let end = u64::from(offset)
            .checked_add(4)
            .ok_or(HermesPlatformError::InvalidRequest)?;
        if offset & 3 != 0 || end > record.length {
            return Err(HermesPlatformError::InvalidRequest);
        }
        let pointer = record
            .pointer
            .checked_add(offset as usize)
            .ok_or(HermesPlatformError::InvalidRequest)? as *const u32;
        // SAFETY: The live MMIO lease owns this aligned, in-bounds register.
        Ok(unsafe { pointer.read_volatile() })
    }

    pub fn write32(
        &self,
        lease: MmioLease,
        offset: u32,
        value: u32,
    ) -> Result<(), HermesPlatformError> {
        let state = self.state.lock();
        let record = mmio_record(&state, lease)?;
        let end = u64::from(offset)
            .checked_add(4)
            .ok_or(HermesPlatformError::InvalidRequest)?;
        if offset & 3 != 0 || end > record.length {
            return Err(HermesPlatformError::InvalidRequest);
        }
        let pointer = record
            .pointer
            .checked_add(offset as usize)
            .ok_or(HermesPlatformError::InvalidRequest)? as *mut u32;
        // SAFETY: The live MMIO lease owns this aligned, in-bounds register.
        unsafe { pointer.write_volatile(value) };
        Ok(())
    }

    pub fn revoke_bar(&self, lease: MmioLease) -> Result<(), HermesPlatformError> {
        let service = self.mmio.ok_or(HermesPlatformError::MissingMmioService)?;
        let mut state = self.state.lock();
        let record = mmio_record_mut(&mut state, lease)?;
        let status = service.unmap(record.backend);
        if status != STATUS_OK {
            return Err(HermesPlatformError::Backend(status));
        }
        record.active = false;
        record.pointer = 0;
        record.length = 0;
        Ok(())
    }

    pub fn allocate_dma(
        &self,
        domain: DomainLease,
        length: usize,
        alignment: usize,
        purpose: DmaPurpose,
    ) -> Result<AllocatedDma, HermesPlatformError> {
        let service = self.dma.ok_or(HermesPlatformError::MissingDmaService)?;
        if length == 0 || !alignment.is_power_of_two() || !service.supports(purpose) {
            return Err(HermesPlatformError::InvalidRequest);
        }
        let mut state = self.state.lock();
        let backend_domain = state.domain(domain)?;
        let slot = state
            .dma
            .iter()
            .position(|record| !record.active && record.generation != u32::MAX)
            .ok_or(HermesPlatformError::LeaseCapacity)?;
        let allocation = service
            .allocate(backend_domain, length, alignment, purpose)
            .map_err(HermesPlatformError::Backend)?;
        let generation = next_generation(state.dma[slot].generation)?;
        state.dma[slot] = DmaRecord {
            backend: allocation.handle,
            device_address: allocation.device_address,
            length,
            alignment,
            purpose,
            generation,
            active: true,
        };
        if allocation.handle == 0
            || allocation.device_address == 0
            || allocation.device_address % alignment as u64 != 0
        {
            return Err(HermesPlatformError::InvalidRequest);
        }
        Ok(AllocatedDma {
            lease: DmaLease {
                slot: slot as u8,
                generation,
            },
            device_address: allocation.device_address,
            length,
            alignment,
            purpose,
        })
    }

    pub fn dma_write(
        &self,
        lease: DmaLease,
        offset: usize,
        bytes: &[u8],
    ) -> Result<(), HermesPlatformError> {
        let service = self.dma.ok_or(HermesPlatformError::MissingDmaService)?;
        let state = self.state.lock();
        let record = dma_record(&state, lease)?;
        validate_span(record.length, offset, bytes.len())?;
        status(service.write(record.backend, offset, bytes))
    }

    pub fn dma_read(
        &self,
        lease: DmaLease,
        offset: usize,
        bytes: &mut [u8],
    ) -> Result<(), HermesPlatformError> {
        let service = self.dma.ok_or(HermesPlatformError::MissingDmaService)?;
        let state = self.state.lock();
        let record = dma_record(&state, lease)?;
        validate_span(record.length, offset, bytes.len())?;
        status(service.read(record.backend, offset, bytes))
    }

    pub fn dma_publish(
        &self,
        lease: DmaLease,
        offset: usize,
        length: usize,
    ) -> Result<(), HermesPlatformError> {
        self.dma_visibility(lease, offset, length, true)
    }

    pub fn dma_acquire(
        &self,
        lease: DmaLease,
        offset: usize,
        length: usize,
    ) -> Result<(), HermesPlatformError> {
        self.dma_visibility(lease, offset, length, false)
    }

    fn dma_visibility(
        &self,
        lease: DmaLease,
        offset: usize,
        length: usize,
        publish: bool,
    ) -> Result<(), HermesPlatformError> {
        let service = self.dma.ok_or(HermesPlatformError::MissingDmaService)?;
        let state = self.state.lock();
        let record = dma_record(&state, lease)?;
        validate_span(record.length, offset, length)?;
        status(if publish {
            service.publish(record.backend, offset, length)
        } else {
            service.acquire(record.backend, offset, length)
        })
    }

    pub fn revoke_dma(&self, lease: DmaLease) -> Result<(), HermesPlatformError> {
        let service = self.dma.ok_or(HermesPlatformError::MissingDmaService)?;
        let mut state = self.state.lock();
        let domain = state.domain.backend;
        let record = dma_record_mut(&mut state, lease)?;
        let release_status = service.release(domain, record.backend);
        if release_status != STATUS_OK {
            return Err(HermesPlatformError::Backend(release_status));
        }
        record.active = false;
        record.length = 0;
        record.device_address = 0;
        record.alignment = 0;
        Ok(())
    }

    /// Registers the fingerprinted legacy interrupt. The caller must keep
    /// `driver_context` live until the returned lease is revoked.
    pub unsafe fn register_irq(
        &self,
        domain: DomainLease,
        handler: IrqHandler,
        driver_context: *mut c_void,
    ) -> Result<IrqLease, HermesPlatformError> {
        let service = self.irq.ok_or(HermesPlatformError::MissingIrqService)?;
        if self.fingerprint.interrupt_pin == 0 || self.fingerprint.interrupt_line == u8::MAX {
            return Err(HermesPlatformError::InvalidRequest);
        }
        let mut state = self.state.lock();
        state.domain(domain)?;
        if state.irq.active {
            return Err(HermesPlatformError::ResourcesStillLive);
        }
        let generation = next_generation(state.irq.generation)?;
        let backend = service
            .register(
                u32::from(self.fingerprint.interrupt_line),
                0,
                handler,
                driver_context,
            )
            .map_err(HermesPlatformError::Backend)?;
        if backend == 0 {
            return Err(HermesPlatformError::InvalidRequest);
        }
        state.irq = IrqRecord {
            backend,
            generation,
            enabled: false,
            active: true,
        };
        let enabled = service.set_enabled(backend, true);
        if enabled != STATUS_OK {
            if service.unregister(backend) == STATUS_OK {
                state.irq.active = false;
            }
            return Err(HermesPlatformError::Backend(enabled));
        }
        state.irq.enabled = true;
        Ok(IrqLease { generation })
    }

    pub fn revoke_irq(&self, lease: IrqLease) -> Result<(), HermesPlatformError> {
        let service = self.irq.ok_or(HermesPlatformError::MissingIrqService)?;
        let mut state = self.state.lock();
        if !state.irq.active || state.irq.generation != lease.generation {
            return Err(HermesPlatformError::StaleLease);
        }
        if state.irq.enabled {
            let disabled = service.set_enabled(state.irq.backend, false);
            if disabled != STATUS_OK {
                return Err(HermesPlatformError::Backend(disabled));
            }
            state.irq.enabled = false;
        }
        let removed = service.unregister(state.irq.backend);
        if removed != STATUS_OK {
            return Err(HermesPlatformError::Backend(removed));
        }
        state.irq.active = false;
        Ok(())
    }

    /// Attempts reverse-order revocation. Failed resources remain live in the
    /// ledger and are retried by the next call with the same domain lease.
    pub fn cleanup(&self, domain: DomainLease) -> Result<(), CleanupReport> {
        let mut report = CleanupReport {
            pending_bars: 0,
            pending_dma: 0,
            pending_irq: false,
            pending_domain: false,
            first_status: STATUS_OK,
        };
        let mut state = self.state.lock();
        let backend_domain = match state.domain(domain) {
            Ok(handle) => handle,
            Err(_) => {
                report.pending_domain = true;
                report.first_status = sisyphus_driver_abi::STATUS_NOT_FOUND;
                return Err(report);
            }
        };

        if state.irq.active {
            let service = self.irq;
            let result = service.map_or(sisyphus_driver_abi::STATUS_UNSUPPORTED, |irq| {
                if state.irq.enabled {
                    let disabled = irq.set_enabled(state.irq.backend, false);
                    if disabled != STATUS_OK {
                        return disabled;
                    }
                    state.irq.enabled = false;
                }
                irq.unregister(state.irq.backend)
            });
            if result == STATUS_OK {
                state.irq.active = false;
            } else {
                report.pending_irq = true;
                record_status(&mut report, result);
            }
        }

        for index in (0..state.dma.len()).rev() {
            if !state.dma[index].active {
                continue;
            }
            let result = self
                .dma
                .map_or(sisyphus_driver_abi::STATUS_UNSUPPORTED, |dma| {
                    dma.release(backend_domain, state.dma[index].backend)
                });
            if result == STATUS_OK {
                state.dma[index].active = false;
            } else {
                report.pending_dma |= 1 << index;
                record_status(&mut report, result);
            }
        }

        for index in (0..state.bars.len()).rev() {
            if !state.bars[index].active {
                continue;
            }
            let result = self
                .mmio
                .map_or(sisyphus_driver_abi::STATUS_UNSUPPORTED, |mmio| {
                    mmio.unmap(state.bars[index].backend)
                });
            if result == STATUS_OK {
                state.bars[index].active = false;
                state.bars[index].pointer = 0;
            } else {
                report.pending_bars |= 1 << index;
                record_status(&mut report, result);
            }
        }

        if report.pending_irq || report.pending_dma != 0 || report.pending_bars != 0 {
            report.pending_domain = true;
            return Err(report);
        }
        let isolation = match self.isolation {
            Some(service) => service,
            None => {
                report.pending_domain = true;
                record_status(&mut report, sisyphus_driver_abi::STATUS_UNSUPPORTED);
                return Err(report);
            }
        };
        let released = isolation.release_domain(backend_domain);
        if released != STATUS_OK {
            report.pending_domain = true;
            record_status(&mut report, released);
            return Err(report);
        }
        state.domain.active = false;
        Ok(())
    }

    pub fn retry_cleanup(&self) -> Result<(), CleanupReport> {
        let domain = {
            let state = self.state.lock();
            DomainLease {
                generation: state.domain.generation,
            }
        };
        self.cleanup(domain)
    }
}

impl HermesPlatform for KernelHermesPlatform<'_> {
    type Domain = DomainLease;
    type Mmio = MmioLease;
    type Dma = DmaLease;

    fn isolate_device(&self, identity: HermesPciIdentity) -> Result<Self::Domain, HermesFault> {
        self.acquire_domain(identity)
            .map_err(|_| HermesFault::DeviceIsolation)
    }

    fn release_domain(&self, domain: Self::Domain) {
        let _ = self.cleanup(domain);
    }

    fn map_bar(
        &self,
        domain: Self::Domain,
        bar: u8,
        minimum_length: u64,
    ) -> Result<HermesMmioWindow<Self::Mmio>, HermesFault> {
        let mapped = KernelHermesPlatform::map_bar(self, domain, bar, minimum_length)
            .map_err(|_| HermesFault::BarUnavailable)?;
        Ok(HermesMmioWindow {
            handle: mapped.lease,
            bar: mapped.bar,
            length: mapped.length,
        })
    }

    fn unmap_bar(&self, window: HermesMmioWindow<Self::Mmio>) {
        if window.bar == window.handle.slot {
            let _ = self.revoke_bar(window.handle);
        }
    }

    fn read32(
        &self,
        window: HermesMmioWindow<Self::Mmio>,
        offset: u32,
    ) -> Result<u32, HermesFault> {
        self.validate_mmio_window(window)
            .map_err(|_| HermesFault::MmioRead)?;
        KernelHermesPlatform::read32(self, window.handle, offset).map_err(|_| HermesFault::MmioRead)
    }

    fn write32(
        &self,
        window: HermesMmioWindow<Self::Mmio>,
        offset: u32,
        value: u32,
    ) -> Result<(), HermesFault> {
        self.validate_mmio_window(window)
            .map_err(|_| HermesFault::MmioWrite)?;
        KernelHermesPlatform::write32(self, window.handle, offset, value)
            .map_err(|_| HermesFault::MmioWrite)
    }

    fn io_fence(&self) -> Result<(), HermesFault> {
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    fn allocate_dma(
        &self,
        domain: Self::Domain,
        length: usize,
        alignment: usize,
        purpose: DmaPurpose,
    ) -> Result<HermesDmaRegion<Self::Dma>, HermesFault> {
        let allocation =
            KernelHermesPlatform::allocate_dma(self, domain, length, alignment, purpose)
                .map_err(|_| HermesFault::DmaAllocation)?;
        Ok(HermesDmaRegion {
            handle: allocation.lease,
            device_address: allocation.device_address,
            length: allocation.length,
            alignment: allocation.alignment,
            purpose: allocation.purpose,
        })
    }

    fn release_dma(&self, region: HermesDmaRegion<Self::Dma>) {
        if self.validate_dma_region(region).is_ok() {
            let _ = self.revoke_dma(region.handle);
        }
    }

    fn dma_write(
        &self,
        region: HermesDmaRegion<Self::Dma>,
        offset: usize,
        bytes: &[u8],
    ) -> Result<(), HermesFault> {
        self.validate_dma_region(region)
            .map_err(|_| HermesFault::DmaAccess)?;
        KernelHermesPlatform::dma_write(self, region.handle, offset, bytes)
            .map_err(|_| HermesFault::DmaAccess)
    }

    fn dma_read(
        &self,
        region: HermesDmaRegion<Self::Dma>,
        offset: usize,
        bytes: &mut [u8],
    ) -> Result<(), HermesFault> {
        self.validate_dma_region(region)
            .map_err(|_| HermesFault::DmaAccess)?;
        KernelHermesPlatform::dma_read(self, region.handle, offset, bytes)
            .map_err(|_| HermesFault::DmaAccess)
    }

    fn dma_publish(
        &self,
        region: HermesDmaRegion<Self::Dma>,
        offset: usize,
        length: usize,
    ) -> Result<(), HermesFault> {
        self.validate_dma_region(region)
            .map_err(|_| HermesFault::DmaAccess)?;
        KernelHermesPlatform::dma_publish(self, region.handle, offset, length)
            .map_err(|_| HermesFault::DmaAccess)
    }

    fn dma_acquire(
        &self,
        region: HermesDmaRegion<Self::Dma>,
        offset: usize,
        length: usize,
    ) -> Result<(), HermesFault> {
        self.validate_dma_region(region)
            .map_err(|_| HermesFault::DmaAccess)?;
        KernelHermesPlatform::dma_acquire(self, region.handle, offset, length)
            .map_err(|_| HermesFault::DmaAccess)
    }

    fn now_tick(&self) -> u64 {
        self.clock.monotonic_ns()
    }

    fn relax(&self) {
        core::hint::spin_loop();
    }
}

impl KernelHermesPlatform<'_> {
    fn validate_mmio_window(
        &self,
        window: HermesMmioWindow<MmioLease>,
    ) -> Result<(), HermesPlatformError> {
        let state = self.state.lock();
        let record = mmio_record(&state, window.handle)?;
        if window.bar == window.handle.slot && window.length == record.length {
            Ok(())
        } else {
            Err(HermesPlatformError::StaleLease)
        }
    }

    fn validate_dma_region(
        &self,
        region: HermesDmaRegion<DmaLease>,
    ) -> Result<(), HermesPlatformError> {
        let state = self.state.lock();
        let record = dma_record(&state, region.handle)?;
        if region.device_address == record.device_address
            && region.length == record.length
            && region.alignment == record.alignment
            && region.purpose == record.purpose
        {
            Ok(())
        } else {
            Err(HermesPlatformError::StaleLease)
        }
    }
}

fn identity_matches(fingerprint: &GpuFingerprint, identity: HermesPciIdentity) -> bool {
    fingerprint.segment == identity.segment
        && fingerprint.bus == identity.bus
        && fingerprint.slot == identity.slot
        && fingerprint.function == identity.function
        && fingerprint.vendor_id == identity.vendor_id
        && fingerprint.device_id == identity.device_id
        && fingerprint.subsystem_vendor_id == identity.subsystem_vendor_id
        && fingerprint.subsystem_device_id == identity.subsystem_device_id
        && fingerprint.revision == identity.revision
        && fingerprint.class_code == identity.class_code
        && fingerprint.subclass == identity.subclass
        && fingerprint.programming_interface == identity.programming_interface
}

fn bar_physical_address(
    raw_low: u32,
    raw_high: u32,
    flags: u8,
) -> Result<u64, HermesPlatformError> {
    if flags & BAR_PRESENT == 0 || flags & BAR_IO != 0 {
        return Err(HermesPlatformError::BarEvidenceMismatch);
    }
    let memory_type = (raw_low >> 1) & 0x3;
    let claims_64_bit = flags & BAR_64BIT != 0;
    if raw_low & 1 != 0 || memory_type == 3 || claims_64_bit != (memory_type == 2) {
        return Err(HermesPlatformError::BarEvidenceMismatch);
    }
    let low = u64::from(raw_low & 0xffff_fff0);
    let address = if flags & BAR_64BIT != 0 {
        (u64::from(raw_high) << 32) | low
    } else {
        if raw_high != 0 {
            return Err(HermesPlatformError::BarEvidenceMismatch);
        }
        low
    };
    if address == 0 {
        Err(HermesPlatformError::BarEvidenceMismatch)
    } else {
        Ok(address)
    }
}

fn mmio_record(state: &LeaseState, lease: MmioLease) -> Result<MmioRecord, HermesPlatformError> {
    let record = state
        .bars
        .get(usize::from(lease.slot))
        .copied()
        .ok_or(HermesPlatformError::StaleLease)?;
    if record.active && record.generation == lease.generation {
        Ok(record)
    } else {
        Err(HermesPlatformError::StaleLease)
    }
}

fn mmio_record_mut(
    state: &mut LeaseState,
    lease: MmioLease,
) -> Result<&mut MmioRecord, HermesPlatformError> {
    let record = state
        .bars
        .get_mut(usize::from(lease.slot))
        .ok_or(HermesPlatformError::StaleLease)?;
    if record.active && record.generation == lease.generation {
        Ok(record)
    } else {
        Err(HermesPlatformError::StaleLease)
    }
}

fn dma_record(state: &LeaseState, lease: DmaLease) -> Result<DmaRecord, HermesPlatformError> {
    let record = state
        .dma
        .get(usize::from(lease.slot))
        .copied()
        .ok_or(HermesPlatformError::StaleLease)?;
    if record.active && record.generation == lease.generation {
        Ok(record)
    } else {
        Err(HermesPlatformError::StaleLease)
    }
}

fn dma_record_mut(
    state: &mut LeaseState,
    lease: DmaLease,
) -> Result<&mut DmaRecord, HermesPlatformError> {
    let record = state
        .dma
        .get_mut(usize::from(lease.slot))
        .ok_or(HermesPlatformError::StaleLease)?;
    if record.active && record.generation == lease.generation {
        Ok(record)
    } else {
        Err(HermesPlatformError::StaleLease)
    }
}

fn validate_span(total: usize, offset: usize, length: usize) -> Result<(), HermesPlatformError> {
    if length == 0 || offset.checked_add(length).is_none_or(|end| end > total) {
        Err(HermesPlatformError::InvalidRequest)
    } else {
        Ok(())
    }
}

fn next_generation(current: u32) -> Result<u32, HermesPlatformError> {
    current
        .checked_add(1)
        .ok_or(HermesPlatformError::LeaseCapacity)
}

fn status(value: Status) -> Result<(), HermesPlatformError> {
    if value == STATUS_OK {
        Ok(())
    } else {
        Err(HermesPlatformError::Backend(value))
    }
}

fn record_status(report: &mut CleanupReport, value: Status) {
    if report.first_status == STATUS_OK {
        report.first_status = value;
    }
}

#[cfg(test)]
mod tests {
    use core::ptr::NonNull;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use sisyphus_driver_abi::STATUS_BUSY;

    use super::*;
    use crate::drivers::drivernet::fingerprint::{BarEvidence, TOPOLOGY_IOMMU_ISOLATED};
    use crate::hw::pci::PciAddress;
    use crate::shim::MmioMapping;

    struct Services {
        fail_unmap: AtomicBool,
        fail_dma_release: AtomicBool,
        invalid_dma_address: AtomicBool,
        domain_releases: AtomicUsize,
    }

    impl Services {
        const fn new() -> Self {
            Self {
                fail_unmap: AtomicBool::new(false),
                fail_dma_release: AtomicBool::new(false),
                invalid_dma_address: AtomicBool::new(false),
                domain_releases: AtomicUsize::new(0),
            }
        }
    }

    impl DmaRemappingBackend for Services {
        fn isolate_device(&self, _device: PciAddress) -> Result<Handle, Status> {
            Ok(7)
        }

        fn map(
            &self,
            _domain: Handle,
            _device_address: u64,
            _physical_address: u64,
            _length: usize,
            _access: crate::hw::iommu::DmaAccess,
        ) -> Status {
            STATUS_OK
        }

        fn unmap(&self, _domain: Handle, _device_address: u64, _length: usize) -> Status {
            STATUS_OK
        }

        fn release_domain(&self, _domain: Handle) -> Status {
            self.domain_releases.fetch_add(1, Ordering::Relaxed);
            STATUS_OK
        }
    }

    impl ClockService for Services {
        fn monotonic_ns(&self) -> u64 {
            1_000_000
        }
    }

    impl MmioService for Services {
        fn map(
            &self,
            _physical_address: u64,
            _length: usize,
            _flags: u64,
        ) -> Result<MmioMapping, Status> {
            Ok(MmioMapping {
                handle: 11,
                pointer: NonNull::dangling(),
            })
        }

        fn unmap(&self, _mapping: Handle) -> Status {
            if self.fail_unmap.load(Ordering::Relaxed) {
                STATUS_BUSY
            } else {
                STATUS_OK
            }
        }
    }

    impl HermesDomainDmaService for Services {
        fn supports(&self, _purpose: DmaPurpose) -> bool {
            true
        }

        fn allocate(
            &self,
            _domain: Handle,
            _length: usize,
            _alignment: usize,
            _purpose: DmaPurpose,
        ) -> Result<DmaAllocation, Status> {
            Ok(DmaAllocation {
                handle: 12,
                cpu_pointer: NonNull::dangling(),
                device_address: if self.invalid_dma_address.load(Ordering::Relaxed) {
                    1
                } else {
                    0x4000
                },
            })
        }

        fn release(&self, _domain: Handle, _allocation: Handle) -> Status {
            if self.fail_dma_release.load(Ordering::Relaxed) {
                STATUS_BUSY
            } else {
                STATUS_OK
            }
        }

        fn write(&self, _allocation: Handle, _offset: usize, _bytes: &[u8]) -> Status {
            STATUS_OK
        }

        fn read(&self, _allocation: Handle, _offset: usize, _bytes: &mut [u8]) -> Status {
            STATUS_OK
        }

        fn publish(&self, _allocation: Handle, _offset: usize, _length: usize) -> Status {
            STATUS_OK
        }

        fn acquire(&self, _allocation: Handle, _offset: usize, _length: usize) -> Status {
            STATUS_OK
        }
    }

    fn fingerprint() -> GpuFingerprint {
        let mut fingerprint = GpuFingerprint::EMPTY;
        fingerprint.segment = 0;
        fingerprint.bus = 1;
        fingerprint.slot = 2;
        fingerprint.function = 0;
        fingerprint.vendor_id = 0x10de;
        fingerprint.device_id = 0x1234;
        fingerprint.class_code = 3;
        fingerprint.topology_flags = TOPOLOGY_IOMMU_ISOLATED;
        fingerprint.interrupt_line = 5;
        fingerprint.interrupt_pin = 1;
        fingerprint.bars[0] = BarEvidence {
            raw_low: 0x8000_0004,
            raw_high: 0,
            length: 0x2000,
            flags: BAR_PRESENT | BAR_64BIT,
        };
        fingerprint
    }

    fn identity(fingerprint: &GpuFingerprint) -> HermesPciIdentity {
        HermesPciIdentity {
            segment: fingerprint.segment,
            bus: fingerprint.bus,
            slot: fingerprint.slot,
            function: fingerprint.function,
            revision: fingerprint.revision,
            vendor_id: fingerprint.vendor_id,
            device_id: fingerprint.device_id,
            subsystem_vendor_id: fingerprint.subsystem_vendor_id,
            subsystem_device_id: fingerprint.subsystem_device_id,
            class_code: fingerprint.class_code,
            subclass: fingerprint.subclass,
            programming_interface: fingerprint.programming_interface,
            reserved: 0,
        }
    }

    #[test]
    fn service_visibility_is_exact() {
        let services = Services::new();
        let fingerprint = fingerprint();
        let platform = KernelHermesPlatform::new(fingerprint, None, None, None, None, &services);
        assert_eq!(
            platform.available_services(),
            HermesKernelServices {
                isolation: false,
                mmio: false,
                dma: false,
                irq: false,
                firmware_dma: false,
            }
        );
        assert_eq!(
            platform.acquire_domain(identity(&fingerprint)),
            Err(HermesPlatformError::MissingIsolationService)
        );
    }

    #[test]
    fn mismatched_identity_and_bar_evidence_fail_closed() {
        let services = Services::new();
        let fingerprint = fingerprint();
        let platform = KernelHermesPlatform::new(
            fingerprint,
            Some(&services),
            Some(&services),
            Some(&services),
            None,
            &services,
        );
        let mut wrong = identity(&fingerprint);
        wrong.device_id ^= 1;
        assert_eq!(
            platform.acquire_domain(wrong),
            Err(HermesPlatformError::IdentityMismatch)
        );
        let domain = platform.acquire_domain(identity(&fingerprint)).unwrap();
        assert_eq!(
            platform.map_bar(domain, 0, 0x3000),
            Err(HermesPlatformError::BarEvidenceMismatch)
        );
        assert!(platform.cleanup(domain).is_ok());

        let mut malformed = fingerprint;
        malformed.bars[0].raw_low &= !0x6;
        let platform = KernelHermesPlatform::new(
            malformed,
            Some(&services),
            Some(&services),
            Some(&services),
            None,
            &services,
        );
        let domain = platform.acquire_domain(identity(&malformed)).unwrap();
        assert_eq!(
            platform.map_bar(domain, 0, 0x1000),
            Err(HermesPlatformError::BarEvidenceMismatch)
        );
        assert!(platform.cleanup(domain).is_ok());
    }

    #[test]
    fn failed_cleanup_preserves_leases_for_retry() {
        let services = Services::new();
        let fingerprint = fingerprint();
        let platform = KernelHermesPlatform::new(
            fingerprint,
            Some(&services),
            Some(&services),
            Some(&services),
            None,
            &services,
        );
        let domain = platform.acquire_domain(identity(&fingerprint)).unwrap();
        let bar = platform.map_bar(domain, 0, 0x1000).unwrap();
        let dma = platform
            .allocate_dma(domain, 4096, 4096, DmaPurpose::CommandRing)
            .unwrap();
        services.fail_unmap.store(true, Ordering::Relaxed);
        services.fail_dma_release.store(true, Ordering::Relaxed);

        let report = platform.cleanup(domain).unwrap_err();
        assert_eq!(report.pending_bars, 1);
        assert_eq!(report.pending_dma, 1);
        assert!(report.pending_domain);
        assert_eq!(services.domain_releases.load(Ordering::Relaxed), 0);
        assert_eq!(
            platform.revoke_bar(bar.lease),
            Err(HermesPlatformError::Backend(STATUS_BUSY))
        );
        assert_eq!(
            platform.revoke_dma(dma.lease),
            Err(HermesPlatformError::Backend(STATUS_BUSY))
        );

        services.fail_unmap.store(false, Ordering::Relaxed);
        services.fail_dma_release.store(false, Ordering::Relaxed);
        assert!(platform.cleanup(domain).is_ok());
        assert_eq!(services.domain_releases.load(Ordering::Relaxed), 1);
        assert_eq!(
            platform.revoke_bar(bar.lease),
            Err(HermesPlatformError::StaleLease)
        );
        assert_eq!(
            platform.revoke_dma(dma.lease),
            Err(HermesPlatformError::StaleLease)
        );
    }

    #[test]
    fn hermes_contract_rejects_forged_region_metadata_and_releases_exact_leases() {
        let services = Services::new();
        let fingerprint = fingerprint();
        let platform = KernelHermesPlatform::new(
            fingerprint,
            Some(&services),
            Some(&services),
            Some(&services),
            None,
            &services,
        );
        let contract: &dyn HermesPlatform<Domain = DomainLease, Mmio = MmioLease, Dma = DmaLease> =
            &platform;
        let domain = contract.isolate_device(identity(&fingerprint)).unwrap();
        let window = contract.map_bar(domain, 0, 0x1000).unwrap();
        let region = contract
            .allocate_dma(domain, 4096, 4096, DmaPurpose::CommandRing)
            .unwrap();
        let mut forged = region;
        forged.length += 4096;
        assert_eq!(
            contract.dma_publish(forged, 0, 4096),
            Err(HermesFault::DmaAccess)
        );
        assert_eq!(contract.now_tick(), 1_000_000);

        contract.release_dma(region);
        contract.unmap_bar(window);
        contract.release_domain(domain);
        assert_eq!(services.domain_releases.load(Ordering::Relaxed), 1);
        assert!(platform.retry_cleanup().is_err());
    }

    #[test]
    fn malformed_dma_receipt_is_quarantined_for_domain_cleanup() {
        let services = Services::new();
        services.invalid_dma_address.store(true, Ordering::Relaxed);
        let fingerprint = fingerprint();
        let platform = KernelHermesPlatform::new(
            fingerprint,
            Some(&services),
            Some(&services),
            Some(&services),
            None,
            &services,
        );
        let domain = platform.acquire_domain(identity(&fingerprint)).unwrap();
        assert_eq!(
            platform.allocate_dma(domain, 4096, 4096, DmaPurpose::CommandRing),
            Err(HermesPlatformError::InvalidRequest)
        );
        assert!(platform.cleanup(domain).is_ok());
        assert_eq!(services.domain_releases.load(Ordering::Relaxed), 1);
    }
}
