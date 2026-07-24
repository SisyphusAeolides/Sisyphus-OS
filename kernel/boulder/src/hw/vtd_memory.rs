//! Pinned direct-map storage for Intel VT-d translation tables.
//!
//! The allocator ledger is shared with process address spaces. This adapter
//! adds a second, local provenance ledger: a physical frame is writable or
//! releasable through this interface only while both ledgers say that this
//! VT-d owner holds it.

use core::mem::size_of;
use core::ptr;
use core::sync::atomic::{AtomicU64, Ordering, fence};

use abyss::paging::{PAGE_SIZE, PhysicalAddress};

use crate::capability::{Capability, PhysicalMemoryControl};
use crate::memory::frame_pool::PhysicalFramePool;

use super::pci::PciAddress;
use super::vtd::{ContextEntryTable, RootEntryTable};
use super::vtd_backend::VtdRootContextStorage;
use super::vtd_slpt::{SlptFrame, SlptMemoryError, SlptPageMemory};

const TABLE_ENTRIES: usize = PAGE_SIZE / size_of::<u64>();
const OUT_OF_FRAMES: u64 = 1;
const PROVENANCE_CAPACITY_EXHAUSTED: u64 = 2;
const INVALID_FRAME: u64 = 3;
const UNKNOWN_FRAME: u64 = 4;
const INVALID_ENTRY: u64 = 5;
const ADDRESS_OVERFLOW: u64 = 6;
const OUTSIDE_DIRECT_MAP: u64 = 7;
const ALLOCATOR_REJECTED_RELEASE: u64 = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectMapVtdTablesError {
    OutOfFrames,
    InvalidDirectMap,
    ZeroPhysicalAddress,
    EntriesStillLive,
    AllocatorRejectedRelease,
}

/// Retry receipt returned when root/context storage cannot yet be reclaimed.
pub struct DirectMapVtdTablesCloseFailure<Tables> {
    fault: DirectMapVtdTablesError,
    tables: Tables,
}

impl<Tables> DirectMapVtdTablesCloseFailure<Tables> {
    pub const fn fault(&self) -> DirectMapVtdTablesError {
        self.fault
    }

    pub fn into_tables(self) -> Tables {
        self.tables
    }
}

impl<Tables> core::fmt::Debug for DirectMapVtdTablesCloseFailure<Tables> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("DirectMapVtdTablesCloseFailure")
            .field("fault", &self.fault)
            .finish_non_exhaustive()
    }
}

/// Two adjacent pinned pages containing a legacy root and context table.
///
/// Contiguity is not required by VT-d; it is used here to make allocation and
/// reclamation one all-or-nothing ledger transaction.
#[must_use = "live VT-d table frames must be explicitly closed after hardware authority is released"]
pub struct DirectMapVtdTables<'allocator, 'storage> {
    frames: &'allocator PhysicalFramePool<'storage>,
    direct_map_base: usize,
    first_frame: PhysicalAddress,
}

