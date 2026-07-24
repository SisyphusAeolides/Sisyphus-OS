use abyss::frame::{BitmapFrameAllocator, FrameAllocatorError};
use abyss::paging::PhysicalAddress;

use crate::sync::SpinLock;

/// Serializes ownership changes in Boulder's physical-frame allocator.
///
/// Address spaces and DMA domains must draw from the same ownership ledger;
/// otherwise the same physical page can be handed to both a process and a
/// device. The pool keeps that ledger behind one kernel spin lock while the
/// direct-map contents remain accessible through their stable aliases.
pub struct PhysicalFramePool<'storage> {
    allocator: SpinLock<BitmapFrameAllocator<'storage>>,
}

impl<'storage> PhysicalFramePool<'storage> {
    pub const fn new(allocator: BitmapFrameAllocator<'storage>) -> Self {
        Self {
            allocator: SpinLock::new(allocator),
        }
    }

    pub fn allocate(&self) -> Option<PhysicalAddress> {
        self.allocator.lock().allocate()
    }

    pub fn allocate_contiguous(
        &self,
        frame_count: usize,
        alignment_frames: usize,
    ) -> Option<PhysicalAddress> {
        self.allocator
            .lock()
            .allocate_contiguous(frame_count, alignment_frames)
    }

    pub fn release(&self, frame: PhysicalAddress) -> Result<(), FrameAllocatorError> {
        self.allocator.lock().deallocate(frame)
    }

    pub fn release_contiguous(
        &self,
        first_frame: PhysicalAddress,
        frame_count: usize,
    ) -> Result<(), FrameAllocatorError> {
        self.allocator
            .lock()
            .deallocate_contiguous(first_frame, frame_count)
    }

    pub fn free_frames(&self) -> usize {
        self.allocator.lock().free_frames()
    }

    pub fn managed_frames(&self) -> usize {
        self.allocator.lock().managed_frames()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use abyss::memory::{MemoryMap, MemoryRegion, MemoryRegionKind};
    use abyss::paging::PAGE_SIZE;

    #[test]
    fn serializes_allocation_and_reclamation_through_one_ledger() {
        let mut map = MemoryMap::new();
        map.push(MemoryRegion::new(
            PhysicalAddress::new(0),
            PhysicalAddress::new(4 * PAGE_SIZE as u64),
            MemoryRegionKind::Usable,
        ))
        .unwrap();
        let mut storage = [0_u64; 2];
        let allocator =
            BitmapFrameAllocator::new(&map, 4 * PAGE_SIZE as u64, &mut storage).unwrap();
        let pool = PhysicalFramePool::new(allocator);

        let first = pool.allocate().unwrap();
        let second = pool.allocate().unwrap();
        assert_ne!(first, second);
        assert_eq!(pool.free_frames(), 2);

        pool.release(first).unwrap();
        assert_eq!(pool.allocate(), Some(first));
        assert_eq!(pool.managed_frames(), 4);
    }

    #[test]
    fn serializes_contiguous_frame_runs_through_the_same_ledger() {
        let mut map = MemoryMap::new();
        map.push(MemoryRegion::new(
            PhysicalAddress::new(0),
            PhysicalAddress::new(8 * PAGE_SIZE as u64),
            MemoryRegionKind::Usable,
        ))
        .unwrap();
        let mut storage = [0_u64; 2];
        let allocator =
            BitmapFrameAllocator::new(&map, 8 * PAGE_SIZE as u64, &mut storage).unwrap();
        let pool = PhysicalFramePool::new(allocator);

        let run = pool.allocate_contiguous(3, 2).unwrap();
        assert_eq!(run.as_u64() % (2 * PAGE_SIZE as u64), 0);
        assert_eq!(pool.free_frames(), 5);
        pool.release_contiguous(run, 3).unwrap();
        assert_eq!(pool.free_frames(), 8);
    }
}
