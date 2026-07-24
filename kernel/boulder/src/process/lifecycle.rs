//! Fixed-capacity process lifecycle state.
//!
//! This is a truthful lifecycle registry: it only admits processes after an
//! architecture backend has supplied a complete launch image. PID allocation
//! alone never creates a runnable process.

use core::sync::atomic::{AtomicU64, Ordering};

use super::context::{
    AuthorizedUserReturn, DispatchContext, SavedUserContext, valid_kernel_stack_pointer,
    valid_page_table_root, valid_user_address,
};
use crate::sync::SpinLock;

pub const MAXIMUM_PROCESSES: usize = 64;
pub const NO_PID: u32 = 0;
pub const INIT_PID: u32 = 1;
const PID0_GENERATION: u32 = 1;
const PID0_IDENTITY: u64 = (PID0_GENERATION as u64) << 32;
pub const ERROR_AGAIN: isize = -11;
pub const ERROR_NO_CHILD: isize = -10;
pub const ERROR_INVALID: isize = -22;
pub const ERROR_CAPACITY: isize = -28;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ProcessPhase {
    Free = 0,
    Installing = 1,
    Runnable = 2,
    Running = 3,
    Blocked = 4,
    Zombie = 5,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessLaunch {
    pub address_space_root: u64,
    pub entry_point: u64,
    pub user_stack_pointer: u64,
    pub kernel_stack_pointer: u64,
    pub image_measurement_root: u64,
    pub capability_root: u64,
    pub service_class: u16,
    pub priority: u8,
}

impl ProcessLaunch {
    pub fn validate(self) -> bool {
        valid_page_table_root(self.address_space_root)
            && valid_user_address(self.entry_point)
            && valid_user_address(self.user_stack_pointer)
            && valid_kernel_stack_pointer(self.kernel_stack_pointer)
            && self.image_measurement_root != 0
            && self.capability_root != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessHandle {
    pub pid: u32,
    pub generation: u32,
}

/// Lock-free snapshot used to bind an IRQ request to one exact user
/// execution and scheduler epoch. A request carrying an older PID generation
/// or epoch can never schedule a recycled or superseded process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecutionAuthority {
    pub handle: ProcessHandle,
    pub scheduler_epoch: u64,
}

impl ExecutionAuthority {
    pub(crate) const fn encode_identity(self) -> u64 {
        self.handle.encode()
    }

    pub(crate) const fn decode(identity: u64, scheduler_epoch: u64) -> Option<Self> {
        if scheduler_epoch == 0 {
            return None;
        }
        let Some(handle) = ProcessHandle::decode(identity) else {
            return None;
        };
        Some(Self {
            handle,
            scheduler_epoch,
        })
    }
}

impl ProcessHandle {
    const fn encode(self) -> u64 {
        (self.generation as u64) << 32 | self.pid as u64
    }

    const fn decode(encoded: u64) -> Option<Self> {
        if encoded == 0 {
            return None;
        }
        let handle = Self {
            pid: encoded as u32,
            generation: (encoded >> 32) as u32,
        };
        if handle.pid == NO_PID || handle.generation == 0 {
            None
        } else {
            Some(handle)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessSnapshot {
    pub handle: ProcessHandle,
    pub parent: u32,
    pub phase: ProcessPhase,
    pub launch: ProcessLaunch,
    pub exit_code: isize,
    pub wait_sequence: u64,
}

/// A lifecycle-selected process and the complete machine state needed by the
/// architecture dispatch path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScheduledProcess {
    pub handle: ProcessHandle,
    pub context: DispatchContext,
    pub scheduler_epoch: u64,
}

impl ScheduledProcess {
    pub const fn authorized_return(self) -> AuthorizedUserReturn {
        AuthorizedUserReturn {
            dispatch: self.context,
            pid: self.handle.pid,
            generation: self.handle.generation,
            scheduler_epoch: self.scheduler_epoch,
        }
    }
}

/// Epoch-bound authority for the kernel scheduler/idle context.
///
/// PID0 is not a process slot and has no user register image, address space,
/// parent, or exit state. Its permanent generation distinguishes the explicit
/// kernel execution identity from an uninitialized atomic value, while the
/// epoch invalidates every superseded idle decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Pid0Authority {
    generation: u32,
    scheduler_epoch: u64,
}

impl Pid0Authority {
    pub const fn generation(self) -> u32 {
        self.generation
    }

    pub const fn scheduler_epoch(self) -> u64 {
        self.scheduler_epoch
    }
}

/// Complete result of a lifecycle scheduling decision. PID0 deliberately has
/// no conversion to [`AuthorizedUserReturn`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScheduleDecision {
    User(ScheduledProcess),
    Pid0(Pid0Authority),
}

#[derive(Clone, Copy)]
struct ProcessSlot {
    occupied: bool,
    generation: u32,
    pid: u32,
    parent: u32,
    phase: ProcessPhase,
    launch: ProcessLaunch,
    context: SavedUserContext,
    exit_code: isize,
    wait_sequence: u64,
}

impl ProcessSlot {
    const EMPTY: Self = Self {
        occupied: false,
        generation: 1,
        pid: 0,
        parent: 0,
        phase: ProcessPhase::Free,
        launch: ProcessLaunch {
            address_space_root: 0,
            entry_point: 0,
            user_stack_pointer: 0,
            kernel_stack_pointer: 0,
            image_measurement_root: 0,
            capability_root: 0,
            service_class: 0,
            priority: 0,
        },
        context: SavedUserContext::EMPTY,
        exit_code: 0,
        wait_sequence: 0,
    };

    fn snapshot(self) -> ProcessSnapshot {
        ProcessSnapshot {
            handle: ProcessHandle {
                pid: self.pid,
                generation: self.generation,
            },
            parent: self.parent,
            phase: self.phase,
            launch: self.launch,
            exit_code: self.exit_code,
            wait_sequence: self.wait_sequence,
        }
    }
}

struct ProcessTable {
    slots: [ProcessSlot; MAXIMUM_PROCESSES],
    next_pid: u32,
    scheduler_epoch: u64,
}

impl ProcessTable {
    const fn new() -> Self {
        Self {
            slots: [ProcessSlot::EMPTY; MAXIMUM_PROCESSES],
            next_pid: 2,
            scheduler_epoch: 0,
        }
    }

    fn allocate_pid(&mut self) -> Option<u32> {
        for _ in 0..=MAXIMUM_PROCESSES {
            let candidate = self.next_pid.max(2);
            self.next_pid = candidate.wrapping_add(1).max(2);

            if !self
                .slots
                .iter()
                .any(|slot| slot.occupied && slot.pid == candidate)
            {
                return Some(candidate);
            }
        }
        None
    }

    fn slot_by_handle_mut(&mut self, handle: ProcessHandle) -> Option<&mut ProcessSlot> {
        self.slots.iter_mut().find(|slot| {
            slot.occupied && slot.pid == handle.pid && slot.generation == handle.generation
        })
    }

    fn slot_by_handle(&self, handle: ProcessHandle) -> Option<&ProcessSlot> {
        self.slots.iter().find(|slot| {
            slot.occupied && slot.pid == handle.pid && slot.generation == handle.generation
        })
    }

    fn slot_by_pid(&self, pid: u32) -> Option<&ProcessSlot> {
        self.slots
            .iter()
            .find(|slot| slot.occupied && slot.pid == pid)
    }

    fn next_runnable_index(&self, after: usize) -> Option<usize> {
        (1..=self.slots.len())
            .map(|offset| (after + offset) % self.slots.len())
            .find(|index| {
                let slot = self.slots[*index];
                slot.occupied && slot.phase == ProcessPhase::Runnable
            })
    }

    fn scheduled(
        &self,
        index: usize,
        user: SavedUserContext,
        scheduler_epoch: u64,
    ) -> ScheduledProcess {
        let slot = self.slots[index];
        ScheduledProcess {
            handle: ProcessHandle {
                pid: slot.pid,
                generation: slot.generation,
            },
            context: DispatchContext {
                user,
                address_space_root: slot.launch.address_space_root,
                kernel_stack_pointer: slot.launch.kernel_stack_pointer,
            },
            scheduler_epoch,
        }
    }
}

const fn next_scheduler_epoch(epoch: u64) -> Result<u64, LifecycleError> {
    match epoch.checked_add(1) {
        Some(next) => Ok(next),
        None => Err(LifecycleError::EpochExhausted),
    }
}

static TABLE: SpinLock<ProcessTable> = SpinLock::new(ProcessTable::new());
// PID0 is a real kernel execution identity, not the absence of state. User
// identities use the same atomic pid:generation encoding, while epoch-bound
// dispatch authority remains serialized by `TABLE`.
static CURRENT_EXECUTION: AtomicU64 = AtomicU64::new(PID0_IDENTITY);
// IRQ code cannot take TABLE: a timer may interrupt code that already owns the
// lifecycle lock. This mirror is published after every successful epoch
// transition and is used only to mint fail-closed scheduling requests. The
// locked table remains the authority when a request is consumed.
static CURRENT_SCHEDULER_EPOCH: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleError {
    InvalidLaunch,
    InvalidContext,
    Capacity,
    InvalidHandle,
    InvalidTransition,
    NotChild,
    StillRunning,
    NoChild,
    EpochExhausted,
    StaleDispatchAuthority,
    StalePreemptionAuthority,
    StalePid0Authority,
    Pid0Immutable,
}

pub fn current_pid() -> u32 {
    current_handle().map_or(NO_PID, |handle| handle.pid)
}

pub fn current_handle() -> Option<ProcessHandle> {
    ProcessHandle::decode(CURRENT_EXECUTION.load(Ordering::Acquire))
}

/// Returns a bounded, allocation-free execution snapshot suitable for IRQ
/// context. Concurrent publication may make this return `None`; it can never
/// manufacture a valid mixed authority because the consumer revalidates both
/// fields under [`TABLE`].
pub fn current_execution_authority_from_irq() -> Option<ExecutionAuthority> {
    let epoch_before = CURRENT_SCHEDULER_EPOCH.load(Ordering::Acquire);
    let handle = ProcessHandle::decode(CURRENT_EXECUTION.load(Ordering::Acquire))?;
    let epoch_after = CURRENT_SCHEDULER_EPOCH.load(Ordering::Acquire);
    if epoch_before == 0 || epoch_before != epoch_after {
        return None;
    }
    Some(ExecutionAuthority {
        handle,
        scheduler_epoch: epoch_before,
    })
}

fn publish_scheduler_epoch(epoch: u64) {
    CURRENT_SCHEDULER_EPOCH.store(epoch, Ordering::Release);
}

fn current_user_handle() -> Result<ProcessHandle, LifecycleError> {
    let encoded = CURRENT_EXECUTION.load(Ordering::Acquire);
    if encoded == PID0_IDENTITY {
        return Err(LifecycleError::Pid0Immutable);
    }
    ProcessHandle::decode(encoded).ok_or(LifecycleError::InvalidHandle)
}

fn pid0_authority(scheduler_epoch: u64) -> Pid0Authority {
    Pid0Authority {
        generation: PID0_GENERATION,
        scheduler_epoch,
    }
}

fn validate_pid0_authority(
    table: &ProcessTable,
    authority: Pid0Authority,
) -> Result<(), LifecycleError> {
    if CURRENT_EXECUTION.load(Ordering::Acquire) != PID0_IDENTITY
        || authority.generation != PID0_GENERATION
        || authority.scheduler_epoch != table.scheduler_epoch
    {
        return Err(LifecycleError::StalePid0Authority);
    }
    Ok(())
}

pub fn current_pid0_authority() -> Result<Pid0Authority, LifecycleError> {
    let table = TABLE.lock();
    let authority = pid0_authority(table.scheduler_epoch);
    validate_pid0_authority(&table, authority)?;
    Ok(authority)
}

pub fn authorize_pid0(authority: Pid0Authority) -> Result<(), LifecycleError> {
    let table = TABLE.lock();
    validate_pid0_authority(&table, authority)
}

pub fn publish_current(handle: ProcessHandle) -> Result<(), LifecycleError> {
    if handle.pid == NO_PID {
        return Err(LifecycleError::Pid0Immutable);
    }
    let table = TABLE.lock();
    let Some(slot) = table.slot_by_handle(handle) else {
        return Err(LifecycleError::InvalidHandle);
    };
    if slot.phase != ProcessPhase::Running {
        return Err(LifecycleError::InvalidTransition);
    }
    drop(table);
    CURRENT_EXECUTION.store(handle.encode(), Ordering::Release);
    Ok(())
}

pub fn register_init(launch: ProcessLaunch) -> Result<ProcessHandle, LifecycleError> {
    if !launch.validate() {
        return Err(LifecycleError::InvalidLaunch);
    }

    let mut table = TABLE.lock();
    if table.slot_by_pid(INIT_PID).is_some() {
        return Err(LifecycleError::InvalidTransition);
    }

    let slot = table
        .slots
        .iter_mut()
        .find(|slot| !slot.occupied && slot.generation != u32::MAX)
        .ok_or(LifecycleError::Capacity)?;

    slot.occupied = true;
    slot.pid = INIT_PID;
    slot.parent = NO_PID;
    slot.phase = ProcessPhase::Runnable;
    slot.launch = launch;
    slot.context = SavedUserContext::initial(launch.entry_point, launch.user_stack_pointer);
    slot.exit_code = 0;
    slot.wait_sequence = 0;

    Ok(ProcessHandle {
        pid: INIT_PID,
        generation: slot.generation,
    })
}

pub fn commit_child(parent: u32, launch: ProcessLaunch) -> Result<ProcessHandle, LifecycleError> {
    if parent == NO_PID || !launch.validate() {
        return Err(LifecycleError::InvalidLaunch);
    }

    let mut table = TABLE.lock();
    let Some(parent_slot) = table.slot_by_pid(parent) else {
        return Err(LifecycleError::InvalidHandle);
    };
    if matches!(parent_slot.phase, ProcessPhase::Free | ProcessPhase::Zombie) {
        return Err(LifecycleError::InvalidTransition);
    }

    let pid = table.allocate_pid().ok_or(LifecycleError::Capacity)?;
    let scheduler_epoch = table.scheduler_epoch;
    let generation = {
        let slot = table
            .slots
            .iter_mut()
            .find(|slot| !slot.occupied && slot.generation != u32::MAX)
            .ok_or(LifecycleError::Capacity)?;

        slot.occupied = true;
        slot.generation = slot
            .generation
            .checked_add(1)
            .ok_or(LifecycleError::Capacity)?;
        slot.pid = pid;
        slot.parent = parent;
        slot.phase = ProcessPhase::Runnable;
        slot.launch = launch;
        slot.context = SavedUserContext::initial(launch.entry_point, launch.user_stack_pointer);
        slot.exit_code = 0;
        slot.wait_sequence = scheduler_epoch;
        slot.generation
    };

    Ok(ProcessHandle { pid, generation })
}

pub fn mark_running(handle: ProcessHandle) -> Result<ProcessSnapshot, LifecycleError> {
    if handle.pid == NO_PID {
        return Err(LifecycleError::Pid0Immutable);
    }
    let mut table = TABLE.lock();
    if CURRENT_EXECUTION.load(Ordering::Acquire) != PID0_IDENTITY
        || table
            .slots
            .iter()
            .any(|slot| slot.occupied && slot.phase == ProcessPhase::Running)
    {
        return Err(LifecycleError::InvalidTransition);
    }
    let index = table
        .slots
        .iter()
        .position(|slot| {
            slot.occupied && slot.pid == handle.pid && slot.generation == handle.generation
        })
        .ok_or(LifecycleError::InvalidHandle)?;
    if table.slots[index].phase != ProcessPhase::Runnable {
        return Err(LifecycleError::InvalidTransition);
    }

    table.scheduler_epoch = next_scheduler_epoch(table.scheduler_epoch)?;
    table.slots[index].phase = ProcessPhase::Running;
    let snapshot = table.slots[index].snapshot();
    CURRENT_EXECUTION.store(snapshot.handle.encode(), Ordering::Release);
    publish_scheduler_epoch(table.scheduler_epoch);
    Ok(snapshot)
}

/// Retains the caller as the running process after a non-scheduling syscall.
///
/// Every user return receives a fresh epoch even when the selected process
/// does not change. Consequently an older return authority stops validating
/// as soon as the process crosses another syscall boundary.
pub fn resume_current(saved: SavedUserContext) -> Result<ScheduledProcess, LifecycleError> {
    saved
        .validate()
        .map_err(|_| LifecycleError::InvalidContext)?;

    let current = current_user_handle()?;
    let mut table = TABLE.lock();
    let current_index = table
        .slots
        .iter()
        .position(|slot| {
            slot.occupied && slot.pid == current.pid && slot.generation == current.generation
        })
        .ok_or(LifecycleError::InvalidHandle)?;
    if table.slots[current_index].phase != ProcessPhase::Running {
        return Err(LifecycleError::InvalidTransition);
    }

    let next_epoch = next_scheduler_epoch(table.scheduler_epoch)?;
    let scheduled = table.scheduled(current_index, saved, next_epoch);
    scheduled
        .context
        .validate()
        .map_err(|_| LifecycleError::InvalidContext)?;
    table.slots[current_index].context = saved;
    table.scheduler_epoch = next_epoch;
    publish_scheduler_epoch(next_epoch);
    Ok(scheduled)
}

/// Revalidates the exact lifecycle decision consumed by the architecture
/// return path. This comparison binds PID generation, epoch, saved registers,
/// CR3, and RSP0 to one currently running slot.
pub fn authorize_user_return(
    authority: AuthorizedUserReturn,
) -> Result<ScheduledProcess, LifecycleError> {
    authority
        .validate()
        .map_err(|_| LifecycleError::InvalidContext)?;
    let handle = ProcessHandle {
        pid: authority.pid,
        generation: authority.generation,
    };

    let table = TABLE.lock();
    if current_handle() != Some(handle) || table.scheduler_epoch != authority.scheduler_epoch {
        return Err(LifecycleError::StaleDispatchAuthority);
    }
    let index = table
        .slots
        .iter()
        .position(|slot| {
            slot.occupied && slot.pid == handle.pid && slot.generation == handle.generation
        })
        .ok_or(LifecycleError::StaleDispatchAuthority)?;
    if table.slots[index].phase != ProcessPhase::Running {
        return Err(LifecycleError::StaleDispatchAuthority);
    }

    let expected = table.scheduled(index, table.slots[index].context, table.scheduler_epoch);
    if expected.authorized_return() != authority {
        return Err(LifecycleError::StaleDispatchAuthority);
    }
    Ok(expected)
}

/// Saves a valid Ring 3 context, returns the next runnable process in
/// round-robin order, and marks that target running as one atomic table
/// transition. If no peer is runnable, the caller is selected again.
pub fn schedule_yield(mut saved: SavedUserContext) -> Result<ScheduledProcess, LifecycleError> {
    saved
        .validate()
        .map_err(|_| LifecycleError::InvalidContext)?;
    saved.set_syscall_result(0);

    let current = current_user_handle()?;
    let mut table = TABLE.lock();
    let current_index = table
        .slots
        .iter()
        .position(|slot| {
            slot.occupied && slot.pid == current.pid && slot.generation == current.generation
        })
        .ok_or(LifecycleError::InvalidHandle)?;
    if table.slots[current_index].phase != ProcessPhase::Running {
        return Err(LifecycleError::InvalidTransition);
    }

    let target_index = table
        .next_runnable_index(current_index)
        .unwrap_or(current_index);
    let next_epoch = next_scheduler_epoch(table.scheduler_epoch)?;
    let target_user = if target_index == current_index {
        saved
    } else {
        table.slots[target_index].context
    };
    let scheduled = table.scheduled(target_index, target_user, next_epoch);
    scheduled
        .context
        .validate()
        .map_err(|_| LifecycleError::InvalidContext)?;

    table.slots[current_index].context = saved;
    table.slots[current_index].phase = ProcessPhase::Runnable;
    table.scheduler_epoch = scheduled.scheduler_epoch;
    table.slots[target_index].phase = ProcessPhase::Running;
    CURRENT_EXECUTION.store(scheduled.handle.encode(), Ordering::Release);
    publish_scheduler_epoch(scheduled.scheduler_epoch);
    Ok(scheduled)
}

/// Consumes a timer-issued execution authority at a safe kernel boundary.
///
/// Unlike an explicit yield, involuntary preemption preserves RAX exactly.
/// The IRQ-side snapshot is only a request: PID generation and epoch are
/// checked again while the lifecycle table is locked before any state changes.
pub fn schedule_preempt(
    saved: SavedUserContext,
    authority: ExecutionAuthority,
) -> Result<ScheduledProcess, LifecycleError> {
    saved
        .validate()
        .map_err(|_| LifecycleError::InvalidContext)?;

    let mut table = TABLE.lock();
    if CURRENT_EXECUTION.load(Ordering::Acquire) != authority.handle.encode()
        || table.scheduler_epoch != authority.scheduler_epoch
    {
        return Err(LifecycleError::StalePreemptionAuthority);
    }
    let current_index = table
        .slots
        .iter()
        .position(|slot| {
            slot.occupied
                && slot.pid == authority.handle.pid
                && slot.generation == authority.handle.generation
        })
        .ok_or(LifecycleError::StalePreemptionAuthority)?;
    if table.slots[current_index].phase != ProcessPhase::Running {
        return Err(LifecycleError::StalePreemptionAuthority);
    }

    let target_index = table
        .next_runnable_index(current_index)
        .unwrap_or(current_index);
    let next_epoch = next_scheduler_epoch(table.scheduler_epoch)?;
    let target_user = if target_index == current_index {
        saved
    } else {
        table.slots[target_index].context
    };
    let scheduled = table.scheduled(target_index, target_user, next_epoch);
    scheduled
        .context
        .validate()
        .map_err(|_| LifecycleError::InvalidContext)?;

    table.slots[current_index].context = saved;
    table.slots[current_index].phase = ProcessPhase::Runnable;
    table.scheduler_epoch = next_epoch;
    table.slots[target_index].phase = ProcessPhase::Running;
    CURRENT_EXECUTION.store(scheduled.handle.encode(), Ordering::Release);
    publish_scheduler_epoch(next_epoch);
    Ok(scheduled)
}

/// Terminates the running process and atomically selects either another user
/// process or the explicit PID0 scheduler context.
pub fn schedule_exit(exit_code: isize) -> Result<ScheduleDecision, LifecycleError> {
    let current = current_user_handle()?;
    let mut table = TABLE.lock();
    let current_index = table
        .slots
        .iter()
        .position(|slot| {
            slot.occupied && slot.pid == current.pid && slot.generation == current.generation
        })
        .ok_or(LifecycleError::InvalidHandle)?;
    if table.slots[current_index].phase != ProcessPhase::Running {
        return Err(LifecycleError::InvalidTransition);
    }

    let next_epoch = next_scheduler_epoch(table.scheduler_epoch)?;
    let target = table
        .next_runnable_index(current_index)
        .map(|target_index| {
            (
                target_index,
                table.scheduled(target_index, table.slots[target_index].context, next_epoch),
            )
        });
    if target.is_some_and(|(_, scheduled)| scheduled.context.validate().is_err()) {
        return Err(LifecycleError::InvalidContext);
    }

    table.slots[current_index].phase = ProcessPhase::Zombie;
    table.slots[current_index].exit_code = exit_code;
    table.slots[current_index].wait_sequence = next_epoch;
    table.scheduler_epoch = next_epoch;

    if let Some((target_index, mut scheduled)) = target {
        scheduled.scheduler_epoch = next_epoch;
        table.slots[target_index].phase = ProcessPhase::Running;
        CURRENT_EXECUTION.store(scheduled.handle.encode(), Ordering::Release);
        publish_scheduler_epoch(next_epoch);
        Ok(ScheduleDecision::User(scheduled))
    } else {
        CURRENT_EXECUTION.store(PID0_IDENTITY, Ordering::Release);
        publish_scheduler_epoch(next_epoch);
        Ok(ScheduleDecision::Pid0(pid0_authority(next_epoch)))
    }
}

/// Re-runs selection from an exact PID0 authority. With no runnable process a
/// fresh PID0 epoch is returned; otherwise one validated saved context becomes
/// the sole running user identity. A stale idle decision changes no state.
pub fn schedule_from_pid0(authority: Pid0Authority) -> Result<ScheduleDecision, LifecycleError> {
    let mut table = TABLE.lock();
    validate_pid0_authority(&table, authority)?;
    if table
        .slots
        .iter()
        .any(|slot| slot.occupied && slot.phase == ProcessPhase::Running)
    {
        return Err(LifecycleError::InvalidTransition);
    }

    let next_epoch = next_scheduler_epoch(table.scheduler_epoch)?;
    let target = table
        .slots
        .iter()
        .position(|slot| slot.occupied && slot.phase == ProcessPhase::Runnable)
        .map(|target_index| {
            (
                target_index,
                table.scheduled(target_index, table.slots[target_index].context, next_epoch),
            )
        });
    if target.is_some_and(|(_, scheduled)| scheduled.context.validate().is_err()) {
        return Err(LifecycleError::InvalidContext);
    }

    table.scheduler_epoch = next_epoch;
    if let Some((target_index, scheduled)) = target {
        table.slots[target_index].phase = ProcessPhase::Running;
        CURRENT_EXECUTION.store(scheduled.handle.encode(), Ordering::Release);
        publish_scheduler_epoch(next_epoch);
        Ok(ScheduleDecision::User(scheduled))
    } else {
        CURRENT_EXECUTION.store(PID0_IDENTITY, Ordering::Release);
        publish_scheduler_epoch(next_epoch);
        Ok(ScheduleDecision::Pid0(pid0_authority(next_epoch)))
    }
}

pub fn mark_runnable(handle: ProcessHandle) -> Result<(), LifecycleError> {
    if handle.pid == NO_PID {
        return Err(LifecycleError::Pid0Immutable);
    }
    let mut table = TABLE.lock();
    let slot = table
        .slot_by_handle_mut(handle)
        .ok_or(LifecycleError::InvalidHandle)?;
    if !matches!(slot.phase, ProcessPhase::Running | ProcessPhase::Blocked) {
        return Err(LifecycleError::InvalidTransition);
    }
    slot.phase = ProcessPhase::Runnable;
    Ok(())
}

pub fn mark_blocked(handle: ProcessHandle) -> Result<(), LifecycleError> {
    if handle.pid == NO_PID {
        return Err(LifecycleError::Pid0Immutable);
    }
    let mut table = TABLE.lock();
    let slot = table
        .slot_by_handle_mut(handle)
        .ok_or(LifecycleError::InvalidHandle)?;
    if slot.phase != ProcessPhase::Running {
        return Err(LifecycleError::InvalidTransition);
    }
    slot.phase = ProcessPhase::Blocked;
    Ok(())
}

pub fn wait_child(
    parent: u32,
    requested_pid: Option<u32>,
) -> Result<ProcessSnapshot, LifecycleError> {
    let table = TABLE.lock();

    let mut saw_child = false;
    for slot in &table.slots {
        if !slot.occupied || slot.parent != parent {
            continue;
        }
        if requested_pid.is_some_and(|pid| slot.pid != pid) {
            continue;
        }
        saw_child = true;
        if slot.phase == ProcessPhase::Zombie {
            return Ok(slot.snapshot());
        }
    }

    if saw_child {
        Err(LifecycleError::StillRunning)
    } else {
        Err(LifecycleError::NoChild)
    }
}

pub fn reap_child(parent: u32, handle: ProcessHandle) -> Result<isize, LifecycleError> {
    if handle.pid == NO_PID {
        return Err(LifecycleError::Pid0Immutable);
    }
    let mut table = TABLE.lock();
    let slot = table
        .slot_by_handle_mut(handle)
        .ok_or(LifecycleError::InvalidHandle)?;

    if slot.parent != parent {
        return Err(LifecycleError::NotChild);
    }
    if slot.phase != ProcessPhase::Zombie {
        return Err(LifecycleError::StillRunning);
    }

    let status = slot.exit_code;
    let next_generation = slot.generation.checked_add(1);
    *slot = ProcessSlot::EMPTY;
    // Exhausted generations permanently retire the slot. This fails closed
    // instead of permitting a handle to become valid again after wraparound.
    slot.generation = next_generation.unwrap_or(u32::MAX);
    Ok(status)
}

pub fn snapshot(pid: u32) -> Option<ProcessSnapshot> {
    TABLE
        .lock()
        .slot_by_pid(pid)
        .copied()
        .map(ProcessSlot::snapshot)
}

pub fn next_runnable(after_pid: u32) -> Option<ProcessSnapshot> {
    let table = TABLE.lock();
    let start = table
        .slots
        .iter()
        .position(|slot| slot.occupied && slot.pid == after_pid)
        .map(|index| index + 1)
        .unwrap_or(0);

    table
        .slots
        .iter()
        .cycle()
        .skip(start)
        .take(table.slots.len())
        .find(|slot| slot.occupied && slot.phase == ProcessPhase::Runnable)
        .copied()
        .map(ProcessSlot::snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_SERIALIZATION: SpinLock<()> = SpinLock::new(());

    fn reset_lifecycle() {
        *TABLE.lock() = ProcessTable::new();
        CURRENT_EXECUTION.store(PID0_IDENTITY, Ordering::Release);
        CURRENT_SCHEDULER_EPOCH.store(0, Ordering::Release);
    }

    fn launch(seed: u64) -> ProcessLaunch {
        ProcessLaunch {
            address_space_root: 0x1000 + seed * 0x1000,
            entry_point: 0x2000,
            user_stack_pointer: 0x8000,
            kernel_stack_pointer: 0xffff_8000_0000_0000 + seed * 0x4000,
            image_measurement_root: seed.max(1),
            capability_root: seed.max(1),
            service_class: seed as u16,
            priority: 10,
        }
    }

    fn saved(seed: u64) -> SavedUserContext {
        SavedUserContext {
            r15: seed + 15,
            r14: seed + 14,
            r13: seed + 13,
            r12: seed + 12,
            r11: seed + 11,
            r10: seed + 10,
            r9: seed + 9,
            r8: seed + 8,
            rbp: seed + 7,
            rdi: seed + 6,
            rsi: seed + 5,
            rdx: seed + 4,
            rcx: seed + 3,
            rbx: seed + 2,
            rax: seed + 1,
            instruction_pointer: 0x2000 + seed,
            flags: super::super::context::INITIAL_USER_FLAGS,
            stack_pointer: 0x8000 + seed,
        }
    }

    #[test]
    fn lifecycle_requires_real_launch_and_reaps_only_zombies() {
        let _serial = TEST_SERIALIZATION.lock();
        reset_lifecycle();

        assert_eq!(
            commit_child(
                1,
                ProcessLaunch {
                    address_space_root: 0,
                    ..launch(1)
                }
            ),
            Err(LifecycleError::InvalidLaunch),
        );

        let init = register_init(launch(1)).unwrap();
        assert_eq!(current_handle(), None);
        mark_running(init).unwrap();
        assert_eq!(current_handle(), Some(init));

        let retained = resume_current(saved(0x40)).unwrap();
        assert_eq!(
            authorize_user_return(retained.authorized_return()),
            Ok(retained)
        );
        assert_eq!(
            authorize_user_return(AuthorizedUserReturn {
                generation: retained.handle.generation + 1,
                ..retained.authorized_return()
            }),
            Err(LifecycleError::StaleDispatchAuthority),
        );

        let stale_init = ProcessHandle {
            pid: init.pid,
            generation: init.generation + 1,
        };
        CURRENT_EXECUTION.store(stale_init.encode(), Ordering::Release);
        assert_eq!(
            schedule_yield(saved(0x80)),
            Err(LifecycleError::InvalidHandle)
        );
        assert_eq!(snapshot(init.pid).unwrap().phase, ProcessPhase::Running);
        CURRENT_EXECUTION.store(init.encode(), Ordering::Release);

        let child = commit_child(1, launch(2)).unwrap();
        assert_eq!(reap_child(1, child), Err(LifecycleError::StillRunning),);

        let invalid = SavedUserContext {
            stack_pointer: 0,
            ..saved(0x100)
        };
        assert_eq!(schedule_yield(invalid), Err(LifecycleError::InvalidContext));
        assert_eq!(snapshot(init.pid).unwrap().phase, ProcessPhase::Running);

        let mut expected_init = saved(0x100);
        expected_init.set_syscall_result(0);
        let child_dispatch = schedule_yield(saved(0x100)).unwrap();
        let child_authority = child_dispatch.authorized_return();
        assert_eq!(authorize_user_return(child_authority), Ok(child_dispatch));
        assert_eq!(child_dispatch.handle, child);
        assert_eq!(
            child_dispatch.context.user,
            SavedUserContext::initial(0x2000, 0x8000)
        );
        assert_eq!(
            child_dispatch.context.address_space_root,
            launch(2).address_space_root
        );
        assert_eq!(current_pid(), child.pid);

        let init_dispatch = schedule_yield(saved(0x200)).unwrap();
        assert_eq!(
            authorize_user_return(child_authority),
            Err(LifecycleError::StaleDispatchAuthority),
        );
        assert_eq!(init_dispatch.handle, init);
        assert_eq!(init_dispatch.context.user, expected_init);
        assert!(init_dispatch.context.validate().is_ok());

        let child_dispatch = schedule_yield(saved(0x300)).unwrap();
        assert_eq!(child_dispatch.handle, child);
        let ScheduleDecision::User(resumed) = schedule_exit(7).unwrap() else {
            panic!("runnable init must be selected");
        };
        assert_eq!(resumed.handle, init);
        assert_eq!(resumed.context.user.rax, 0);
        assert_eq!(current_pid(), init.pid);

        let zombie = wait_child(1, Some(child.pid)).unwrap();
        assert_eq!(zombie.phase, ProcessPhase::Zombie);
        assert_eq!(reap_child(1, child), Ok(7));

        let ScheduleDecision::Pid0(pid0) = schedule_exit(0).unwrap() else {
            panic!("PID0 must be selected after the final user exit");
        };
        assert_eq!(authorize_pid0(pid0), Ok(()));
        assert_eq!(current_pid(), NO_PID);
        assert_eq!(current_handle(), None);
        assert_eq!(snapshot(init.pid).unwrap().phase, ProcessPhase::Zombie);
    }

    #[test]
    fn timer_preemption_is_bound_to_exact_pid_generation_and_epoch() {
        let _serial = TEST_SERIALIZATION.lock();
        reset_lifecycle();

        let init = register_init(launch(1)).unwrap();
        mark_running(init).unwrap();
        let child = commit_child(INIT_PID, launch(2)).unwrap();
        let authority = current_execution_authority_from_irq().unwrap();
        assert_eq!(authority.handle, init);
        assert_eq!(authority.scheduler_epoch, 1);

        let init_context = saved(0x510);
        let child_dispatch = schedule_preempt(init_context, authority).unwrap();
        assert_eq!(child_dispatch.handle, child);
        assert_eq!(child_dispatch.scheduler_epoch, 2);
        assert_eq!(snapshot(init.pid).unwrap().phase, ProcessPhase::Runnable);
        assert_eq!(snapshot(child.pid).unwrap().phase, ProcessPhase::Running);

        let before_init = snapshot(init.pid).unwrap();
        let before_child = snapshot(child.pid).unwrap();
        assert_eq!(
            schedule_preempt(saved(0x520), authority),
            Err(LifecycleError::StalePreemptionAuthority)
        );
        assert_eq!(snapshot(init.pid), Some(before_init));
        assert_eq!(snapshot(child.pid), Some(before_child));
    }

    #[test]
    fn retained_timer_slice_preserves_rax_and_advances_authority_epoch() {
        let _serial = TEST_SERIALIZATION.lock();
        reset_lifecycle();

        let init = register_init(launch(1)).unwrap();
        mark_running(init).unwrap();
        let authority = current_execution_authority_from_irq().unwrap();
        let mut interrupted = saved(0x620);
        interrupted.rax = 0xfeed_face_cafe_beef;

        let retained = schedule_preempt(interrupted, authority).unwrap();
        assert_eq!(retained.handle, init);
        assert_eq!(retained.context.user.rax, interrupted.rax);
        assert_eq!(retained.scheduler_epoch, authority.scheduler_epoch + 1);
        assert_eq!(
            current_execution_authority_from_irq(),
            Some(ExecutionAuthority {
                handle: init,
                scheduler_epoch: retained.scheduler_epoch,
            })
        );
    }

    #[test]
    fn final_init_exit_selects_generation_safe_pid0() {
        let _serial = TEST_SERIALIZATION.lock();
        reset_lifecycle();

        let boot_idle = current_pid0_authority().unwrap();
        assert_eq!(boot_idle.generation(), PID0_GENERATION);
        assert_eq!(boot_idle.scheduler_epoch(), 0);

        let init = register_init(launch(1)).unwrap();
        mark_running(init).unwrap();
        let ScheduleDecision::Pid0(idle) = schedule_exit(37).unwrap() else {
            panic!("no user process remains runnable");
        };

        assert_eq!(idle.generation(), PID0_GENERATION);
        assert_eq!(idle.scheduler_epoch(), 2);
        assert_eq!(current_pid0_authority(), Ok(idle));
        assert_eq!(
            authorize_pid0(boot_idle),
            Err(LifecycleError::StalePid0Authority)
        );
        let init_snapshot = snapshot(INIT_PID).unwrap();
        assert_eq!(init_snapshot.phase, ProcessPhase::Zombie);
        assert_eq!(init_snapshot.exit_code, 37);
        assert_eq!(init_snapshot.wait_sequence, idle.scheduler_epoch());
    }

    #[test]
    fn pid0_reselection_then_handoff_is_atomic_and_invalidates_old_epochs() {
        let _serial = TEST_SERIALIZATION.lock();
        reset_lifecycle();

        let init = register_init(launch(1)).unwrap();
        mark_running(init).unwrap();
        let child = commit_child(INIT_PID, launch(2)).unwrap();
        TABLE.lock().slot_by_handle_mut(child).unwrap().phase = ProcessPhase::Blocked;

        let ScheduleDecision::Pid0(idle) = schedule_exit(0).unwrap() else {
            panic!("blocked child cannot be selected");
        };
        let ScheduleDecision::Pid0(reselected) = schedule_from_pid0(idle).unwrap() else {
            panic!("PID0 must remain selected without runnable work");
        };
        assert_eq!(
            authorize_pid0(idle),
            Err(LifecycleError::StalePid0Authority)
        );
        assert_eq!(reselected.scheduler_epoch(), idle.scheduler_epoch() + 1);

        mark_runnable(child).unwrap();
        let ScheduleDecision::User(dispatched) = schedule_from_pid0(reselected).unwrap() else {
            panic!("newly runnable child must leave PID0");
        };
        assert_eq!(dispatched.handle, child);
        assert_eq!(current_handle(), Some(child));
        assert_eq!(snapshot(child.pid).unwrap().phase, ProcessPhase::Running);
        assert_eq!(
            schedule_from_pid0(reselected),
            Err(LifecycleError::StalePid0Authority)
        );
    }

    #[test]
    fn pid0_cannot_terminate_become_a_process_or_be_reaped() {
        let _serial = TEST_SERIALIZATION.lock();
        reset_lifecycle();

        let idle = current_pid0_authority().unwrap();
        let forged_process = ProcessHandle {
            pid: NO_PID,
            generation: PID0_GENERATION,
        };
        assert_eq!(schedule_exit(9), Err(LifecycleError::Pid0Immutable));
        assert_eq!(
            publish_current(forged_process),
            Err(LifecycleError::Pid0Immutable)
        );
        assert_eq!(
            mark_running(forged_process),
            Err(LifecycleError::Pid0Immutable)
        );
        assert_eq!(
            mark_runnable(forged_process),
            Err(LifecycleError::Pid0Immutable)
        );
        assert_eq!(
            mark_blocked(forged_process),
            Err(LifecycleError::Pid0Immutable)
        );
        assert_eq!(
            reap_child(INIT_PID, forged_process),
            Err(LifecycleError::Pid0Immutable)
        );
        assert_eq!(snapshot(NO_PID), None);
        assert_eq!(authorize_pid0(idle), Ok(()));
    }

    #[test]
    fn pid0_epoch_exhaustion_rolls_back_runnable_selection_exactly() {
        let _serial = TEST_SERIALIZATION.lock();
        reset_lifecycle();

        let init = register_init(launch(1)).unwrap();
        TABLE.lock().scheduler_epoch = u64::MAX;
        let exhausted = current_pid0_authority().unwrap();
        let before = snapshot(init.pid).unwrap();
        let encoded_before = CURRENT_EXECUTION.load(Ordering::Acquire);

        assert_eq!(
            schedule_from_pid0(exhausted),
            Err(LifecycleError::EpochExhausted)
        );
        assert_eq!(snapshot(init.pid), Some(before));
        assert_eq!(CURRENT_EXECUTION.load(Ordering::Acquire), encoded_before);
        assert_eq!(current_pid0_authority(), Ok(exhausted));
    }

    #[test]
    fn pid0_rejects_stale_generation_and_epoch_without_mutation() {
        let _serial = TEST_SERIALIZATION.lock();
        reset_lifecycle();

        let current = current_pid0_authority().unwrap();
        let stale_generation = Pid0Authority {
            generation: current.generation() + 1,
            ..current
        };
        let stale_epoch = Pid0Authority {
            scheduler_epoch: current.scheduler_epoch() + 1,
            ..current
        };
        assert_eq!(
            schedule_from_pid0(stale_generation),
            Err(LifecycleError::StalePid0Authority)
        );
        assert_eq!(
            schedule_from_pid0(stale_epoch),
            Err(LifecycleError::StalePid0Authority)
        );
        assert_eq!(current_pid0_authority(), Ok(current));
        assert_eq!(TABLE.lock().scheduler_epoch, current.scheduler_epoch());

        let stale_identity = (u64::from(PID0_GENERATION) + 1) << 32;
        CURRENT_EXECUTION.store(stale_identity, Ordering::Release);
        assert_eq!(
            authorize_pid0(current),
            Err(LifecycleError::StalePid0Authority)
        );
        CURRENT_EXECUTION.store(PID0_IDENTITY, Ordering::Release);
        assert_eq!(authorize_pid0(current), Ok(()));
    }

    #[test]
    fn execution_identity_cannot_alias_a_reused_pid_or_wrapped_generation() {
        let old = ProcessHandle {
            pid: 7,
            generation: 41,
        };
        let replacement = ProcessHandle {
            pid: 7,
            generation: 42,
        };
        assert_eq!(ProcessHandle::decode(old.encode()), Some(old));
        assert_ne!(old.encode(), replacement.encode());
        assert_eq!(ProcessHandle::decode(0), None);
        assert_ne!(PID0_IDENTITY, 0);
        assert_eq!(ProcessHandle::decode(PID0_IDENTITY), None);

        let mut exhausted = ProcessSlot::EMPTY;
        exhausted.occupied = true;
        exhausted.pid = 9;
        exhausted.generation = u32::MAX;
        exhausted.phase = ProcessPhase::Zombie;
        exhausted.parent = INIT_PID;
        let retired_generation = exhausted.generation.checked_add(1).unwrap_or(u32::MAX);
        *&mut exhausted = ProcessSlot::EMPTY;
        exhausted.generation = retired_generation;
        assert_eq!(exhausted.generation, u32::MAX);
        assert!(!exhausted.occupied);
        assert!(exhausted.generation.checked_add(1).is_none());
    }
}
