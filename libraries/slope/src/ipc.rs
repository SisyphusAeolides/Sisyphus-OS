// QUANTUM CHANNEL — typed capability-bearing IPC
//
// QuantumChannel: a kernel-brokered pair of single-producer/single-consumer queues.
//   Each endpoint holds one send ring and one recv ring.
//   Messages are fixed 64-byte ChannelMessage frames.
//   Capability field: the kernel validates cap tokens on send, revokes on close.
//
// ChannelMessage layout (64 bytes):
//   [0..8]   capability: u64  — kernel-issued cap token (0 = none)
//   [8..12]  tag:        u32  — application-defined message type
//   [12..16] sequence:   u32  — monotonic send counter
//   [16..64] payload:   [u8;48]
//
// ChannelEndpoint: one end of a channel pair.
//   send(msg): lock-free enqueue to TX ring.
//   recv():    lock-free dequeue from RX ring; returns None if empty.
//
// ChannelPair: both endpoints, split after kernel creates the channel.
//   split() consumes the pair into (client_endpoint, server_endpoint).
//
// RingSlot: one cell in the channel ring, cache-line aligned (64B).
// CHANNEL_DEPTH: number of in-flight messages per direction.

extern crate alloc;

use core::sync::atomic::{AtomicU64, AtomicU32, Ordering};
use core::cell::UnsafeCell;
use crate::syscalls::SYS_CHANNEL;
use crate::syscall;

pub const CHANNEL_DEPTH:   usize = 128;
pub const MESSAGE_BYTES:   usize = 64;
pub const PAYLOAD_BYTES:   usize = 48;

// ─── MESSAGE ───────────────────────────────────────────────────────────────

#[repr(C, align(64))]
#[derive(Clone, Copy)]
pub struct ChannelMessage {
    pub capability: u64,
    pub tag:        u32,
    pub sequence:   u32,
    pub payload:    [u8; PAYLOAD_BYTES],
}

const _: () = assert!(core::mem::size_of::<ChannelMessage>() == MESSAGE_BYTES);

impl ChannelMessage {
    pub const fn empty() -> Self {
        Self { capability: 0, tag: 0, sequence: 0, payload: [0; PAYLOAD_BYTES] }
    }

    pub const fn with_tag(tag: u32) -> Self {
        Self { capability: 0, tag, sequence: 0, payload: [0; PAYLOAD_BYTES] }
    }

    pub fn with_payload(tag: u32, data: &[u8]) -> Self {
        let mut msg = Self::with_tag(tag);
        let n = data.len().min(PAYLOAD_BYTES);
        msg.payload[..n].copy_from_slice(&data[..n]);
        msg
    }
}

// ─── RING SLOT ─────────────────────────────────────────────────────────────

const SLOT_EMPTY:    u64 = 0;
const SLOT_WRITING:  u64 = 1;
const SLOT_READY:    u64 = 2;
const SLOT_READING:  u64 = 3;

#[repr(C, align(64))]
struct RingSlot {
    state:   AtomicU64,
    message: UnsafeCell<ChannelMessage>,
}

impl RingSlot {
    const fn new() -> Self {
        Self {
            state:   AtomicU64::new(SLOT_EMPTY),
            message: UnsafeCell::new(ChannelMessage::empty()),
        }
    }
}

unsafe impl Sync for RingSlot {}

// ─── SPSC RING ─────────────────────────────────────────────────────────────

#[repr(C, align(4096))]
pub struct SpscRing {
    producer: AtomicU32,
    consumer: AtomicU32,
    _pad:     [u8; 56],
    slots:    [RingSlot; CHANNEL_DEPTH],
}

impl SpscRing {
    pub const fn new() -> Self {
        Self {
            producer: AtomicU32::new(0),
            consumer: AtomicU32::new(0),
            _pad:     [0; 56],
            slots:    [const { RingSlot::new() }; CHANNEL_DEPTH],
        }
    }

    pub fn send(&self, msg: ChannelMessage) -> Result<(), ChannelError> {
        let head = self.producer.load(Ordering::Relaxed) as usize;
        let slot = &self.slots[head % CHANNEL_DEPTH];
        slot.state
            .compare_exchange(SLOT_EMPTY, SLOT_WRITING, Ordering::Acquire, Ordering::Relaxed)
            .map_err(|_| ChannelError::Full)?;
        // SAFETY: We hold WRITING — exclusive producer access.
        unsafe { *slot.message.get() = msg; }
        slot.state.store(SLOT_READY, Ordering::Release);
        self.producer.store(((head + 1) % CHANNEL_DEPTH) as u32, Ordering::Release);
        Ok(())
    }

