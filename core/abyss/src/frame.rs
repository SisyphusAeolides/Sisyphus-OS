use crate::memory::{MemoryMap, MemoryRegionKind};
use crate::paging::{FrameAllocator, PAGE_SIZE, PhysicalAddress};
use crate::reservation::ReservationTable;

const BITS_PER_WORD: usize = u64::BITS as usize;

pub struct BitmapFrameAllocator<'a> {
    allocated: &'a mut [u64],
    reserved: &'a mut [u64],
    frame_count: usize,
    next_search: usize,
    free_frames: usize,
}

impl<'a> BitmapFrameAllocator<'a> {
    pub fn storage_words(maximum_address: u64) -> Result<usize, FrameAllocatorError> {
        let frames = frame_count(maximum_address)?;
        words_for_frames(frames)
            .checked_mul(2)
            .ok_or(FrameAllocatorError::AddressOverflow)
    }

    pub fn new(
        memory_map: &MemoryMap,
        maximum_address: u64,
        storage: &'a mut [u64],
    ) -> Result<Self, FrameAllocatorError> {
        let frame_count = frame_count(maximum_address)?;
        let words = words_for_frames(frame_count);
        if storage.len() < words.saturating_mul(2) {
            return Err(FrameAllocatorError::StorageTooSmall);
        }
        let (allocated, remaining) = storage.split_at_mut(words);
        let reserved = &mut remaining[..words];
        allocated.fill(u64::MAX);
        reserved.fill(u64::MAX);

        let mut allocator = Self {
            allocated,
            reserved,
            frame_count,
            next_search: 0,
            free_frames: 0,
        };
        for region in memory_map.regions() {
            if region.kind != MemoryRegionKind::Usable {
                continue;
            }
            let Some(start) = align_up(region.start.as_u64(), PAGE_SIZE as u64) else {
                continue;
            };
            let end = align_down(region.end.as_u64().min(maximum_address), PAGE_SIZE as u64);
            allocator.mark_usable(start, end);
        }
        Ok(allocator)
    }

    pub fn apply_reservations<const N: usize>(&mut self, reservations: &ReservationTable<N>) {
        for reservation in reservations.entries() {
            self.reserve_range(reservation.start.as_u64(), reservation.end.as_u64());
        }
    }

    pub fn reserve_range(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        let first = (start / PAGE_SIZE as u64) as usize;
        let Some(rounded_end) = align_up(end, PAGE_SIZE as u64) else {
            return;
        };
        let last = ((rounded_end / PAGE_SIZE as u64) as usize).min(self.frame_count);
        for frame in first.min(self.frame_count)..last {
            if !self.is_allocated(frame) {
                self.free_frames -= 1;
            }
            self.set_allocated(frame, true);
            self.set_reserved(frame, true);
        }
    }

    pub fn allocate(&mut self) -> Option<PhysicalAddress> {
        if self.free_frames == 0 {
            return None;
        }
        for offset in 0..self.frame_count {
            let frame = (self.next_search + offset) % self.frame_count;
            if !self.is_allocated(frame) {
                self.set_allocated(frame, true);
                self.free_frames -= 1;
                self.next_search = (frame + 1) % self.frame_count;
                return Some(PhysicalAddress::new(frame as u64 * PAGE_SIZE as u64));
            }
        }
        None
    }

    pub fn deallocate(&mut self, frame: PhysicalAddress) -> Result<(), FrameAllocatorError> {
        if !frame.is_page_aligned() {
            return Err(FrameAllocatorError::UnalignedFrame);
        }
        let index = (frame.as_u64() / PAGE_SIZE as u64) as usize;
        if index >= self.frame_count {
            return Err(FrameAllocatorError::FrameOutOfRange);
        }
        if self.is_reserved(index) {
            return Err(FrameAllocatorError::ReservedFrame);
        }
        if !self.is_allocated(index) {
            return Err(FrameAllocatorError::DoubleFree);
        }
        self.set_allocated(index, false);
        self.free_frames += 1;
        self.next_search = index;
        Ok(())
    }

    pub const fn free_frames(&self) -> usize {
        self.free_frames
    }

    pub const fn managed_frames(&self) -> usize {
        self.frame_count
    }

    fn mark_usable(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        let first = (start / PAGE_SIZE as u64) as usize;
        let last = ((end / PAGE_SIZE as u64) as usize).min(self.frame_count);
        for frame in first.min(self.frame_count)..last {
            if self.is_allocated(frame) {
                self.free_frames += 1;
            }
            self.set_allocated(frame, false);
            self.set_reserved(frame, false);
        }
    }

