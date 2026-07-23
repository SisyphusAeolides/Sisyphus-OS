use crate::causal_lattice::{CausalClock, CausalStamp};
use crate::sync::SpinLock;

pub const CELL_PRESENT: u16 = 1 << 0;
pub const CELL_READ_ONLY: u16 = 1 << 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct StateCell {
    pub value: u64,
    pub version: u32,
    pub class: u16,
    pub flags: u16,
}

impl StateCell {
    pub const EMPTY: Self = Self {
        value: 0,
        version: 0,
        class: 0,
        flags: 0,
    };

    pub const fn is_present(self) -> bool {
        self.flags & CELL_PRESENT != 0
    }

    pub const fn is_read_only(self) -> bool {
        self.flags & CELL_READ_ONLY != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct TransactionId {
    pub slot: u16,
    pub generation: u16,
}

impl TransactionId {
    pub const INVALID: Self = Self {
        slot: u16::MAX,
        generation: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum MutationOp {
    Set = 0,
    AddSigned = 1,
    Xor = 2,
    Min = 3,
    Max = 4,
    MaskedReplace = 5,
    CompareExchange = 6,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct ReadConstraint {
    pub cell: u16,
    pub reserved: u16,
    pub expected_version: u32,
    pub value_mask: u64,
    pub expected_value: u64,
}

impl ReadConstraint {
    pub const EMPTY: Self = Self {
        cell: u16::MAX,
        reserved: 0,
        expected_version: 0,
        value_mask: 0,
        expected_value: 0,
    };

    pub const fn exact(cell: u16, expected_version: u32) -> Self {
        Self {
            cell,
            reserved: 0,
            expected_version,
            value_mask: 0,
            expected_value: 0,
        }
    }

    pub const fn masked(
        cell: u16,
        expected_version: u32,
        value_mask: u64,
        expected_value: u64,
    ) -> Self {
        Self {
            cell,
            reserved: 0,
            expected_version,
            value_mask,
            expected_value,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct Mutation {
    pub cell: u16,
    pub op: MutationOp,
    pub flags: u8,
    pub operand: u64,
    pub mask: u64,
    pub expected: u64,
}

impl Mutation {
    pub const EMPTY: Self = Self {
        cell: u16::MAX,
        op: MutationOp::Set,
        flags: 0,
        operand: 0,
        mask: 0,
        expected: 0,
    };

    pub const fn set(cell: u16, value: u64) -> Self {
        Self {
            cell,
            op: MutationOp::Set,
            flags: 0,
            operand: value,
            mask: 0,
            expected: 0,
        }
    }

    pub const fn add_signed(cell: u16, delta: i64) -> Self {
        Self {
            cell,
            op: MutationOp::AddSigned,
            flags: 0,
            operand: delta as u64,
            mask: 0,
            expected: 0,
        }
    }

    pub const fn compare_exchange(cell: u16, expected: u64, replacement: u64) -> Self {
        Self {
            cell,
            op: MutationOp::CompareExchange,
            flags: 0,
            operand: replacement,
            mask: 0,
            expected,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DraftError {
    ReadCapacity,
    WriteCapacity,
    DependencyCapacity,
    DuplicateRead,
    DuplicateWrite,
    DuplicateDependency,
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct TransactionDraft<const READS: usize, const WRITES: usize, const DEPS: usize> {
    pub kind: u16,
    pub priority: u8,
    pub required_witnesses: u8,
    pub deadline_tick: u64,
    pub salt: u64,
    reads: [ReadConstraint; READS],
    writes: [Mutation; WRITES],
    dependencies: [TransactionId; DEPS],
    read_count: u16,
    write_count: u16,
    dependency_count: u16,
    reserved: u16,
}

impl<const READS: usize, const WRITES: usize, const DEPS: usize>
    TransactionDraft<READS, WRITES, DEPS>
{
    pub const fn new(
        kind: u16,
        priority: u8,
        required_witnesses: u8,
        deadline_tick: u64,
        salt: u64,
    ) -> Self {
        Self {
            kind,
            priority,
            required_witnesses,
            deadline_tick,
            salt,
            reads: [ReadConstraint::EMPTY; READS],
            writes: [Mutation::EMPTY; WRITES],
            dependencies: [TransactionId::INVALID; DEPS],
            read_count: 0,
            write_count: 0,
            dependency_count: 0,
            reserved: 0,
        }
    }

    pub fn push_read(&mut self, constraint: ReadConstraint) -> Result<(), DraftError> {
        let count = usize::from(self.read_count);
        if count == READS {
            return Err(DraftError::ReadCapacity);
        }
        if self.reads[..count]
            .iter()
            .any(|existing| existing.cell == constraint.cell)
        {
            return Err(DraftError::DuplicateRead);
        }
        self.reads[count] = constraint;
        self.read_count = self.read_count.saturating_add(1);
        Ok(())
    }

    pub fn push_write(&mut self, mutation: Mutation) -> Result<(), DraftError> {
        let count = usize::from(self.write_count);
        if count == WRITES {
            return Err(DraftError::WriteCapacity);
        }
        if self.writes[..count]
            .iter()
            .any(|existing| existing.cell == mutation.cell)
        {
            return Err(DraftError::DuplicateWrite);
        }
        self.writes[count] = mutation;
        self.write_count = self.write_count.saturating_add(1);
        Ok(())
    }

    pub fn push_dependency(&mut self, dependency: TransactionId) -> Result<(), DraftError> {
        let count = usize::from(self.dependency_count);
        if count == DEPS {
            return Err(DraftError::DependencyCapacity);
        }
        if self.dependencies[..count].contains(&dependency) {
            return Err(DraftError::DuplicateDependency);
        }
        self.dependencies[count] = dependency;
        self.dependency_count = self.dependency_count.saturating_add(1);
        Ok(())
    }

    pub fn reads(&self) -> &[ReadConstraint] {
        &self.reads[..usize::from(self.read_count).min(READS)]
    }

    pub fn writes(&self) -> &[Mutation] {
        &self.writes[..usize::from(self.write_count).min(WRITES)]
    }

    pub fn dependencies(&self) -> &[TransactionId] {
        &self.dependencies[..usize::from(self.dependency_count).min(DEPS)]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct VectorClock<const NODES: usize> {
    lanes: [u64; NODES],
}

impl<const NODES: usize> VectorClock<NODES> {
    pub const ZERO: Self = Self { lanes: [0; NODES] };

    pub fn merge(&mut self, other: &Self) {
        let mut index = 0;
        while index < NODES {
            self.lanes[index] = self.lanes[index].max(other.lanes[index]);
            index += 1;
        }
    }

    pub fn advance(&mut self, node: usize) -> bool {
        let Some(lane) = self.lanes.get_mut(node) else {
            return false;
        };
        *lane = lane.saturating_add(1);
        true
    }

    pub fn happens_before(&self, other: &Self) -> bool {
        let mut strictly_less = false;
        let mut index = 0;
        while index < NODES {
            if self.lanes[index] > other.lanes[index] {
                return false;
            }
            strictly_less |= self.lanes[index] < other.lanes[index];
            index += 1;
        }
        strictly_less
    }

    pub fn lane(&self, node: usize) -> Option<u64> {
        self.lanes.get(node).copied()
    }

    pub fn structural_root(&self) -> u64 {
        let mut root = 0x5645_4354_4F52_0001;
        for (index, lane) in self.lanes.iter().copied().enumerate() {
            fold_word(&mut root, (index as u64) << 32 | lane.rotate_left(17));
        }
        root
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum TransactionStatus {
    Free = 0,
    Prepared = 1,
    Committed = 2,
    Aborted = 3,
    Expired = 4,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RejectReason {
    DependencyFailed,
    ReadConflict,
    MutationConflict,
    PolicyDenied,
    InvariantViolation,
    DeadlineElapsed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManifoldError {
    InvalidNode,
    InvalidQuorum,
    InvalidCell,
    CellMissing,
    CellReadOnly,
    InvalidTransaction,
    TransactionNotPrepared,
    NoCapacity,
    SlotReferenced,
    SeedClosed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct CommitCertificate {
    pub transaction: TransactionId,
    pub kind: u16,
    pub priority: u8,
    pub reserved: u8,
    pub epoch: u64,
    pub sequence: u64,
    pub stamp: CausalStamp,
    pub vector_root: u64,
    pub transaction_root: u64,
    pub state_before: u64,
    pub state_after: u64,
    pub witness_mask: u64,
}

impl CommitCertificate {
    pub const EMPTY: Self = Self {
        transaction: TransactionId::INVALID,
        kind: 0,
        priority: 0,
        reserved: 0,
        epoch: 0,
        sequence: 0,
        stamp: CausalStamp::ZERO,
        vector_root: 0,
        transaction_root: 0,
        state_before: 0,
        state_after: 0,
        witness_mask: 0,
    };
}

pub trait AxiomPolicy {
    type Fault;

    fn authorize(
        &self,
        kind: u16,
        mutation: &Mutation,
        before: &StateCell,
    ) -> Result<(), Self::Fault>;

    fn validate_state(&self, cells: &[StateCell]) -> Result<(), Self::Fault>;
}

#[derive(Debug, Eq, PartialEq)]
pub enum DriveOutcome<F> {
    Idle,
    Blocked { prepared: usize },
    Rejected {
        transaction: TransactionId,
        reason: RejectReason,
        fault: Option<F>,
    },
    Committed(CommitCertificate),
}

#[derive(Clone, Copy)]
struct TransactionSlot<const NODES: usize, const READS: usize, const WRITES: usize, const DEPS: usize> {
    generation: u16,
    status: TransactionStatus,
    reason: Option<RejectReason>,
    draft: TransactionDraft<READS, WRITES, DEPS>,
    vector: VectorClock<NODES>,
    stamp: CausalStamp,
    witness_mask: u64,
    sequence: u64,
    certificate: CommitCertificate,
}

impl<const NODES: usize, const READS: usize, const WRITES: usize, const DEPS: usize>
    TransactionSlot<NODES, READS, WRITES, DEPS>
{
    const EMPTY: Self = Self {
        generation: 1,
        status: TransactionStatus::Free,
        reason: None,
        draft: TransactionDraft::new(0, 0, 0, 0, 0),
        vector: VectorClock::ZERO,
        stamp: CausalStamp::ZERO,
        witness_mask: 0,
        sequence: 0,
        certificate: CommitCertificate::EMPTY,
    };
}

struct Inner<
    const CELLS: usize,
    const SLOTS: usize,
    const NODES: usize,
    const READS: usize,
    const WRITES: usize,
    const DEPS: usize,
> {
    cells: [StateCell; CELLS],
    slots: [TransactionSlot<NODES, READS, WRITES, DEPS>; SLOTS],
    origin_frontiers: [VectorClock<NODES>; NODES],
    state_root: u64,
    epoch: u64,
    sequence: u64,
    submitted: u64,
}

impl<
        const CELLS: usize,
        const SLOTS: usize,
        const NODES: usize,
        const READS: usize,
        const WRITES: usize,
        const DEPS: usize,
    > Inner<CELLS, SLOTS, NODES, READS, WRITES, DEPS>
{
    const fn new() -> Self {
        Self {
            cells: [StateCell::EMPTY; CELLS],
            slots: [TransactionSlot::EMPTY; SLOTS],
            origin_frontiers: [VectorClock::ZERO; NODES],
            state_root: 0,
            epoch: 0,
            sequence: 0,
            submitted: 0,
        }
    }
}

pub struct AxiomManifold<
    const CELLS: usize,
    const SLOTS: usize,
    const NODES: usize,
    const READS: usize,
    const WRITES: usize,
    const DEPS: usize,
> {
    clock: CausalClock,
    inner: SpinLock<Inner<CELLS, SLOTS, NODES, READS, WRITES, DEPS>>,
}

impl<
        const CELLS: usize,
        const SLOTS: usize,
        const NODES: usize,
        const READS: usize,
        const WRITES: usize,
        const DEPS: usize,
    > AxiomManifold<CELLS, SLOTS, NODES, READS, WRITES, DEPS>
{
    pub const fn new(clock_node: u16) -> Self {
        Self {
            clock: CausalClock::new(clock_node),
            inner: SpinLock::new(Inner::new()),
        }
    }

    pub fn seed_cell(
        &self,
        index: usize,
        value: u64,
        class: u16,
        flags: u16,
    ) -> Result<(), ManifoldError> {
        let mut inner = self.inner.lock();
        if inner.submitted != 0 {
            return Err(ManifoldError::SeedClosed);
        }
        let Some(cell) = inner.cells.get(index).copied() else {
            return Err(ManifoldError::InvalidCell);
        };

        let replacement = StateCell {
            value,
            version: 1,
            class,
            flags: flags | CELL_PRESENT,
        };
        inner.state_root ^= cell_fingerprint(index, cell) ^ cell_fingerprint(index, replacement);
        inner.cells[index] = replacement;
        Ok(())
    }

    pub fn submit(
        &self,
        origin: usize,
        wall_tick: u64,
        draft: TransactionDraft<READS, WRITES, DEPS>,
    ) -> Result<TransactionId, ManifoldError> {
        if origin >= NODES || NODES > 64 {
            return Err(ManifoldError::InvalidNode);
        }
        if usize::from(draft.required_witnesses) > NODES {
            return Err(ManifoldError::InvalidQuorum);
        }
        validate_draft_shape::<CELLS, READS, WRITES, DEPS>(&draft)?;

        let stamp = self.clock.stamp(wall_tick);
        let mut inner = self.inner.lock();

        for read in draft.reads() {
            if !inner.cells[usize::from(read.cell)].is_present() {
                return Err(ManifoldError::CellMissing);
            }
        }
        for write in draft.writes() {
            let cell = inner.cells[usize::from(write.cell)];
            if !cell.is_present() {
                return Err(ManifoldError::CellMissing);
            }
            if cell.is_read_only() {
                return Err(ManifoldError::CellReadOnly);
            }
        }

        let slot_index = inner
            .slots
            .iter()
            .position(|slot| slot.status == TransactionStatus::Free)
            .ok_or(ManifoldError::NoCapacity)?;
        if slot_index > usize::from(u16::MAX) {
            return Err(ManifoldError::NoCapacity);
        }

        let mut vector = inner.origin_frontiers[origin];
        for dependency in draft.dependencies() {
            let dependency_index = validate_id(&inner.slots, *dependency)?;
            vector.merge(&inner.slots[dependency_index].vector);
        }
        if !vector.advance(origin) {
            return Err(ManifoldError::InvalidNode);
        }

        inner.sequence = inner.sequence.saturating_add(1);
        inner.submitted = inner.submitted.saturating_add(1);
        let sequence = inner.sequence;
        let generation = inner.slots[slot_index].generation;
        let transaction = TransactionId {
            slot: slot_index as u16,
            generation,
        };

        inner.slots[slot_index] = TransactionSlot {
            generation,
            status: TransactionStatus::Prepared,
            reason: None,
            draft,
            vector,
            stamp,
            witness_mask: 0,
            sequence,
            certificate: CommitCertificate::EMPTY,
        };
        inner.origin_frontiers[origin] = vector;
        Ok(transaction)
    }

    pub fn attest(&self, transaction: TransactionId, node: usize) -> Result<bool, ManifoldError> {
        if node >= NODES || NODES > 64 {
            return Err(ManifoldError::InvalidNode);
        }
        let mut inner = self.inner.lock();
        let index = validate_id(&inner.slots, transaction)?;
        let slot = &mut inner.slots[index];
        if slot.status != TransactionStatus::Prepared {
            return Err(ManifoldError::TransactionNotPrepared);
        }
        let bit = 1_u64 << node;
        let fresh = slot.witness_mask & bit == 0;
        slot.witness_mask |= bit;
        Ok(fresh)
    }

    pub fn drive<P: AxiomPolicy>(&self, now_tick: u64, policy: &P) -> DriveOutcome<P::Fault> {
        let mut inner = self.inner.lock();

        if let Some((index, reason)) = terminalize_one_dependency_or_deadline(&mut inner, now_tick) {
            let transaction = id_for(index, inner.slots[index].generation);
            return DriveOutcome::Rejected {
                transaction,
                reason,
                fault: None,
            };
        }

        let prepared = inner
            .slots
            .iter()
            .filter(|slot| slot.status == TransactionStatus::Prepared)
            .count();
        if prepared == 0 {
            return DriveOutcome::Idle;
        }

        let Some(index) = select_candidate(&inner) else {
            return DriveOutcome::Blocked { prepared };
        };

        let transaction = id_for(index, inner.slots[index].generation);
        let draft = inner.slots[index].draft;
        let vector = inner.slots[index].vector;
        let stamp = inner.slots[index].stamp;
        let witness_mask = inner.slots[index].witness_mask;
        let sequence = inner.slots[index].sequence;

        if !reads_match(&inner.cells, draft.reads()) {
            reject_slot(&mut inner.slots[index], RejectReason::ReadConflict);
            return DriveOutcome::Rejected {
                transaction,
                reason: RejectReason::ReadConflict,
                fault: None,
            };
        }

        let mut projected_values = [0_u64; WRITES];
        for (write_index, mutation) in draft.writes().iter().enumerate() {
            let cell_index = usize::from(mutation.cell);
            let before = inner.cells[cell_index];
            if let Err(fault) = policy.authorize(draft.kind, mutation, &before) {
                reject_slot(&mut inner.slots[index], RejectReason::PolicyDenied);
                return DriveOutcome::Rejected {
                    transaction,
                    reason: RejectReason::PolicyDenied,
                    fault: Some(fault),
                };
            }
            let Some(value) = project_value(before.value, *mutation) else {
                reject_slot(&mut inner.slots[index], RejectReason::MutationConflict);
                return DriveOutcome::Rejected {
                    transaction,
                    reason: RejectReason::MutationConflict,
                    fault: None,
                };
            };
            projected_values[write_index] = value;
        }

        let state_before = inner.state_root;
        let mut undo = [UndoEntry::EMPTY; WRITES];
        for (write_index, mutation) in draft.writes().iter().enumerate() {
            let cell_index = usize::from(mutation.cell);
            let before = inner.cells[cell_index];
            let after = StateCell {
                value: projected_values[write_index],
                version: before.version.saturating_add(1),
                class: before.class,
                flags: before.flags,
            };
            undo[write_index] = UndoEntry {
                cell: mutation.cell,
                before,
                active: true,
            };
            inner.state_root ^=
                cell_fingerprint(cell_index, before) ^ cell_fingerprint(cell_index, after);
            inner.cells[cell_index] = after;
        }

        if let Err(fault) = policy.validate_state(&inner.cells) {
            rollback(&mut inner.cells, &undo);
            inner.state_root = state_before;
            reject_slot(&mut inner.slots[index], RejectReason::InvariantViolation);
            return DriveOutcome::Rejected {
                transaction,
                reason: RejectReason::InvariantViolation,
                fault: Some(fault),
            };
        }

        inner.epoch = inner.epoch.saturating_add(1);
        let certificate = CommitCertificate {
            transaction,
            kind: draft.kind,
            priority: draft.priority,
            reserved: 0,
            epoch: inner.epoch,
            sequence,
            stamp,
            vector_root: vector.structural_root(),
            transaction_root: draft_root(&draft),
            state_before,
            state_after: inner.state_root,
            witness_mask,
        };
        let slot = &mut inner.slots[index];
        slot.status = TransactionStatus::Committed;
        slot.reason = None;
        slot.certificate = certificate;
        DriveOutcome::Committed(certificate)
    }

    pub fn reap(&self, transaction: TransactionId) -> Result<(), ManifoldError> {
        let mut inner = self.inner.lock();
        let index = validate_id(&inner.slots, transaction)?;
        if matches!(
            inner.slots[index].status,
            TransactionStatus::Free | TransactionStatus::Prepared
        ) {
            return Err(ManifoldError::TransactionNotPrepared);
        }
        if inner.slots.iter().any(|slot| {
            slot.status == TransactionStatus::Prepared
                && slot.draft.dependencies().contains(&transaction)
        }) {
            return Err(ManifoldError::SlotReferenced);
        }

        let generation = next_generation(inner.slots[index].generation);
        inner.slots[index] = TransactionSlot {
            generation,
            ..TransactionSlot::EMPTY
        };
        Ok(())
    }

    pub fn cell(&self, index: usize) -> Option<StateCell> {
        self.inner.lock().cells.get(index).copied()
    }

    pub fn state_root(&self) -> u64 {
        self.inner.lock().state_root
    }

    pub fn status(
        &self,
        transaction: TransactionId,
    ) -> Result<(TransactionStatus, Option<RejectReason>), ManifoldError> {
        let inner = self.inner.lock();
        let index = validate_id(&inner.slots, transaction)?;
        let slot = inner.slots[index];
        Ok((slot.status, slot.reason))
    }

    pub fn certificate(
        &self,
        transaction: TransactionId,
    ) -> Result<Option<CommitCertificate>, ManifoldError> {
        let inner = self.inner.lock();
        let index = validate_id(&inner.slots, transaction)?;
        let slot = inner.slots[index];
        Ok((slot.status == TransactionStatus::Committed).then_some(slot.certificate))
    }
}

#[derive(Clone, Copy)]
struct UndoEntry {
    cell: u16,
    before: StateCell,
    active: bool,
}

impl UndoEntry {
    const EMPTY: Self = Self {
        cell: u16::MAX,
        before: StateCell::EMPTY,
        active: false,
    };
}

fn validate_draft_shape<
    const CELLS: usize,
    const READS: usize,
    const WRITES: usize,
    const DEPS: usize,
>(
    draft: &TransactionDraft<READS, WRITES, DEPS>,
) -> Result<(), ManifoldError> {
    for read in draft.reads() {
        if usize::from(read.cell) >= CELLS {
            return Err(ManifoldError::InvalidCell);
        }
    }
    for write in draft.writes() {
        if usize::from(write.cell) >= CELLS {
            return Err(ManifoldError::InvalidCell);
        }
    }
    Ok(())
}

fn validate_id<const NODES: usize, const READS: usize, const WRITES: usize, const DEPS: usize>(
    slots: &[TransactionSlot<NODES, READS, WRITES, DEPS>],
    transaction: TransactionId,
) -> Result<usize, ManifoldError> {
    let index = usize::from(transaction.slot);
    let Some(slot) = slots.get(index) else {
        return Err(ManifoldError::InvalidTransaction);
    };
    if slot.generation != transaction.generation || slot.status == TransactionStatus::Free {
        return Err(ManifoldError::InvalidTransaction);
    }
    Ok(index)
}

fn id_for(index: usize, generation: u16) -> TransactionId {
    TransactionId {
        slot: index as u16,
        generation,
    }
}

fn next_generation(current: u16) -> u16 {
    let next = current.wrapping_add(1);
    if next == 0 { 1 } else { next }
}

fn reject_slot<const NODES: usize, const READS: usize, const WRITES: usize, const DEPS: usize>(
    slot: &mut TransactionSlot<NODES, READS, WRITES, DEPS>,
    reason: RejectReason,
) {
    slot.status = if reason == RejectReason::DeadlineElapsed {
        TransactionStatus::Expired
    } else {
        TransactionStatus::Aborted
    };
    slot.reason = Some(reason);
}

fn terminalize_one_dependency_or_deadline<
    const CELLS: usize,
    const SLOTS: usize,
    const NODES: usize,
    const READS: usize,
    const WRITES: usize,
    const DEPS: usize,
>(
    inner: &mut Inner<CELLS, SLOTS, NODES, READS, WRITES, DEPS>,
    now_tick: u64,
) -> Option<(usize, RejectReason)> {
    for index in 0..SLOTS {
        if inner.slots[index].status != TransactionStatus::Prepared {
            continue;
        }
        let draft = inner.slots[index].draft;
        if draft.deadline_tick != 0 && now_tick > draft.deadline_tick {
            reject_slot(&mut inner.slots[index], RejectReason::DeadlineElapsed);
            return Some((index, RejectReason::DeadlineElapsed));
        }
        let dependency_failed = draft.dependencies().iter().any(|dependency| {
            let dependency_index = usize::from(dependency.slot);
            let Some(slot) = inner.slots.get(dependency_index) else {
                return true;
            };
            slot.generation != dependency.generation
                || matches!(
                    slot.status,
                    TransactionStatus::Aborted | TransactionStatus::Expired | TransactionStatus::Free
                )
        });
        if dependency_failed {
            reject_slot(&mut inner.slots[index], RejectReason::DependencyFailed);
            return Some((index, RejectReason::DependencyFailed));
        }
    }
    None
}

fn select_candidate<
    const CELLS: usize,
    const SLOTS: usize,
    const NODES: usize,
    const READS: usize,
    const WRITES: usize,
    const DEPS: usize,
>(
    inner: &Inner<CELLS, SLOTS, NODES, READS, WRITES, DEPS>,
) -> Option<usize> {
    let mut best: Option<usize> = None;
    for index in 0..SLOTS {
        let slot = &inner.slots[index];
        if slot.status != TransactionStatus::Prepared {
            continue;
        }
        if slot.witness_mask.count_ones() < u32::from(slot.draft.required_witnesses) {
            continue;
        }
        let dependencies_committed = slot.draft.dependencies().iter().all(|dependency| {
            let dependency_index = usize::from(dependency.slot);
            inner
                .slots
                .get(dependency_index)
                .is_some_and(|candidate| {
                    candidate.generation == dependency.generation
                        && candidate.status == TransactionStatus::Committed
                })
        });
        if !dependencies_committed {
            continue;
        }

        match best {
            None => best = Some(index),
            Some(previous) if candidate_precedes(slot, &inner.slots[previous]) => best = Some(index),
            Some(_) => {}
        }
    }
    best
}

fn candidate_precedes<const NODES: usize, const READS: usize, const WRITES: usize, const DEPS: usize>(
    left: &TransactionSlot<NODES, READS, WRITES, DEPS>,
    right: &TransactionSlot<NODES, READS, WRITES, DEPS>,
) -> bool {
    let left_deadline = if left.draft.deadline_tick == 0 {
        u64::MAX
    } else {
        left.draft.deadline_tick
    };
    let right_deadline = if right.draft.deadline_tick == 0 {
        u64::MAX
    } else {
        right.draft.deadline_tick
    };

    (left_deadline, u8::MAX - left.draft.priority, left.sequence)
        < (right_deadline, u8::MAX - right.draft.priority, right.sequence)
}

fn reads_match(cells: &[StateCell], reads: &[ReadConstraint]) -> bool {
    reads.iter().all(|read| {
        let Some(cell) = cells.get(usize::from(read.cell)).copied() else {
            return false;
        };
        cell.is_present()
            && cell.version == read.expected_version
            && (read.value_mask == 0
                || (cell.value & read.value_mask) == (read.expected_value & read.value_mask))
    })
}

fn project_value(before: u64, mutation: Mutation) -> Option<u64> {
    match mutation.op {
        MutationOp::Set => Some(mutation.operand),
        MutationOp::AddSigned => {
            let delta = mutation.operand as i64;
            if delta >= 0 {
                Some(before.saturating_add(delta as u64))
            } else {
                Some(before.saturating_sub(delta.unsigned_abs()))
            }
        }
        MutationOp::Xor => Some(before ^ mutation.operand),
        MutationOp::Min => Some(before.min(mutation.operand)),
        MutationOp::Max => Some(before.max(mutation.operand)),
        MutationOp::MaskedReplace => {
            Some((before & !mutation.mask) | (mutation.operand & mutation.mask))
        }
        MutationOp::CompareExchange => {
            (before == mutation.expected).then_some(mutation.operand)
        }
    }
}

fn rollback(cells: &mut [StateCell], undo: &[UndoEntry]) {
    for entry in undo.iter().rev().copied() {
        if entry.active {
            cells[usize::from(entry.cell)] = entry.before;
        }
    }
}

fn draft_root<const READS: usize, const WRITES: usize, const DEPS: usize>(
    draft: &TransactionDraft<READS, WRITES, DEPS>,
) -> u64 {
    let mut root = 0x4158_494F_4D00_0001;
    fold_word(&mut root, u64::from(draft.kind));
    fold_word(&mut root, u64::from(draft.priority));
    fold_word(&mut root, u64::from(draft.required_witnesses));
    fold_word(&mut root, draft.deadline_tick);
    fold_word(&mut root, draft.salt);

    for read in draft.reads() {
        fold_word(
            &mut root,
            u64::from(read.cell) | (u64::from(read.expected_version) << 16),
        );
        fold_word(&mut root, read.value_mask);
        fold_word(&mut root, read.expected_value);
    }
    for write in draft.writes() {
        fold_word(
            &mut root,
            u64::from(write.cell) | ((write.op as u64) << 16) | (u64::from(write.flags) << 24),
        );
        fold_word(&mut root, write.operand);
        fold_word(&mut root, write.mask);
        fold_word(&mut root, write.expected);
    }
    for dependency in draft.dependencies() {
        fold_word(
            &mut root,
            u64::from(dependency.slot) | (u64::from(dependency.generation) << 16),
        );
    }
    root
}

fn cell_fingerprint(index: usize, cell: StateCell) -> u64 {
    if !cell.is_present() {
        return 0;
    }
    let mut root = avalanche((index as u64) ^ 0x4345_4C4C_0000_0001);
    fold_word(&mut root, cell.value);
    fold_word(
        &mut root,
        u64::from(cell.version)
            | (u64::from(cell.class) << 32)
            | (u64::from(cell.flags) << 48),
    );
    root
}

fn fold_word(root: &mut u64, word: u64) {
    *root = avalanche(root.wrapping_add(avalanche(word ^ 0x9E37_79B9_7F4A_7C15)));
}

fn avalanche(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Fault {
        WrongClass,
        ConservationBroken,
    }

    struct ConservationPolicy;

    impl AxiomPolicy for ConservationPolicy {
        type Fault = Fault;

        fn authorize(
            &self,
            kind: u16,
            _mutation: &Mutation,
            before: &StateCell,
        ) -> Result<(), Self::Fault> {
            if kind == before.class {
                Ok(())
            } else {
                Err(Fault::WrongClass)
            }
        }

        fn validate_state(&self, cells: &[StateCell]) -> Result<(), Self::Fault> {
            if cells[0].value.saturating_add(cells[1].value) == 100 {
                Ok(())
            } else {
                Err(Fault::ConservationBroken)
            }
        }
    }

    type TestManifold = AxiomManifold<8, 16, 4, 8, 8, 4>;
    type TestDraft = TransactionDraft<8, 8, 4>;

    fn seeded() -> TestManifold {
        let manifold = TestManifold::new(0);
        manifold.seed_cell(0, 60, 7, 0).unwrap();
        manifold.seed_cell(1, 40, 7, 0).unwrap();
        manifold
    }

    fn transfer(
        from_version: u32,
        to_version: u32,
        delta: i64,
        required_witnesses: u8,
        salt: u64,
    ) -> TestDraft {
        let mut draft = TestDraft::new(7, 200, required_witnesses, 100, salt);
        draft
            .push_read(ReadConstraint::exact(0, from_version))
            .unwrap();
        draft
            .push_read(ReadConstraint::exact(1, to_version))
            .unwrap();
        draft.push_write(Mutation::add_signed(0, -delta)).unwrap();
        draft.push_write(Mutation::add_signed(1, delta)).unwrap();
        draft
    }

    #[test]
    fn quorum_guarded_transfer_commits() {
        let manifold = seeded();
        let transaction = manifold.submit(0, 1, transfer(1, 1, 10, 2, 0xA11CE)).unwrap();

        assert_eq!(manifold.drive(2, &ConservationPolicy), DriveOutcome::Blocked { prepared: 1 });
        assert!(manifold.attest(transaction, 0).unwrap());
        assert!(manifold.attest(transaction, 2).unwrap());
        assert!(!manifold.attest(transaction, 2).unwrap());

        let DriveOutcome::Committed(certificate) = manifold.drive(3, &ConservationPolicy) else {
            panic!("transaction did not commit");
        };

        assert_eq!(certificate.transaction, transaction);
        assert_eq!(certificate.witness_mask, 0b0101);
        assert_ne!(certificate.state_before, certificate.state_after);
        assert_eq!(manifold.cell(0).unwrap().value, 50);
        assert_eq!(manifold.cell(1).unwrap().value, 50);
    }

    #[test]
    fn invariant_failure_restores_exact_pre_state() {
        let manifold = seeded();
        let root_before = manifold.state_root();
        let mut draft = TestDraft::new(7, 255, 1, 100, 0xBAD);
        draft.push_write(Mutation::set(0, 0)).unwrap();
        let transaction = manifold.submit(0, 1, draft).unwrap();
        manifold.attest(transaction, 0).unwrap();

        assert_eq!(
            manifold.drive(2, &ConservationPolicy),
            DriveOutcome::Rejected {
                transaction,
                reason: RejectReason::InvariantViolation,
                fault: Some(Fault::ConservationBroken),
            }
        );
        assert_eq!(manifold.cell(0).unwrap().value, 60);
        assert_eq!(manifold.cell(0).unwrap().version, 1);
        assert_eq!(manifold.state_root(), root_before);
    }

    #[test]
    fn dependencies_form_a_deterministic_causal_chain() {
        let manifold = seeded();
        let first = manifold.submit(0, 1, transfer(1, 1, 10, 1, 1)).unwrap();
        manifold.attest(first, 0).unwrap();

        let mut second_draft = transfer(2, 2, -5, 1, 2);
        second_draft.push_dependency(first).unwrap();
        let second = manifold.submit(1, 2, second_draft).unwrap();
        manifold.attest(second, 1).unwrap();

        assert!(matches!(
            manifold.drive(3, &ConservationPolicy),
            DriveOutcome::Committed(certificate) if certificate.transaction == first
        ));
        assert!(matches!(
            manifold.drive(4, &ConservationPolicy),
            DriveOutcome::Committed(certificate) if certificate.transaction == second
        ));
        assert_eq!(manifold.cell(0).unwrap().value, 55);
        assert_eq!(manifold.cell(1).unwrap().value, 45);
    }

    #[test]
    fn stale_snapshot_is_aborted_after_competing_commit() {
        let manifold = seeded();
        let first = manifold.submit(0, 1, transfer(1, 1, 10, 1, 11)).unwrap();
        let second = manifold.submit(1, 1, transfer(1, 1, 20, 1, 12)).unwrap();
        manifold.attest(first, 0).unwrap();
        manifold.attest(second, 1).unwrap();

        assert!(matches!(
            manifold.drive(2, &ConservationPolicy),
            DriveOutcome::Committed(certificate) if certificate.transaction == first
        ));
        assert_eq!(
            manifold.drive(3, &ConservationPolicy),
            DriveOutcome::Rejected {
                transaction: second,
                reason: RejectReason::ReadConflict,
                fault: None,
            }
        );
    }
}
