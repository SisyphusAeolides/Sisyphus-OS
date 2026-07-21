use core::marker::PhantomData;

use crate::arch::{Architecture, InterruptState};

mod sealed {
    pub trait Sealed {}
}

pub trait Right: sealed::Sealed {}

pub struct FabricControl;
pub struct PhysicalMemoryControl;
pub struct DeviceMemoryControl;
pub struct DmaControl;
pub struct SchedulerControl;
pub struct PolicyControl;
pub struct MachineProfileControl;
pub struct ResonanceControl;
pub struct LearningControl;
pub struct MemorySharingControl;
pub struct FaultPolicyControl;
pub struct ArtifactSynthesisControl;
pub struct UserlandImageControl;

impl sealed::Sealed for FabricControl {}
impl sealed::Sealed for PhysicalMemoryControl {}
impl sealed::Sealed for DeviceMemoryControl {}
impl sealed::Sealed for DmaControl {}
impl sealed::Sealed for SchedulerControl {}
impl sealed::Sealed for PolicyControl {}
impl sealed::Sealed for MachineProfileControl {}
impl sealed::Sealed for ResonanceControl {}
impl sealed::Sealed for LearningControl {}
impl sealed::Sealed for MemorySharingControl {}
impl sealed::Sealed for FaultPolicyControl {}
impl sealed::Sealed for ArtifactSynthesisControl {}
impl sealed::Sealed for UserlandImageControl {}

impl Right for FabricControl {}
impl Right for PhysicalMemoryControl {}
impl Right for DeviceMemoryControl {}
impl Right for DmaControl {}
impl Right for SchedulerControl {}
impl Right for PolicyControl {}
impl Right for MachineProfileControl {}
impl Right for ResonanceControl {}
impl Right for LearningControl {}
impl Right for MemorySharingControl {}
impl Right for FaultPolicyControl {}
impl Right for ArtifactSynthesisControl {}
impl Right for UserlandImageControl {}

/// Root from which scoped kernel authority proofs are issued.
///
/// The root is intentionally neither `Copy` nor `Clone`. Creating it is an
/// explicit unsafe bootstrap operation; ordinary subsystems receive only the
/// rights they need.
pub struct Authority {
    _not_send_or_sync: PhantomData<*mut ()>,
}

impl Authority {
    /// Establishes the kernel's root authority.
    ///
    /// # Safety
    ///
    /// The caller must be the trusted kernel bootstrap path and must ensure
    /// that no independently constructed authority root can violate resource
    /// ownership policy.
    pub const unsafe fn assume_root() -> Self {
        Self {
            _not_send_or_sync: PhantomData,
        }
    }

    pub const fn grant<R: Right>(&self) -> Capability<'_, R> {
        Capability {
            _authority: PhantomData,
            _right: PhantomData,
        }
    }
}

/// A lifetime-bound proof that an authority root granted one specific right.
pub struct Capability<'authority, R: Right> {
    _authority: PhantomData<&'authority Authority>,
    _right: PhantomData<R>,
}

/// Guard proving that maskable interrupts are disabled on the current CPU.
///
/// The raw-pointer marker prevents the guard from moving to another thread or
/// CPU through safe code. Dropping the guard restores the captured state.
pub struct InterruptGuard<A: Architecture> {
    state: InterruptState,
    _architecture: PhantomData<A>,
    _not_send_or_sync: PhantomData<*mut ()>,
}

impl<A: Architecture> InterruptGuard<A> {
    pub fn enter() -> Self {
        Self {
            state: A::save_and_disable_interrupts(),
            _architecture: PhantomData,
            _not_send_or_sync: PhantomData,
        }
    }

    pub const fn proof(&self) -> InterruptsDisabled<'_> {
        InterruptsDisabled {
            _guard: PhantomData,
        }
    }
}

impl<A: Architecture> Drop for InterruptGuard<A> {
    fn drop(&mut self) {
        // SAFETY: The guard is non-transferable and owns this saved state. Its
        // destructor runs once and preserves critical-section nesting.
        unsafe { A::restore_interrupts(self.state) };
    }
}

pub struct InterruptsDisabled<'guard> {
    _guard: PhantomData<&'guard mut ()>,
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    static DISABLED: AtomicBool = AtomicBool::new(false);

    struct TestArchitecture;

    impl Architecture for TestArchitecture {
        const NAME: &'static str = "test";
        const PAGE_SHIFT: usize = 12;
        const CACHE_LINE_SIZE: usize = 64;
        const MAXIMUM_CPUS: usize = 1;

        fn hardware_thread_id() -> u32 {
            0
        }

        fn counter_sample() -> u64 {
            0
        }

        fn spin_wait() {}

        fn halt() -> ! {
            panic!("halt")
        }

        fn save_and_disable_interrupts() -> InterruptState {
            let was_enabled = !DISABLED.swap(true, Ordering::SeqCst);
            InterruptState::new(was_enabled)
        }

        unsafe fn restore_interrupts(state: InterruptState) {
            DISABLED.store(!state.interrupts_were_enabled(), Ordering::SeqCst);
        }

        unsafe fn invalidate_local_page(_virtual_address: usize) {}
    }

    #[test]
    fn authority_issues_distinct_scoped_rights() {
        let authority = unsafe { Authority::assume_root() };
        let _: Capability<'_, FabricControl> = authority.grant();
        let _: Capability<'_, DmaControl> = authority.grant();
    }

    #[test]
    fn interrupt_guard_restores_the_previous_state() {
        DISABLED.store(false, Ordering::SeqCst);
        {
            let guard = InterruptGuard::<TestArchitecture>::enter();
            let _proof = guard.proof();
            assert!(DISABLED.load(Ordering::SeqCst));
        }
        assert!(!DISABLED.load(Ordering::SeqCst));
    }
}