impl<'allocator, 'storage> DirectMapVtdTables<'allocator, 'storage> {
    /// Allocates, pins, and initializes the legacy root/context pair.
    ///
    /// # Safety
    ///
    /// The complete physical range managed by `frames` must have one stable,
    /// writable, cache-coherent alias at `direct_map_base + physical_address`.
    /// Frame zero must not be allocatable. No independent owner may mutate the
    /// allocated pages until `close` succeeds.
    pub unsafe fn allocate(
        frames: &'allocator PhysicalFramePool<'storage>,
        direct_map_base: usize,
        mapped_physical_limit: u64,
        _authority: &Capability<'_, PhysicalMemoryControl>,
    ) -> Result<Self, DirectMapVtdTablesError> {
        let managed_bytes = (frames.managed_frames() as u64)
            .checked_mul(PAGE_SIZE as u64)
            .ok_or(DirectMapVtdTablesError::InvalidDirectMap)?;
        let virtual_limit = usize::try_from(mapped_physical_limit)
            .ok()
            .and_then(|length| direct_map_base.checked_add(length));
        if managed_bytes > mapped_physical_limit || virtual_limit.is_none() {
            return Err(DirectMapVtdTablesError::InvalidDirectMap);
        }
        let first_frame = frames
            .allocate_contiguous(2, 1)
            .ok_or(DirectMapVtdTablesError::OutOfFrames)?;
        if first_frame.as_u64() == 0 {
            frames
                .release_contiguous(first_frame, 2)
                .map_err(|_| DirectMapVtdTablesError::AllocatorRejectedRelease)?;
            return Err(DirectMapVtdTablesError::ZeroPhysicalAddress);
        }
        let first_physical = usize::try_from(first_frame.as_u64())
            .map_err(|_| DirectMapVtdTablesError::InvalidDirectMap)?;
        let first_virtual = direct_map_base
            .checked_add(first_physical)
            .ok_or(DirectMapVtdTablesError::InvalidDirectMap)?;
        // SAFETY: The caller's direct-map proof and the atomic contiguous
        // allocation give exclusive writable access to both complete pages.
        unsafe { ptr::write_bytes(first_virtual as *mut u8, 0, 2 * PAGE_SIZE) };
        fence(Ordering::Release);
        Ok(Self {
            frames,
            direct_map_base,
            first_frame,
        })
    }

    pub const fn root_physical_address(&self) -> u64 {
        self.first_frame.as_u64()
    }

    pub const fn context_physical_address(&self) -> u64 {
        self.first_frame.as_u64() + PAGE_SIZE as u64
    }

    pub fn close(
        self,
    ) -> Result<(), DirectMapVtdTablesCloseFailure<DirectMapVtdTables<'allocator, 'storage>>> {
        if !self.entries_are_empty() {
            return Err(DirectMapVtdTablesCloseFailure {
                fault: DirectMapVtdTablesError::EntriesStillLive,
                tables: self,
            });
        }
        if self.frames.release_contiguous(self.first_frame, 2).is_err() {
            return Err(DirectMapVtdTablesCloseFailure {
                fault: DirectMapVtdTablesError::AllocatorRejectedRelease,
                tables: self,
            });
        }
        Ok(())
    }

    fn root_pointer(&self) -> *const RootEntryTable {
        (self.direct_map_base + self.root_physical_address() as usize) as *const RootEntryTable
    }

    fn context_pointer(&self) -> *const ContextEntryTable {
        (self.direct_map_base + self.context_physical_address() as usize)
            as *const ContextEntryTable
    }

    fn entries_are_empty(&self) -> bool {
        let roots_empty = (0..=u8::MAX).all(|bus| self.root_table().entry(bus).raw() == (0, 0));
        let contexts_empty = (0..32).all(|slot| {
            (0..8).all(|function| {
                self.context_table()
                    .entry(PciAddress {
                        bus: 0,
                        slot,
                        function,
                    })
                    .raw()
                    == (0, 0)
            })
        });
        roots_empty && contexts_empty
    }
}

// SAFETY: Construction pins two exclusive frames for the value's lifetime;
// the typed references are exact stable aliases and physical addresses name
// those same frames. Entry implementations provide atomic publication.
unsafe impl VtdRootContextStorage for DirectMapVtdTables<'_, '_> {
    fn root_table(&self) -> &RootEntryTable {
        // SAFETY: Guaranteed by `allocate`; RootEntryTable is exactly one page
        // and the physical frame/direct-map base are page aligned.
        unsafe { &*self.root_pointer() }
    }

    fn root_table_physical_address(&self) -> u64 {
        self.root_physical_address()
    }

    fn context_table(&self) -> &ContextEntryTable {
        // SAFETY: The context table occupies the second complete pinned page.
        unsafe { &*self.context_pointer() }
    }

    fn context_table_physical_address(&self) -> u64 {
        self.context_physical_address()
    }
}