    pub fn recv(&self) -> Option<ChannelMessage> {
        let tail = self.consumer.load(Ordering::Relaxed) as usize;
        let slot = &self.slots[tail % CHANNEL_DEPTH];
        slot.state
            .compare_exchange(SLOT_READY, SLOT_READING, Ordering::Acquire, Ordering::Relaxed)
            .ok()?;
        // SAFETY: We hold READING — exclusive consumer access.
        let msg = unsafe { *slot.message.get() };
        slot.state.store(SLOT_EMPTY, Ordering::Release);
        self.consumer.store(((tail + 1) % CHANNEL_DEPTH) as u32, Ordering::Release);
        Some(msg)
    }

    pub fn is_empty(&self) -> bool {
        let tail = self.consumer.load(Ordering::Relaxed) as usize;
        let slot = &self.slots[tail % CHANNEL_DEPTH];
        slot.state.load(Ordering::Relaxed) != SLOT_READY
    }
}

// ─── CHANNEL PAIR ──────────────────────────────────────────────────────────
// A ChannelPair owns two rings in a single kernel-mapped allocation.
// After kernel creates it, split() gives each side one endpoint.

pub struct ChannelPair {
    a_to_b: SpscRing,
    b_to_a: SpscRing,
    _kernel_handle: u64,
}

impl ChannelPair {
    /// Allocate a new channel pair via the Boulder capability broker.
    pub fn create() -> Result<Self, ChannelError> {
        let args = [0usize; 6];
        let handle = unsafe { syscall(SYS_CHANNEL, args) }
            .map_err(|_| ChannelError::KernelRefused)?;
        Ok(Self {
            a_to_b: SpscRing::new(),
            b_to_a: SpscRing::new(),
            _kernel_handle: handle as u64,
        })
    }

    /// Returns (endpoint_a, endpoint_b). Consumes self.
    /// endpoint_a sends on a_to_b, recvs on b_to_a.
    /// endpoint_b sends on b_to_a, recvs on a_to_b.
    pub fn split(self) -> (ChannelEndpointA<'static>, ChannelEndpointB<'static>) {
        // Leak the pair into a static allocation — both endpoints borrow it.
        // In a real kernel the rings live in the shared mapping.
        // Here we box-leak for now; replace with SYS_SHMAP in production.
        let leaked: &'static mut ChannelPair = alloc::boxed::Box::leak(alloc::boxed::Box::new(self));
        (
            ChannelEndpointA { pair: leaked },
            ChannelEndpointB { pair: leaked },
        )
    }
}

pub struct ChannelEndpointA<'p> {
    pair: &'p ChannelPair,
}

pub struct ChannelEndpointB<'p> {
    pair: &'p ChannelPair,
}

impl ChannelEndpointA<'_> {
    pub fn send(&self, msg: ChannelMessage) -> Result<(), ChannelError> {
        self.pair.a_to_b.send(msg)
    }
    pub fn recv(&self) -> Option<ChannelMessage> {
        self.pair.b_to_a.recv()
    }
    pub fn is_inbox_empty(&self) -> bool { self.pair.b_to_a.is_empty() }
}

impl ChannelEndpointB<'_> {
    pub fn send(&self, msg: ChannelMessage) -> Result<(), ChannelError> {
        self.pair.b_to_a.send(msg)
    }
    pub fn recv(&self) -> Option<ChannelMessage> {
        self.pair.a_to_b.recv()
    }
    pub fn is_inbox_empty(&self) -> bool { self.pair.a_to_b.is_empty() }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelError {
    Full,
    Empty,
    KernelRefused,
    CapabilityRevoked,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spsc_ring_send_recv_roundtrip() {
        let ring = SpscRing::new();
        let msg  = ChannelMessage::with_tag(42);
        ring.send(msg).unwrap();
        let got = ring.recv().unwrap();
        assert_eq!(got.tag, 42);
    }

    #[test]
    fn ring_full_returns_error() {
        let ring = SpscRing::new();
        for i in 0..CHANNEL_DEPTH {
            ring.send(ChannelMessage::with_tag(i as u32)).unwrap();
        }
        assert_eq!(ring.send(ChannelMessage::with_tag(999)), Err(ChannelError::Full));
    }

    #[test]
    fn ring_empty_returns_none() {
        let ring = SpscRing::new();
        assert!(ring.recv().is_none());
    }
}
