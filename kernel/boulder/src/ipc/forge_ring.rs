use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicUsize, Ordering};

pub const RING_SIZE: usize = 1024;
pub const MAXIMUM_PAYLOAD_LENGTH: u32 = 1024 * 1024;

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IpcCommand {
    pub opcode: u32,
    pub target_pid: u32,
    pub payload_handle: u64,
    pub payload_length: u32,
    pub flags: u32,
}

impl IpcCommand {
    fn is_valid(self) -> bool {
        self.opcode != 0
            && self.payload_length <= MAXIMUM_PAYLOAD_LENGTH
            && (self.payload_length == 0 || self.payload_handle != 0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitError {
    InvalidCommand,
    Full,
}

/// Single-producer, single-consumer command ring shared across privilege levels.
///
/// Exactly one Ring 3 producer may call `submit`, and exactly one Ring 0
/// consumer may call `consume`. Payload handles must be resolved and validated
/// by the consumer against the sending process before use.
#[repr(C, align(4096))]
pub struct ForgeSubmissionRing {
    head: AtomicUsize,
    tail: AtomicUsize,
    buffer: [UnsafeCell<MaybeUninit<IpcCommand>>; RING_SIZE],
}

impl ForgeSubmissionRing {
    pub const fn new() -> Self {
        Self {
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            buffer: [const { UnsafeCell::new(MaybeUninit::uninit()) }; RING_SIZE],
        }
    }

    pub fn submit(&self, command: IpcCommand) -> Result<(), SubmitError> {
        if !command.is_valid() {
            return Err(SubmitError::InvalidCommand);
        }
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail.wrapping_sub(head) >= RING_SIZE {
            return Err(SubmitError::Full);
        }
        let slot = tail % RING_SIZE;
        // SAFETY: The SPSC contract grants the producer exclusive access to
        // the slot at tail until the release store publishes it.
        unsafe { (*self.buffer[slot].get()).write(command) };
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    pub fn consume(&self) -> Option<IpcCommand> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head == tail {
            return None;
        }
        let slot = head % RING_SIZE;
        // SAFETY: The acquire load observed publication of this initialized
        // slot, and the SPSC contract grants the consumer exclusive access.
        let command = unsafe { (*self.buffer[slot].get()).assume_init_read() };
        self.head.store(head.wrapping_add(1), Ordering::Release);
        Some(command)
    }

    pub fn pending(&self) -> usize {
        self.tail
            .load(Ordering::Acquire)
            .wrapping_sub(self.head.load(Ordering::Acquire))
            .min(RING_SIZE)
    }
}

impl Default for ForgeSubmissionRing {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: Slot access is synchronized by the SPSC ownership contract and the
// release/acquire publication of tail and head.
unsafe impl Sync for ForgeSubmissionRing {}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMAND: IpcCommand = IpcCommand {
        opcode: 1,
        target_pid: 7,
        payload_handle: 9,
        payload_length: 16,
        flags: 0,
    };

    #[test]
    fn publishes_and_consumes_commands_in_order() {
        let ring = ForgeSubmissionRing::new();
        ring.submit(COMMAND).unwrap();
        ring.submit(IpcCommand {
            opcode: 2,
            ..COMMAND
        })
        .unwrap();

        assert_eq!(ring.pending(), 2);
        assert_eq!(ring.consume(), Some(COMMAND));
        assert_eq!(ring.consume().map(|command| command.opcode), Some(2));
        assert_eq!(ring.consume(), None);
    }

    #[test]
    fn rejects_invalid_commands_and_capacity_overflow() {
        let ring = ForgeSubmissionRing::new();
        assert_eq!(
            ring.submit(IpcCommand {
                opcode: 0,
                ..COMMAND
            }),
            Err(SubmitError::InvalidCommand)
        );
        for _ in 0..RING_SIZE {
            ring.submit(COMMAND).unwrap();
        }
        assert_eq!(ring.submit(COMMAND), Err(SubmitError::Full));
    }
}
