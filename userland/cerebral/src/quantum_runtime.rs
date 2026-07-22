use core::cell::UnsafeCell;
use core::future::Future;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::{Context, Poll};

use slope::capability::{
    CapHandle, CapabilityEnvelope, CapabilityError, FabricRight,
    LearningRight, ResonanceRight, SchedulerRight,
};
use slope::env::EnvSnapshot;
use slope::executor::Spawner;
use slope::scheduler::{self, PhaseHint, Priority};

static INSTALLED: AtomicBool = AtomicBool::new(false);

struct TaskCell<T>(UnsafeCell<MaybeUninit<T>>);

// A TaskCell is initialized exactly once before the executor can poll it.
unsafe impl<T> Sync for TaskCell<T> {}

impl<T> TaskCell<T> {
    const fn uninitialized() -> Self {
        Self(UnsafeCell::new(MaybeUninit::uninit()))
    }

    unsafe fn initialize(&self, value: T) -> *mut T {
        // SAFETY: install() serializes initialization with INSTALLED.
        let slot = unsafe { &mut *self.0.get() };
        slot.write(value) as *mut T
    }
}

#[derive(Clone, Copy)]
pub struct CerebralCapabilities {
    pub fabric: CapHandle<FabricRight>,
    pub resonance: CapHandle<ResonanceRight>,
    pub scheduler: CapHandle<SchedulerRight>,
    pub learning: CapHandle<LearningRight>,
}

impl CerebralCapabilities {
    pub fn receive(environment: &EnvSnapshot) -> Result<Self, CapabilityError> {
        let envelope = CapabilityEnvelope::new(environment);

        Ok(Self {
            fabric: envelope.recv()?,
            resonance: envelope.recv()?,
            scheduler: envelope.recv()?,
            learning: envelope.recv()?,
        })
    }

    const fn phase_seed(self) -> u16 {
        let mixed = self.fabric.as_raw()
            ^ self.resonance.as_raw().rotate_left(13)
            ^ self.scheduler.as_raw().rotate_left(29)
            ^ self.learning.as_raw().rotate_left(47);

        mixed as u16
    }
}

struct NexusTask {
    phase: u16,
    coherence: u16,
}

impl NexusTask {
    const fn new(capabilities: CerebralCapabilities) -> Self {
        Self {
            phase: capabilities.phase_seed() & 0x03ff,
            coherence: 768,
        }
    }
}

impl Future for NexusTask {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        let _ = scheduler::yield_with_hint(PhaseHint {
            phase_bin: self.phase,
            coherence: self.coherence,
            priority: Priority::Nexus,
            flags: 1,
        });

        self.phase = self.phase.wrapping_add(37) & 0x03ff;
        self.coherence = self.coherence.saturating_sub(1).max(512);

        context.waker().wake_by_ref();
        Poll::Pending
    }
}

static NEXUS_TASK: TaskCell<NexusTask> = TaskCell::uninitialized();

#[derive(Debug, Eq, PartialEq)]
pub enum InstallError<SpawnError> {
    AlreadyInstalled,
    Spawn(SpawnError),
}

pub fn install<S: Spawner>(
    spawner: &mut S,
    capabilities: CerebralCapabilities,
) -> Result<(), InstallError<S::Error>> {
    if INSTALLED.swap(true, Ordering::AcqRel) {
        return Err(InstallError::AlreadyInstalled);
    }

    // SAFETY: INSTALLED grants this path exclusive one-time initialization.
    let task = unsafe { NEXUS_TASK.initialize(NexusTask::new(capabilities)) };

    // SAFETY: NEXUS_TASK has static storage and is never moved after this point.
    if let Err(error) = unsafe { spawner.spawn(task) } {
        INSTALLED.store(false, Ordering::Release);
        return Err(InstallError::Spawn(error));
    }

    Ok(())
}
