use abyss::allocator::BumpAllocator;
use core::alloc::{GlobalAlloc, Layout};
use core::ptr::NonNull;
use sisyphus_driver_abi::{STATUS_NO_MEMORY, Status};

use super::AllocationService;

pub struct AbyssAllocator<'a> {
    allocator: &'a BumpAllocator,
}

impl<'a> AbyssAllocator<'a> {
    pub const fn new(allocator: &'a BumpAllocator) -> Self {
        Self { allocator }
    }
}

impl AllocationService for AbyssAllocator<'_> {
    fn allocate(&self, layout: Layout, _flags: u64) -> Result<NonNull<u8>, Status> {
        // SAFETY: BumpAllocator owns its initialized region and GlobalAlloc's
        // contract is represented by the validated Layout value.
        let pointer = unsafe { self.allocator.alloc(layout) };
        NonNull::new(pointer).ok_or(STATUS_NO_MEMORY)
    }

    unsafe fn deallocate(&self, pointer: NonNull<u8>, layout: Layout) {
        // SAFETY: The caller of this trait method guarantees that this pointer
        // and layout came from this allocator.
        unsafe { self.allocator.dealloc(pointer.as_ptr(), layout) };
    }
}
