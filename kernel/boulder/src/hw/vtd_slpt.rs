//! Intel VT-d legacy second-level page-table construction.
//!
//! This module owns only page-table structure and mapping evidence. It does
//! not enable a remapping engine or claim that DMA isolation is active.

const PAGE_SHIFT: u8 = 12;
const PAGE_SIZE: u64 = 1 << PAGE_SHIFT;
const ENTRIES_PER_TABLE: usize = 512;
const MAXIMUM_LEVELS: usize = 5;
const PHYSICAL_PAGE_MASK: u64 = 0x000f_ffff_ffff_f000;
const READ: u64 = 1;
const WRITE: u64 = 1 << 1;
const PERMISSION_MASK: u64 = READ | WRITE;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SlptMemoryError(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SlptInvalidationError(pub u64);

/// Page storage used by the SLPT core.
///
/// A failing operation must leave ownership and contents unchanged. In
/// particular, `release_table` returning an error means the caller still owns
/// the frame and may retry. Allocated frames must be exclusive to this core.
/// Successful zero/write operations must provide the release ordering needed
/// before a VT-d page walker can observe a subsequently published parent/leaf.
pub trait SlptPageMemory {
    fn allocate_table(&mut self) -> Result<SlptFrame, SlptMemoryError>;
    fn zero_table(&mut self, frame: SlptFrame) -> Result<(), SlptMemoryError>;
    fn read_entry(&self, frame: SlptFrame, index: usize) -> Result<u64, SlptMemoryError>;
    fn write_entry(
        &mut self,
        frame: SlptFrame,
        index: usize,
        value: u64,
    ) -> Result<(), SlptMemoryError>;
    fn release_table(&mut self, frame: SlptFrame) -> Result<(), SlptMemoryError>;
}

/// Domain-scoped invalidation supplied by the remapping-engine integration.
/// A global invalidation is a valid implementation when page-selective
/// invalidation is unavailable.
pub trait SlptInvalidator {
    fn invalidate_after_page_table_change(
        &mut self,
        iova: u64,
    ) -> Result<(), SlptInvalidationError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SlptFrame(u64);

impl SlptFrame {
    pub const fn from_physical_address(address: u64) -> Option<Self> {
        if address != 0 && address & (PAGE_SIZE - 1) == 0 && address & !PHYSICAL_PAGE_MASK == 0 {
            Some(Self(address))
        } else {
            None
        }
    }

    pub const fn physical_address(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SlptConfig {
    pub supported_adjusted_guest_widths: u8,
    pub maximum_guest_address_width: u8,
    pub address_width_encoding: u8,
}

impl SlptConfig {
    pub const fn adjusted_guest_address_width(self) -> Option<u8> {
        match self.address_width_encoding {
            0 => Some(30),
            1 => Some(39),
            2 => Some(48),
            3 => Some(57),
            _ => None,
        }
    }

    const fn validate(self) -> Result<u8, SlptFault> {
        let Some(width) = self.adjusted_guest_address_width() else {
            return Err(SlptFault::InvalidAddressWidth);
        };
        if self.supported_adjusted_guest_widths & (1 << self.address_width_encoding) == 0
            || self.maximum_guest_address_width < 30
            || self.maximum_guest_address_width > 64
            || width > self.maximum_guest_address_width
        {
            return Err(SlptFault::UnsupportedAddressWidth);
        }
        Ok(width)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmaPermissions {
    pub read: bool,
    pub write: bool,
}

impl DmaPermissions {
    pub const READ_ONLY: Self = Self {
        read: true,
        write: false,
    };
    pub const WRITE_ONLY: Self = Self {
        read: false,
        write: true,
    };
    pub const READ_WRITE: Self = Self {
        read: true,
        write: true,
    };

    const fn bits(self) -> Result<u64, SlptFault> {
        let bits = (if self.read { READ } else { 0 }) | (if self.write { WRITE } else { 0 });
        if bits == 0 {
            Err(SlptFault::InvalidPermissions)
        } else {
            Ok(bits)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MappingPhase {
    AwaitingMapInvalidation,
    Active,
    AwaitingUnmapInvalidation,
    AwaitingCleanup,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlptState {
    Operational,
    Poisoned,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlptFault {
    InvalidAddressWidth,
    UnsupportedAddressWidth,
    InvalidRoot,
    RootNotEmpty,
    InvalidIova,
    InvalidPhysicalAddress,
    InvalidPermissions,
    CapacityExhausted,
    IovaOverlap,
    PhysicalAlias,
    ForeignEntry,
    MalformedEntry,
    FrameAlias,
    UnknownMapping,
    InvalidPhase,
    StaleReceipt,
    EntryMismatch,
    Memory(SlptMemoryError),
    RollbackFailed,
    Poisoned,
}

impl From<SlptMemoryError> for SlptFault {
    fn from(error: SlptMemoryError) -> Self {
        Self::Memory(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MappingHandle {
    slot: u16,
    generation: u32,
}

impl MappingHandle {
    pub const fn slot(self) -> u16 {
        self.slot
    }

    pub const fn generation(self) -> u32 {
        self.generation
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MapReceipt(MappingHandle);

impl MapReceipt {
    pub const fn handle(self) -> MappingHandle {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnmapReceipt(MappingHandle);

impl UnmapReceipt {
    pub const fn handle(self) -> MappingHandle {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingCause {
    Invalidation(SlptInvalidationError),
    Cleanup(SlptMemoryError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MapOutcome {
    Active(MappingHandle),
    Pending {
        receipt: MapReceipt,
        cause: PendingCause,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnmapOutcome {
    Complete,
    Pending {
        receipt: UnmapReceipt,
        cause: PendingCause,
    },
}

#[derive(Clone, Copy)]
struct PagePath {
    frames: [SlptFrame; MAXIMUM_LEVELS],
    indices: [u16; MAXIMUM_LEVELS],
    depth: u8,
}

impl PagePath {
    const EMPTY_FRAME: SlptFrame = SlptFrame(0);

    const fn empty(depth: u8) -> Self {
        Self {
            frames: [Self::EMPTY_FRAME; MAXIMUM_LEVELS],
            indices: [0; MAXIMUM_LEVELS],
            depth,
        }
    }
}

#[derive(Clone, Copy)]
struct MappingRecord {
    iova: u64,
    physical_address: u64,
    leaf: u64,
    path: PagePath,
    cleanup_depth: u8,
    phase: MappingPhase,
}

#[derive(Clone, Copy)]
struct MappingSlot {
    generation: u32,
    record: Option<MappingRecord>,
    retired: bool,
}

impl MappingSlot {
    const EMPTY: Self = Self {
        generation: 1,
        record: None,
        retired: false,
    };
}

#[derive(Clone, Copy)]
struct NewLink {
    parent: SlptFrame,
    index: u16,
    child: SlptFrame,
}

impl NewLink {
    const EMPTY: Self = Self {
        parent: SlptFrame(0),
        index: 0,
        child: SlptFrame(0),
    };
}

/// A fixed-capacity ownership ledger and legacy VT-d SLPT root.
///
/// The root frame remains caller-owned. Intermediate frames allocated through
/// `SlptPageMemory` are reclaimed once their last mapping is removed.
pub struct Slpt<const MAPPINGS: usize> {
    root: SlptFrame,
    adjusted_width: u8,
    levels: u8,
    slots: [MappingSlot; MAPPINGS],
    state: SlptState,
}

impl<const MAPPINGS: usize> Slpt<MAPPINGS> {
    pub fn attach(
        memory: &impl SlptPageMemory,
        root: SlptFrame,
        config: SlptConfig,
    ) -> Result<Self, SlptFault> {
        if MAPPINGS == 0 || MAPPINGS > u16::MAX as usize || root.0 == 0 {
            return Err(SlptFault::InvalidRoot);
        }
        let adjusted_width = config.validate()?;
        for index in 0..ENTRIES_PER_TABLE {
            if memory.read_entry(root, index)? != 0 {
                return Err(SlptFault::RootNotEmpty);
            }
        }
        Ok(Self {
            root,
            adjusted_width,
            levels: (adjusted_width - PAGE_SHIFT).div_ceil(9),
            slots: [MappingSlot::EMPTY; MAPPINGS],
            state: SlptState::Operational,
        })
    }

    pub const fn root(&self) -> SlptFrame {
        self.root
    }

    pub const fn adjusted_guest_address_width(&self) -> u8 {
        self.adjusted_width
    }

    pub const fn address_width_encoding(&self) -> u8 {
        self.levels - 2
    }

    pub const fn state(&self) -> SlptState {
        self.state
    }

    pub fn phase(&self, handle: MappingHandle) -> Result<MappingPhase, SlptFault> {
        Ok(self.record(handle)?.phase)
    }

    pub fn map_page(
        &mut self,
        memory: &mut impl SlptPageMemory,
        invalidator: &mut impl SlptInvalidator,
        iova: u64,
        physical_address: u64,
        permissions: DmaPermissions,
    ) -> Result<MapOutcome, SlptFault> {
        self.ensure_operational()?;
        self.validate_iova(iova)?;
        validate_mapped_physical_address(physical_address)?;
        let permission_bits = permissions.bits()?;
        for slot in &self.slots {
            if let Some(record) = slot.record {
                if record.iova == iova {
                    return Err(SlptFault::IovaOverlap);
                }
                if record.physical_address == physical_address {
                    return Err(SlptFault::PhysicalAlias);
                }
            }
        }
        let slot_index = self
            .slots
            .iter()
            .position(|slot| slot.record.is_none() && !slot.retired)
            .ok_or(SlptFault::CapacityExhausted)?;

        let mut path = PagePath::empty(self.levels);
        path.frames[0] = self.root;
        let mut new_links = [NewLink::EMPTY; MAXIMUM_LEVELS - 1];
        let mut new_link_count = 0usize;

        let path_result =
            self.build_path(memory, iova, &mut path, &mut new_links, &mut new_link_count);
        if let Err(error) = path_result {
            if self
                .rollback_new_links(memory, &new_links, new_link_count)
                .is_err()
            {
                self.state = SlptState::Poisoned;
                return Err(SlptFault::RollbackFailed);
            }
            if error == SlptFault::RollbackFailed {
                self.state = SlptState::Poisoned;
            }
            return Err(error);
        }

        let leaf_position = usize::from(self.levels - 1);
        let leaf_table = path.frames[leaf_position];
        let leaf_index = path.indices[leaf_position] as usize;
        let existing_leaf = match memory.read_entry(leaf_table, leaf_index) {
            Ok(entry) => entry,
            Err(error) => {
                if self
                    .rollback_new_links(memory, &new_links, new_link_count)
                    .is_err()
                {
                    self.state = SlptState::Poisoned;
                    return Err(SlptFault::RollbackFailed);
                }
                return Err(error.into());
            }
        };
        if existing_leaf != 0 {
            if self
                .rollback_new_links(memory, &new_links, new_link_count)
                .is_err()
            {
                self.state = SlptState::Poisoned;
                return Err(SlptFault::RollbackFailed);
            }
            self.state = SlptState::Poisoned;
            return Err(SlptFault::ForeignEntry);
        }

        let leaf = physical_address | permission_bits;
        if let Err(error) = memory.write_entry(leaf_table, leaf_index, leaf) {
            if self
                .rollback_new_links(memory, &new_links, new_link_count)
                .is_err()
            {
                self.state = SlptState::Poisoned;
                return Err(SlptFault::RollbackFailed);
            }
            return Err(error.into());
        }

        let generation = self.slots[slot_index].generation;
        let handle = MappingHandle {
            slot: slot_index as u16,
            generation,
        };
        self.slots[slot_index].record = Some(MappingRecord {
            iova,
            physical_address,
            leaf,
            path,
            cleanup_depth: 0,
            phase: MappingPhase::AwaitingMapInvalidation,
        });

        match invalidator.invalidate_after_page_table_change(iova) {
            Ok(()) => {
                self.slots[slot_index].record.as_mut().unwrap().phase = MappingPhase::Active;
                Ok(MapOutcome::Active(handle))
            }
            Err(error) => Ok(MapOutcome::Pending {
                receipt: MapReceipt(handle),
                cause: PendingCause::Invalidation(error),
            }),
        }
    }

    pub fn finish_map(
        &mut self,
        invalidator: &mut impl SlptInvalidator,
        receipt: MapReceipt,
    ) -> Result<MapOutcome, SlptFault> {
        self.ensure_operational()?;
        let index = self.record_index(receipt.0)?;
        let record = self.slots[index].record.ok_or(SlptFault::UnknownMapping)?;
        if record.phase != MappingPhase::AwaitingMapInvalidation {
            return Err(SlptFault::StaleReceipt);
        }
        match invalidator.invalidate_after_page_table_change(record.iova) {
            Ok(()) => {
                self.slots[index].record.as_mut().unwrap().phase = MappingPhase::Active;
                Ok(MapOutcome::Active(receipt.0))
            }
            Err(error) => Ok(MapOutcome::Pending {
                receipt,
                cause: PendingCause::Invalidation(error),
            }),
        }
    }

    pub fn unmap_page(
        &mut self,
        memory: &mut impl SlptPageMemory,
        invalidator: &mut impl SlptInvalidator,
        handle: MappingHandle,
    ) -> Result<UnmapOutcome, SlptFault> {
        self.ensure_operational()?;
        let index = self.record_index(handle)?;
        let record = self.slots[index].record.ok_or(SlptFault::UnknownMapping)?;
        if record.phase != MappingPhase::Active {
            return Err(SlptFault::InvalidPhase);
        }
        let leaf_position = usize::from(record.path.depth - 1);
        let leaf_table = record.path.frames[leaf_position];
        let leaf_index = record.path.indices[leaf_position] as usize;
        if memory.read_entry(leaf_table, leaf_index)? != record.leaf {
            self.state = SlptState::Poisoned;
            return Err(SlptFault::EntryMismatch);
        }
        memory.write_entry(leaf_table, leaf_index, 0)?;
        self.slots[index].record.as_mut().unwrap().phase = MappingPhase::AwaitingUnmapInvalidation;
        self.finish_unmap(memory, invalidator, UnmapReceipt(handle))
    }

    /// Abandons a map whose publication invalidation did not complete.
    ///
    /// The leaf is removed first and the operation then follows the ordinary
    /// unmap invalidation/cleanup phases. This remains safe when the earlier
    /// invalidation error was ambiguous and hardware may have observed it.
    pub fn cancel_pending_map(
        &mut self,
        memory: &mut impl SlptPageMemory,
        invalidator: &mut impl SlptInvalidator,
        receipt: MapReceipt,
    ) -> Result<UnmapOutcome, SlptFault> {
        self.ensure_operational()?;
        let index = self.record_index(receipt.0)?;
        let record = self.slots[index].record.ok_or(SlptFault::UnknownMapping)?;
        if record.phase != MappingPhase::AwaitingMapInvalidation {
            return Err(SlptFault::StaleReceipt);
        }
        let leaf_position = usize::from(record.path.depth - 1);
        let leaf_table = record.path.frames[leaf_position];
        let leaf_index = record.path.indices[leaf_position] as usize;
        if memory.read_entry(leaf_table, leaf_index)? != record.leaf {
            self.state = SlptState::Poisoned;
            return Err(SlptFault::EntryMismatch);
        }
        memory.write_entry(leaf_table, leaf_index, 0)?;
        self.slots[index].record.as_mut().unwrap().phase = MappingPhase::AwaitingUnmapInvalidation;
        self.finish_unmap(memory, invalidator, UnmapReceipt(receipt.0))
    }

    pub fn finish_unmap(
        &mut self,
        memory: &mut impl SlptPageMemory,
        invalidator: &mut impl SlptInvalidator,
        receipt: UnmapReceipt,
    ) -> Result<UnmapOutcome, SlptFault> {
        self.ensure_operational()?;
        let index = self.record_index(receipt.0)?;
        let phase = self.slots[index]
            .record
            .ok_or(SlptFault::UnknownMapping)?
            .phase;
        if phase == MappingPhase::AwaitingUnmapInvalidation {
            let iova = self.slots[index].record.unwrap().iova;
            if let Err(error) = invalidator.invalidate_after_page_table_change(iova) {
                return Ok(UnmapOutcome::Pending {
                    receipt,
                    cause: PendingCause::Invalidation(error),
                });
            }
            self.slots[index].record.as_mut().unwrap().phase = MappingPhase::AwaitingCleanup;
        } else if phase != MappingPhase::AwaitingCleanup {
            return Err(SlptFault::StaleReceipt);
        }

        match self.reclaim_empty_tables(memory, invalidator, index) {
            Ok(()) => {
                self.retire_slot(index);
                Ok(UnmapOutcome::Complete)
            }
            Err(ReclaimError::Memory(error)) => Ok(UnmapOutcome::Pending {
                receipt,
                cause: PendingCause::Cleanup(error),
            }),
            Err(ReclaimError::Invalidation(error)) => Ok(UnmapOutcome::Pending {
                receipt,
                cause: PendingCause::Invalidation(error),
            }),
            Err(ReclaimError::Fatal) => {
                self.state = SlptState::Poisoned;
                Err(SlptFault::RollbackFailed)
            }
        }
    }

    fn build_path(
        &mut self,
        memory: &mut impl SlptPageMemory,
        iova: u64,
        path: &mut PagePath,
        new_links: &mut [NewLink; MAXIMUM_LEVELS - 1],
        new_link_count: &mut usize,
    ) -> Result<(), SlptFault> {
        for position in 0..usize::from(self.levels) {
            let index = self.index_at(iova, position);
            path.indices[position] = index as u16;
            if position + 1 == usize::from(self.levels) {
                break;
            }
            let parent = path.frames[position];
            let entry = memory.read_entry(parent, index)?;
            let child = if entry == 0 {
                let child = memory.allocate_table()?;
                if child.0 == 0
                    || self.frame_is_known(child)
                    || path.frames[..=position].contains(&child)
                {
                    self.state = SlptState::Poisoned;
                    return Err(SlptFault::FrameAlias);
                }
                if let Err(error) = memory.zero_table(child) {
                    if memory.release_table(child).is_err() {
                        return Err(SlptFault::RollbackFailed);
                    }
                    return Err(error.into());
                }
                if let Err(error) = memory.write_entry(parent, index, child.0 | PERMISSION_MASK) {
                    if memory.release_table(child).is_err() {
                        return Err(SlptFault::RollbackFailed);
                    }
                    return Err(error.into());
                }
                new_links[*new_link_count] = NewLink {
                    parent,
                    index: index as u16,
                    child,
                };
                *new_link_count += 1;
                child
            } else {
                match decode_table_pointer(entry) {
                    Ok(child) if self.frame_is_known(child) => child,
                    Ok(_) => {
                        self.state = SlptState::Poisoned;
                        return Err(SlptFault::ForeignEntry);
                    }
                    Err(error) => {
                        self.state = SlptState::Poisoned;
                        return Err(error);
                    }
                }
            };
            if path.frames[..=position].contains(&child) {
                self.state = SlptState::Poisoned;
                return Err(SlptFault::MalformedEntry);
            }
            path.frames[position + 1] = child;
        }
        Ok(())
    }

    fn rollback_new_links(
        &self,
        memory: &mut impl SlptPageMemory,
        links: &[NewLink; MAXIMUM_LEVELS - 1],
        count: usize,
    ) -> Result<(), SlptFault> {
        for link in links[..count].iter().rev() {
            memory.write_entry(link.parent, link.index as usize, 0)?;
            if let Err(error) = memory.release_table(link.child) {
                if memory
                    .write_entry(
                        link.parent,
                        link.index as usize,
                        link.child.0 | PERMISSION_MASK,
                    )
                    .is_err()
                {
                    return Err(SlptFault::RollbackFailed);
                }
                return Err(error.into());
            }
        }
        Ok(())
    }

    fn reclaim_empty_tables(
        &mut self,
        memory: &mut impl SlptPageMemory,
        invalidator: &mut impl SlptInvalidator,
        slot_index: usize,
    ) -> Result<(), ReclaimError> {
        loop {
            let record = self.slots[slot_index].record.unwrap();
            let depth = usize::from(record.path.depth);
            let released = usize::from(record.cleanup_depth);
            if released + 1 >= depth {
                return Ok(());
            }
            let child_position = depth - 1 - released;
            let child = record.path.frames[child_position];
            if !table_is_empty(memory, child)? {
                return Ok(());
            }
            let parent_position = child_position - 1;
            let parent = record.path.frames[parent_position];
            let parent_index = record.path.indices[parent_position] as usize;
            let pointer = child.0 | PERMISSION_MASK;
            if memory.read_entry(parent, parent_index)? != pointer {
                return Err(ReclaimError::Fatal);
            }
            memory.write_entry(parent, parent_index, 0)?;
            if let Err(error) = invalidator.invalidate_after_page_table_change(record.iova) {
                if memory.write_entry(parent, parent_index, pointer).is_err() {
                    return Err(ReclaimError::Fatal);
                }
                return Err(ReclaimError::Invalidation(error));
            }
            if let Err(error) = memory.release_table(child) {
                if memory.write_entry(parent, parent_index, pointer).is_err() {
                    return Err(ReclaimError::Fatal);
                }
                return Err(ReclaimError::Memory(error));
            }
            self.slots[slot_index]
                .record
                .as_mut()
                .unwrap()
                .cleanup_depth += 1;
        }
    }

    fn index_at(&self, iova: u64, position: usize) -> usize {
        let remaining_levels = usize::from(self.levels) - 1 - position;
        ((iova >> (PAGE_SHIFT as usize + remaining_levels * 9)) & 0x1ff) as usize
    }

    fn validate_iova(&self, iova: u64) -> Result<(), SlptFault> {
        if iova & (PAGE_SIZE - 1) != 0 {
            return Err(SlptFault::InvalidIova);
        }
        let last = iova
            .checked_add(PAGE_SIZE - 1)
            .ok_or(SlptFault::InvalidIova)?;
        if self.adjusted_width < 64 && last >= (1u64 << self.adjusted_width) {
            return Err(SlptFault::InvalidIova);
        }
        Ok(())
    }

    fn frame_is_known(&self, frame: SlptFrame) -> bool {
        if frame == self.root {
            return true;
        }
        self.slots.iter().any(|slot| {
            slot.record.is_some_and(|record| {
                let retained_depth = usize::from(record.path.depth - record.cleanup_depth);
                record.path.frames[..retained_depth].contains(&frame)
            })
        })
    }

    fn record(&self, handle: MappingHandle) -> Result<&MappingRecord, SlptFault> {
        let index = self.record_index(handle)?;
        self.slots[index]
            .record
            .as_ref()
            .ok_or(SlptFault::UnknownMapping)
    }

    fn record_index(&self, handle: MappingHandle) -> Result<usize, SlptFault> {
        let index = handle.slot as usize;
        let Some(slot) = self.slots.get(index) else {
            return Err(SlptFault::UnknownMapping);
        };
        if slot.generation != handle.generation || slot.record.is_none() {
            return Err(SlptFault::UnknownMapping);
        }
        Ok(index)
    }

    fn retire_slot(&mut self, index: usize) {
        let slot = &mut self.slots[index];
        slot.record = None;
        if slot.generation == u32::MAX {
            slot.retired = true;
        } else {
            slot.generation += 1;
        }
    }

    fn ensure_operational(&self) -> Result<(), SlptFault> {
        if self.state == SlptState::Operational {
            Ok(())
        } else {
            Err(SlptFault::Poisoned)
        }
    }
}

enum ReclaimError {
    Memory(SlptMemoryError),
    Invalidation(SlptInvalidationError),
    Fatal,
}

impl From<SlptMemoryError> for ReclaimError {
    fn from(error: SlptMemoryError) -> Self {
        Self::Memory(error)
    }
}

fn decode_table_pointer(entry: u64) -> Result<SlptFrame, SlptFault> {
    if entry & PERMISSION_MASK != PERMISSION_MASK
        || entry & !(PHYSICAL_PAGE_MASK | PERMISSION_MASK) != 0
        || entry & PHYSICAL_PAGE_MASK == 0
    {
        return Err(SlptFault::MalformedEntry);
    }
    SlptFrame::from_physical_address(entry & PHYSICAL_PAGE_MASK).ok_or(SlptFault::MalformedEntry)
}

fn validate_mapped_physical_address(address: u64) -> Result<(), SlptFault> {
    if address & (PAGE_SIZE - 1) != 0 || address & !PHYSICAL_PAGE_MASK != 0 {
        Err(SlptFault::InvalidPhysicalAddress)
    } else {
        Ok(())
    }
}

fn table_is_empty(memory: &impl SlptPageMemory, frame: SlptFrame) -> Result<bool, SlptMemoryError> {
    for index in 0..ENTRIES_PER_TABLE {
        if memory.read_entry(frame, index)? != 0 {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_PAGE_COUNT: usize = 16;
    const MEMORY_FAILURE: SlptMemoryError = SlptMemoryError(7);
    const INVALIDATION_FAILURE: SlptInvalidationError = SlptInvalidationError(11);

    #[derive(Clone, Copy)]
    struct TestPage {
        physical_address: u64,
        allocated: bool,
        entries: [u64; ENTRIES_PER_TABLE],
    }

    impl TestPage {
        const EMPTY: Self = Self {
            physical_address: 0,
            allocated: false,
            entries: [0; ENTRIES_PER_TABLE],
        };
    }

    struct TestMemory {
        pages: [TestPage; TEST_PAGE_COUNT],
        allocation_calls: usize,
        release_calls: usize,
        fail_allocation_at: Option<usize>,
        fail_release_once: bool,
        bad_publication: bool,
    }

    impl TestMemory {
        fn new() -> Self {
            let mut pages = [TestPage::EMPTY; TEST_PAGE_COUNT];
            for (index, page) in pages.iter_mut().enumerate() {
                page.physical_address = ((index + 1) as u64) * PAGE_SIZE;
            }
            pages[0].allocated = true;
            Self {
                pages,
                allocation_calls: 0,
                release_calls: 0,
                fail_allocation_at: None,
                fail_release_once: false,
                bad_publication: false,
            }
        }

        fn root(&self) -> SlptFrame {
            SlptFrame::from_physical_address(self.pages[0].physical_address).unwrap()
        }

        fn page_index(&self, frame: SlptFrame) -> Result<usize, SlptMemoryError> {
            self.pages
                .iter()
                .position(|page| page.physical_address == frame.0 && page.allocated)
                .ok_or(MEMORY_FAILURE)
        }

        fn allocated_pages(&self) -> usize {
            self.pages.iter().filter(|page| page.allocated).count()
        }

        fn root_is_empty(&self) -> bool {
            self.pages[0].entries.iter().all(|entry| *entry == 0)
        }
    }

    impl SlptPageMemory for TestMemory {
        fn allocate_table(&mut self) -> Result<SlptFrame, SlptMemoryError> {
            self.allocation_calls += 1;
            if self.fail_allocation_at == Some(self.allocation_calls) {
                return Err(MEMORY_FAILURE);
            }
            let page = self
                .pages
                .iter_mut()
                .find(|page| !page.allocated)
                .ok_or(MEMORY_FAILURE)?;
            page.allocated = true;
            page.entries.fill(0xa5a5_a5a5_a5a5_a5a5);
            SlptFrame::from_physical_address(page.physical_address).ok_or(MEMORY_FAILURE)
        }

        fn zero_table(&mut self, frame: SlptFrame) -> Result<(), SlptMemoryError> {
            let index = self.page_index(frame)?;
            self.pages[index].entries.fill(0);
            Ok(())
        }

        fn read_entry(&self, frame: SlptFrame, index: usize) -> Result<u64, SlptMemoryError> {
            let page = self.page_index(frame)?;
            self.pages[page]
                .entries
                .get(index)
                .copied()
                .ok_or(MEMORY_FAILURE)
        }

        fn write_entry(
            &mut self,
            frame: SlptFrame,
            index: usize,
            value: u64,
        ) -> Result<(), SlptMemoryError> {
            let page_index = self.page_index(frame)?;
            let target_address = value & PHYSICAL_PAGE_MASK;
            if value != 0
                && self.pages.iter().any(|page| {
                    page.allocated
                        && page.physical_address == target_address
                        && page.entries.iter().any(|entry| *entry != 0)
                })
            {
                self.bad_publication = true;
                return Err(MEMORY_FAILURE);
            }
            let entry = self.pages[page_index]
                .entries
                .get_mut(index)
                .ok_or(MEMORY_FAILURE)?;
            *entry = value;
            Ok(())
        }

        fn release_table(&mut self, frame: SlptFrame) -> Result<(), SlptMemoryError> {
            self.release_calls += 1;
            if self.fail_release_once {
                self.fail_release_once = false;
                return Err(MEMORY_FAILURE);
            }
            let index = self.page_index(frame)?;
            if self.pages[index].entries.iter().any(|entry| *entry != 0) {
                return Err(MEMORY_FAILURE);
            }
            self.pages[index].allocated = false;
            Ok(())
        }
    }

    struct TestInvalidator {
        failures_remaining: usize,
        fail_on_call: Option<usize>,
        calls: usize,
    }

    impl TestInvalidator {
        const fn reliable() -> Self {
            Self {
                failures_remaining: 0,
                fail_on_call: None,
                calls: 0,
            }
        }

        const fn fail_once() -> Self {
            Self {
                failures_remaining: 1,
                fail_on_call: None,
                calls: 0,
            }
        }
    }

    impl SlptInvalidator for TestInvalidator {
        fn invalidate_after_page_table_change(
            &mut self,
            _iova: u64,
        ) -> Result<(), SlptInvalidationError> {
            self.calls += 1;
            if self.fail_on_call == Some(self.calls) {
                self.fail_on_call = None;
                Err(INVALIDATION_FAILURE)
            } else if self.failures_remaining != 0 {
                self.failures_remaining -= 1;
                Err(INVALIDATION_FAILURE)
            } else {
                Ok(())
            }
        }
    }

    const fn config() -> SlptConfig {
        SlptConfig {
            supported_adjusted_guest_widths: 1 << 2,
            maximum_guest_address_width: 48,
            address_width_encoding: 2,
        }
    }

    fn table<const N: usize>(memory: &TestMemory) -> Slpt<N> {
        Slpt::attach(memory, memory.root(), config()).unwrap()
    }

    fn active_handle(outcome: MapOutcome) -> MappingHandle {
        match outcome {
            MapOutcome::Active(handle) => handle,
            MapOutcome::Pending { .. } => panic!("unexpected pending map"),
        }
    }

    #[test]
    fn maps_then_reclaims_a_four_level_path() {
        let mut memory = TestMemory::new();
        let mut invalidator = TestInvalidator::reliable();
        let mut slpt = table::<4>(&memory);
        let handle = active_handle(
            slpt.map_page(
                &mut memory,
                &mut invalidator,
                0x4000,
                0x8000_0000,
                DmaPermissions::READ_WRITE,
            )
            .unwrap(),
        );

        assert_eq!(slpt.phase(handle), Ok(MappingPhase::Active));
        assert_eq!(memory.allocated_pages(), 4);
        assert!(!memory.bad_publication);
        assert_eq!(
            slpt.map_page(
                &mut memory,
                &mut invalidator,
                0x4000,
                0x9000_0000,
                DmaPermissions::READ_ONLY,
            ),
            Err(SlptFault::IovaOverlap)
        );
        assert_eq!(
            slpt.map_page(
                &mut memory,
                &mut invalidator,
                0x5000,
                0x8000_0000,
                DmaPermissions::READ_ONLY,
            ),
            Err(SlptFault::PhysicalAlias)
        );

        assert_eq!(
            slpt.unmap_page(&mut memory, &mut invalidator, handle),
            Ok(UnmapOutcome::Complete)
        );
        assert_eq!(memory.allocated_pages(), 1);
        assert!(memory.root_is_empty());
        // Map publication, leaf removal, then one unlink invalidation for each
        // of the three reclaimed intermediate levels.
        assert_eq!(invalidator.calls, 5);
        assert_eq!(slpt.phase(handle), Err(SlptFault::UnknownMapping));
    }

    #[test]
    fn failed_map_invalidation_retains_alias_evidence_until_retry() {
        let mut memory = TestMemory::new();
        let mut invalidator = TestInvalidator::fail_once();
        let mut slpt = table::<4>(&memory);
        let receipt = match slpt
            .map_page(
                &mut memory,
                &mut invalidator,
                0x10_0000,
                0x8100_0000,
                DmaPermissions::WRITE_ONLY,
            )
            .unwrap()
        {
            MapOutcome::Pending { receipt, cause } => {
                assert_eq!(cause, PendingCause::Invalidation(INVALIDATION_FAILURE));
                receipt
            }
            MapOutcome::Active(_) => panic!("fault injection did not fire"),
        };
        assert_eq!(
            slpt.phase(receipt.handle()),
            Ok(MappingPhase::AwaitingMapInvalidation)
        );
        assert_eq!(
            slpt.map_page(
                &mut memory,
                &mut invalidator,
                0x20_0000,
                0x8100_0000,
                DmaPermissions::READ_WRITE,
            ),
            Err(SlptFault::PhysicalAlias)
        );
        assert_eq!(
            slpt.finish_map(&mut invalidator, receipt),
            Ok(MapOutcome::Active(receipt.handle()))
        );
    }

    #[test]
    fn failed_unmap_invalidation_retains_mapping_and_tables() {
        let mut memory = TestMemory::new();
        let mut invalidator = TestInvalidator::reliable();
        let mut slpt = table::<4>(&memory);
        let handle = active_handle(
            slpt.map_page(
                &mut memory,
                &mut invalidator,
                0x2000,
                0x8200_0000,
                DmaPermissions::READ_WRITE,
            )
            .unwrap(),
        );
        invalidator.failures_remaining = 1;
        let receipt = match slpt
            .unmap_page(&mut memory, &mut invalidator, handle)
            .unwrap()
        {
            UnmapOutcome::Pending { receipt, cause } => {
                assert_eq!(cause, PendingCause::Invalidation(INVALIDATION_FAILURE));
                receipt
            }
            UnmapOutcome::Complete => panic!("fault injection did not fire"),
        };
        assert_eq!(
            slpt.phase(handle),
            Ok(MappingPhase::AwaitingUnmapInvalidation)
        );
        assert_eq!(memory.allocated_pages(), 4);
        assert_eq!(
            slpt.map_page(
                &mut memory,
                &mut invalidator,
                0x3000,
                0x8200_0000,
                DmaPermissions::READ_WRITE,
            ),
            Err(SlptFault::PhysicalAlias)
        );
        assert_eq!(
            slpt.finish_unmap(&mut memory, &mut invalidator, receipt),
            Ok(UnmapOutcome::Complete)
        );
        assert_eq!(memory.allocated_pages(), 1);
    }

    #[test]
    fn pending_map_can_be_cancelled_without_claiming_activation() {
        let mut memory = TestMemory::new();
        let mut invalidator = TestInvalidator::fail_once();
        let mut slpt = table::<2>(&memory);
        let receipt = match slpt
            .map_page(
                &mut memory,
                &mut invalidator,
                0x6000,
                0x8250_0000,
                DmaPermissions::READ_WRITE,
            )
            .unwrap()
        {
            MapOutcome::Pending { receipt, .. } => receipt,
            MapOutcome::Active(_) => panic!("fault injection did not fire"),
        };
        assert_eq!(
            slpt.cancel_pending_map(&mut memory, &mut invalidator, receipt),
            Ok(UnmapOutcome::Complete)
        );
        assert_eq!(memory.allocated_pages(), 1);
        assert!(memory.root_is_empty());
    }

    #[test]
    fn release_failure_restores_parent_link_and_cleanup_is_retryable() {
        let mut memory = TestMemory::new();
        let mut invalidator = TestInvalidator::reliable();
        let mut slpt = table::<4>(&memory);
        let handle = active_handle(
            slpt.map_page(
                &mut memory,
                &mut invalidator,
                0x3000,
                0x8300_0000,
                DmaPermissions::READ_WRITE,
            )
            .unwrap(),
        );
        memory.fail_release_once = true;
        let receipt = match slpt
            .unmap_page(&mut memory, &mut invalidator, handle)
            .unwrap()
        {
            UnmapOutcome::Pending { receipt, cause } => {
                assert_eq!(cause, PendingCause::Cleanup(MEMORY_FAILURE));
                receipt
            }
            UnmapOutcome::Complete => panic!("fault injection did not fire"),
        };
        assert_eq!(slpt.phase(handle), Ok(MappingPhase::AwaitingCleanup));
        assert_eq!(memory.allocated_pages(), 4);
        let invalidations_before_retry = invalidator.calls;
        assert_eq!(
            slpt.finish_unmap(&mut memory, &mut invalidator, receipt),
            Ok(UnmapOutcome::Complete)
        );
        assert_eq!(invalidator.calls, invalidations_before_retry + 3);
        assert_eq!(memory.allocated_pages(), 1);
        assert!(memory.root_is_empty());
    }

    #[test]
    fn cleanup_unlink_is_invalidated_before_frame_reuse_and_retries_safely() {
        let mut memory = TestMemory::new();
        let mut invalidator = TestInvalidator::reliable();
        let mut slpt = table::<2>(&memory);
        let handle = active_handle(
            slpt.map_page(
                &mut memory,
                &mut invalidator,
                0x7000,
                0x8340_0000,
                DmaPermissions::READ_WRITE,
            )
            .unwrap(),
        );
        // Call 2 invalidates the cleared leaf. Call 3 is the first parent
        // unlink and must complete before that child frame can be released.
        invalidator.fail_on_call = Some(3);
        let receipt = match slpt
            .unmap_page(&mut memory, &mut invalidator, handle)
            .unwrap()
        {
            UnmapOutcome::Pending { receipt, cause } => {
                assert_eq!(cause, PendingCause::Invalidation(INVALIDATION_FAILURE));
                receipt
            }
            UnmapOutcome::Complete => panic!("cleanup invalidation fault did not fire"),
        };
        assert_eq!(slpt.phase(handle), Ok(MappingPhase::AwaitingCleanup));
        assert_eq!(memory.allocated_pages(), 4);
        assert_eq!(
            slpt.finish_unmap(&mut memory, &mut invalidator, receipt),
            Ok(UnmapOutcome::Complete)
        );
        assert_eq!(memory.allocated_pages(), 1);
        assert!(memory.root_is_empty());
    }

    #[test]
    fn shared_lower_tables_are_reclaimed_only_after_the_last_leaf() {
        let mut memory = TestMemory::new();
        let mut invalidator = TestInvalidator::reliable();
        let mut slpt = table::<4>(&memory);
        let first = active_handle(
            slpt.map_page(
                &mut memory,
                &mut invalidator,
                0x4000,
                0x8310_0000,
                DmaPermissions::READ_WRITE,
            )
            .unwrap(),
        );
        let second = active_handle(
            slpt.map_page(
                &mut memory,
                &mut invalidator,
                0x5000,
                0x8320_0000,
                DmaPermissions::READ_WRITE,
            )
            .unwrap(),
        );
        assert_eq!(memory.allocated_pages(), 4);
        assert_eq!(
            slpt.unmap_page(&mut memory, &mut invalidator, first),
            Ok(UnmapOutcome::Complete)
        );
        assert_eq!(memory.allocated_pages(), 4);
        assert_eq!(slpt.phase(second), Ok(MappingPhase::Active));
        assert_eq!(
            slpt.unmap_page(&mut memory, &mut invalidator, second),
            Ok(UnmapOutcome::Complete)
        );
        assert_eq!(memory.allocated_pages(), 1);
    }

    #[test]
    fn allocation_failure_rolls_back_every_new_lower_level() {
        let mut memory = TestMemory::new();
        memory.fail_allocation_at = Some(2);
        let mut invalidator = TestInvalidator::reliable();
        let mut slpt = table::<4>(&memory);
        assert_eq!(
            slpt.map_page(
                &mut memory,
                &mut invalidator,
                0x5000,
                0x8400_0000,
                DmaPermissions::READ_WRITE,
            ),
            Err(SlptFault::Memory(MEMORY_FAILURE))
        );
        assert_eq!(memory.allocated_pages(), 1);
        assert!(memory.root_is_empty());
        assert_eq!(invalidator.calls, 0);
        assert_eq!(slpt.state(), SlptState::Operational);
    }

    #[test]
    fn foreign_intermediate_pointer_poisons_instead_of_becoming_owned() {
        let mut memory = TestMemory::new();
        let mut slpt = table::<1>(&memory);
        memory.pages[1].allocated = true;
        memory.pages[0].entries[0] = memory.pages[1].physical_address | PERMISSION_MASK;
        let mut invalidator = TestInvalidator::reliable();

        assert_eq!(
            slpt.map_page(
                &mut memory,
                &mut invalidator,
                0x4000,
                0x8480_0000,
                DmaPermissions::READ_WRITE,
            ),
            Err(SlptFault::ForeignEntry)
        );
        assert_eq!(slpt.state(), SlptState::Poisoned);
        assert_eq!(invalidator.calls, 0);
    }

    #[test]
    fn validates_sagaw_width_and_page_boundaries() {
        let memory = TestMemory::new();
        assert!(matches!(
            Slpt::<1>::attach(
                &memory,
                memory.root(),
                SlptConfig {
                    supported_adjusted_guest_widths: 1 << 1,
                    maximum_guest_address_width: 48,
                    address_width_encoding: 2,
                }
            ),
            Err(SlptFault::UnsupportedAddressWidth)
        ));

        let mut slpt = table::<1>(&memory);
        let mut memory = memory;
        let mut invalidator = TestInvalidator::reliable();
        assert_eq!(
            slpt.map_page(
                &mut memory,
                &mut invalidator,
                1u64 << 48,
                0x8500_0000,
                DmaPermissions::READ_ONLY,
            ),
            Err(SlptFault::InvalidIova)
        );
        assert_eq!(
            slpt.map_page(
                &mut memory,
                &mut invalidator,
                0,
                0x8500_0001,
                DmaPermissions::READ_ONLY,
            ),
            Err(SlptFault::InvalidPhysicalAddress)
        );

        let wide_memory = TestMemory::new();
        assert!(matches!(
            Slpt::<1>::attach(
                &wide_memory,
                wide_memory.root(),
                SlptConfig {
                    supported_adjusted_guest_widths: 1 << 4,
                    maximum_guest_address_width: 64,
                    address_width_encoding: 4,
                },
            ),
            Err(SlptFault::InvalidAddressWidth)
        ));
    }
}
