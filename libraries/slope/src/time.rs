#[cfg(not(target_arch = "x86_64"))]
use core::sync::atomic::{AtomicU64, Ordering};

/// Reads a monotonically increasing platform counter.
///
/// Counter frequency is platform-defined. Callers may compare deltas but must
/// not interpret them as nanoseconds without a kernel-provided scale.
#[inline(always)]
pub fn read_counter() -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        let low: u32;
        let high: u32;
        // SAFETY: RDTSC is configured as a user-accessible observation on the
        // x86-64 process ABI. It does not dereference memory or alter state.
        unsafe {
            core::arch::asm!(
                "rdtsc",
                out("eax") low,
                out("edx") high,
                options(nomem, nostack)
            );
        }
        (u64::from(high) << 32) | u64::from(low)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        static FALLBACK_COUNTER: AtomicU64 = AtomicU64::new(0);
        FALLBACK_COUNTER.fetch_add(1, Ordering::Relaxed)
    }
}
