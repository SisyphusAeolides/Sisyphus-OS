// OUROBOROS EXECUTOR — zero-alloc cooperative async executor
//
// Fixed arena of MAX_TASKS task slots. Each slot holds a type-erased Future
// as a manually-managed fat pointer pair (data: *mut u8, poll_fn: PollFn).
//
// Waker: a RawWaker backed by a (arena_ptr, slot_index) pair.
//   No Arc. No heap. No vtable beyond the four required fn pointers.
//   Wake marks the slot READY; wake_by_ref does the same without consuming.
//
// Scheduler: a 64-bit bitmask of READY slots (up to 64 tasks).
//   run_until_stall() polls all READY slots once per pass.
//   run_forever() loops run_until_stall() + tachyon yield between passes.
//
// Spawn: caller provides a pinned, static-lifetime task buffer.
//   The executor stores only the fat pointer — no Box, no Pin on the heap.
//   Slots recycle on task completion (ouroboros: the snake eats its tail).
//
// PollFn: extern "C" fn(*mut u8, *mut RawContext) -> u8
//   0 = Pending, 1 = Ready
//   Called with the task's data pointer and a RawContext wrapping our Waker.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use core::sync::atomic::{AtomicU64, Ordering};
use crate::process::tachyon;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutorError {
    ArenaFull,
}

pub const MAX_TASKS: usize = 64;

// ─── RAW WAKER ──────────────────────────────────────────────────────────────

// Packed waker data: [arena_ptr: 48 bits][slot: 16 bits] — fits in one usize.
// We store it as a raw *const () pointing to a WakerData on the executor stack.

#[repr(C)]
struct WakerData {
    ready_mask: *const AtomicU64,
    slot:       usize,
}

unsafe fn waker_clone(data: *const ()) -> RawWaker {
    // Data pointer is the WakerData itself — just clone the pointer.
    RawWaker::new(data, &WAKER_VTABLE)
}

unsafe fn waker_wake(data: *const ()) {
    unsafe {
        let wd = &*(data as *const WakerData);
        (*wd.ready_mask).fetch_or(1u64 << wd.slot, Ordering::Release);
    }
}

unsafe fn waker_wake_by_ref(data: *const ()) {
    unsafe { waker_wake(data); }
}

unsafe fn waker_drop(_data: *const ()) {}

static WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
    waker_clone,
    waker_wake,
    waker_wake_by_ref,
    waker_drop,
);

// ─── TASK SLOT ─────────────────────────────────────────────────────────────

const SLOT_FREE:    u8 = 0;
const SLOT_PARKED:  u8 = 1; // waiting for wake
const SLOT_READY:   u8 = 2; // scheduled to poll
const SLOT_RUNNING: u8 = 3; // currently polling
const SLOT_DONE:    u8 = 4; // future returned Ready — recyclable

// Type-erased poll function: (data_ptr, waker_data_ptr) -> is_ready: bool
type PollFn = unsafe fn(*mut u8, *const WakerData) -> bool;

struct TaskSlot {
    data:    *mut u8,
    poll_fn: PollFn,
    state:   u8,
}

impl TaskSlot {
    const fn empty() -> Self {
        Self {
            data:    core::ptr::null_mut(),
            poll_fn: |_, _| true,
            state:   SLOT_FREE,
        }
    }
}

// ─── EXECUTOR ──────────────────────────────────────────────────────────────

pub struct OuroborosExecutor {
    slots:      [TaskSlot; MAX_TASKS],
    ready_mask: AtomicU64,  // bit i = slot i is READY
    count:      usize,
}

impl OuroborosExecutor {
    pub const fn new() -> Self {
        Self {
            slots:      [const { TaskSlot::empty() }; MAX_TASKS],
            ready_mask: AtomicU64::new(0),
            count:      0,
        }
    }

    /// Spawn a future into the executor.
    ///
    /// `storage` must be a pinned, caller-owned buffer whose lifetime exceeds
    /// the executor. The executor stores only raw pointers into it.
    ///
    /// # Safety
    /// `storage` must remain valid and pinned until the task completes.
    pub unsafe fn spawn_raw<F>(&mut self, storage: *mut F) -> Result<usize, ExecutorError>
    where
        F: Future<Output = ()>,
    {
        let slot = self.find_free_slot().ok_or(ExecutorError::ArenaFull)?;

        unsafe fn poll_erased<F: Future<Output = ()>>(
            data: *mut u8,
            wd: *const WakerData,
        ) -> bool {
            let future: Pin<&mut F> = unsafe { Pin::new_unchecked(&mut *(data as *mut F)) };
            let raw_waker = RawWaker::new(wd as *const (), &WAKER_VTABLE);
            let waker = unsafe { Waker::from_raw(raw_waker) };
            let mut ctx = Context::from_waker(&waker);
            matches!(future.poll(&mut ctx), Poll::Ready(()))
        }

        self.slots[slot].data    = storage as *mut u8;
        self.slots[slot].poll_fn = poll_erased::<F>;
        self.slots[slot].state   = SLOT_READY;
        self.ready_mask.fetch_or(1u64 << slot, Ordering::Release);
        self.count += 1;
        Ok(slot)
    }

    fn find_free_slot(&self) -> Option<usize> {
        self.slots.iter().position(|s| s.state == SLOT_FREE || s.state == SLOT_DONE)
    }

    /// Poll all READY tasks once. Returns number of tasks still alive.
    pub fn run_until_stall(&mut self) -> usize {
        let mut mask = self.ready_mask.swap(0, Ordering::Acquire);
        while mask != 0 {
            let bit  = mask.trailing_zeros() as usize;
            mask    &= !(1u64 << bit);
            let slot = &mut self.slots[bit];
            if slot.state != SLOT_READY { continue; }
            slot.state = SLOT_RUNNING;
            let wd = WakerData {
                ready_mask: &self.ready_mask,
                slot: bit,
            };
            let done = unsafe { (slot.poll_fn)(slot.data, &wd) };
            slot.state = if done { SLOT_DONE } else { SLOT_PARKED };
            if done { self.count = self.count.saturating_sub(1); }
        }
        self.count
    }

    /// Loop until all tasks complete. Yields cooperatively between passes.
    pub fn run_forever(&mut self) {
        loop {
            let alive = self.run_until_stall();
            if alive == 0 { break; }
            let _ = tachyon::yield_retrocausally(alive as u64);
        }
    }

    pub const fn task_count(&self) -> usize { self.count }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    struct CountFuture { steps: u32, done: u32 }
    impl Future for CountFuture {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            self.done += 1;
            COUNTER.fetch_add(1, Ordering::Relaxed);
            if self.done >= self.steps {
                Poll::Ready(())
            } else {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    #[test]
    fn executor_runs_future_to_completion() {
        let mut exec = OuroborosExecutor::new();
        let mut task = CountFuture { steps: 3, done: 0 };
        COUNTER.store(0, Ordering::Relaxed);
        unsafe { exec.spawn_raw(&mut task).unwrap(); }
        exec.run_forever();
        assert_eq!(COUNTER.load(Ordering::Relaxed), 3);
        assert_eq!(exec.task_count(), 0);
    }
}
