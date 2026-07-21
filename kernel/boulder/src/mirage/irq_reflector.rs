use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicUsize, Ordering};

use sisyphus_driver_abi::{Handle, STATUS_BUSY, Status};

const REFLECTOR_QUEUE_SIZE: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IrqEvent {
    pub irq_number: u32,
    pub registration: Handle,
}

pub trait IpiWakeup: Sync {
    fn wake_enclave(&self, destination_apic_id: u32) -> Status;
}

/// Single-producer, single-consumer IRQ handoff from Aegis to one enclave CPU.
pub struct CoreReflector {
    head: AtomicUsize,
    tail: AtomicUsize,
    buffer: [UnsafeCell<MaybeUninit<IrqEvent>>; REFLECTOR_QUEUE_SIZE],
    enclave_apic_id: u32,
}

impl CoreReflector {
    pub const fn new(enclave_apic_id: u32) -> Self {
        Self {
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            buffer: [const { UnsafeCell::new(MaybeUninit::uninit()) }; REFLECTOR_QUEUE_SIZE],
            enclave_apic_id,
        }
    }

    pub fn reflect_hardware_interrupt(&self, event: IrqEvent, wakeup: &dyn IpiWakeup) -> Status {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail.wrapping_sub(head) >= REFLECTOR_QUEUE_SIZE {
            return STATUS_BUSY;
        }
        let slot = tail % REFLECTOR_QUEUE_SIZE;
        unsafe { (*self.buffer[slot].get()).write(event) };
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        wakeup.wake_enclave(self.enclave_apic_id)
    }

    pub fn consume(&self) -> Option<IrqEvent> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head == tail {
            return None;
        }
        let slot = head % REFLECTOR_QUEUE_SIZE;
        let event = unsafe { (*self.buffer[slot].get()).assume_init_read() };
        self.head.store(head.wrapping_add(1), Ordering::Release);
        Some(event)
    }
}

// SAFETY: The queue contract permits one Aegis producer and one enclave
// consumer. Release/acquire publication prevents concurrent slot ownership.
unsafe impl Sync for CoreReflector {}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestWakeup;

    impl IpiWakeup for TestWakeup {
        fn wake_enclave(&self, destination_apic_id: u32) -> Status {
            assert_eq!(destination_apic_id, 7);
            sisyphus_driver_abi::STATUS_OK
        }
    }

    #[test]
    fn reflects_opaque_registration_handles_without_raw_pointers() {
        let reflector = CoreReflector::new(7);
        let event = IrqEvent {
            irq_number: 5,
            registration: 42,
        };
        assert_eq!(
            reflector.reflect_hardware_interrupt(event, &TestWakeup),
            sisyphus_driver_abi::STATUS_OK
        );
        assert_eq!(reflector.consume(), Some(event));
        assert_eq!(reflector.consume(), None);
    }
}
