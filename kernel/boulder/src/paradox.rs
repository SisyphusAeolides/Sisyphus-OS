use crate::capability::{Capability, PolicyControl};
use crate::sync::SpinLock;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct TimelineId(u32);

impl TimelineId {
    pub const INVALID: Self = Self(0);

    pub const fn generation(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParadoxError {
    TimelineAlreadyOpen,
    NoOpenTimeline,
    StaleTimeline,
    CellOutOfRange,
    JournalFull,
}

#[derive(Clone, Copy)]
struct JournalEntry {
    cell: u16,
    before: u64,
    after: u64,
}

impl JournalEntry {
    const EMPTY: Self = Self {
        cell: 0,
        before: 0,
        after: 0,
    };
}

struct ParadoxState<const CELLS: usize, const LOG: usize> {
    cells: [u64; CELLS],
    journal: [JournalEntry; LOG],
    journal_len: usize,
    active: Option<TimelineId>,
    next_generation: u32,
    committed: u64,
    rolled_back: u64,
}

impl<const CELLS: usize, const LOG: usize> ParadoxState<CELLS, LOG> {
    const fn new(initial: [u64; CELLS]) -> Self {
        Self {
            cells: initial,
            journal: [JournalEntry::EMPTY; LOG],
            journal_len: 0,
            active: None,
            next_generation: 1,
            committed: 0,
            rolled_back: 0,
        }
    }

    fn require(&self, timeline: TimelineId) -> Result<(), ParadoxError> {
        match self.active {
            Some(active) if active == timeline => Ok(()),
            Some(_) => Err(ParadoxError::StaleTimeline),
            None => Err(ParadoxError::NoOpenTimeline),
        }
    }

    fn clear_journal(&mut self) {
        for entry in &mut self.journal[..self.journal_len] {
            *entry = JournalEntry::EMPTY;
        }
        self.journal_len = 0;
    }
}

pub struct ParadoxEngine<const CELLS: usize, const LOG: usize> {
    state: SpinLock<ParadoxState<CELLS, LOG>>,
}

impl<const CELLS: usize, const LOG: usize> ParadoxEngine<CELLS, LOG> {
    pub const fn new(initial: [u64; CELLS]) -> Self {
        Self {
            state: SpinLock::new(ParadoxState::new(initial)),
        }
    }

    pub fn begin(
        &self,
        _authority: &Capability<'_, PolicyControl>,
    ) -> Result<TimelineId, ParadoxError> {
        let mut state = self.state.lock();

        if state.active.is_some() {
            return Err(ParadoxError::TimelineAlreadyOpen);
        }

        let generation = state.next_generation.max(1);
        state.next_generation = state.next_generation.wrapping_add(1).max(1);

        let timeline = TimelineId(generation);
        state.active = Some(timeline);
        state.clear_journal();

        Ok(timeline)
    }

    pub fn read(&self, cell: usize) -> Result<u64, ParadoxError> {
        self.state
            .lock()
            .cells
            .get(cell)
            .copied()
            .ok_or(ParadoxError::CellOutOfRange)
    }

    pub fn write(
        &self,
        timeline: TimelineId,
        cell: usize,
        value: u64,
        _authority: &Capability<'_, PolicyControl>,
    ) -> Result<(), ParadoxError> {
        let mut state = self.state.lock();
        state.require(timeline)?;

        if cell >= CELLS {
            return Err(ParadoxError::CellOutOfRange);
        }

        if state.journal_len == LOG {
            return Err(ParadoxError::JournalFull);
        }

        let before = state.cells[cell];
        let journal_index = state.journal_len;

        state.journal[journal_index] = JournalEntry {
            cell: cell as u16,
            before,
            after: value,
        };
        state.journal_len += 1;
        state.cells[cell] = value;

        Ok(())
    }

    pub fn commit(
        &self,
        timeline: TimelineId,
        _authority: &Capability<'_, PolicyControl>,
    ) -> Result<u64, ParadoxError> {
        let mut state = self.state.lock();
        state.require(timeline)?;

        state.active = None;
        state.committed = state.committed.saturating_add(1);
        state.clear_journal();

        Ok(state_digest(&state.cells))
    }

    pub fn rollback(
        &self,
        timeline: TimelineId,
        _authority: &Capability<'_, PolicyControl>,
    ) -> Result<u64, ParadoxError> {
        let mut state = self.state.lock();
        state.require(timeline)?;

        while state.journal_len != 0 {
            state.journal_len -= 1;
            let index = state.journal_len;
            let entry = state.journal[index];

            state.cells[usize::from(entry.cell)] = entry.before;
            state.journal[index] = JournalEntry::EMPTY;
        }

        state.active = None;
        state.rolled_back = state.rolled_back.saturating_add(1);

        Ok(state_digest(&state.cells))
    }

    pub fn mutation_digest(&self) -> u64 {
        let state = self.state.lock();
        let mut digest = state_digest(&state.cells);

        for entry in &state.journal[..state.journal_len] {
            digest = mix(digest, u64::from(entry.cell));
            digest = mix(digest, entry.before);
            digest = mix(digest, entry.after);
        }

        digest
    }

    pub fn totals(&self) -> (u64, u64) {
        let state = self.state.lock();
        (state.committed, state.rolled_back)
    }
}

fn state_digest(cells: &[u64]) -> u64 {
    cells.iter().copied().fold(0xcbf2_9ce4_8422_2325, mix)
}

fn mix(mut state: u64, word: u64) -> u64 {
    state ^= word;
    state = state.wrapping_mul(0x0000_0100_0000_01b3);
    state ^= state >> 29;
    state
}
