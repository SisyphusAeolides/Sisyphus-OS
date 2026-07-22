use core::sync::atomic::{AtomicUsize, Ordering};
use core::{cell::UnsafeCell, hint, marker::PhantomData};

const UNLOCKED: usize = 0;
const LOCKED: usize = 1;

/// Page-alignable shared storage guarded by a process-shared atomic lock.
#[repr(C, align(4096))]
pub struct SpookyCell<T> {
    lock: AtomicUsize,
    data: UnsafeCell<T>,
}

// SAFETY: Access to `data` is serialized by `lock`; moving T across the
// process-shared boundary requires T: Send.
unsafe impl<T: Send> Sync for SpookyCell<T> {}

impl<T> SpookyCell<T> {
    pub const fn new(value: T) -> Self {
        Self {
            lock: AtomicUsize::new(UNLOCKED),
            data: UnsafeCell::new(value),
        }
    }

    fn try_lock(&self) -> Result<SpookyGuard<'_, T>, EntanglementError> {
        self.lock
            .compare_exchange(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .map_err(|_| EntanglementError::Busy)?;
        Ok(SpookyGuard { cell: self })
    }
}

struct SpookyGuard<'cell, T> {
    cell: &'cell SpookyCell<T>,
}

impl<T> Drop for SpookyGuard<'_, T> {
    fn drop(&mut self) {
        self.cell.lock.store(UNLOCKED, Ordering::Release);
    }
}

/// A borrowed endpoint whose lifetime is bounded by an external mapping lease.
pub struct EntangledPair<'lease, T> {
    cell: &'lease SpookyCell<T>,
    dimension_id: u32,
    _lease: PhantomData<&'lease SpookyCell<T>>,
}

impl<'lease, T> EntangledPair<'lease, T> {
    /// Attaches to storage initialized by the authoritative mapping creator.
    ///
    /// # Safety
    ///
    /// The mapping must remain present and refer to this `SpookyCell<T>` for
    /// `'lease`. Every participant must use this lock protocol.
    pub const unsafe fn from_mapping(cell: &'lease SpookyCell<T>, dimension_id: u32) -> Self {
        Self {
            cell,
            dimension_id,
            _lease: PhantomData,
        }
    }

    pub const fn dimension_id(&self) -> u32 {
        self.dimension_id
    }

    pub fn try_mutate<R>(
        &self,
        operation: impl FnOnce(&mut T) -> R,
    ) -> Result<R, EntanglementError> {
        let guard = self.cell.try_lock()?;
        // SAFETY: The guard owns the process-shared lock for this cell.
        let result = operation(unsafe { &mut *guard.cell.data.get() });
        Ok(result)
    }

    pub fn try_observe<R>(&self, operation: impl FnOnce(&T) -> R) -> Result<R, EntanglementError> {
        let guard = self.cell.try_lock()?;
        // SAFETY: The guard owns the process-shared lock for this cell.
        let result = operation(unsafe { &*guard.cell.data.get() });
        Ok(result)
    }

    /// Retries a bounded number of times and never spins forever in Ring 3.
    pub fn mutate_bounded<R>(
        &self,
        attempts: usize,
        mut operation: impl FnMut(&mut T) -> R,
    ) -> Result<R, EntanglementError> {
        for _ in 0..attempts {
            match self.try_mutate(&mut operation) {
                Ok(result) => return Ok(result),
                Err(EntanglementError::Busy) => hint::spin_loop(),
            }
        }
        Err(EntanglementError::Busy)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntanglementError {
    Busy,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_mutation_and_observation() {
        let cell = SpookyCell::new(5_u64);
        // SAFETY: The local cell outlives the borrowed pair.
        let pair = unsafe { EntangledPair::from_mapping(&cell, 7) };
        pair.try_mutate(|value| *value += 3).unwrap();
        assert_eq!(pair.try_observe(|value| *value).unwrap(), 8);
        assert_eq!(pair.dimension_id(), 7);
    }
}
