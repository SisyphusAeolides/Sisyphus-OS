use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicU8, Ordering};

const EMPTY: u8 = 0;
const INITIALIZING: u8 = 1;
const READY: u8 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootCellError {
    Initializing,
    AlreadyInitialized,
}

pub struct BootCell<T> {
    state: AtomicU8,
    value: UnsafeCell<MaybeUninit<T>>,
}

// Initialization is serialized by state. Once READY, the value is immutable.
unsafe impl<T: Send + Sync> Sync for BootCell<T> {}

impl<T> BootCell<T> {
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(EMPTY),
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    pub fn initialize(&self, value: T) -> Result<&T, BootCellError> {
        match self
            .state
            .compare_exchange(EMPTY, INITIALIZING, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {}
            Err(INITIALIZING) => {
                return Err(BootCellError::Initializing);
            }
            Err(_) => {
                return Err(BootCellError::AlreadyInitialized);
            }
        }

        // SAFETY: This thread exclusively owns initialization after the
        // successful EMPTY -> INITIALIZING transition.
        unsafe {
            (*self.value.get()).write(value);
        }

        self.state.store(READY, Ordering::Release);

        // READY was just published by this thread.
        Ok(self.get().expect("boot cell publication failed"))
    }

    #[inline(always)]
    pub fn get(&self) -> Option<&T> {
        if self.state.load(Ordering::Acquire) != READY {
            return None;
        }

        // SAFETY: READY is published only after the complete value is written.
        Some(unsafe { (*self.value.get()).assume_init_ref() })
    }

    #[inline(always)]
    pub fn is_ready(&self) -> bool {
        self.state.load(Ordering::Acquire) == READY
    }
}

impl<T> Default for BootCell<T> {
    fn default() -> Self {
        Self::new()
    }
}
