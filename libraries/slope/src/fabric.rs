// FABRIC CLIENT — userland thread spawning over SYS_THREAD_SPAWN
//
// FabricFiber<F>: owns a caller-provided stack buffer + a typed entry fn.
//   The kernel gets: (entry_trampoline, stack_top, arg_ptr)
//   arg_ptr points to the FiberArgs on the fiber's own stack — zero copy.
//
// FiberOutcome: atomic exit value written by the thread before SYS_THREAD_EXIT.
//   Parent polls try_harvest() — non-blocking, no futex, no kernel wait.
//
// FiberArgs: the typed closure + outcome pointer packed into one struct.
//   Stored inside the fiber's stack buffer before spawn — no separate alloc.
//
// Stack alignment: the kernel receives a 16-byte aligned stack_top.
//   We carve out FiberArgs from the TOP of the buffer, then align down.
//
// FabricWeave: a fixed array of up to MAX_FIBERS live fibers.
//   spawn() adds a fiber; harvest_all() collects completed ones.
//   No heap involvement anywhere.

use core::sync::atomic::{AtomicU64, Ordering};
use crate::syscalls::{SYS_THREAD_SPAWN, SYS_THREAD_EXIT};
use crate::syscall;
use crate::SyscallError;

pub const MAX_FIBERS:    usize = 32;
pub const MIN_STACK_BYTES: usize = 4096;

pub const OUTCOME_PENDING:   u64 = u64::MAX;
pub const OUTCOME_PANICKED:  u64 = u64::MAX - 1;

// ─── FIBER ARGS (lives at top of stack) ────────────────────────────────────

type _EntryFn = unsafe extern "C" fn(*mut u8) -> !;

// Stored at aligned offset inside the fiber stack.
struct FiberArgs {
    entry:   *const (),       // pointer to run_fiber::<F>
    closure: *mut u8,         // pointer to the F value (also in stack)
    outcome: *const AtomicU64,
}

// ─── TRAMPOLINE ────────────────────────────────────────────────────────────

/// Called by the kernel on the new thread. Extracts args, runs F, exits.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _fabric_trampoline(args_ptr: *mut u8) -> ! {
    let args = unsafe { &*(args_ptr as *const FiberArgs) };
    let run: unsafe fn(*mut u8, *const AtomicU64) -> ! =
        unsafe { core::mem::transmute(args.entry) };
    unsafe { run(args.closure, args.outcome) }
}

unsafe fn run_fiber<F: FnOnce() -> u64>(closure_ptr: *mut u8, outcome: *const AtomicU64) -> ! {
    let f = unsafe { core::ptr::read(closure_ptr as *mut F) };
    let result = f();
    unsafe { (*outcome).store(result, Ordering::Release); }
    let _ = unsafe { syscall(SYS_THREAD_EXIT, [0usize; 6]) };
    loop { core::hint::spin_loop(); }
}

// ─── FIBER HANDLE ──────────────────────────────────────────────────────────

pub struct FabricFiber {
    outcome: AtomicU64,
    tid:     u64,
}

impl FabricFiber {
    const fn new() -> Self {
        Self { outcome: AtomicU64::new(OUTCOME_PENDING), tid: 0 }
    }

    pub fn try_harvest(&self) -> Option<u64> {
        let v = self.outcome.load(Ordering::Acquire);
        if v == OUTCOME_PENDING { None } else { Some(v) }
    }

    pub const fn tid(&self) -> u64 { self.tid }

    pub fn spin_until_done(&self) -> u64 {
        loop {
            if let Some(v) = self.try_harvest() { return v; }
            core::hint::spin_loop();
        }
    }
}

// ─── SPAWN ─────────────────────────────────────────────────────────────────

/// Spawn a fiber using `stack_buf` as its stack.
///
/// `closure` is moved into the top of `stack_buf` before the syscall.
/// `fiber` is the caller-owned FabricFiber whose outcome the thread will write.
///
/// # Safety
/// - `stack_buf` must be at least MIN_STACK_BYTES long and remain valid until
///   `fiber.try_harvest()` returns Some.
/// - `fiber` must not move after spawn (the thread holds a raw pointer to it).
/// - `closure` must be Send.
pub unsafe fn spawn<F>(
    fiber:     &mut FabricFiber,
    stack_buf: &mut [u8],
    closure:   F,
) -> Result<(), SyscallError>
where
    F: FnOnce() -> u64 + Send,
{
    assert!(stack_buf.len() >= MIN_STACK_BYTES);

    // Reset outcome
    fiber.outcome.store(OUTCOME_PENDING, Ordering::Relaxed);

    // Carve space for FiberArgs + F at the top of the stack, 16-byte aligned.
    let total_args = core::mem::size_of::<FiberArgs>() + core::mem::size_of::<F>();
    let buf_end    = stack_buf.as_mut_ptr() as usize + stack_buf.len();
    let args_start = (buf_end - total_args) & !15usize;
    let clos_start = args_start + core::mem::size_of::<FiberArgs>();

    let args_ptr = args_start as *mut FiberArgs;
    let clos_ptr = clos_start as *mut F;

    // Write closure into stack
    unsafe { core::ptr::write(clos_ptr, closure); }

    // Write FiberArgs
    unsafe {
        core::ptr::write(args_ptr, FiberArgs {
            entry:   run_fiber::<F> as *const (),
            closure: clos_ptr as *mut u8,
            outcome: &fiber.outcome as *const AtomicU64,
        });
    }

    // Stack top for the kernel = args_start aligned down to 16
    let stack_top = args_start & !15usize;

    let sys_args = [
        _fabric_trampoline as *const () as usize,
        stack_top,
        args_ptr as usize,
        0, 0, 0,
    ];
    let tid = unsafe { syscall(SYS_THREAD_SPAWN, sys_args) }?;
    fiber.tid = tid as u64;
    Ok(())
}

// ─── WEAVE — fixed fiber pool ───────────────────────────────────────────────

pub struct FabricWeave {
    fibers: [FabricFiber; MAX_FIBERS],
    count:  usize,
}

impl FabricWeave {
    pub const fn new() -> Self {
        Self {
            fibers: [const { FabricFiber::new() }; MAX_FIBERS],
            count:  0,
        }
    }

    /// Borrow the next free fiber slot.
    pub fn next_slot(&mut self) -> Option<&mut FabricFiber> {
        if self.count >= MAX_FIBERS { return None; }
        let slot = &mut self.fibers[self.count];
        self.count += 1;
        Some(slot)
    }

    /// Collect all finished fibers. Calls `on_done(tid, outcome)` for each.
    pub fn harvest_all(&mut self, mut on_done: impl FnMut(u64, u64)) {
        for fiber in self.fibers[..self.count].iter() {
            if let Some(outcome) = fiber.try_harvest() {
                on_done(fiber.tid(), outcome);
            }
        }
    }

    /// Block until all spawned fibers complete.
    pub fn join_all(&self) {
        for fiber in self.fibers[..self.count].iter() {
            fiber.spin_until_done();
        }
    }

    pub const fn live_count(&self) -> usize { self.count }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fiber_harvest_before_done_is_none() {
        let fiber = FabricFiber::new();
        assert!(fiber.try_harvest().is_none());
    }

    #[test]
    fn fiber_harvest_after_store_returns_value() {
        let fiber = FabricFiber::new();
        fiber.outcome.store(42, Ordering::Release);
        assert_eq!(fiber.try_harvest(), Some(42));
    }
}
