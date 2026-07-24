//! Fixed-capacity process lifecycle state.
//!
//! This is a truthful lifecycle registry: it only admits processes after an
//! architecture backend has supplied a complete launch image. PID allocation
//! alone never creates a runnable process.

use core::sync::atomic::{AtomicU64, Ordering};

use super::context::{
    valid_kernel_stack_pointer, valid_page_table_root, valid_user_address, DispatchContext,
    SavedUserContext,
};
use crate::sync::SpinLock;

pub const MAXIMUM_PROCESSES: usize = 64;
pub const NO_PID: u32 = 0;
pub const INIT_PID: u32 = 1;
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
// Zero means that no process owns this CPU. A PID alone is not an execution
// authority: it can be reused after reaping, so the published identity always
// carries the lifecycle generation as one atomic value.
static CURRENT_PROCESS: AtomicU64 = AtomicU64::new(0);

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
}

pub fn current_pid() -> u32 {
    current_handle().map_or(NO_PID, |handle| handle.pid)
}

pub fn current_handle() -> Option<ProcessHandle> {
    ProcessHandle::decode(CURRENT_PROCESS.load(Ordering::Acquire))
}

pub fn publish_current(handle: ProcessHandle) -> Result<(), LifecycleError> {
    let table = TABLE.lock();
    let Some(slot) = table.slot_by_handle(handle) else {
        return Err(LifecycleError::InvalidHandle);
    };
    if slot.phase != ProcessPhase::Running {
        return Err(LifecycleError::InvalidTransition);
    }
    drop(table);
    CURRENT_PROCESS.store(handle.encode(), Ordering::Release);
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
    let mut table = TABLE.lock();
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
    CURRENT_PROCESS.store(snapshot.handle.encode(), Ordering::Release);
    Ok(snapshot)
}

/// Saves a valid Ring 3 context, returns the next runnable process in
/// round-robin order, and marks that target running as one atomic table
/// transition. If no peer is runnable, the caller is selected again.
pub fn schedule_yield(mut saved: SavedUserContext) -> Result<ScheduledProcess, LifecycleError> {
    saved
        .validate()
        .map_err(|_| LifecycleError::InvalidContext)?;
    saved.set_syscall_result(0);

    let current = current_handle().ok_or(LifecycleError::InvalidHandle)?;
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
    CURRENT_PROCESS.store(scheduled.handle.encode(), Ordering::Release);
    Ok(scheduled)
}

/// Terminates the running process and selects a different runnable context.
/// `None` means no process remains runnable and the architecture must enter
/// its idle path instead of returning to the terminated caller.
pub fn schedule_exit(exit_code: isize) -> Result<Option<ScheduledProcess>, LifecycleError> {
    let current = current_handle().ok_or(LifecycleError::InvalidHandle)?;
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
        CURRENT_PROCESS.store(scheduled.handle.encode(), Ordering::Release);
        Ok(Some(scheduled))
    } else {
        CURRENT_PROCESS.store(0, Ordering::Release);
        Ok(None)
    }
}

pub fn mark_runnable(handle: ProcessHandle) -> Result<(), LifecycleError> {
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

pub fn exit_current(exit_code: isize) -> Result<(), LifecycleError> {
    let current = current_handle().ok_or(LifecycleError::InvalidHandle)?;
    let mut table = TABLE.lock();
    let next_epoch = next_scheduler_epoch(table.scheduler_epoch)?;
    {
        let slot = table
            .slot_by_handle_mut(current)
            .ok_or(LifecycleError::InvalidHandle)?;
        if !matches!(
            slot.phase,
            ProcessPhase::Running | ProcessPhase::Runnable | ProcessPhase::Blocked
        ) {
            return Err(LifecycleError::InvalidTransition);
        }
        slot.phase = ProcessPhase::Zombie;
        slot.exit_code = exit_code;
        slot.wait_sequence = next_epoch;
    }
    table.scheduler_epoch = next_epoch;
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
        *TABLE.lock() = ProcessTable::new();
        CURRENT_PROCESS.store(0, Ordering::Release);

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

        let stale_init = ProcessHandle {
            pid: init.pid,
            generation: init.generation + 1,
        };
        CURRENT_PROCESS.store(stale_init.encode(), Ordering::Release);
        assert_eq!(
            schedule_yield(saved(0x80)),
            Err(LifecycleError::InvalidHandle)
        );
        assert_eq!(snapshot(init.pid).unwrap().phase, ProcessPhase::Running);
        CURRENT_PROCESS.store(init.encode(), Ordering::Release);

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
        assert_eq!(init_dispatch.handle, init);
        assert_eq!(init_dispatch.context.user, expected_init);
        assert!(init_dispatch.context.validate().is_ok());

        let child_dispatch = schedule_yield(saved(0x300)).unwrap();
        assert_eq!(child_dispatch.handle, child);
        let resumed = schedule_exit(7).unwrap().unwrap();
        assert_eq!(resumed.handle, init);
        assert_eq!(resumed.context.user.rax, 0);
        assert_eq!(current_pid(), init.pid);

        let zombie = wait_child(1, Some(child.pid)).unwrap();
        assert_eq!(zombie.phase, ProcessPhase::Zombie);
        assert_eq!(reap_child(1, child), Ok(7));

        assert_eq!(schedule_exit(0), Ok(None));
        assert_eq!(current_pid(), NO_PID);
        assert_eq!(current_handle(), None);
        assert_eq!(snapshot(init.pid).unwrap().phase, ProcessPhase::Zombie);
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
