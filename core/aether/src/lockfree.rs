use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

const STATE_UNINITIALIZED: u8 = 0;
const STATE_INITIALIZING: u8 = 1;
const STATE_READY: u8 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueInitError {
    ZeroCapacity,
    Initializing,
    AlreadyInitialized,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueError {
    Uninitialized,
    Full,
    Empty,
}

#[repr(C, align(128))]
struct QueueSlot<T> {
    sequence: AtomicU64,
    value: UnsafeCell<MaybeUninit<T>>,
}

impl<T> QueueSlot<T> {
    const fn empty() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }
}

// Access to value is serialized by the per-slot sequence protocol.
unsafe impl<T: Copy + Send> Sync for QueueSlot<T> {}

#[repr(C, align(128))]
pub struct BoundedMpmc<T, const N: usize> {
    state: AtomicU8,
    enqueue_position: AtomicU64,
    dequeue_position: AtomicU64,
    slots: [QueueSlot<T>; N],
}

unsafe impl<T: Copy + Send, const N: usize> Sync for BoundedMpmc<T, N> {}

impl<T: Copy + Send, const N: usize> BoundedMpmc<T, N> {
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(STATE_UNINITIALIZED),
            enqueue_position: AtomicU64::new(0),
            dequeue_position: AtomicU64::new(0),
            slots: [const { QueueSlot::empty() }; N],
        }
    }

    pub fn initialize(&self) -> Result<(), QueueInitError> {
        if N == 0 {
            return Err(QueueInitError::ZeroCapacity);
        }

        match self.state.compare_exchange(
            STATE_UNINITIALIZED,
            STATE_INITIALIZING,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {}
            Err(STATE_INITIALIZING) => {
                return Err(QueueInitError::Initializing);
            }
            Err(_) => {
                return Err(QueueInitError::AlreadyInitialized);
            }
        }

        self.enqueue_position.store(0, Ordering::Relaxed);
        self.dequeue_position.store(0, Ordering::Relaxed);

        for (index, slot) in self.slots.iter().enumerate() {
            slot.sequence.store(index as u64, Ordering::Relaxed);
        }

        self.state.store(STATE_READY, Ordering::Release);
        Ok(())
    }

    #[inline(always)]
    pub fn is_ready(&self) -> bool {
        self.state.load(Ordering::Acquire) == STATE_READY
    }

    pub fn push(&self, value: T) -> Result<(), QueueError> {
        if !self.is_ready() {
            return Err(QueueError::Uninitialized);
        }

        let mut position = self.enqueue_position.load(Ordering::Relaxed);

        loop {
            let slot = &self.slots[position as usize % N];
            let sequence = slot.sequence.load(Ordering::Acquire);
            let difference = sequence.wrapping_sub(position) as i64;

            if difference == 0 {
                match self.enqueue_position.compare_exchange_weak(
                    position,
                    position.wrapping_add(1),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        // SAFETY: This producer exclusively owns the claimed
                        // slot until the Release sequence publication.
                        unsafe {
                            (*slot.value.get()).write(value);
                        }

                        slot.sequence
                            .store(position.wrapping_add(1), Ordering::Release);

                        return Ok(());
                    }

                    Err(observed) => position = observed,
                }
            } else if difference < 0 {
                return Err(QueueError::Full);
            } else {
                position = self.enqueue_position.load(Ordering::Relaxed);
            }

            core::hint::spin_loop();
        }
    }

    pub fn pop(&self) -> Result<T, QueueError> {
        if !self.is_ready() {
            return Err(QueueError::Uninitialized);
        }

        let mut position = self.dequeue_position.load(Ordering::Relaxed);

        loop {
            let slot = &self.slots[position as usize % N];
            let expected = position.wrapping_add(1);
            let sequence = slot.sequence.load(Ordering::Acquire);
            let difference = sequence.wrapping_sub(expected) as i64;

            if difference == 0 {
                match self.dequeue_position.compare_exchange_weak(
                    position,
                    position.wrapping_add(1),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        // SAFETY: The producer published this initialized value
                        // with Release and this consumer exclusively owns it.
                        let value = unsafe { (*slot.value.get()).assume_init_read() };

                        slot.sequence
                            .store(position.wrapping_add(N as u64), Ordering::Release);

                        return Ok(value);
                    }

                    Err(observed) => position = observed,
                }
            } else if difference < 0 {
                return Err(QueueError::Empty);
            } else {
                position = self.dequeue_position.load(Ordering::Relaxed);
            }

            core::hint::spin_loop();
        }
    }

    pub fn length_approximate(&self) -> usize {
        let enqueue = self.enqueue_position.load(Ordering::Acquire);
        let dequeue = self.dequeue_position.load(Ordering::Acquire);

        enqueue.wrapping_sub(dequeue).min(N as u64) as usize
    }
}

impl<T: Copy + Send, const N: usize> Default for BoundedMpmc<T, N> {
    fn default() -> Self {
        Self::new()
    }
}
