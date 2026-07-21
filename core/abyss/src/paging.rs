pub const PAGE_SIZE: usize = 4096;

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PhysicalAddress(u64);

impl PhysicalAddress {
    pub const fn new(address: u64) -> Self {
        Self(address)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }

    pub const fn is_page_aligned(self) -> bool {
        self.0 & (PAGE_SIZE as u64 - 1) == 0
    }
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct VirtualAddress(usize);

impl VirtualAddress {
    pub const fn new(address: usize) -> Self {
        Self(address)
    }

    pub const fn as_usize(self) -> usize {
        self.0
    }

    pub const fn is_page_aligned(self) -> bool {
        self.0 & (PAGE_SIZE - 1) == 0
    }
}

pub trait FrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysicalAddress>;
    fn deallocate_frame(&mut self, frame: PhysicalAddress);
}

pub struct LinearFrameAllocator {
    next: u64,
    end: u64,
}

impl LinearFrameAllocator {
    pub const fn new(start: PhysicalAddress, end: PhysicalAddress) -> Self {
        Self {
            next: start.as_u64(),
            end: end.as_u64(),
        }
    }
}

impl FrameAllocator for LinearFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysicalAddress> {
        let next_end = self.next.checked_add(PAGE_SIZE as u64)?;
        if next_end > self.end {
            return None;
        }
        let frame = PhysicalAddress::new(self.next);
        self.next = next_end;
        Some(frame)
    }

    fn deallocate_frame(&mut self, _frame: PhysicalAddress) {
        // Linear bootstrap allocation is intentionally monotonic. A reclaiming
        // allocator replaces it after the physical memory map is established.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocator_stops_at_end() {
        let mut allocator =
            LinearFrameAllocator::new(PhysicalAddress::new(0x1000), PhysicalAddress::new(0x3000));
        assert_eq!(
            allocator.allocate_frame(),
            Some(PhysicalAddress::new(0x1000))
        );
        assert_eq!(
            allocator.allocate_frame(),
            Some(PhysicalAddress::new(0x2000))
        );
        assert_eq!(allocator.allocate_frame(), None);
    }
}
