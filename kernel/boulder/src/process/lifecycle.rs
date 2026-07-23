//! Fixed-capacity process lifecycle state.
//!
//! This is a truthful lifecycle registry: it only admits processes after an
//! architecture backend has supplied a complete launch image. PID allocation
//! alone never creates a runnable process.

use core::sync::atomic::{AtomicU32, Ordering};

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
        const USER_LIMIT: u64 = 0x0000_8000_0000_0000;

        self.address_space_root != 0
            && self.address_space_root & 0xfff == 0
            && (0x1000..USER_LIMIT).contains(&self.entry_point)
            && (0x1000..USER_LIMIT).contains(&self.user_stack_pointer)
            && self.kernel_stack_pointer != 0
            && self.image_measurement_root != 0
            && self.capability_root != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessHandle {
    pub pid: u32,
    pub generation: u32,
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

#[derive(Clone, Copy)]
struct ProcessSlot {
    occupied: bool,
    generation: u32,
    pid: u32,
    parent: u32,
    phase: ProcessPhase,
    launch: ProcessLaunch,
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

    fn slot_by_handle_mut(
        &mut self,
        handle: ProcessHandle,
    ) -> Option<&mut ProcessSlot> {
        self.slots.iter_mut().find(|slot| {
            slot.occupied
                && slot.pid == handle.pid
                && slot.generation == handle.generation
        })
    }

    fn slot_by_pid_mut(&mut self, pid: u32) -> Option<&mut ProcessSlot> {
        self.slots
            .iter_mut()
            .find(|slot| slot.occupied && slot.pid == pid)
    }

    fn slot_by_pid(&self, pid: u32) -> Option<&ProcessSlot> {
        self.slots
            .iter()
            .find(|slot| slot.occupied && slot.pid == pid)
    }
}

static TABLE: SpinLock<ProcessTable> =
    SpinLock::new(ProcessTable::new());
static CURRENT_PID: AtomicU32 = AtomicU32::new(INIT_PID);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleError {
    InvalidLaunch,
    Capacity,
    InvalidHandle,
    InvalidTransition,
    NotChild,
    StillRunning,
    NoChild,
}

pub fn current_pid() -> u32 {
    CURRENT_PID.load(Ordering::Acquire)
}

pub fn publish_current_pid(pid: u32) -> Result<(), LifecycleError> {
    let table = TABLE.lock();
    let Some(slot) = table.slot_by_pid(pid) else {
        return Err(LifecycleError::InvalidHandle);
    };
    if !matches!(
        slot.phase,
        ProcessPhase::Runnable | ProcessPhase::Running
    ) {
        return Err(LifecycleError::InvalidTransition);
    }
    drop(table);
    CURRENT_PID.store(pid, Ordering::Release);
    Ok(())
}

pub fn register_init(
    launch: ProcessLaunch,
) -> Result<ProcessHandle, LifecycleError> {
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
        .find(|slot| !slot.occupied)
        .ok_or(LifecycleError::Capacity)?;

    slot.occupied = true;
    slot.pid = INIT_PID;
    slot.parent = NO_PID;
    slot.phase = ProcessPhase::Runnable;
    slot.launch = launch;
    slot.exit_code = 0;
    slot.wait_sequence = 0;

    Ok(ProcessHandle {
        pid: INIT_PID,
        generation: slot.generation,
    })
}

pub fn commit_child(
    parent: u32,
    launch: ProcessLaunch,
) -> Result<ProcessHandle, LifecycleError> {
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
            .find(|slot| !slot.occupied)
            .ok_or(LifecycleError::Capacity)?;

        slot.occupied = true;
        slot.generation = slot.generation.wrapping_add(1).max(1);
        slot.pid = pid;
        slot.parent = parent;
        slot.phase = ProcessPhase::Runnable;
        slot.launch = launch;
        slot.exit_code = 0;
        slot.wait_sequence = scheduler_epoch;
        slot.generation
    };

    Ok(ProcessHandle { pid, generation })
}

pub fn mark_running(
    handle: ProcessHandle,
) -> Result<ProcessSnapshot, LifecycleError> {
    let mut table = TABLE.lock();
    let index = table
        .slots
        .iter()
        .position(|slot| {
            slot.occupied
                && slot.pid == handle.pid
                && slot.generation == handle.generation
        })
        .ok_or(LifecycleError::InvalidHandle)?;
    if table.slots[index].phase != ProcessPhase::Runnable {
        return Err(LifecycleError::InvalidTransition);
    }

    table.scheduler_epoch = table.scheduler_epoch.wrapping_add(1);
    table.slots[index].phase = ProcessPhase::Running;
    let snapshot = table.slots[index].snapshot();
    CURRENT_PID.store(snapshot.handle.pid, Ordering::Release);
    Ok(snapshot)
}

pub fn mark_runnable(
    handle: ProcessHandle,
) -> Result<(), LifecycleError> {
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

pub fn mark_blocked(
    handle: ProcessHandle,
) -> Result<(), LifecycleError> {
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
    let pid = current_pid();
    let mut table = TABLE.lock();
    let next_epoch = table.scheduler_epoch.wrapping_add(1);
    {
        let slot = table
            .slot_by_pid_mut(pid)
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

pub fn reap_child(
    parent: u32,
    handle: ProcessHandle,
) -> Result<isize, LifecycleError> {
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
    let next_generation = slot.generation.wrapping_add(1).max(1);
    *slot = ProcessSlot::EMPTY;
    slot.generation = next_generation;
    Ok(status)
}

pub fn snapshot(pid: u32) -> Option<ProcessSnapshot> {
    TABLE.lock().slot_by_pid(pid).copied().map(ProcessSlot::snapshot)
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

    #[test]
    fn lifecycle_requires_real_launch_and_reaps_only_zombies() {
        *TABLE.lock() = ProcessTable::new();
        CURRENT_PID.store(INIT_PID, Ordering::Release);

        assert_eq!(
            commit_child(1, ProcessLaunch {
                address_space_root: 0,
                ..launch(1)
            }),
            Err(LifecycleError::InvalidLaunch),
        );

        let init = register_init(launch(1)).unwrap();
        mark_running(init).unwrap();

        let child = commit_child(1, launch(2)).unwrap();
        assert_eq!(
            reap_child(1, child),
            Err(LifecycleError::StillRunning),
        );

        mark_runnable(init).unwrap();
        mark_running(child).unwrap();
        exit_current(7).unwrap();

        let zombie = wait_child(1, Some(child.pid)).unwrap();
        assert_eq!(zombie.phase, ProcessPhase::Zombie);
        assert_eq!(reap_child(1, child), Ok(7));
    }
}