/// Frame-backed VT-d table memory over Boulder's stable higher-half map.
///
/// `OWNED` is an independent bound on live table frames, not a mapping count.
/// Keeping the provenance ledger fixed-size makes exhaustion deterministic in
/// the allocator's most failure-sensitive path.
pub struct DirectMapSlptMemory<'allocator, 'storage, const OWNED: usize> {
    frames: &'allocator PhysicalFramePool<'storage>,
    direct_map_base: usize,
    mapped_physical_limit: u64,
    owned: [Option<SlptFrame>; OWNED],
}

impl<'allocator, 'storage, const OWNED: usize> DirectMapSlptMemory<'allocator, 'storage, OWNED> {
    /// Binds VT-d table access to a stable direct map and the global frame
    /// ownership ledger.
    ///
    /// # Safety
    ///
    /// Every frame the pool can return below `mapped_physical_limit` must have
    /// one stable, writable, cache-coherent alias at
    /// `direct_map_base + physical_address`. No other writable alias may be
    /// used to mutate a frame while this adapter records it as owned.
    pub const unsafe fn new(
        frames: &'allocator PhysicalFramePool<'storage>,
        direct_map_base: usize,
        mapped_physical_limit: u64,
        _authority: &Capability<'_, PhysicalMemoryControl>,
    ) -> Self {
        Self {
            frames,
            direct_map_base,
            mapped_physical_limit,
            owned: [None; OWNED],
        }
    }

    pub fn owned_table_count(&self) -> usize {
        self.owned.iter().filter(|slot| slot.is_some()).count()
    }

    pub fn owns(&self, frame: SlptFrame) -> bool {
        self.owned.iter().any(|candidate| *candidate == Some(frame))
    }

    fn pointer(&self, frame: SlptFrame) -> Result<*mut u8, SlptMemoryError> {
        let physical_address = frame.physical_address();
        if physical_address
            .checked_add(PAGE_SIZE as u64)
            .is_none_or(|end| end > self.mapped_physical_limit)
        {
            return Err(SlptMemoryError(OUTSIDE_DIRECT_MAP));
        }
        let physical =
            usize::try_from(physical_address).map_err(|_| SlptMemoryError(ADDRESS_OVERFLOW))?;
        let virtual_address = self
            .direct_map_base
            .checked_add(physical)
            .ok_or(SlptMemoryError(ADDRESS_OVERFLOW))?;
        Ok(virtual_address as *mut u8)
    }

    fn owned_pointer(&self, frame: SlptFrame) -> Result<*mut u8, SlptMemoryError> {
        if !self.owns(frame) {
            return Err(SlptMemoryError(UNKNOWN_FRAME));
        }
        self.pointer(frame)
    }
}

impl<const OWNED: usize> SlptPageMemory for DirectMapSlptMemory<'_, '_, OWNED> {
    fn allocate_table(&mut self) -> Result<SlptFrame, SlptMemoryError> {
        let slot = self
            .owned
            .iter()
            .position(Option::is_none)
            .ok_or(SlptMemoryError(PROVENANCE_CAPACITY_EXHAUSTED))?;
        let physical = self
            .frames
            .allocate()
            .ok_or(SlptMemoryError(OUT_OF_FRAMES))?;
        let Some(frame) = SlptFrame::from_physical_address(physical.as_u64()) else {
            self.frames
                .release(physical)
                .map_err(|_| SlptMemoryError(ALLOCATOR_REJECTED_RELEASE))?;
            return Err(SlptMemoryError(INVALID_FRAME));
        };
        if let Err(error) = self.pointer(frame) {
            self.frames
                .release(physical)
                .map_err(|_| SlptMemoryError(ALLOCATOR_REJECTED_RELEASE))?;
            return Err(error);
        }
        self.owned[slot] = Some(frame);
        Ok(frame)
    }

    fn zero_table(&mut self, frame: SlptFrame) -> Result<(), SlptMemoryError> {
        let pointer = self.owned_pointer(frame)?;
        // SAFETY: Local provenance proves exclusive ownership of the complete
        // mapped frame. It is not linked into a hardware-visible hierarchy
        // while the SLPT core requests zeroing.
        unsafe { ptr::write_bytes(pointer, 0, PAGE_SIZE) };
        fence(Ordering::Release);
        Ok(())
    }

