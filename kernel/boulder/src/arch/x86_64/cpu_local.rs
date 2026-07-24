use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::sync::SpinLock;

pub const MAXIMUM_CPU_AUTHORITIES: usize = 256;
pub const KERNEL_ENTRY_STACK_BYTES: usize = 16 * 1024;

const STATE_FREE: u32 = 0;
const STATE_REGISTERED: u32 = 1;
const INVALID_LOGICAL_ID: u32 = u32::MAX;
const KERNEL_ADDRESS_MINIMUM: u64 = 0xffff_8000_0000_0000;

// Keep these offsets synchronized with syscall.S. Compile-time assertions at
// the bottom of the file turn any representation drift into a build failure.
pub const SELF_POINTER_OFFSET: usize = 0;
pub const ENTRY_STACK_TOP_OFFSET: usize = 8;
pub const USER_STACK_POINTER_OFFSET: usize = 16;
pub const TSS_POINTER_OFFSET: usize = 24;
pub const TSS_RSP0_OFFSET: usize = 32;
pub const GENERATION_OFFSET: usize = 40;
pub const ENTRY_GENERATION_OFFSET: usize = 48;
pub const ARMED_GENERATION_OFFSET: usize = 56;
pub const APIC_ID_OFFSET: usize = 64;
pub const LOGICAL_ID_OFFSET: usize = 68;
pub const NESTING_OFFSET: usize = 72;
pub const STATE_OFFSET: usize = 76;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuRegistration {
    pub apic_id: u32,
    pub logical_id: u32,
    pub generation: u64,
    pub entry_stack_top: u64,
    pub tss_pointer: u64,
    record_pointer: u64,
}

