use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::nexus_wire::{
    NEXUS_WIRE_MAGIC, NEXUS_WIRE_VERSION, NexusCommand, NexusReply, NexusTelemetry,
};

pub const RESONANCE_PAGE_BYTES: usize = 4096;

const INGRESS_SIGNATURE: u64 =
    NEXUS_WIRE_MAGIC as u64 | ((NEXUS_WIRE_VERSION as u64) << 32) | (0x494e_u64 << 48);

const OBSERVATION_SIGNATURE: u64 =
    NEXUS_WIRE_MAGIC as u64 | ((NEXUS_WIRE_VERSION as u64) << 32) | (0x4f42_u64 << 48);

mod sealed {
    pub trait Sealed {}

    impl Sealed for crate::nexus_wire::NexusCommand {}
    impl Sealed for crate::nexus_wire::NexusReply {}
    impl Sealed for crate::nexus_wire::NexusTelemetry {}
}

trait Wire64: sealed::Sealed + Copy {}

impl Wire64 for NexusCommand {}
impl Wire64 for NexusReply {}
impl Wire64 for NexusTelemetry {}

#[repr(C, align(64))]
struct AtomicWire64 {
    guard: AtomicU64,
    words: [AtomicU64; 8],
}

const _: () = assert!(core::mem::size_of::<AtomicWire64>() == 128);

impl AtomicWire64 {
    const fn new() -> Self {
        Self {
            guard: AtomicU64::new(0),
            words: [const { AtomicU64::new(0) }; 8],
        }
    }

    /// Single-writer publication.
    fn publish<T: Wire64>(&self, value: &T) {
        let words = encode_wire(value);

        let odd = self.guard.fetch_add(1, Ordering::AcqRel).wrapping_add(1);

        debug_assert!(odd & 1 == 1);

        for (target, source) in self.words.iter().zip(words) {
            target.store(source, Ordering::Relaxed);
        }

        self.guard.store(odd.wrapping_add(1), Ordering::Release);
    }

    fn snapshot<T: Wire64>(&self, maximum_attempts: usize) -> Option<T> {
        for _ in 0..maximum_attempts.max(1) {
            let before = self.guard.load(Ordering::Acquire);

            if before & 1 != 0 {
                core::hint::spin_loop();
                continue;
            }

            let mut words = [0_u64; 8];

            for (target, source) in words.iter_mut().zip(self.words.iter()) {
                *target = source.load(Ordering::Relaxed);
            }

            let after = self.guard.load(Ordering::Acquire);

            if before == after {
                return Some(decode_wire(words));
            }

            core::hint::spin_loop();
        }

        None
    }
}

#[repr(C, align(64))]
struct IngressCore {
    signature: AtomicU64,
    generation: AtomicU64,
    published_sequence: AtomicU64,
    doorbell: AtomicU64,
    malformed_frames: AtomicU64,
    overwritten_frames: AtomicU64,
    reserved: [AtomicU64; 2],

    command: AtomicWire64,
}

const _: () = assert!(core::mem::size_of::<IngressCore>() == 192);

#[repr(C, align(4096))]
pub struct ResonanceIngressPage {
    core: IngressCore,
    padding: [u8; RESONANCE_PAGE_BYTES - core::mem::size_of::<IngressCore>()],
}

const _: () = assert!(core::mem::size_of::<ResonanceIngressPage>() == 4096);

impl ResonanceIngressPage {
    pub const fn new() -> Self {
        Self {
            core: IngressCore {
                signature: AtomicU64::new(0),
                generation: AtomicU64::new(0),
                published_sequence: AtomicU64::new(0),
                doorbell: AtomicU64::new(0),
                malformed_frames: AtomicU64::new(0),
                overwritten_frames: AtomicU64::new(0),
                reserved: [AtomicU64::new(0), AtomicU64::new(0)],
                command: AtomicWire64::new(),
            },
            padding: [0; RESONANCE_PAGE_BYTES - core::mem::size_of::<IngressCore>()],
        }
    }

    pub fn initialize(&self, generation: u64) {
        self.core.generation.store(generation, Ordering::Relaxed);
        self.core
            .signature
            .store(INGRESS_SIGNATURE, Ordering::Release);
    }

    pub fn compatible(&self) -> bool {
        self.core.signature.load(Ordering::Acquire) == INGRESS_SIGNATURE
    }

    /// Userland producer.
    pub fn submit(&self, command: &NexusCommand) {
        let previous = self.core.published_sequence.load(Ordering::Acquire);

        if previous != 0 && previous != command.sequence {
            self.core.overwritten_frames.fetch_add(1, Ordering::Relaxed);
        }

        self.core.command.publish(command);

        self.core
            .published_sequence
            .store(command.sequence, Ordering::Release);

        self.core.doorbell.fetch_add(1, Ordering::Release);
    }

    /// Kernel consumer.
    ///
    /// `private_cursor` must remain kernel-private. Never store it in this
    /// user-writable page.
    pub fn take_new(&self, private_cursor: &mut u64) -> Option<NexusCommand> {
        let published = self.core.published_sequence.load(Ordering::Acquire);

        if published == 0 || published == *private_cursor {
            return None;
        }

        let Some(command) = self.core.command.snapshot::<NexusCommand>(8) else {
            self.core.malformed_frames.fetch_add(1, Ordering::Relaxed);
            return None;
        };

        if command.sequence != published {
            self.core.malformed_frames.fetch_add(1, Ordering::Relaxed);
            return None;
        }

        *private_cursor = published;
        Some(command)
    }

