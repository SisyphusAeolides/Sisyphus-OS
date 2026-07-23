use crate::sync::SpinLock;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

pub static NEXT_PID: AtomicU32 = AtomicU32::new(2);

pub struct ProcessState {
    pub pid: u32,
    pub parent: u32,
    pub exited: bool,
    pub exit_code: isize,
    pub memory_base: u64,
    pub memory_size: usize,
}

pub struct ProcessTable {
    pub processes: BTreeMap<u32, ProcessState>,
    pub runqueue: Vec<u32>,
}

impl ProcessTable {
    pub const fn new() -> Self {
        Self {
            processes: BTreeMap::new(),
            runqueue: Vec::new(),
        }
    }
}

pub static PROCESS_TABLE: SpinLock<ProcessTable> = SpinLock::new(ProcessTable::new());

pub fn spawn(parent_pid: u32, memory_base: u64, memory_size: usize) -> u32 {
    let pid = NEXT_PID.fetch_add(1, Ordering::SeqCst);
    let mut table = PROCESS_TABLE.lock();
    table.processes.insert(
        pid,
        ProcessState {
            pid,
            parent: parent_pid,
            exited: false,
            exit_code: 0,
            memory_base,
            memory_size,
        },
    );
    table.runqueue.push(pid);
    pid
}

pub fn exit(pid: u32, exit_code: isize) {
    let mut table = PROCESS_TABLE.lock();
    if let Some(proc) = table.processes.get_mut(&pid) {
        proc.exited = true;
        proc.exit_code = exit_code;
    }
    table.runqueue.retain(|&p| p != pid);
}

pub fn wait(target_pid: u32) -> isize {
    let mut table = PROCESS_TABLE.lock();
    if let Some(proc) = table.processes.get(&target_pid) {
        if proc.exited {
            let code = proc.exit_code;
            table.processes.remove(&target_pid);
            return code;
        } else {
            return -11; // ERROR_AGAIN
        }
    }
    -10 // ERROR_NO_CHILD
}
