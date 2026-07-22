use core::sync::atomic::{
    AtomicU64, Ordering,
};

use crate::lockfree::{
    BoundedMpmc, QueueError, QueueInitError,
};
use crate::nexus_wire::{
    NEXUS_WIRE_MAGIC, NEXUS_WIRE_VERSION,
    NexusCommand, NexusReply, NexusTelemetry,
};

pub const RESONANCE_QUEUE_DEPTH: usize = 8;

const INITIALIZING_SIGNATURE: u64 = 1;

const RESONANCE_SIGNATURE: u64 =
    NEXUS_WIRE_MAGIC as u64
        | ((NEXUS_WIRE_VERSION as u64) << 32)
        | (0x5250_u64 << 48);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaneInitError {
    AlreadyInitialized,
    Queue(QueueInitError),
}

#[repr(C, align(64))]
struct AtomicTelemetry {
    guard: AtomicU64,
    frame_sequence: AtomicU64,
    logical_tick: AtomicU64,
    global_phase: AtomicU64,
    pair_generation: AtomicU64,
    heat: AtomicU64,
    collapses: AtomicU64,
    reserved: AtomicU64,
}

impl AtomicTelemetry {
    const fn new() -> Self {
        Self {
            guard: AtomicU64::new(0),
            frame_sequence: AtomicU64::new(0),
            logical_tick: AtomicU64::new(0),
            global_phase: AtomicU64::new(0),
            pair_generation: AtomicU64::new(0),
            heat: AtomicU64::new(0),
            collapses: AtomicU64::new(0),
            reserved: AtomicU64::new(0),
        }
    }

    fn publish(&self, telemetry: &NexusTelemetry) {
        // Single kernel writer.
        let odd = self
            .guard
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);

        debug_assert!(odd & 1 == 1);

        self.frame_sequence
            .store(telemetry.sequence, Ordering::Relaxed);

        self.logical_tick
            .store(telemetry.logical_tick, Ordering::Relaxed);

        self.global_phase
            .store(telemetry.global_phase, Ordering::Relaxed);

        self.pair_generation.store(
            u64::from(telemetry.pairs_live)
                | (u64::from(telemetry.generation) << 32),
            Ordering::Relaxed,
        );

        self.heat.store(telemetry.heat, Ordering::Relaxed);

        self.collapses
            .store(telemetry.collapses, Ordering::Relaxed);

        self.guard.store(odd.wrapping_add(1), Ordering::Release);
    }

    fn snapshot(&self, maximum_attempts: usize) -> Option<NexusTelemetry> {
        for _ in 0..maximum_attempts.max(1) {
            let before = self.guard.load(Ordering::Acquire);

            if before & 1 != 0 {
                core::hint::spin_loop();
                continue;
            }

            let sequence =
                self.frame_sequence.load(Ordering::Relaxed);
            let logical_tick =
                self.logical_tick.load(Ordering::Relaxed);
            let global_phase =
                self.global_phase.load(Ordering::Relaxed);
            let pair_generation =
                self.pair_generation.load(Ordering::Relaxed);
            let heat = self.heat.load(Ordering::Relaxed);
            let collapses =
                self.collapses.load(Ordering::Relaxed);

            let after = self.guard.load(Ordering::Acquire);

            if before == after {
                return Some(NexusTelemetry::new(
                    sequence,
                    logical_tick,
                    global_phase,
                    pair_generation as u32,
                    (pair_generation >> 32) as u32,
                    heat,
                    collapses,
                ));
            }

            core::hint::spin_loop();
        }

        None
    }
}

#[repr(C, align(4096))]
pub struct ResonancePlane {
    signature: AtomicU64,
    kernel_epoch: AtomicU64,
    doorbell: AtomicU64,
    command_drops: AtomicU64,
    reply_drops: AtomicU64,
    reserved_header: [AtomicU64; 3],

    telemetry: AtomicTelemetry,

