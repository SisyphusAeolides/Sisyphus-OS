use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::ops::Deref;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

const STATE_EMPTY: u8 = 0;
const STATE_INITIALIZING: u8 = 1;
const STATE_READY: u8 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConstellationError {
    Uninitialized,
    AlreadyInitialized,
    Initializing,
    WriterBusy,
    ReadersActive,
}

pub struct PolicyConstellation<P: Copy> {
    state: AtomicU8,
    writer: AtomicBool,
    active: AtomicU8,
    generation: AtomicU64,
    readers: [AtomicU64; 2],
    banks: [UnsafeCell<MaybeUninit<P>>; 2],
}

unsafe impl<P: Copy + Send + Sync> Sync for PolicyConstellation<P> {}

impl<P: Copy + Send + Sync> PolicyConstellation<P> {
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(STATE_EMPTY),
            writer: AtomicBool::new(false),
            active: AtomicU8::new(0),
            generation: AtomicU64::new(0),
            readers: [AtomicU64::new(0), AtomicU64::new(0)],
            banks: [
                UnsafeCell::new(MaybeUninit::uninit()),
                UnsafeCell::new(MaybeUninit::uninit()),
            ],
        }
    }

    pub fn initialize(&self, initial: P) -> Result<(), ConstellationError> {
        match self.state.compare_exchange(
            STATE_EMPTY,
            STATE_INITIALIZING,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {}

            Err(STATE_INITIALIZING) => {
                return Err(ConstellationError::Initializing);
            }

            Err(_) => {
                return Err(ConstellationError::AlreadyInitialized);
            }
        }

        // SAFETY: Initialization owns both banks before STATE_READY.
        unsafe {
            (*self.banks[0].get()).write(initial);
            (*self.banks[1].get()).write(initial);
        }

        self.active.store(0, Ordering::Relaxed);
        self.generation.store(1, Ordering::Relaxed);
        self.state.store(STATE_READY, Ordering::Release);

        Ok(())
    }

    pub fn read(&self) -> Result<PolicyGuard<'_, P>, ConstellationError> {
        if self.state.load(Ordering::Acquire) != STATE_READY {
            return Err(ConstellationError::Uninitialized);
        }

        loop {
            let index = usize::from(self.active.load(Ordering::Acquire));

            self.readers[index].fetch_add(1, Ordering::AcqRel);

            if usize::from(self.active.load(Ordering::Acquire)) == index {
                // SAFETY: STATE_READY initialized both banks. The reader count
                // prevents a writer from replacing this bank.
                let value = unsafe { (*self.banks[index].get()).assume_init_ref() };

                return Ok(PolicyGuard {
                    constellation: self,
                    index,
                    value,
                });
            }

            self.readers[index].fetch_sub(1, Ordering::Release);
            core::hint::spin_loop();
        }
    }

    pub fn publish(&self, next: P) -> Result<u64, ConstellationError> {
        if self.state.load(Ordering::Acquire) != STATE_READY {
            return Err(ConstellationError::Uninitialized);
        }

        if self
            .writer
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(ConstellationError::WriterBusy);
        }

        let active = usize::from(self.active.load(Ordering::Acquire));
        let target = active ^ 1;

        if self.readers[target].load(Ordering::Acquire) != 0 {
            self.writer.store(false, Ordering::Release);
            return Err(ConstellationError::ReadersActive);
        }

        // SAFETY: target is inactive and has no readers.
        unsafe {
            (*self.banks[target].get()).write(next);
        }

        let generation = self
            .generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);

        self.active.store(target as u8, Ordering::Release);
        self.writer.store(false, Ordering::Release);

        Ok(generation)
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
}

impl<P: Copy + Send + Sync> Default for PolicyConstellation<P> {
    fn default() -> Self {
        Self::new()
    }
}

pub struct PolicyGuard<'constellation, P: Copy + Send + Sync> {
    constellation: &'constellation PolicyConstellation<P>,
    index: usize,
    value: &'constellation P,
}

impl<P: Copy + Send + Sync> Deref for PolicyGuard<'_, P> {
    type Target = P;

    fn deref(&self) -> &Self::Target {
        self.value
    }
}

impl<P: Copy + Send + Sync> Drop for PolicyGuard<'_, P> {
    fn drop(&mut self) {
        self.constellation.readers[self.index].fetch_sub(1, Ordering::Release);
    }
}
