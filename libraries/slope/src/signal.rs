// CORONAL DISCHARGE — typed signal replacement
//
// No POSIX signals. No reentrancy hazard. No async-signal-safe restriction.
// The kernel delivers DischargeEvent notifications by calling the registered
// CoronalTrampoline — a function pointer the process installs via SYS_SIGNAL_DELIVER.
//
// DischargeClass: the numeric type of the notification.
//   0   = Reserved / null
//   1   = TerminationRequest (equivalent to SIGTERM)
//   2   = FaultNotification  (equivalent to SIGSEGV — process may inspect and recover)
//   3   = ResourcePressure   (kernel is under memory pressure — shed load)
//   4   = ThermalThrottle    (CPU thermal limit approached — slow down)
//   5   = PeerDisconnected   (a channel endpoint was closed by the other side)
//   6   = CapabilityRevoked  (a kernel capability was revoked)
//   7   = UserDefined(u8)    (application-defined, 7–255)
//
// CoronalMatrix: a fixed-size dispatch table mapping DischargeClass → handler fn.
//   install() registers the trampoline with the kernel.
//   dispatch() is called by the trampoline — routes to the correct handler.
//
// The trampoline is `extern "C"` with a specific calling convention.
// No heap use in the signal path. No allocator calls. No locks.

use crate::syscalls::SYS_SIGNAL_DELIVER;
use crate::syscall;
use crate::SyscallError;

pub const MAX_DISCHARGE_CLASSES: usize = 256;
pub const DISCHARGE_PAYLOAD_BYTES: usize = 48;

// ─── DISCHARGE CLASSES ─────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum DischargeClass {
    Reserved          = 0,
    TerminationRequest= 1,
    FaultNotification = 2,
    ResourcePressure  = 3,
    ThermalThrottle   = 4,
    PeerDisconnected  = 5,
    CapabilityRevoked = 6,
    UserDefined(u8),  // 7–255
}

impl DischargeClass {
    pub const fn from_raw(n: u8) -> Self {
        match n {
            0 => Self::Reserved,
            1 => Self::TerminationRequest,
            2 => Self::FaultNotification,
            3 => Self::ResourcePressure,
            4 => Self::ThermalThrottle,
            5 => Self::PeerDisconnected,
            6 => Self::CapabilityRevoked,
            n => Self::UserDefined(n),
        }
    }

    pub const fn to_raw(self) -> u8 {
        match self {
            Self::Reserved           => 0,
            Self::TerminationRequest => 1,
            Self::FaultNotification  => 2,
            Self::ResourcePressure   => 3,
            Self::ThermalThrottle    => 4,
            Self::PeerDisconnected   => 5,
            Self::CapabilityRevoked  => 6,
            Self::UserDefined(n)     => n,
        }
    }
}

// ─── DISCHARGE EVENT ───────────────────────────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DischargeEvent {
    pub class:   u8,
    pub _pad:    [u8; 7],
    pub payload: [u8; DISCHARGE_PAYLOAD_BYTES],
    pub sender_pid: u64,
}

// ─── HANDLER TYPE ──────────────────────────────────────────────────────────

pub type DischargeHandler = fn(event: &DischargeEvent);

fn default_handler(event: &DischargeEvent) {
    // Default: termination request exits cleanly; others are ignored.
    if event.class == 1 {
        let _ = unsafe { syscall(crate::syscalls::SYS_EXIT, [0usize; 6]) };
    }
}

// ─── CORONAL MATRIX ────────────────────────────────────────────────────────

pub struct CoronalMatrix {
    handlers: [DischargeHandler; MAX_DISCHARGE_CLASSES],
}

impl CoronalMatrix {
    pub const fn new() -> Self {
        Self { handlers: [default_handler; MAX_DISCHARGE_CLASSES] }
    }

    pub fn set(&mut self, class: DischargeClass, handler: DischargeHandler) {
        self.handlers[class.to_raw() as usize] = handler;
    }

    pub fn clear(&mut self, class: DischargeClass) {
        self.handlers[class.to_raw() as usize] = default_handler;
    }

    /// Called by the kernel trampoline. Routes to the registered handler.
    /// Must be signal-safe: no heap, no locks, no panics.
    pub fn dispatch(&self, event: &DischargeEvent) {
        let handler = self.handlers[event.class as usize];
        handler(event);
    }

    /// Register the trampoline with the Boulder kernel.
    /// `trampoline` is an `extern "C" fn(*const DischargeEvent)` set up by _start.
    pub fn install(&self, trampoline: usize) -> Result<(), SyscallError> {
        let args = [
            trampoline,
            self as *const Self as usize,
            0, 0, 0, 0,
        ];
        unsafe { syscall(SYS_SIGNAL_DELIVER, args) }.map(|_| ())
    }
}

/// The kernel calls this. It dispatches into the matrix.
/// # Safety: called from kernel context — no Rust stack unwinding.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _coronal_trampoline(
    matrix_ptr: *const CoronalMatrix,
    event_ptr:  *const DischargeEvent,
) {
    if matrix_ptr.is_null() || event_ptr.is_null() { return; }
    unsafe { (*matrix_ptr).dispatch(&*event_ptr); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicU8, Ordering};

    static LAST_CLASS: AtomicU8 = AtomicU8::new(255);

    fn test_handler(event: &DischargeEvent) {
        LAST_CLASS.store(event.class, Ordering::Relaxed);
    }

    #[test]
    fn matrix_dispatches_to_registered_handler() {
        let mut matrix = CoronalMatrix::new();
        matrix.set(DischargeClass::ThermalThrottle, test_handler);
        let event = DischargeEvent {
            class:      DischargeClass::ThermalThrottle.to_raw(),
            _pad:       [0; 7],
            payload:    [0; DISCHARGE_PAYLOAD_BYTES],
            sender_pid: 0,
        };
        matrix.dispatch(&event);
        assert_eq!(LAST_CLASS.load(Ordering::Relaxed), 4);
    }
}