    commands:
        BoundedMpmc<NexusCommand, RESONANCE_QUEUE_DEPTH>,

    replies:
        BoundedMpmc<NexusReply, RESONANCE_QUEUE_DEPTH>,
}

const _: () =
    assert!(core::mem::size_of::<ResonancePlane>() == 4096);

impl ResonancePlane {
    pub const fn new() -> Self {
        Self {
            signature: AtomicU64::new(0),
            kernel_epoch: AtomicU64::new(0),
            doorbell: AtomicU64::new(0),
            command_drops: AtomicU64::new(0),
            reply_drops: AtomicU64::new(0),
            reserved_header: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
            telemetry: AtomicTelemetry::new(),
            commands: BoundedMpmc::new(),
            replies: BoundedMpmc::new(),
        }
    }

    pub fn initialize(&self) -> Result<(), PlaneInitError> {
        self.signature
            .compare_exchange(
                0,
                INITIALIZING_SIGNATURE,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| PlaneInitError::AlreadyInitialized)?;

        if let Err(error) = self.commands.initialize() {
            self.signature.store(0, Ordering::Release);
            return Err(PlaneInitError::Queue(error));
        }

        if let Err(error) = self.replies.initialize() {
            self.signature.store(0, Ordering::Release);
            return Err(PlaneInitError::Queue(error));
        }

        self.kernel_epoch.store(1, Ordering::Relaxed);
        self.signature
            .store(RESONANCE_SIGNATURE, Ordering::Release);

        Ok(())
    }

    #[inline(always)]
    pub fn is_compatible(&self) -> bool {
        self.signature.load(Ordering::Acquire)
            == RESONANCE_SIGNATURE
    }

    pub fn publish_telemetry(
        &self,
        telemetry: &NexusTelemetry,
    ) {
        self.telemetry.publish(telemetry);
        self.kernel_epoch.fetch_add(1, Ordering::Release);
    }

    pub fn telemetry(
        &self,
        maximum_attempts: usize,
    ) -> Option<NexusTelemetry> {
        if !self.is_compatible() {
            return None;
        }

        self.telemetry.snapshot(maximum_attempts)
    }

    pub fn submit_command(
        &self,
        command: NexusCommand,
    ) -> Result<(), QueueError> {
        match self.commands.push(command) {
            Ok(()) => {
                self.doorbell.fetch_add(1, Ordering::Release);
                Ok(())
            }

            Err(error) => {
                if error == QueueError::Full {
                    self.command_drops
                        .fetch_add(1, Ordering::Relaxed);
                }

                Err(error)
            }
        }
    }

    pub fn take_command(&self) -> Result<NexusCommand, QueueError> {
        self.commands.pop()
    }

    pub fn publish_reply(
        &self,
        reply: NexusReply,
    ) -> Result<(), QueueError> {
        match self.replies.push(reply) {
            Ok(()) => Ok(()),

            Err(error) => {
                if error == QueueError::Full {
                    self.reply_drops
                        .fetch_add(1, Ordering::Relaxed);
                }

                Err(error)
            }
        }
    }

    pub fn take_reply(&self) -> Result<NexusReply, QueueError> {
        self.replies.pop()
    }

    pub fn command_depth_approximate(&self) -> usize {
        self.commands.length_approximate()
    }

    pub fn reply_depth_approximate(&self) -> usize {
        self.replies.length_approximate()
    }

    pub fn epoch(&self) -> u64 {
        self.kernel_epoch.load(Ordering::Acquire)
    }

    pub fn doorbell(&self) -> u64 {
        self.doorbell.load(Ordering::Acquire)
    }

    pub fn dropped_commands(&self) -> u64 {
        self.command_drops.load(Ordering::Acquire)
    }

    pub fn dropped_replies(&self) -> u64 {
        self.reply_drops.load(Ordering::Acquire)
    }
}

impl Default for ResonancePlane {
    fn default() -> Self {
        Self::new()
    }
}
