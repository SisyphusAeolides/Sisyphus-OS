use core::alloc::{GlobalAlloc, Layout};
use core::ptr;
use core::sync::atomic::{AtomicUsize, Ordering};

pub struct BumpAllocator {
    next: AtomicUsize,
    end: AtomicUsize,
}

impl BumpAllocator {
    pub const fn empty() -> Self {
        Self {
            next: AtomicUsize::new(0),
            end: AtomicUsize::new(0),
        }
    }

    /// Initializes the allocation region exactly once.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that `[start, start + size)` is valid,
    /// writable, exclusively owned memory that remains available for every
    /// allocation made through this allocator. Initialization must complete
    /// before the allocator is made visible to concurrent callers.
    pub unsafe fn initialize(&self, start: usize, size: usize) -> Result<(), InitializeError> {
        let end = start
            .checked_add(size)
            .ok_or(InitializeError::AddressOverflow)?;
        if start == 0 || size == 0 {
            return Err(InitializeError::EmptyRegion);
        }
        if self
            .next
            .compare_exchange(0, start, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(InitializeError::AlreadyInitialized);
        }
        self.end.store(end, Ordering::Release);
        Ok(())
    }

    pub fn remaining(&self) -> usize {
        self.end
            .load(Ordering::Acquire)
            .saturating_sub(self.next.load(Ordering::Acquire))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitializeError {
    EmptyRegion,
    AddressOverflow,
    AlreadyInitialized,
}

unsafe impl GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut current = self.next.load(Ordering::Acquire);
        let end = self.end.load(Ordering::Acquire);

        loop {
            let Some(aligned) = current
                .checked_add(layout.align() - 1)
                .map(|value| value & !(layout.align() - 1))
            else {
                return ptr::null_mut();
            };
            let Some(next) = aligned.checked_add(layout.size()) else {
                return ptr::null_mut();
            };
            if next > end {
                return ptr::null_mut();
            }

            match self.next.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return aligned as *mut u8,
                Err(observed) => current = observed,
            }
        }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // Bootstrap bump allocation is monotonic. Reclamation begins only when
        // the frame-backed allocator takes ownership after boot.
        let _ = (pointer, layout);
    }
}