impl CpuRegistration {
    pub const fn record_pointer(self) -> u64 {
        self.record_pointer
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EntryLease {
    pub apic_id: u32,
    pub logical_id: u32,
    pub generation: u64,
    pub tss_pointer: u64,
    pub tss_rsp0: u64,
    slot: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReturnLease {
    pub entry: EntryLease,
    pub next_generation: u64,
    pub next_tss_rsp0: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CpuLocalError {
    InvalidApicId,
    InvalidLogicalId,
    InvalidEntryStack,
    InvalidTaskStateSegment,
    InvalidTaskStateStack,
    DuplicateApicId,
    DuplicateLogicalId,
    DuplicateEntryStack,
    DuplicateTaskStateSegment,
    CapacityExceeded,
    UnregisteredCpu,
    CorruptRecord,
    Reentered,
    EntryNotActive,
    StaleGeneration,
    TaskStateSegmentMismatch,
    TaskStateStackMismatch,
    GenerationExhausted,
    StaleLease,
}

/// Assembly-visible authority for exactly one registered hardware thread.
///
/// The record is never moved after publication. Registration and revocation
/// are serialized; entry fields are atomic because assembly and Rust observe
/// the same memory without borrowing it. x86 syscall entry runs with IF
/// cleared, so the nesting transition itself is local to one hardware thread.
#[repr(C, align(64))]
struct CpuLocalRecord {
    self_pointer: AtomicU64,
    entry_stack_top: AtomicU64,
    user_stack_pointer: AtomicU64,
    tss_pointer: AtomicU64,
    tss_rsp0: AtomicU64,
    generation: AtomicU64,
    entry_generation: AtomicU64,
    armed_generation: AtomicU64,
    apic_id: AtomicU32,
    logical_id: AtomicU32,
    nesting: AtomicU32,
    state: AtomicU32,
}

impl CpuLocalRecord {
    const fn new() -> Self {
        Self {
            self_pointer: AtomicU64::new(0),
            entry_stack_top: AtomicU64::new(0),
            user_stack_pointer: AtomicU64::new(0),
            tss_pointer: AtomicU64::new(0),
            tss_rsp0: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            entry_generation: AtomicU64::new(0),
            armed_generation: AtomicU64::new(0),
            apic_id: AtomicU32::new(0),
            logical_id: AtomicU32::new(INVALID_LOGICAL_ID),
            nesting: AtomicU32::new(0),
            state: AtomicU32::new(STATE_FREE),
        }
    }
}

struct CpuAuthorityTable<const CAPACITY: usize> {
    registration: SpinLock<()>,
    records: [CpuLocalRecord; CAPACITY],
}

impl<const CAPACITY: usize> CpuAuthorityTable<CAPACITY> {
    const fn new() -> Self {
        Self {
            registration: SpinLock::new(()),
            records: [const { CpuLocalRecord::new() }; CAPACITY],
        }
    }

    #[cfg(any(target_os = "none", test))]
    fn register(
        &self,
        apic_id: u32,
        logical_id: u32,
        entry_stack_top: u64,
        tss_pointer: u64,
        tss_rsp0: u64,
    ) -> Result<CpuRegistration, CpuLocalError> {
        if apic_id == u32::MAX {
            return Err(CpuLocalError::InvalidApicId);
        }
        if usize::try_from(logical_id).map_or(true, |id| id >= MAXIMUM_CPU_AUTHORITIES) {
            return Err(CpuLocalError::InvalidLogicalId);
        }
        if entry_stack_top < KERNEL_ADDRESS_MINIMUM || entry_stack_top & 0xf != 0 {
            return Err(CpuLocalError::InvalidEntryStack);
        }
        if tss_pointer < KERNEL_ADDRESS_MINIMUM || tss_pointer.checked_add(103).is_none() {
            return Err(CpuLocalError::InvalidTaskStateSegment);
        }
        if tss_rsp0 < KERNEL_ADDRESS_MINIMUM || tss_rsp0 & 0xf != 0 {
            return Err(CpuLocalError::InvalidTaskStateStack);
        }

        let _registration = self.registration.lock();
        for record in &self.records {
            if record.state.load(Ordering::Acquire) != STATE_REGISTERED {
                continue;
            }
            if record.apic_id.load(Ordering::Relaxed) == apic_id {
                return Err(CpuLocalError::DuplicateApicId);
            }
            if record.logical_id.load(Ordering::Relaxed) == logical_id {
                return Err(CpuLocalError::DuplicateLogicalId);
            }
            if record.entry_stack_top.load(Ordering::Relaxed) == entry_stack_top {
                return Err(CpuLocalError::DuplicateEntryStack);
            }
            if record.tss_pointer.load(Ordering::Relaxed) == tss_pointer {
                return Err(CpuLocalError::DuplicateTaskStateSegment);
            }
        }
        let record = self
            .records
            .iter()
            .find(|record| record.state.load(Ordering::Acquire) == STATE_FREE)
            .ok_or(CpuLocalError::CapacityExceeded)?;
        let generation = record
            .generation
            .load(Ordering::Relaxed)
            .checked_add(1)
            .ok_or(CpuLocalError::GenerationExhausted)?;
        let record_pointer = record as *const CpuLocalRecord as u64;

        record.self_pointer.store(record_pointer, Ordering::Relaxed);
        record
            .entry_stack_top
            .store(entry_stack_top, Ordering::Relaxed);
        record.user_stack_pointer.store(0, Ordering::Relaxed);
        record.tss_pointer.store(tss_pointer, Ordering::Relaxed);
        record.tss_rsp0.store(tss_rsp0, Ordering::Relaxed);
        record.generation.store(generation, Ordering::Relaxed);
        record.entry_generation.store(0, Ordering::Relaxed);
        record.armed_generation.store(generation, Ordering::Relaxed);
        record.apic_id.store(apic_id, Ordering::Relaxed);
        record.logical_id.store(logical_id, Ordering::Relaxed);
        record.nesting.store(0, Ordering::Relaxed);
        record.state.store(STATE_REGISTERED, Ordering::Release);

        Ok(CpuRegistration {
            apic_id,
            logical_id,
            generation,
            entry_stack_top,
            tss_pointer,
            record_pointer,
        })
    }

    fn revoke(&self, registration: CpuRegistration) -> Result<(), CpuLocalError> {
        let _registration_guard = self.registration.lock();
        let record = self
            .find(registration.apic_id)
            .ok_or(CpuLocalError::UnregisteredCpu)?;
        if record.logical_id.load(Ordering::Acquire) != registration.logical_id
            || record.generation.load(Ordering::Acquire) != registration.generation
            || record.self_pointer.load(Ordering::Acquire) != registration.record_pointer
        {
            return Err(CpuLocalError::StaleLease);
        }
        if record.nesting.load(Ordering::Acquire) != 0 {
            return Err(CpuLocalError::Reentered);
        }
        record.state.store(STATE_FREE, Ordering::Release);
        record.entry_stack_top.store(0, Ordering::Relaxed);
        record.user_stack_pointer.store(0, Ordering::Relaxed);
        record.tss_pointer.store(0, Ordering::Relaxed);
        record.tss_rsp0.store(0, Ordering::Relaxed);
        record.entry_generation.store(0, Ordering::Relaxed);
        record.armed_generation.store(0, Ordering::Relaxed);
        record.apic_id.store(0, Ordering::Relaxed);
        record
            .logical_id
            .store(INVALID_LOGICAL_ID, Ordering::Relaxed);
        record.self_pointer.store(0, Ordering::Relaxed);
        Ok(())
    }

    fn find(&self, apic_id: u32) -> Option<&CpuLocalRecord> {
        self.records.iter().find(|record| {
            record.state.load(Ordering::Acquire) == STATE_REGISTERED
                && record.apic_id.load(Ordering::Relaxed) == apic_id
        })
    }

    #[cfg(test)]
    fn begin_entry(
        &self,
        apic_id: u32,
        presented_generation: u64,
    ) -> Result<EntryLease, CpuLocalError> {
        let record = self.find(apic_id).ok_or(CpuLocalError::UnregisteredCpu)?;
        let generation = record.generation.load(Ordering::Acquire);
        if generation == 0
            || record.armed_generation.load(Ordering::Acquire) != generation
            || presented_generation != generation
        {
            return Err(CpuLocalError::StaleGeneration);
        }
        record
            .nesting
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| CpuLocalError::Reentered)?;
        record.entry_generation.store(generation, Ordering::Release);
        self.validate_entry(apic_id)
    }

    fn validate_entry(&self, apic_id: u32) -> Result<EntryLease, CpuLocalError> {
        let record = self.find(apic_id).ok_or(CpuLocalError::UnregisteredCpu)?;
        let slot = record as *const CpuLocalRecord as usize - self.records.as_ptr() as usize;
        let slot = slot / core::mem::size_of::<CpuLocalRecord>();
        let generation = record.generation.load(Ordering::Acquire);
        if record.self_pointer.load(Ordering::Acquire) != record as *const CpuLocalRecord as u64
            || record.entry_stack_top.load(Ordering::Acquire) < KERNEL_ADDRESS_MINIMUM
            || record.tss_pointer.load(Ordering::Acquire) < KERNEL_ADDRESS_MINIMUM
        {
            return Err(CpuLocalError::CorruptRecord);
        }
        if record.nesting.load(Ordering::Acquire) != 1 {
            return Err(CpuLocalError::EntryNotActive);
        }
        if generation == 0
            || record.entry_generation.load(Ordering::Acquire) != generation
            || record.armed_generation.load(Ordering::Acquire) != generation
        {
            return Err(CpuLocalError::StaleGeneration);
        }
        Ok(EntryLease {
            apic_id,
            logical_id: record.logical_id.load(Ordering::Acquire),
            generation,
            tss_pointer: record.tss_pointer.load(Ordering::Acquire),
            tss_rsp0: record.tss_rsp0.load(Ordering::Acquire),
            slot,
        })
    }

    fn validate_machine_entry(
        &self,
        apic_id: u32,
        tss_pointer: u64,
        tss_rsp0: u64,
    ) -> Result<EntryLease, CpuLocalError> {
        let lease = self.validate_entry(apic_id)?;
        if lease.tss_pointer != tss_pointer {
            return Err(CpuLocalError::TaskStateSegmentMismatch);
        }
        if lease.tss_rsp0 != tss_rsp0 {
            return Err(CpuLocalError::TaskStateStackMismatch);
        }
        Ok(lease)
    }

    fn prepare_return(
        &self,
        apic_id: u32,
        tss_pointer: u64,
        current_tss_rsp0: u64,
        next_tss_rsp0: u64,
    ) -> Result<ReturnLease, CpuLocalError> {
        if next_tss_rsp0 < KERNEL_ADDRESS_MINIMUM || next_tss_rsp0 & 0xf != 0 {
            return Err(CpuLocalError::InvalidTaskStateStack);
        }
        let entry = self.validate_machine_entry(apic_id, tss_pointer, current_tss_rsp0)?;
        let next_generation = entry
            .generation
            .checked_add(1)
            .ok_or(CpuLocalError::GenerationExhausted)?;
        Ok(ReturnLease {
            entry,
            next_generation,
            next_tss_rsp0,
        })
    }

    fn commit_return(
        &self,
        lease: ReturnLease,
        tss_pointer: u64,
        committed_tss_rsp0: u64,
    ) -> Result<u64, CpuLocalError> {
        let record = self
            .records
            .get(lease.entry.slot)
            .ok_or(CpuLocalError::StaleLease)?;
        if record.state.load(Ordering::Acquire) != STATE_REGISTERED
            || record.apic_id.load(Ordering::Acquire) != lease.entry.apic_id
            || record.logical_id.load(Ordering::Acquire) != lease.entry.logical_id
            || record.self_pointer.load(Ordering::Acquire) != record as *const CpuLocalRecord as u64
            || record.nesting.load(Ordering::Acquire) != 1
            || record.generation.load(Ordering::Acquire) != lease.entry.generation
            || record.entry_generation.load(Ordering::Acquire) != lease.entry.generation
        {
            return Err(CpuLocalError::StaleLease);
        }
        if tss_pointer != lease.entry.tss_pointer
            || record.tss_pointer.load(Ordering::Acquire) != tss_pointer
        {
            return Err(CpuLocalError::TaskStateSegmentMismatch);
        }
        if committed_tss_rsp0 != lease.next_tss_rsp0 {
            return Err(CpuLocalError::TaskStateStackMismatch);
        }

        record.tss_rsp0.store(committed_tss_rsp0, Ordering::Release);
        record
            .armed_generation
            .store(lease.next_generation, Ordering::Release);
        record
            .generation
            .store(lease.next_generation, Ordering::Release);
        Ok(lease.next_generation)
    }

    #[cfg(test)]
    fn finish_return(&self, apic_id: u32, generation: u64) -> Result<(), CpuLocalError> {
        let record = self.find(apic_id).ok_or(CpuLocalError::UnregisteredCpu)?;
        if record.generation.load(Ordering::Acquire) != generation
            || record.armed_generation.load(Ordering::Acquire) != generation
        {
            return Err(CpuLocalError::StaleGeneration);
        }
        if record.nesting.load(Ordering::Acquire) != 1 {
            return Err(CpuLocalError::EntryNotActive);
        }
        record.entry_generation.store(0, Ordering::Release);
        record.nesting.store(0, Ordering::Release);
        Ok(())
    }
}

#[repr(C, align(64))]
#[cfg(target_os = "none")]
struct KernelEntryStack {
    bytes: [u8; KERNEL_ENTRY_STACK_BYTES],
}

static CPU_AUTHORITIES: CpuAuthorityTable<MAXIMUM_CPU_AUTHORITIES> = CpuAuthorityTable::new();

#[cfg(target_os = "none")]
static mut KERNEL_ENTRY_STACKS: [KernelEntryStack; MAXIMUM_CPU_AUTHORITIES] = [const {
    KernelEntryStack {
        bytes: [0; KERNEL_ENTRY_STACK_BYTES],
    }
};
    MAXIMUM_CPU_AUTHORITIES];

/// Returns the immutable top of the entry stack assigned to a logical CPU.
#[cfg(target_os = "none")]
pub fn entry_stack_top(logical_id: u32) -> Result<u64, CpuLocalError> {
    let index = usize::try_from(logical_id).map_err(|_| CpuLocalError::InvalidLogicalId)?;
    if index >= MAXIMUM_CPU_AUTHORITIES {
        return Err(CpuLocalError::InvalidLogicalId);
    }
    let stacks = core::ptr::addr_of_mut!(KERNEL_ENTRY_STACKS).cast::<KernelEntryStack>();
    // SAFETY: `index` was bounded above. No reference to the mutable static is
    // created; only its permanent address is used to derive the stack top.
    let base = unsafe { core::ptr::addr_of_mut!((*stacks.add(index)).bytes).cast::<u8>() as u64 };
    base.checked_add(KERNEL_ENTRY_STACK_BYTES as u64)
        .ok_or(CpuLocalError::InvalidEntryStack)
}

/// Registers the currently executing hardware thread before enabling its
/// syscall MSRs. Registration alone does not claim that any secondary CPU is
/// online or has descriptor tables installed.
#[cfg(target_os = "none")]
pub unsafe fn register_current_cpu(
    apic_id: u32,
    logical_id: u32,
    tss_pointer: u64,
    tss_rsp0: u64,
) -> Result<CpuRegistration, CpuLocalError> {
    let stack_top = entry_stack_top(logical_id)?;
    CPU_AUTHORITIES.register(apic_id, logical_id, stack_top, tss_pointer, tss_rsp0)
}

pub fn revoke(registration: CpuRegistration) -> Result<(), CpuLocalError> {
    CPU_AUTHORITIES.revoke(registration)
}

pub fn validate_machine_entry(
    apic_id: u32,
    tss_pointer: u64,
    tss_rsp0: u64,
) -> Result<EntryLease, CpuLocalError> {
    CPU_AUTHORITIES.validate_machine_entry(apic_id, tss_pointer, tss_rsp0)
}

pub fn prepare_return(
    apic_id: u32,
    tss_pointer: u64,
    current_tss_rsp0: u64,
    next_tss_rsp0: u64,
) -> Result<ReturnLease, CpuLocalError> {
    CPU_AUTHORITIES.prepare_return(apic_id, tss_pointer, current_tss_rsp0, next_tss_rsp0)
}

pub fn commit_return(
    lease: ReturnLease,
    tss_pointer: u64,
    committed_tss_rsp0: u64,
) -> Result<u64, CpuLocalError> {
    CPU_AUTHORITIES.commit_return(lease, tss_pointer, committed_tss_rsp0)
}

const _: () = assert!(core::mem::offset_of!(CpuLocalRecord, self_pointer) == SELF_POINTER_OFFSET);
const _: () =
    assert!(core::mem::offset_of!(CpuLocalRecord, entry_stack_top) == ENTRY_STACK_TOP_OFFSET);
const _: () =
    assert!(core::mem::offset_of!(CpuLocalRecord, user_stack_pointer) == USER_STACK_POINTER_OFFSET);
const _: () = assert!(core::mem::offset_of!(CpuLocalRecord, tss_pointer) == TSS_POINTER_OFFSET);
const _: () = assert!(core::mem::offset_of!(CpuLocalRecord, tss_rsp0) == TSS_RSP0_OFFSET);
const _: () = assert!(core::mem::offset_of!(CpuLocalRecord, generation) == GENERATION_OFFSET);
const _: () =
    assert!(core::mem::offset_of!(CpuLocalRecord, entry_generation) == ENTRY_GENERATION_OFFSET);
const _: () =
    assert!(core::mem::offset_of!(CpuLocalRecord, armed_generation) == ARMED_GENERATION_OFFSET);
const _: () = assert!(core::mem::offset_of!(CpuLocalRecord, apic_id) == APIC_ID_OFFSET);
const _: () = assert!(core::mem::offset_of!(CpuLocalRecord, logical_id) == LOGICAL_ID_OFFSET);
const _: () = assert!(core::mem::offset_of!(CpuLocalRecord, nesting) == NESTING_OFFSET);
const _: () = assert!(core::mem::offset_of!(CpuLocalRecord, state) == STATE_OFFSET);

#[cfg(test)]
mod tests {
    use super::*;

    const STACK0: u64 = 0xffff_8000_0010_0000;
    const STACK1: u64 = 0xffff_8000_0020_0000;
    const STACK2: u64 = 0xffff_8000_0028_0000;
    const TSS0: u64 = 0xffff_8000_0030_0000;
    const TSS1: u64 = 0xffff_8000_0040_0000;
    const TSS2: u64 = 0xffff_8000_0050_0000;

    #[test]
    fn registration_binds_unique_real_and_logical_identities() {
        let table = CpuAuthorityTable::<2>::new();
        let first = table.register(7, 0, STACK0, TSS0, STACK0).unwrap();
        assert_eq!(first.apic_id, 7);
        assert_eq!(first.logical_id, 0);
        assert_eq!(first.generation, 1);
        assert_eq!(
            table.register(7, 1, STACK1, TSS1, STACK1),
            Err(CpuLocalError::DuplicateApicId)
        );
        assert_eq!(
            table.register(9, 0, STACK1, TSS1, STACK1),
            Err(CpuLocalError::DuplicateLogicalId)
        );
        assert_eq!(
            table.register(9, 1, STACK0, TSS1, STACK1),
            Err(CpuLocalError::DuplicateEntryStack)
        );
        assert_eq!(
            table.register(9, 1, STACK1, TSS0, STACK1),
            Err(CpuLocalError::DuplicateTaskStateSegment)
        );
        table.register(9, 1, STACK1, TSS1, STACK1).unwrap();
        assert_eq!(
            table.register(11, 2, STACK2, TSS2, STACK2),
            Err(CpuLocalError::CapacityExceeded)
        );
    }

    #[test]
    fn entry_rejects_unknown_stale_and_nested_transitions() {
        let table = CpuAuthorityTable::<1>::new();
        let registration = table.register(0x123, 0, STACK0, TSS0, STACK0).unwrap();
        assert_eq!(
            table.begin_entry(0x124, registration.generation),
            Err(CpuLocalError::UnregisteredCpu)
        );
        assert_eq!(
            table.begin_entry(0x123, registration.generation + 1),
            Err(CpuLocalError::StaleGeneration)
        );
        let entry = table.begin_entry(0x123, registration.generation).unwrap();
        assert_eq!(entry.tss_pointer, TSS0);
        assert_eq!(
            table.begin_entry(0x123, registration.generation),
            Err(CpuLocalError::Reentered)
        );
    }

    #[test]
    fn tss_identity_and_rsp0_are_part_of_entry_authority() {
        let table = CpuAuthorityTable::<1>::new();
        let registration = table.register(3, 0, STACK0, TSS0, STACK0).unwrap();
        table.begin_entry(3, registration.generation).unwrap();
        assert_eq!(
            table.validate_machine_entry(3, TSS1, STACK0),
            Err(CpuLocalError::TaskStateSegmentMismatch)
        );
        assert_eq!(
            table.validate_machine_entry(3, TSS0, STACK1),
            Err(CpuLocalError::TaskStateStackMismatch)
        );
        assert!(table.validate_machine_entry(3, TSS0, STACK0).is_ok());
    }

    #[test]
    fn return_commit_advances_generation_only_after_exact_tss_publication() {
        let table = CpuAuthorityTable::<1>::new();
        let registration = table.register(3, 0, STACK0, TSS0, STACK0).unwrap();
        table.begin_entry(3, registration.generation).unwrap();
        let lease = table.prepare_return(3, TSS0, STACK0, STACK1).unwrap();

        assert_eq!(
            table.commit_return(lease, TSS1, STACK1),
            Err(CpuLocalError::TaskStateSegmentMismatch)
        );
        assert_eq!(
            table.commit_return(lease, TSS0, STACK0),
            Err(CpuLocalError::TaskStateStackMismatch)
        );
        assert_eq!(
            table
                .validate_machine_entry(3, TSS0, STACK0)
                .unwrap()
                .generation,
            registration.generation
        );

        let next_generation = table.commit_return(lease, TSS0, STACK1).unwrap();
        assert_eq!(next_generation, registration.generation + 1);
        assert_eq!(
            table.commit_return(lease, TSS0, STACK1),
            Err(CpuLocalError::StaleLease)
        );
        table.finish_return(3, next_generation).unwrap();
        assert_eq!(
            table.begin_entry(3, registration.generation),
            Err(CpuLocalError::StaleGeneration)
        );
        assert!(table.begin_entry(3, next_generation).is_ok());
    }

    #[test]
    fn revocation_refuses_active_entry_and_never_reuses_generation() {
        let table = CpuAuthorityTable::<1>::new();
        let first = table.register(1, 0, STACK0, TSS0, STACK0).unwrap();
        table.begin_entry(1, first.generation).unwrap();
        assert_eq!(table.revoke(first), Err(CpuLocalError::Reentered));
        let lease = table.prepare_return(1, TSS0, STACK0, STACK0).unwrap();
        let next_generation = table.commit_return(lease, TSS0, STACK0).unwrap();
        table.finish_return(1, next_generation).unwrap();
        let current = CpuRegistration {
            generation: next_generation,
            ..first
        };
        table.revoke(current).unwrap();

        let second = table.register(1, 0, STACK0, TSS0, STACK0).unwrap();
        assert!(second.generation > current.generation);
        assert_eq!(table.revoke(first), Err(CpuLocalError::StaleLease));
    }
}
