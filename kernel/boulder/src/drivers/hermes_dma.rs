//! Domain-bound, frame-backed DMA memory for Hermes.
//!
//! This service makes allocation and translation one ownership transaction:
//! physical pages come independently from Boulder's global frame ledger,
//! device addresses come from one contiguous IOVA lease, and a lease is
//! published only after the remapping backend confirms every page.  This
//! deliberately removes any multi-page physical-contiguity requirement.

use core::ptr::{self, NonNull};
use core::sync::atomic::{fence, Ordering};

use abyss::paging::{PhysicalAddress, PAGE_SIZE};
use sisyphus_driver_abi::{
    Handle, Status, STATUS_BUSY, STATUS_INVALID_ARGUMENT, STATUS_IO_ERROR, STATUS_NOT_FOUND,
    STATUS_OK,
};

use super::hermes_gsp::DmaPurpose;
use super::hermes_platform::HermesDomainDmaService;
use crate::capability::{Capability, PhysicalMemoryControl};
use crate::hw::iommu::{DmaAccess, DmaRemappingBackend};
use crate::hw::iova::{IovaLease, IovaLedger, IovaRange};
use crate::memory::frame_pool::PhysicalFramePool;
use crate::shim::DmaAllocation;
use crate::sync::SpinLock;

const MAXIMUM_HERMES_DMA_LEASES: usize = 8;
const MAXIMUM_RESERVED_IOVA_RANGES: usize = 16;
/// Total scatter pages retained by one Hermes DMA service (2 MiB at 4 KiB).
///
/// This is an explicit deterministic capacity rather than a hidden heap
/// allocation. Firmware larger than this capacity must be streamed by a
/// future Hermes protocol extension instead of silently demanding contiguous
/// physical RAM.
pub const MAXIMUM_HERMES_DMA_PAGES: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AllocationPhase {
    Empty,
    Mapping,
    Mapped,
    ReclaimingMappings,
    HardwareUnmapped,
    IovaReleased,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PagePhase {
    Empty,
    FrameOwned,
    Mapped,
}

#[derive(Clone, Copy)]
struct PageRecord {
    owner_slot: u16,
    owner_generation: u32,
    page_index: u16,
    frame: PhysicalAddress,
    phase: PagePhase,
}

impl PageRecord {
    const EMPTY: Self = Self {
        owner_slot: 0,
        owner_generation: 0,
        page_index: 0,
        frame: PhysicalAddress::new(0),
        phase: PagePhase::Empty,
    };
}

#[derive(Clone, Copy)]
struct AllocationRecord {
    generation: u32,
    phase: AllocationPhase,
    domain: Handle,
    resident_pages: usize,
    requested_length: usize,
    iova: IovaLease,
    device_address: u64,
}

impl AllocationRecord {
    const EMPTY: Self = Self {
        generation: 0,
        phase: AllocationPhase::Empty,
        domain: 0,
        resident_pages: 0,
        requested_length: 0,
        iova: IovaLease::INVALID,
        device_address: 0,
    };
}

struct DmaState {
    iovas: IovaLedger<MAXIMUM_HERMES_DMA_LEASES, MAXIMUM_RESERVED_IOVA_RANGES>,
    records: [AllocationRecord; MAXIMUM_HERMES_DMA_LEASES],
    pages: [PageRecord; MAXIMUM_HERMES_DMA_PAGES],
    poisoned: bool,
}

/// Coherent x86 DMA storage bound to one remapping backend.
#[must_use = "the DMA service retains physical frames and IOVA leases until every allocation is released"]
pub struct FrameBackedHermesDma<'a, 'storage> {
    frames: &'a PhysicalFramePool<'storage>,
    backend: &'a dyn DmaRemappingBackend,
    direct_map_base: usize,
    mapped_physical_limit: u64,
    state: SpinLock<DmaState>,
}

impl<'a, 'storage> FrameBackedHermesDma<'a, 'storage> {
    /// Creates a domain-aware DMA allocator over the stable direct map.
    ///
    /// # Safety
    ///
    /// Every physical frame managed by `frames` must have exactly one stable,
    /// writable, cache-coherent CPU alias at
    /// `direct_map_base + physical_address`. The target platform must provide
    /// coherent device/CPU visibility with the fences used by this x86 kernel.
    pub unsafe fn new(
        frames: &'a PhysicalFramePool<'storage>,
        backend: &'a dyn DmaRemappingBackend,
        aperture: IovaRange,
        reserved: &[IovaRange],
        direct_map_base: usize,
        mapped_physical_limit: u64,
        _authority: &Capability<'_, PhysicalMemoryControl>,
    ) -> Result<Self, Status> {
        let managed_bytes = (frames.managed_frames() as u64)
            .checked_mul(PAGE_SIZE as u64)
            .ok_or(STATUS_INVALID_ARGUMENT)?;
        if managed_bytes > mapped_physical_limit
            || usize::try_from(mapped_physical_limit)
                .ok()
                .and_then(|length| direct_map_base.checked_add(length))
                .is_none()
        {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        let iovas = IovaLedger::new(aperture, reserved).map_err(|_| STATUS_INVALID_ARGUMENT)?;
        Ok(Self {
            frames,
            backend,
            direct_map_base,
            mapped_physical_limit,
            state: SpinLock::new(DmaState {
                iovas,
                records: [AllocationRecord::EMPTY; MAXIMUM_HERMES_DMA_LEASES],
                pages: [PageRecord::EMPTY; MAXIMUM_HERMES_DMA_PAGES],
                poisoned: false,
            }),
        })
    }

    pub fn active_allocation_count(&self) -> usize {
        self.state
            .lock()
            .records
            .iter()
            .filter(|record| record.phase != AllocationPhase::Empty)
            .count()
    }

    pub fn quarantined_allocation_count(&self) -> usize {
        self.state
            .lock()
            .records
            .iter()
            .filter(|record| {
                matches!(
                    record.phase,
                    AllocationPhase::Mapping | AllocationPhase::ReclaimingMappings
                )
            })
            .count()
    }

    fn pointer(&self, first_frame: PhysicalAddress) -> Result<*mut u8, Status> {
        if first_frame
            .as_u64()
            .checked_add(PAGE_SIZE as u64)
            .is_none_or(|end| end > self.mapped_physical_limit)
        {
            return Err(STATUS_IO_ERROR);
        }
        let physical = usize::try_from(first_frame.as_u64()).map_err(|_| STATUS_IO_ERROR)?;
        self.direct_map_base
            .checked_add(physical)
            .map(|address| address as *mut u8)
            .ok_or(STATUS_IO_ERROR)
    }

    fn record<'guard>(
        records: &'guard [AllocationRecord; MAXIMUM_HERMES_DMA_LEASES],
        handle: Handle,
    ) -> Result<(usize, &'guard AllocationRecord), Status> {
        let (slot, generation) = decode_handle(handle).ok_or(STATUS_NOT_FOUND)?;
        let record = records.get(slot).ok_or(STATUS_NOT_FOUND)?;
        if record.phase == AllocationPhase::Empty || record.generation != generation {
            Err(STATUS_NOT_FOUND)
        } else {
            Ok((slot, record))
        }
    }

    fn page_slot(
        pages: &[PageRecord; MAXIMUM_HERMES_DMA_PAGES],
        owner_slot: usize,
        owner_generation: u32,
        page_index: usize,
    ) -> Option<usize> {
        let owner_slot = u16::try_from(owner_slot + 1).ok()?;
        let page_index = u16::try_from(page_index).ok()?;
        pages.iter().position(|page| {
            page.phase != PagePhase::Empty
                && page.owner_slot == owner_slot
                && page.owner_generation == owner_generation
                && page.page_index == page_index
        })
    }

    fn reclaim_record(&self, state: &mut DmaState, slot: usize) -> Status {
        let mut record = state.records[slot];
        record.phase = AllocationPhase::ReclaimingMappings;
        state.records[slot] = record;

        for page_index in (0..record.resident_pages).rev() {
            let Some(page_slot) =
                Self::page_slot(&state.pages, slot, record.generation, page_index)
            else {
                state.poisoned = true;
                return STATUS_IO_ERROR;
            };
            if state.pages[page_slot].phase == PagePhase::Mapped {
                let Some(device_address) = record
                    .device_address
                    .checked_add(page_index as u64 * PAGE_SIZE as u64)
                else {
                    state.poisoned = true;
                    return STATUS_IO_ERROR;
                };
                let status = self.backend.unmap(record.domain, device_address, PAGE_SIZE);
                if status != STATUS_OK {
                    return status;
                }
                state.pages[page_slot].phase = PagePhase::FrameOwned;
            }
        }
        record.phase = AllocationPhase::HardwareUnmapped;
        state.records[slot] = record;

        if state.iovas.release(record.iova).is_err() {
            state.poisoned = true;
            return STATUS_IO_ERROR;
        }
        record.phase = AllocationPhase::IovaReleased;
        state.records[slot] = record;

        for page_index in (0..record.resident_pages).rev() {
            let Some(page_slot) =
                Self::page_slot(&state.pages, slot, record.generation, page_index)
            else {
                state.poisoned = true;
                return STATUS_IO_ERROR;
            };
            let frame = state.pages[page_slot].frame;
            if self.frames.release(frame).is_err() {
                return STATUS_IO_ERROR;
            }
            state.pages[page_slot] = PageRecord::EMPTY;
        }
        state.records[slot] = AllocationRecord {
            generation: record.generation,
            ..AllocationRecord::EMPTY
        };
        STATUS_OK
    }

    fn reclaim_quarantined(&self, state: &mut DmaState) -> Status {
        for slot in 0..MAXIMUM_HERMES_DMA_LEASES {
            if matches!(
                state.records[slot].phase,
                AllocationPhase::Mapping | AllocationPhase::ReclaimingMappings
            ) {
                let status = self.reclaim_record(state, slot);
                if status != STATUS_OK {
                    return status;
                }
            }
        }
        STATUS_OK
    }

    fn transfer(
        &self,
        allocation: Handle,
        offset: usize,
        bytes: *mut u8,
        length: usize,
        write: bool,
    ) -> Status {
        let state = self.state.lock();
        let Ok((slot, record)) = Self::record(&state.records, allocation) else {
            return STATUS_NOT_FOUND;
        };
        if record.phase != AllocationPhase::Mapped
            || offset
                .checked_add(length)
                .is_none_or(|end| end > record.requested_length)
        {
            return STATUS_INVALID_ARGUMENT;
        }
        let mut transferred = 0;
        while transferred < length {
            let absolute = offset + transferred;
            let page_index = absolute / PAGE_SIZE;
            let page_offset = absolute % PAGE_SIZE;
            let chunk = (PAGE_SIZE - page_offset).min(length - transferred);
            let Some(page_slot) =
                Self::page_slot(&state.pages, slot, record.generation, page_index)
            else {
                return STATUS_IO_ERROR;
            };
            if state.pages[page_slot].phase != PagePhase::Mapped {
                return STATUS_IO_ERROR;
            }
            let Ok(page_pointer) = self.pointer(state.pages[page_slot].frame) else {
                return STATUS_IO_ERROR;
            };
            // SAFETY: `bytes` names the caller-validated slice, the page is
            // exclusively owned by this live lease, and both chunk spans were
            // bounded above. Source and destination cannot overlap because
            // one side is the private DMA frame and the other is caller data.
            unsafe {
                if write {
                    ptr::copy_nonoverlapping(
                        bytes.add(transferred),
                        page_pointer.add(page_offset),
                        chunk,
                    );
                } else {
                    ptr::copy_nonoverlapping(
                        page_pointer.add(page_offset),
                        bytes.add(transferred),
                        chunk,
                    );
                }
            }
            transferred += chunk;
        }
        STATUS_OK
    }
}

impl HermesDomainDmaService for FrameBackedHermesDma<'_, '_> {
    fn supports(&self, _purpose: DmaPurpose) -> bool {
        true
    }

    fn allocate(
        &self,
        domain: Handle,
        length: usize,
        alignment: usize,
        _purpose: DmaPurpose,
    ) -> Result<DmaAllocation, Status> {
        if domain == 0 || length == 0 || alignment == 0 || !alignment.is_power_of_two() {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        let mapped_length = length
            .checked_add(PAGE_SIZE - 1)
            .map(|value| value & !(PAGE_SIZE - 1))
            .ok_or(STATUS_INVALID_ARGUMENT)?;
        let frame_count = mapped_length / PAGE_SIZE;
        if frame_count > MAXIMUM_HERMES_DMA_PAGES || frame_count > u16::MAX as usize {
            return Err(STATUS_BUSY);
        }
        let dma_alignment = alignment.max(PAGE_SIZE);
        let alignment_frames = dma_alignment / PAGE_SIZE;
        let mut state = self.state.lock();
        let quarantine_status = self.reclaim_quarantined(&mut state);
        if quarantine_status != STATUS_OK {
            return Err(quarantine_status);
        }
        if state.poisoned {
            return Err(STATUS_IO_ERROR);
        }
        let slot = state
            .records
            .iter()
            .position(|record| {
                record.phase == AllocationPhase::Empty && record.generation != u32::MAX
            })
            .ok_or(STATUS_BUSY)?;
        if state
            .pages
            .iter()
            .filter(|page| page.phase == PagePhase::Empty)
            .count()
            < frame_count
        {
            return Err(STATUS_BUSY);
        }
        let iova = state
            .iovas
            .reserve_aligned(frame_count as u64, alignment_frames as u64)
            .map_err(|_| STATUS_BUSY)?;
        let range = state.iovas.range(iova).map_err(|_| STATUS_IO_ERROR)?;
        if range.length() != mapped_length as u64 {
            state.poisoned = true;
            return Err(STATUS_IO_ERROR);
        }
        let generation = state.records[slot].generation + 1;
        state.records[slot] = AllocationRecord {
            generation,
            phase: AllocationPhase::Mapping,
            domain,
            resident_pages: 0,
            requested_length: length,
            iova,
            device_address: range.start(),
        };

        let mut cpu_pointer = None;
        for page_index in 0..frame_count {
            let Some(page_slot) = state
                .pages
                .iter()
                .position(|page| page.phase == PagePhase::Empty)
            else {
                state.poisoned = true;
                let _ = self.reclaim_record(&mut state, slot);
                return Err(STATUS_IO_ERROR);
            };
            let Some(frame) = self.frames.allocate() else {
                let cleanup = self.reclaim_record(&mut state, slot);
                return Err(if cleanup == STATUS_OK {
                    STATUS_BUSY
                } else {
                    cleanup
                });
            };
            let pointer = match self.pointer(frame) {
                Ok(pointer) => pointer,
                Err(status) => {
                    state.pages[page_slot] = PageRecord {
                        owner_slot: (slot + 1) as u16,
                        owner_generation: generation,
                        page_index: page_index as u16,
                        frame,
                        phase: PagePhase::FrameOwned,
                    };
                    state.records[slot].resident_pages = page_index + 1;
                    let cleanup = self.reclaim_record(&mut state, slot);
                    return Err(if cleanup == STATUS_OK {
                        status
                    } else {
                        cleanup
                    });
                }
            };
            // SAFETY: `frame` is exclusively owned and `pointer` is its stable,
            // writable direct-map alias for exactly one page.
            unsafe { ptr::write_bytes(pointer, 0, PAGE_SIZE) };
            state.pages[page_slot] = PageRecord {
                owner_slot: (slot + 1) as u16,
                owner_generation: generation,
                page_index: page_index as u16,
                frame,
                phase: PagePhase::FrameOwned,
            };
            state.records[slot].resident_pages = page_index + 1;
            if page_index == 0 {
                cpu_pointer = NonNull::new(pointer);
            }
        }
        fence(Ordering::Release);

        for page_index in 0..frame_count {
            let page_slot = Self::page_slot(&state.pages, slot, generation, page_index)
                .expect("newly allocated DMA page remains in its private ledger");
            let device_address = range.start() + page_index as u64 * PAGE_SIZE as u64;
            let map_status = self.backend.map(
                domain,
                device_address,
                state.pages[page_slot].frame.as_u64(),
                PAGE_SIZE,
                DmaAccess::READ_WRITE,
            );
            if map_status != STATUS_OK {
                let cleanup = self.reclaim_record(&mut state, slot);
                return Err(if cleanup == STATUS_OK {
                    map_status
                } else {
                    cleanup
                });
            }
            state.pages[page_slot].phase = PagePhase::Mapped;
        }
        state.records[slot].phase = AllocationPhase::Mapped;
        let Some(cpu_pointer) = cpu_pointer else {
            state.poisoned = true;
            let _ = self.reclaim_record(&mut state, slot);
            return Err(STATUS_IO_ERROR);
        };
        // Hermes never exposes this common service receipt's CPU pointer; all
        // CPU access is deliberately mediated by the scatter-aware methods
        // below. It therefore identifies the first byte only, not a virtually
        // contiguous alias for the complete allocation.
        Ok(DmaAllocation {
            handle: encode_handle(slot, generation),
            cpu_pointer,
            device_address: range.start(),
        })
    }

    fn release(&self, domain: Handle, allocation: Handle) -> Status {
        let mut state = self.state.lock();
        let Ok((slot, record)) = Self::record(&state.records, allocation) else {
            return STATUS_NOT_FOUND;
        };
        if record.domain != domain {
            return STATUS_NOT_FOUND;
        }
        self.reclaim_record(&mut state, slot)
    }

    fn write(&self, allocation: Handle, offset: usize, bytes: &[u8]) -> Status {
        self.transfer(
            allocation,
            offset,
            bytes.as_ptr().cast_mut(),
            bytes.len(),
            true,
        )
    }

    fn read(&self, allocation: Handle, offset: usize, bytes: &mut [u8]) -> Status {
        self.transfer(allocation, offset, bytes.as_mut_ptr(), bytes.len(), false)
    }

    fn publish(&self, allocation: Handle, offset: usize, length: usize) -> Status {
        let state = self.state.lock();
        let Ok((_, record)) = Self::record(&state.records, allocation) else {
            return STATUS_NOT_FOUND;
        };
        if record.phase != AllocationPhase::Mapped
            || offset
                .checked_add(length)
                .is_none_or(|end| end > record.requested_length)
        {
            return STATUS_INVALID_ARGUMENT;
        }
        fence(Ordering::SeqCst);
        STATUS_OK
    }

    fn acquire(&self, allocation: Handle, offset: usize, length: usize) -> Status {
        let state = self.state.lock();
        let Ok((_, record)) = Self::record(&state.records, allocation) else {
            return STATUS_NOT_FOUND;
        };
        if record.phase != AllocationPhase::Mapped
            || offset
                .checked_add(length)
                .is_none_or(|end| end > record.requested_length)
        {
            return STATUS_INVALID_ARGUMENT;
        }
        fence(Ordering::SeqCst);
        STATUS_OK
    }
}

fn encode_handle(slot: usize, generation: u32) -> Handle {
    ((generation as u64) << 32) | (slot as u64 + 1)
}

fn decode_handle(handle: Handle) -> Option<(usize, u32)> {
    let encoded_slot = handle as u32;
    let generation = (handle >> 32) as u32;
    if encoded_slot == 0 || generation == 0 {
        None
    } else {
        Some(((encoded_slot - 1) as usize, generation))
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicU64, AtomicUsize};

    use abyss::frame::BitmapFrameAllocator;
    use abyss::memory::{MemoryMap, MemoryRegion, MemoryRegionKind};

    use super::*;
    use crate::capability::Authority;
    use crate::drivers::drivernet::fingerprint::GpuFingerprint;
    use crate::drivers::hermes_platform::{HermesKernelServices, KernelHermesPlatform};
    use crate::hw::pci::PciAddress;
    use crate::shim::ClockService;
    use sisyphus_driver_abi::hermes::HermesPciIdentity;

    const RAM_PAGES: usize = 16;
    const MAXIMUM_TEST_CALLS: usize = 32;
    const NEVER_FAIL: usize = usize::MAX;

    #[repr(C, align(4096))]
    struct TestRam([u8; RAM_PAGES * PAGE_SIZE]);

    struct TestBackend {
        maps: AtomicUsize,
        unmaps: AtomicUsize,
        mapped_iovas: [AtomicU64; MAXIMUM_TEST_CALLS],
        mapped_frames: [AtomicU64; MAXIMUM_TEST_CALLS],
        unmapped_iovas: [AtomicU64; MAXIMUM_TEST_CALLS],
        fail_map_at: AtomicUsize,
        fail_unmap_at: AtomicUsize,
    }

    impl TestBackend {
        const fn new() -> Self {
            Self {
                maps: AtomicUsize::new(0),
                unmaps: AtomicUsize::new(0),
                mapped_iovas: [const { AtomicU64::new(0) }; MAXIMUM_TEST_CALLS],
                mapped_frames: [const { AtomicU64::new(0) }; MAXIMUM_TEST_CALLS],
                unmapped_iovas: [const { AtomicU64::new(0) }; MAXIMUM_TEST_CALLS],
                fail_map_at: AtomicUsize::new(NEVER_FAIL),
                fail_unmap_at: AtomicUsize::new(NEVER_FAIL),
            }
        }

        fn fail_map_once(&self, call: usize) {
            self.fail_map_at.store(call, Ordering::Relaxed);
        }

        fn fail_unmap_once(&self, call: usize) {
            self.fail_unmap_at.store(call, Ordering::Relaxed);
        }
    }

    impl DmaRemappingBackend for TestBackend {
        fn isolate_device(&self, _device: PciAddress) -> Result<Handle, Status> {
            Ok(7)
        }

        fn map(
            &self,
            domain: Handle,
            device_address: u64,
            physical_address: u64,
            length: usize,
            access: DmaAccess,
        ) -> Status {
            assert_eq!(domain, 7);
            assert_eq!(physical_address % PAGE_SIZE as u64, 0);
            assert_eq!(length, PAGE_SIZE);
            assert_eq!(access, DmaAccess::READ_WRITE);
            let call = self.maps.fetch_add(1, Ordering::Relaxed);
            self.mapped_iovas[call].store(device_address, Ordering::Relaxed);
            self.mapped_frames[call].store(physical_address, Ordering::Relaxed);
            if self.fail_map_at.load(Ordering::Relaxed) == call {
                STATUS_BUSY
            } else {
                STATUS_OK
            }
        }

        fn unmap(&self, domain: Handle, device_address: u64, length: usize) -> Status {
            assert_eq!(domain, 7);
            assert_eq!(length, PAGE_SIZE);
            let call = self.unmaps.fetch_add(1, Ordering::Relaxed);
            self.unmapped_iovas[call].store(device_address, Ordering::Relaxed);
            if self.fail_unmap_at.load(Ordering::Relaxed) == call {
                STATUS_BUSY
            } else {
                STATUS_OK
            }
        }

        fn release_domain(&self, _domain: Handle) -> Status {
            STATUS_OK
        }
    }

    impl ClockService for TestBackend {
        fn monotonic_ns(&self) -> u64 {
            42
        }
    }

    fn frame_pool<'a>(
        ram: &'a mut TestRam,
        bitmap: &'a mut [u64; 2],
    ) -> (PhysicalFramePool<'a>, usize) {
        let mut map = MemoryMap::new();
        map.push(MemoryRegion::new(
            PhysicalAddress::new(PAGE_SIZE as u64),
            PhysicalAddress::new((RAM_PAGES * PAGE_SIZE) as u64),
            MemoryRegionKind::Usable,
        ))
        .unwrap();
        let allocator =
            BitmapFrameAllocator::new(&map, (RAM_PAGES * PAGE_SIZE) as u64, bitmap).unwrap();
        let pool = PhysicalFramePool::new(allocator);
        let direct_map_base = ram.0.as_mut_ptr() as usize - PAGE_SIZE;
        (pool, direct_map_base)
    }

    fn fragmented_frame_pool<'a>(
        ram: &'a mut TestRam,
        bitmap: &'a mut [u64; 2],
    ) -> (PhysicalFramePool<'a>, usize) {
        let mut map = MemoryMap::new();
        for frame in (1..RAM_PAGES).step_by(2) {
            map.push(MemoryRegion::new(
                PhysicalAddress::new(frame as u64 * PAGE_SIZE as u64),
                PhysicalAddress::new((frame + 1) as u64 * PAGE_SIZE as u64),
                MemoryRegionKind::Usable,
            ))
            .unwrap();
        }
        let allocator =
            BitmapFrameAllocator::new(&map, (RAM_PAGES * PAGE_SIZE) as u64, bitmap).unwrap();
        let pool = PhysicalFramePool::new(allocator);
        let direct_map_base = ram.0.as_mut_ptr() as usize - PAGE_SIZE;
        (pool, direct_map_base)
    }

    #[test]
    fn binds_scattered_frames_to_contiguous_aligned_iova_and_rejects_stale_access() {
        let mut ram = TestRam([0xa5; RAM_PAGES * PAGE_SIZE]);
        let mut bitmap = [0_u64; 2];
        let backend = TestBackend::new();
        let (pool, direct_map_base) = fragmented_frame_pool(&mut ram, &mut bitmap);
        let authority = unsafe { Authority::assume_root() };
        let physical_memory = authority.grant::<PhysicalMemoryControl>();
        let free_before = pool.free_frames();
        let aperture = IovaRange::new(0x1000, 32 * PAGE_SIZE as u64).unwrap();
        let dma = unsafe {
            FrameBackedHermesDma::new(
                &pool,
                &backend,
                aperture,
                &[],
                direct_map_base,
                (RAM_PAGES * PAGE_SIZE) as u64,
                &physical_memory,
            )
        }
        .unwrap();

        let allocation = dma
            .allocate(7, PAGE_SIZE + 17, 2 * PAGE_SIZE, DmaPurpose::CommandRing)
            .unwrap();
        assert_eq!(allocation.device_address % (2 * PAGE_SIZE) as u64, 0);
        assert_eq!(pool.free_frames(), free_before - 2);
        assert_eq!(backend.maps.load(Ordering::Relaxed), 2);
        assert_eq!(
            backend.mapped_iovas[1].load(Ordering::Relaxed)
                - backend.mapped_iovas[0].load(Ordering::Relaxed),
            PAGE_SIZE as u64
        );
        assert_eq!(
            backend.mapped_frames[1].load(Ordering::Relaxed)
                - backend.mapped_frames[0].load(Ordering::Relaxed),
            2 * PAGE_SIZE as u64
        );
        assert_eq!(dma.active_allocation_count(), 1);
        let payload = [1_u8, 2, 3, 4];
        assert_eq!(dma.write(allocation.handle, PAGE_SIZE, &payload), STATUS_OK);
        assert_eq!(dma.publish(allocation.handle, PAGE_SIZE, 4), STATUS_OK);
        let mut observed = [0_u8; 4];
        assert_eq!(dma.acquire(allocation.handle, PAGE_SIZE, 4), STATUS_OK);
        assert_eq!(
            dma.read(allocation.handle, PAGE_SIZE, &mut observed),
            STATUS_OK
        );
        assert_eq!(observed, payload);
        assert_eq!(dma.release(7, allocation.handle), STATUS_OK);
        assert_eq!(pool.free_frames(), free_before);
        assert_eq!(
            dma.read(allocation.handle, 0, &mut observed),
            STATUS_NOT_FOUND
        );
    }

    #[test]
    fn failed_unmap_retains_every_lease_for_exact_retry() {
        let mut ram = TestRam([0; RAM_PAGES * PAGE_SIZE]);
        let mut bitmap = [0_u64; 2];
        let backend = TestBackend::new();
        let (pool, direct_map_base) = frame_pool(&mut ram, &mut bitmap);
        let authority = unsafe { Authority::assume_root() };
        let physical_memory = authority.grant::<PhysicalMemoryControl>();
        let free_before = pool.free_frames();
        let aperture = IovaRange::new(0x1000, 32 * PAGE_SIZE as u64).unwrap();
        let dma = unsafe {
            FrameBackedHermesDma::new(
                &pool,
                &backend,
                aperture,
                &[],
                direct_map_base,
                (RAM_PAGES * PAGE_SIZE) as u64,
                &physical_memory,
            )
        }
        .unwrap();
        let allocation = dma
            .allocate(7, 2 * PAGE_SIZE, PAGE_SIZE, DmaPurpose::EventRing)
            .unwrap();
        backend.fail_unmap_once(0);
        assert_eq!(dma.release(7, allocation.handle), STATUS_BUSY);
        assert_eq!(dma.active_allocation_count(), 1);
        assert_eq!(pool.free_frames(), free_before - 2);
        assert_eq!(dma.release(7, allocation.handle), STATUS_OK);
        assert_eq!(pool.free_frames(), free_before);
        assert_eq!(backend.unmaps.load(Ordering::Relaxed), 3);
        assert_eq!(
            backend.unmapped_iovas[0].load(Ordering::Relaxed),
            allocation.device_address + PAGE_SIZE as u64
        );
        assert_eq!(
            backend.unmapped_iovas[1].load(Ordering::Relaxed),
            allocation.device_address + PAGE_SIZE as u64
        );
        assert_eq!(
            backend.unmapped_iovas[2].load(Ordering::Relaxed),
            allocation.device_address
        );
    }

    #[test]
    fn failed_page_map_rolls_back_every_prior_mapping_and_frame() {
        let mut ram = TestRam([0; RAM_PAGES * PAGE_SIZE]);
        let mut bitmap = [0_u64; 2];
        let backend = TestBackend::new();
        backend.fail_map_once(1);
        let (pool, direct_map_base) = fragmented_frame_pool(&mut ram, &mut bitmap);
        let authority = unsafe { Authority::assume_root() };
        let physical_memory = authority.grant::<PhysicalMemoryControl>();
        let free_before = pool.free_frames();
        let dma = unsafe {
            FrameBackedHermesDma::new(
                &pool,
                &backend,
                IovaRange::new(0x1000, 32 * PAGE_SIZE as u64).unwrap(),
                &[],
                direct_map_base,
                (RAM_PAGES * PAGE_SIZE) as u64,
                &physical_memory,
            )
        }
        .unwrap();

        assert!(matches!(
            dma.allocate(7, 3 * PAGE_SIZE, PAGE_SIZE, DmaPurpose::Firmware),
            Err(STATUS_BUSY)
        ));
        assert_eq!(backend.maps.load(Ordering::Relaxed), 2);
        assert_eq!(backend.unmaps.load(Ordering::Relaxed), 1);
        assert_eq!(pool.free_frames(), free_before);
        assert_eq!(dma.active_allocation_count(), 0);
        assert_eq!(dma.quarantined_allocation_count(), 0);
    }

    #[test]
    fn failed_map_rollback_is_quarantined_then_reclaimed_before_reuse() {
        let mut ram = TestRam([0; RAM_PAGES * PAGE_SIZE]);
        let mut bitmap = [0_u64; 2];
        let backend = TestBackend::new();
        backend.fail_map_once(1);
        backend.fail_unmap_once(0);
        let (pool, direct_map_base) = fragmented_frame_pool(&mut ram, &mut bitmap);
        let authority = unsafe { Authority::assume_root() };
        let physical_memory = authority.grant::<PhysicalMemoryControl>();
        let free_before = pool.free_frames();
        let dma = unsafe {
            FrameBackedHermesDma::new(
                &pool,
                &backend,
                IovaRange::new(0x1000, 32 * PAGE_SIZE as u64).unwrap(),
                &[],
                direct_map_base,
                (RAM_PAGES * PAGE_SIZE) as u64,
                &physical_memory,
            )
        }
        .unwrap();

        assert!(matches!(
            dma.allocate(7, 3 * PAGE_SIZE, PAGE_SIZE, DmaPurpose::Firmware),
            Err(STATUS_BUSY)
        ));
        assert_eq!(dma.quarantined_allocation_count(), 1);
        assert_eq!(pool.free_frames(), free_before - 3);

        backend.fail_map_at.store(NEVER_FAIL, Ordering::Relaxed);
        let replacement = dma
            .allocate(7, PAGE_SIZE, PAGE_SIZE, DmaPurpose::CommandRing)
            .unwrap();
        assert_eq!(dma.quarantined_allocation_count(), 0);
        assert_eq!(dma.active_allocation_count(), 1);
        assert_eq!(pool.free_frames(), free_before - 1);
        assert_eq!(dma.release(7, replacement.handle), STATUS_OK);
        assert_eq!(pool.free_frames(), free_before);
    }

    #[test]
    fn kernel_platform_consumes_the_real_domain_bound_dma_service() {
        let mut ram = TestRam([0; RAM_PAGES * PAGE_SIZE]);
        let mut bitmap = [0_u64; 2];
        let backend = TestBackend::new();
        let (pool, direct_map_base) = frame_pool(&mut ram, &mut bitmap);
        let authority = unsafe { Authority::assume_root() };
        let physical_memory = authority.grant::<PhysicalMemoryControl>();
        let aperture = IovaRange::new(0x2000, 32 * PAGE_SIZE as u64).unwrap();
        let dma = unsafe {
            FrameBackedHermesDma::new(
                &pool,
                &backend,
                aperture,
                &[],
                direct_map_base,
                (RAM_PAGES * PAGE_SIZE) as u64,
                &physical_memory,
            )
        }
        .unwrap();
        let mut fingerprint = GpuFingerprint::EMPTY;
        fingerprint.segment = 0;
        fingerprint.bus = 1;
        fingerprint.slot = 2;
        fingerprint.function = 0;
        fingerprint.vendor_id = 0x10de;
        fingerprint.device_id = 0x1234;
        fingerprint.class_code = 3;
        let identity = HermesPciIdentity {
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
        };
        let platform = KernelHermesPlatform::new(
            fingerprint,
            Some(&backend),
            None,
            Some(&dma),
            None,
            &backend,
        );
        assert_eq!(
            platform.available_services(),
            HermesKernelServices {
                isolation: true,
                mmio: false,
                dma: true,
                irq: false,
                firmware_dma: true,
            }
        );

        let domain = platform.acquire_domain(identity).unwrap();
        let allocation = platform
            .allocate_dma(domain, 2 * PAGE_SIZE, PAGE_SIZE, DmaPurpose::CommandRing)
            .unwrap();
        platform
            .dma_write(allocation.lease, 0, &[0x48, 0x47, 0x53, 0x50])
            .unwrap();
        platform.revoke_dma(allocation.lease).unwrap();
        platform.cleanup(domain).unwrap();
        assert_eq!(dma.active_allocation_count(), 0);
    }
}
