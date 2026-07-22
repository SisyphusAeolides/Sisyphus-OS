use core::{marker::PhantomData, ptr::NonNull};

pub const MAXIMUM_DIRECT_EPOCH_DELTA: u64 = 10_000;

/// Resolves a stale address while retaining the kernel's mapping lease.
pub trait TimelineResolver<T> {
    fn resolve(
        &self,
        original: NonNull<T>,
        origin_epoch: u64,
        current_epoch: u64,
    ) -> Option<NonNull<T>>;
}

/// A process-local address accompanied by explicit causal metadata.
///
/// Metadata is deliberately stored beside the address. Packing an epoch and a
/// velocity into x86-64's sixteen non-address bits would overlap both fields
/// and can create non-canonical pointers.
#[derive(Clone, Copy)]
pub struct RelativisticPtr<T> {
    address: NonNull<T>,
    origin_epoch: u64,
    thread_velocity: u16,
    _marker: PhantomData<T>,
}

impl<T> RelativisticPtr<T> {
    /// Creates a pointer token for an already mapped, kernel-authorized object.
    ///
    /// # Safety
    ///
    /// `address` must identify a live, correctly aligned `T` for the lifetime
    /// of the external mapping lease. This token does not extend that lease.
    pub unsafe fn from_raw(
        address: *mut T,
        origin_epoch: u64,
        thread_velocity: u16,
    ) -> Option<Self> {
        Some(Self {
            address: NonNull::new(address)?,
            origin_epoch,
            thread_velocity,
            _marker: PhantomData,
        })
    }

    pub const fn origin_epoch(&self) -> u64 {
        self.origin_epoch
    }

    pub const fn thread_velocity(&self) -> u16 {
        self.thread_velocity
    }

    pub const fn original_address(&self) -> NonNull<T> {
        self.address
    }

    /// Resolves the current address without manufacturing an unbounded Rust
    /// reference to memory that may later be revoked.
    pub fn resolve<R: TimelineResolver<T>>(
        &self,
        current_epoch: u64,
        resolver: &R,
    ) -> Result<NonNull<T>, RelativisticError> {
        let delta = current_epoch
            .checked_sub(self.origin_epoch)
            .ok_or(RelativisticError::FutureEpoch)?;
        if delta <= MAXIMUM_DIRECT_EPOCH_DELTA {
            return Ok(self.address);
        }
        resolver
            .resolve(self.address, self.origin_epoch, current_epoch)
            .ok_or(RelativisticError::Revoked)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RelativisticError {
    FutureEpoch,
    Revoked,
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MoveTo(NonNull<u32>);

    impl TimelineResolver<u32> for MoveTo {
        fn resolve(
            &self,
            _original: NonNull<u32>,
            _origin_epoch: u64,
            _current_epoch: u64,
        ) -> Option<NonNull<u32>> {
            Some(self.0)
        }
    }

    #[test]
    fn resolves_only_after_the_direct_epoch_window() {
        let mut first = 1_u32;
        let mut second = 2_u32;
        // SAFETY: Both stack objects remain live for the complete test.
        let token = unsafe { RelativisticPtr::from_raw(&mut first, 7, 3).unwrap() };
        let moved = NonNull::from(&mut second);
        assert_eq!(
            token.resolve(10, &MoveTo(moved)).unwrap(),
            token.original_address()
        );
        assert_eq!(
            token
                .resolve(7 + MAXIMUM_DIRECT_EPOCH_DELTA + 1, &MoveTo(moved))
                .unwrap(),
            moved
        );
        assert_eq!(
            token.resolve(6, &MoveTo(moved)),
            Err(RelativisticError::FutureEpoch)
        );
    }
}