    fn read_entry(&self, frame: SlptFrame, index: usize) -> Result<u64, SlptMemoryError> {
        if index >= TABLE_ENTRIES {
            return Err(SlptMemoryError(INVALID_ENTRY));
        }
        let pointer = self.owned_pointer(frame)?;
        // SAFETY: The frame is locally owned, the index is in range, and every
        // entry is naturally aligned. Acquire pairs with entry publication.
        let entry = unsafe { &*pointer.cast::<AtomicU64>().add(index) };
        Ok(entry.load(Ordering::Acquire))
    }

    fn write_entry(
        &mut self,
        frame: SlptFrame,
        index: usize,
        value: u64,
    ) -> Result<(), SlptMemoryError> {
        if index >= TABLE_ENTRIES {
            return Err(SlptMemoryError(INVALID_ENTRY));
        }
        let pointer = self.owned_pointer(frame)?;
        // SAFETY: Local provenance proves exclusive mutation authority and the
        // validated entry is naturally aligned. Release publishes all prior
        // child-table initialization before a parent link becomes observable.
        let entry = unsafe { &*pointer.cast::<AtomicU64>().add(index) };
        entry.store(value, Ordering::Release);
        Ok(())
    }

    fn release_table(&mut self, frame: SlptFrame) -> Result<(), SlptMemoryError> {
        let slot = self
            .owned
            .iter()
            .position(|candidate| *candidate == Some(frame))
            .ok_or(SlptMemoryError(UNKNOWN_FRAME))?;
        self.frames
            .release(PhysicalAddress::new(frame.physical_address()))
            .map_err(|_| SlptMemoryError(ALLOCATOR_REJECTED_RELEASE))?;
        // Clear provenance only after the global allocator accepted release;
        // on failure both ownership ledgers remain unchanged and retryable.
        self.owned[slot] = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use abyss::frame::BitmapFrameAllocator;
    use abyss::memory::{MemoryMap, MemoryRegion, MemoryRegionKind};

    use crate::capability::Authority;

    const RAM_PAGES: usize = 8;

    #[repr(C, align(4096))]
    struct TestRam([u8; RAM_PAGES * PAGE_SIZE]);

    #[test]
    fn pins_publishes_and_reclaims_only_proven_owned_tables() {
        let mut ram = TestRam([0xa5; RAM_PAGES * PAGE_SIZE]);
        let mut map = MemoryMap::new();
        map.push(MemoryRegion::new(
            PhysicalAddress::new(PAGE_SIZE as u64),
            PhysicalAddress::new((RAM_PAGES * PAGE_SIZE) as u64),
            MemoryRegionKind::Usable,
        ))
        .unwrap();
        let mut bitmap = [0_u64; 2];
        let allocator =
            BitmapFrameAllocator::new(&map, (RAM_PAGES * PAGE_SIZE) as u64, &mut bitmap).unwrap();
        let pool = PhysicalFramePool::new(allocator);
        // SAFETY: This test is its serialized bootstrap authority owner.
        let authority = unsafe { Authority::assume_root() };
        let physical_memory = authority.grant::<PhysicalMemoryControl>();
        let direct_map_base = ram.0.as_mut_ptr() as usize - PAGE_SIZE;
        // SAFETY: Test RAM supplies the stable, aligned coherent alias promised
        // to the adapter and remains alive for the complete test.
        let mut memory = unsafe {
            DirectMapSlptMemory::<3>::new(
                &pool,
                direct_map_base,
                (RAM_PAGES * PAGE_SIZE) as u64,
                &physical_memory,
            )
        };

        let first = memory.allocate_table().unwrap();
        let second = memory.allocate_table().unwrap();
        assert_eq!(memory.owned_table_count(), 2);
        memory.zero_table(first).unwrap();
        memory.write_entry(first, 511, 0x1234_5003).unwrap();
        assert_eq!(memory.read_entry(first, 511), Ok(0x1234_5003));
        assert_eq!(memory.read_entry(second, 0), Ok(0xa5a5_a5a5_a5a5_a5a5));

        memory.release_table(first).unwrap();
        assert_eq!(memory.owned_table_count(), 1);
        assert_eq!(
            memory.release_table(first),
            Err(SlptMemoryError(UNKNOWN_FRAME))
        );
        assert_eq!(
            memory.read_entry(first, 0),
            Err(SlptMemoryError(UNKNOWN_FRAME))
        );
    }

    #[test]
    fn refuses_allocation_before_leaking_beyond_local_provenance_capacity() {
        let mut ram = TestRam([0; RAM_PAGES * PAGE_SIZE]);
        let mut map = MemoryMap::new();
        map.push(MemoryRegion::new(
            PhysicalAddress::new(PAGE_SIZE as u64),
            PhysicalAddress::new((RAM_PAGES * PAGE_SIZE) as u64),
            MemoryRegionKind::Usable,
        ))
        .unwrap();
        let mut bitmap = [0_u64; 2];
        let allocator =
            BitmapFrameAllocator::new(&map, (RAM_PAGES * PAGE_SIZE) as u64, &mut bitmap).unwrap();
        let pool = PhysicalFramePool::new(allocator);
        let authority = unsafe { Authority::assume_root() };
        let physical_memory = authority.grant::<PhysicalMemoryControl>();
        let direct_map_base = ram.0.as_mut_ptr() as usize - PAGE_SIZE;
        let mut memory = unsafe {
            DirectMapSlptMemory::<1>::new(
                &pool,
                direct_map_base,
                (RAM_PAGES * PAGE_SIZE) as u64,
                &physical_memory,
            )
        };

        let _owned = memory.allocate_table().unwrap();
        let globally_free_before = pool.free_frames();
        assert_eq!(
            memory.allocate_table(),
            Err(SlptMemoryError(PROVENANCE_CAPACITY_EXHAUSTED))
        );
        assert_eq!(pool.free_frames(), globally_free_before);
    }

    #[test]
    fn root_context_pair_reclaims_atomically_only_after_entries_are_clear() {
        let mut ram = TestRam([0xa5; RAM_PAGES * PAGE_SIZE]);
        let mut map = MemoryMap::new();
        map.push(MemoryRegion::new(
            PhysicalAddress::new(PAGE_SIZE as u64),
            PhysicalAddress::new((RAM_PAGES * PAGE_SIZE) as u64),
            MemoryRegionKind::Usable,
        ))
        .unwrap();
        let mut bitmap = [0_u64; 2];
        let allocator =
            BitmapFrameAllocator::new(&map, (RAM_PAGES * PAGE_SIZE) as u64, &mut bitmap).unwrap();
        let pool = PhysicalFramePool::new(allocator);
        let authority = unsafe { Authority::assume_root() };
        let physical_memory = authority.grant::<PhysicalMemoryControl>();
        let direct_map_base = ram.0.as_mut_ptr() as usize - PAGE_SIZE;
        let free_before = pool.free_frames();
        let tables = unsafe {
            DirectMapVtdTables::allocate(
                &pool,
                direct_map_base,
                (RAM_PAGES * PAGE_SIZE) as u64,
                &physical_memory,
            )
        }
        .unwrap();
        assert_eq!(pool.free_frames(), free_before - 2);
        assert_eq!(tables.root_table().entry(0).raw(), (0, 0));
        assert_eq!(
            tables
                .context_table()
                .entry(PciAddress::new(0, 1, 0).unwrap())
                .raw(),
            (0, 0)
        );

        tables
            .root_table()
            .entry(0)
            .install_context_table(tables.context_physical_address())
            .unwrap();
        let failure = tables.close().unwrap_err();
        assert_eq!(failure.fault(), DirectMapVtdTablesError::EntriesStillLive);
        let tables = failure.into_tables();
        assert_eq!(pool.free_frames(), free_before - 2);
        tables.root_table().entry(0).clear();
        tables.close().unwrap();
        assert_eq!(pool.free_frames(), free_before);
    }
}
