#[cfg(target_arch = "x86_64")]
pub mod x86_64;

/// Compile-time contract implemented by each supported kernel architecture.
///
/// Hardware thread identifiers are firmware identifiers and are not required
/// to be dense array indices. Callers must translate them through the CPU
/// topology before indexing per-CPU storage.
pub trait Architecture: 'static + Send + Sync {
    const NAME: &'static str;
    const PAGE_SHIFT: usize;
    const PAGE_SIZE: usize = 1 << Self::PAGE_SHIFT;
    const CACHE_LINE_SIZE: usize;
    const MAXIMUM_CPUS: usize;

    fn hardware_thread_id() -> u32;
    /// Samples the architecture's inexpensive local time/accounting counter.
    /// The value has no duration or cross-CPU ordering meaning until the
    /// platform timer code validates and calibrates it.
    fn counter_sample() -> u64;
    fn spin_wait();
    fn halt() -> !;

    /// Disables maskable interrupts and returns the previous interrupt state.
    fn save_and_disable_interrupts() -> InterruptState;

    /// Restores a state returned by `save_and_disable_interrupts` on the same
    /// hardware thread.
    ///
    /// # Safety
    ///
    /// `state` must have been captured on the current hardware thread and must
    /// be restored exactly once, in properly nested order.
    unsafe fn restore_interrupts(state: InterruptState);

    /// Invalidates one local translation after its page-table update has been
    /// published. Remote processors require a separate shootdown protocol.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `virtual_address` is the affected mapping
    /// and that the page-table update is globally visible.
    unsafe fn invalidate_local_page(virtual_address: usize);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InterruptState {
    interrupts_were_enabled: bool,
}

impl InterruptState {
    pub(crate) const fn new(interrupts_were_enabled: bool) -> Self {
        Self {
            interrupts_were_enabled,
        }
    }

    pub const fn interrupts_were_enabled(self) -> bool {
        self.interrupts_were_enabled
    }
}

#[cfg(target_arch = "x86_64")]
pub type Active = x86_64::X86_64;