    pub fn doorbell(&self) -> u64 {
        self.core.doorbell.load(Ordering::Acquire)
    }
}

#[repr(C, align(64))]
struct ObservationCore {
    signature: AtomicU64,
    generation: AtomicU64,
    epoch: AtomicU64,
    last_reply_sequence: AtomicU64,
    telemetry_publications: AtomicU64,
    reply_publications: AtomicU64,
    reserved: [AtomicU64; 10],

    telemetry: AtomicWire64,
    reply: AtomicWire64,
}

const _: () = assert!(core::mem::size_of::<ObservationCore>() == 384);

#[repr(C, align(4096))]
pub struct ResonanceObservationPage {
    core: ObservationCore,
    padding: [u8; RESONANCE_PAGE_BYTES - core::mem::size_of::<ObservationCore>()],
}

const _: () = assert!(core::mem::size_of::<ResonanceObservationPage>() == 4096);

impl ResonanceObservationPage {
    pub const fn new() -> Self {
        Self {
            core: ObservationCore {
                signature: AtomicU64::new(0),
                generation: AtomicU64::new(0),
                epoch: AtomicU64::new(0),
                last_reply_sequence: AtomicU64::new(0),
                telemetry_publications: AtomicU64::new(0),
                reply_publications: AtomicU64::new(0),
                reserved: [const { AtomicU64::new(0) }; 10],
                telemetry: AtomicWire64::new(),
                reply: AtomicWire64::new(),
            },
            padding: [0; RESONANCE_PAGE_BYTES - core::mem::size_of::<ObservationCore>()],
        }
    }

    pub fn initialize(&self, generation: u64) {
        self.core.generation.store(generation, Ordering::Relaxed);
        self.core
            .signature
            .store(OBSERVATION_SIGNATURE, Ordering::Release);
    }

    pub fn compatible(&self) -> bool {
        self.core.signature.load(Ordering::Acquire) == OBSERVATION_SIGNATURE
    }

    /// Kernel writer.
    pub fn publish_telemetry(&self, telemetry: &NexusTelemetry) {
        self.core.telemetry.publish(telemetry);
        self.core
            .telemetry_publications
            .fetch_add(1, Ordering::Relaxed);
        self.core.epoch.fetch_add(1, Ordering::Release);
    }

    /// Kernel writer.
    pub fn publish_reply(&self, reply: &NexusReply) {
        self.core.reply.publish(reply);

        self.core
            .last_reply_sequence
            .store(reply.sequence, Ordering::Release);

        self.core.reply_publications.fetch_add(1, Ordering::Relaxed);
    }

    pub fn publish_state_root(&self, root: u64) {
        self.core.reserved[0].store(root, Ordering::Release);
    }

    pub fn state_root(&self) -> u64 {
        self.core.reserved[0].load(Ordering::Acquire)
    }

    pub fn publish_checkpoint_generation(&self, generation: u64) {
        self.core.reserved[1].store(generation, Ordering::Release);
    }

    pub fn checkpoint_generation(&self) -> u64 {
        self.core.reserved[1].load(Ordering::Acquire)
    }

    pub fn publish_witness_root(&self, root: u64) {
        self.core.reserved[2].store(root, Ordering::Release);
    }

    pub fn witness_root(&self) -> u64 {
        self.core.reserved[2].load(Ordering::Acquire)
    }

    /// Userland reader.
    pub fn telemetry(&self) -> Option<NexusTelemetry> {
        if !self.compatible() {
            return None;
        }

        self.core.telemetry.snapshot(8)
    }

    /// Userland reader.
    pub fn reply(&self, expected_sequence: u64) -> Option<NexusReply> {
        let published = self.core.last_reply_sequence.load(Ordering::Acquire);

        if published != expected_sequence {
            return None;
        }

        let reply = self.core.reply.snapshot::<NexusReply>(8)?;

        (reply.sequence == expected_sequence).then_some(reply)
    }

    pub fn epoch(&self) -> u64 {
        self.core.epoch.load(Ordering::Acquire)
    }
}

fn encode_wire<T: Wire64>(value: &T) -> [u64; 8] {
    debug_assert_eq!(core::mem::size_of::<T>(), 64);

    let mut words = [0_u64; 8];

    // SAFETY: All sealed wire types are exactly 64 bytes, contain only scalar
    // integer fields, and the destination covers the full object.
    unsafe {
        core::ptr::copy_nonoverlapping(
            (value as *const T).cast::<u8>(),
            words.as_mut_ptr().cast::<u8>(),
            64,
        );
    }

    words
}

fn decode_wire<T: Wire64>(words: [u64; 8]) -> T {
    debug_assert_eq!(core::mem::size_of::<T>(), 64);

    let mut output = MaybeUninit::<T>::uninit();

    // SAFETY: The sealed wire types permit every scalar bit pattern. Protocol
    // validity is checked separately by validate().
    unsafe {
        core::ptr::copy_nonoverlapping(
            words.as_ptr().cast::<u8>(),
            output.as_mut_ptr().cast::<u8>(),
            64,
        );

        output.assume_init()
    }
}
