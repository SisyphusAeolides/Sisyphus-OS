use sisyphus_driver_abi::{STATUS_BUSY, STATUS_NOT_FOUND, STATUS_OK, Status};

use crate::sync::SpinLock;

use super::pci::PciAddress;

const EVENT_CAPACITY: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciHotplugEvent {
    pub address: PciAddress,
    pub vendor_id: u16,
    pub device_id: u16,
}

impl PciHotplugEvent {
    const EMPTY: Self = Self {
        address: PciAddress {
            bus: 0,
            slot: 0,
            function: 0,
        },
        vendor_id: 0xffff,
        device_id: 0xffff,
    };
}

struct QueueState {
    events: [PciHotplugEvent; EVENT_CAPACITY],
    read_index: usize,
    write_index: usize,
    length: usize,
}

impl QueueState {
    const fn new() -> Self {
        Self {
            events: [PciHotplugEvent::EMPTY; EVENT_CAPACITY],
            read_index: 0,
            write_index: 0,
            length: 0,
        }
    }
}

pub struct HotplugQueue {
    state: SpinLock<QueueState>,
}

impl HotplugQueue {
    pub const fn new() -> Self {
        Self {
            state: SpinLock::new(QueueState::new()),
        }
    }

    pub fn enqueue_from_interrupt(&self, event: PciHotplugEvent) -> Status {
        let mut state = self.state.lock();
        if state.length == EVENT_CAPACITY {
            return STATUS_BUSY;
        }
        let write_index = state.write_index;
        state.events[write_index] = event;
        state.write_index = (write_index + 1) % EVENT_CAPACITY;
        state.length += 1;
        STATUS_OK
    }

    pub fn dequeue(&self) -> Option<PciHotplugEvent> {
        let mut state = self.state.lock();
        if state.length == 0 {
            return None;
        }
        let read_index = state.read_index;
        let event = state.events[read_index];
        state.read_index = (read_index + 1) % EVENT_CAPACITY;
        state.length -= 1;
        Some(event)
    }
}

impl Default for HotplugQueue {
    fn default() -> Self {
        Self::new()
    }
}

pub trait DriverBinder: Sync {
    fn bind_pci_device(&self, event: PciHotplugEvent) -> Status;
}

pub trait DeviceEventSink: Sync {
    fn pci_device_ready(&self, event: PciHotplugEvent) -> Status;
}

pub fn process_one(
    queue: &HotplugQueue,
    binder: &dyn DriverBinder,
    events: &dyn DeviceEventSink,
) -> Status {
    let Some(event) = queue.dequeue() else {
        return STATUS_NOT_FOUND;
    };
    let status = binder.bind_pci_device(event);
    if status != STATUS_OK {
        return status;
    }
    events.pci_device_ready(event)
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct TestConsumer {
        calls: AtomicUsize,
    }

    impl DriverBinder for TestConsumer {
        fn bind_pci_device(&self, _event: PciHotplugEvent) -> Status {
            self.calls.fetch_add(1, Ordering::Relaxed);
            STATUS_OK
        }
    }

    impl DeviceEventSink for TestConsumer {
        fn pci_device_ready(&self, _event: PciHotplugEvent) -> Status {
            self.calls.fetch_add(1, Ordering::Relaxed);
            STATUS_OK
        }
    }

    #[test]
    fn defers_binding_and_notification_outside_the_interrupt() {
        let queue = HotplugQueue::new();
        let event = PciHotplugEvent {
            address: PciAddress::new(0, 3, 0).unwrap(),
            vendor_id: 0x1234,
            device_id: 0x5678,
        };
        assert_eq!(queue.enqueue_from_interrupt(event), STATUS_OK);

        let consumer = TestConsumer {
            calls: AtomicUsize::new(0),
        };
        assert_eq!(process_one(&queue, &consumer, &consumer), STATUS_OK);
        assert_eq!(consumer.calls.load(Ordering::Relaxed), 2);
        assert_eq!(process_one(&queue, &consumer, &consumer), STATUS_NOT_FOUND);
    }

    #[test]
    fn reports_queue_overflow_without_allocating() {
        let queue = HotplugQueue::new();
        let event = PciHotplugEvent::EMPTY;
        for _ in 0..EVENT_CAPACITY {
            assert_eq!(queue.enqueue_from_interrupt(event), STATUS_OK);
        }
        assert_eq!(queue.enqueue_from_interrupt(event), STATUS_BUSY);
    }
}