    fn is_allocated(&self, frame: usize) -> bool {
        bit_is_set(self.allocated, frame)
    }

    fn is_reserved(&self, frame: usize) -> bool {
        bit_is_set(self.reserved, frame)
    }

    fn set_allocated(&mut self, frame: usize, value: bool) {
        set_bit(self.allocated, frame, value);
    }

    fn set_reserved(&mut self, frame: usize, value: bool) {
        set_bit(self.reserved, frame, value);
    }
}

impl FrameAllocator for BitmapFrameAllocator<'_> {
    fn allocate_frame(&mut self) -> Option<PhysicalAddress> {
        self.allocate()
    }

    fn deallocate_frame(&mut self, frame: PhysicalAddress) {
        let _ = self.deallocate(frame);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameAllocatorError {
    AddressOverflow,
    StorageTooSmall,
    UnalignedFrame,
    FrameOutOfRange,
    ReservedFrame,
    DoubleFree,
}

fn frame_count(maximum_address: u64) -> Result<usize, FrameAllocatorError> {
    let frames = maximum_address
        .checked_add(PAGE_SIZE as u64 - 1)
        .ok_or(FrameAllocatorError::AddressOverflow)?
        / PAGE_SIZE as u64;
    usize::try_from(frames).map_err(|_| FrameAllocatorError::AddressOverflow)
}

const fn words_for_frames(frames: usize) -> usize {
    frames.div_ceil(BITS_PER_WORD)
}

fn bit_is_set(bitmap: &[u64], bit: usize) -> bool {
    bitmap[bit / BITS_PER_WORD] & (1_u64 << (bit % BITS_PER_WORD)) != 0
}

fn set_bit(bitmap: &mut [u64], bit: usize, value: bool) {
    let word = &mut bitmap[bit / BITS_PER_WORD];
    let mask = 1_u64 << (bit % BITS_PER_WORD);
    if value {
        *word |= mask;
    } else {
        *word &= !mask;
    }
}

fn align_up(value: u64, alignment: u64) -> Option<u64> {
    value
        .checked_add(alignment - 1)
        .map(|rounded| rounded & !(alignment - 1))
}

const fn align_down(value: u64, alignment: u64) -> u64 {
    value & !(alignment - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{MemoryRegion, MemoryRegionKind};
    use crate::reservation::{Reservation, ReservationKind};

    fn test_map() -> MemoryMap {
        let mut map = MemoryMap::new();
        map.push(MemoryRegion::new(
            PhysicalAddress::new(0),
            PhysicalAddress::new(16 * PAGE_SIZE as u64),
            MemoryRegionKind::Usable,
        ))
        .unwrap();
        map
    }

    #[test]
    fn allocates_only_unreserved_frames() {
        let map = test_map();
        let mut storage = [0_u64; 2];
        let mut allocator =
            BitmapFrameAllocator::new(&map, 16 * PAGE_SIZE as u64, &mut storage).unwrap();
        let mut reservations = ReservationTable::<4>::new();
        reservations
            .push(Reservation::new(
                PhysicalAddress::new(0),
                PhysicalAddress::new(2 * PAGE_SIZE as u64),
                ReservationKind::LowMemory,
            ))
            .unwrap();
        allocator.apply_reservations(&reservations);

        assert_eq!(allocator.free_frames(), 14);
        assert_eq!(
            allocator.allocate(),
            Some(PhysicalAddress::new(2 * PAGE_SIZE as u64))
        );
    }

    #[test]
    fn reclaims_allocated_frames_and_rejects_double_free() {
        let map = test_map();
        let mut storage = [0_u64; 2];
        let mut allocator =
            BitmapFrameAllocator::new(&map, 16 * PAGE_SIZE as u64, &mut storage).unwrap();
        let frame = allocator.allocate().unwrap();
        allocator.deallocate(frame).unwrap();
        assert_eq!(
            allocator.deallocate(frame),
            Err(FrameAllocatorError::DoubleFree)
        );
        assert_eq!(allocator.allocate(), Some(frame));
    }

    #[test]
    fn refuses_to_release_reserved_frames() {
        let map = test_map();
        let mut storage = [0_u64; 2];
        let mut allocator =
            BitmapFrameAllocator::new(&map, 16 * PAGE_SIZE as u64, &mut storage).unwrap();
        allocator.reserve_range(0, PAGE_SIZE as u64);
        assert_eq!(
            allocator.deallocate(PhysicalAddress::new(0)),
            Err(FrameAllocatorError::ReservedFrame)
        );
    }
}
