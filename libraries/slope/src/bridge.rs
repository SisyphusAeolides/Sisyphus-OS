use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, Ordering};

pub const RING_ENTRIES: usize = 256;
pub const MAXIMUM_FRAME_BYTES: usize = 2048;

const RECEIVE_DEVICE_OWNED: u32 = 0;
const RECEIVE_READY: u32 = 1;
const RECEIVE_USER_HELD: u32 = 2;
const TRANSMIT_FREE: u32 = 0;
const TRANSMIT_BUILDING: u32 = 1;
const TRANSMIT_DEVICE_OWNED: u32 = 2;

#[repr(C)]
struct DmaDescriptor {
    buffer: UnsafeCell<[u8; MAXIMUM_FRAME_BYTES]>,
    length: AtomicU32,
    state: AtomicU32,
}

// SAFETY: Buffer access is exclusively transferred through `state`; every
// transition that publishes bytes uses Release and every consumer uses Acquire.
unsafe impl Sync for DmaDescriptor {}

impl DmaDescriptor {
    const fn receive() -> Self {
        Self {
            buffer: UnsafeCell::new([0; MAXIMUM_FRAME_BYTES]),
            length: AtomicU32::new(0),
            state: AtomicU32::new(RECEIVE_DEVICE_OWNED),
        }
    }

    const fn transmit() -> Self {
        Self {
            buffer: UnsafeCell::new([0; MAXIMUM_FRAME_BYTES]),
            length: AtomicU32::new(0),
            state: AtomicU32::new(TRANSMIT_FREE),
        }
    }
}

/// Coherent DMA storage. A capability-bearing kernel driver must pin and map
/// the complete `DMA_SPAN_BYTES`; alignment alone does not make it one page.
#[repr(C, align(4096))]
pub struct BifrostRing {
    receive_consumer: AtomicU32,
    transmit_producer: AtomicU32,
    receive: [DmaDescriptor; RING_ENTRIES],
    transmit: [DmaDescriptor; RING_ENTRIES],
}

pub const DMA_SPAN_BYTES: usize = core::mem::size_of::<BifrostRing>();
const _: () = assert!(core::mem::align_of::<BifrostRing>() == 4096);

impl BifrostRing {
    pub const fn new() -> Self {
        Self {
            receive_consumer: AtomicU32::new(0),
            transmit_producer: AtomicU32::new(0),
            receive: [const { DmaDescriptor::receive() }; RING_ENTRIES],
            transmit: [const { DmaDescriptor::transmit() }; RING_ENTRIES],
        }
    }

    /// Acquires one received packet. Hardware ownership is restored only when
    /// the returned guard is dropped.
    pub fn try_receive(&self) -> Result<Option<ReceivedPacket<'_>>, BridgeError> {
        let index = self.receive_consumer.load(Ordering::Relaxed) as usize % RING_ENTRIES;
        let descriptor = &self.receive[index];
        if descriptor
            .state
            .compare_exchange(
                RECEIVE_READY,
                RECEIVE_USER_HELD,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_err()
        {
            return Ok(None);
        }
        let length = descriptor.length.load(Ordering::Relaxed) as usize;
        if length > MAXIMUM_FRAME_BYTES {
            descriptor.length.store(0, Ordering::Relaxed);
            descriptor
                .state
                .store(RECEIVE_DEVICE_OWNED, Ordering::Release);
            self.receive_consumer
                .store(next_index(index), Ordering::Release);
            return Err(BridgeError::InvalidHardwareLength);
        }
        Ok(Some(ReceivedPacket {
            ring: self,
            index,
            length,
        }))
    }

    /// Copies one frame into a free transmit descriptor and transfers it to
    /// the device. The DMA mapping itself remains zero-copy across privilege.
    pub fn transmit(&self, payload: &[u8]) -> Result<(), BridgeError> {
        if payload.is_empty() || payload.len() > MAXIMUM_FRAME_BYTES {
            return Err(BridgeError::InvalidPayloadLength);
        }
        let index = self.transmit_producer.load(Ordering::Relaxed) as usize % RING_ENTRIES;
        let descriptor = &self.transmit[index];
        descriptor
            .state
            .compare_exchange(
                TRANSMIT_FREE,
                TRANSMIT_BUILDING,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .map_err(|_| BridgeError::TransmitQueueFull)?;
        // SAFETY: TRANSMIT_BUILDING grants this producer exclusive access to
        // the descriptor buffer until Release publishes DEVICE_OWNED.
        unsafe {
            (&mut *descriptor.buffer.get())[..payload.len()].copy_from_slice(payload);
        }
        descriptor
            .length
            .store(payload.len() as u32, Ordering::Relaxed);
        descriptor
            .state
            .store(TRANSMIT_DEVICE_OWNED, Ordering::Release);
        self.transmit_producer
            .store(next_index(index), Ordering::Release);
        Ok(())
    }

    #[cfg(test)]
    fn inject_receive(&self, index: usize, payload: &[u8]) {
        let descriptor = &self.receive[index];
        assert_eq!(
            descriptor.state.load(Ordering::Acquire),
            RECEIVE_DEVICE_OWNED
        );
        // SAFETY: This test helper models the sole DMA writer while the device
        // owns the descriptor.
        unsafe {
            (&mut *descriptor.buffer.get())[..payload.len()].copy_from_slice(payload);
        }
        descriptor
            .length
            .store(payload.len() as u32, Ordering::Relaxed);
        descriptor.state.store(RECEIVE_READY, Ordering::Release);
    }
}

impl Default for BifrostRing {
    fn default() -> Self {
        Self::new()
    }
}

fn next_index(index: usize) -> u32 {
    ((index + 1) % RING_ENTRIES) as u32
}

pub struct ReceivedPacket<'ring> {
    ring: &'ring BifrostRing,
    index: usize,
    length: usize,
}

impl ReceivedPacket<'_> {
    pub fn bytes(&self) -> &[u8] {
        let descriptor = &self.ring.receive[self.index];
        // SAFETY: RECEIVE_USER_HELD prevents the device from mutating this
        // buffer until this guard's Drop returns ownership.
        unsafe { &(&*descriptor.buffer.get())[..self.length] }
    }
}

impl Drop for ReceivedPacket<'_> {
    fn drop(&mut self) {
        let descriptor = &self.ring.receive[self.index];
        descriptor.length.store(0, Ordering::Relaxed);
        descriptor
            .state
            .store(RECEIVE_DEVICE_OWNED, Ordering::Release);
        self.ring
            .receive_consumer
            .store(next_index(self.index), Ordering::Release);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BridgeError {
    InvalidPayloadLength,
    InvalidHardwareLength,
    TransmitQueueFull,
    InvalidCapability,
}

#[cfg(test)]
mod tests {
    use super::*;

    static RING: BifrostRing = BifrostRing::new();

    #[test]
    fn receive_guard_retains_bytes_until_drop() {
        RING.inject_receive(0, b"packet");
        let packet = RING.try_receive().unwrap().unwrap();
        assert_eq!(packet.bytes(), b"packet");
        assert!(RING.try_receive().unwrap().is_none());
        drop(packet);
        assert!(RING.try_receive().unwrap().is_none());
    }
}
