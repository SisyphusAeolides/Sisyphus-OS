use core::sync::atomic::{AtomicBool, Ordering};

use ::aether::flight::Recorder;
use ::aether::policy::{Instruction, Opcode, PolicyError, PolicyVm};

use crate::arch::{Active, Architecture};
use crate::capability::{Capability, PolicyControl};
use crate::sync::SpinLock;

pub const FLIGHT_RECORDER_CAPACITY: usize = 4096;

pub mod event_kind {
    pub const INITIALIZED: u16 = 1;
    pub const POLICY_DECISION: u16 = 2;
    pub const RESONANCE_PLAN: u16 = 3;
}

const PAGE_LIMIT_POLICY: [Instruction; 3] = [
    Instruction::new(Opcode::MoveImmediate, 1, 0, 0, 512),
    Instruction::new(Opcode::SetLessOrEqual, 0, 0, 1, 0),
    Instruction::new(Opcode::Return, 0, 0, 0, 0),
];

static INITIALIZED: AtomicBool = AtomicBool::new(false);
static POLICY_VM: PolicyVm = PolicyVm::new();
static FLIGHT_RECORDER: SpinLock<Recorder<FLIGHT_RECORDER_CAPACITY>> =
    SpinLock::new(Recorder::new());

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitializeError {
    AlreadyInitialized,
    Policy(PolicyError),
    SelfTestFailed,
}

pub fn initialize(_authority: &Capability<'_, PolicyControl>) -> Result<(), InitializeError> {
    if INITIALIZED.swap(true, Ordering::AcqRel) {
        return Err(InitializeError::AlreadyInitialized);
    }
    if let Err(error) = POLICY_VM.load(0, &PAGE_LIMIT_POLICY, 0) {
        INITIALIZED.store(false, Ordering::Release);
        return Err(InitializeError::Policy(error));
    }
    if POLICY_VM.run(0, [512, 0, 0, 0]) != Ok(1) || POLICY_VM.run(0, [513, 0, 0, 0]) != Ok(0) {
        INITIALIZED.store(false, Ordering::Release);
        return Err(InitializeError::SelfTestFailed);
    }
    record(event_kind::INITIALIZED, 1, 0);
    Ok(())
}

pub fn policy_allows_page_count(page_count: u32) -> Result<bool, PolicyError> {
    let allowed = POLICY_VM.run(0, [u64::from(page_count), 0, 0, 0])? != 0;
    record(
        event_kind::POLICY_DECISION,
        u64::from(page_count),
        u64::from(allowed),
    );
    Ok(allowed)
}

pub fn record(kind: u16, argument_zero: u64, argument_one: u64) -> u64 {
    FLIGHT_RECORDER.lock().record(
        Active::counter_sample(),
        Active::hardware_thread_id(),
        kind,
        argument_zero,
        argument_one,
    )
}

pub fn recorded_events() -> usize {
    FLIGHT_RECORDER.lock().retained()
}
