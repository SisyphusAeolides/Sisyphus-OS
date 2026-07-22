use core::{mem::size_of, ptr};

use abyss::frame::{BitmapFrameAllocator, FrameAllocatorError};
use abyss::paging::{PAGE_SIZE, PhysicalAddress};

#[cfg(target_os = "none")]
use crate::arch::x86_64::{X86_64, active_page_table_root, load_page_table_root};
#[cfg(target_os = "none")]
use crate::capability::InterruptGuard;
use crate::capability::{Capability, PhysicalMemoryControl, ProcessInstallControl};
use crate::process::install::{
    MappingPermissions, ProcessImageHandle, ProcessImageInfo, UserAddressSpaceBackend,
};

const PAGE_ADDRESS_MASK: u64 = 0x000f_ffff_ffff_f000;
const ENTRY_PRESENT: u64 = 1 << 0;
const ENTRY_WRITABLE: u64 = 1 << 1;
const ENTRY_USER: u64 = 1 << 2;
const ENTRY_HUGE: u64 = 1 << 7;
const ENTRY_NO_EXECUTE: u64 = 1 << 63;
const USER_PML4_ENTRIES: usize = 256;
const TABLE_ENTRIES: usize = 512;

pub const MAXIMUM_PROCESS_PAGES: usize = 64;
pub const MAXIMUM_OWNED_FRAMES: usize = 128;
pub const INITIAL_USER_STACK_BASE: u64 = 0x7000;
pub const INITIAL_USER_STACK_POINTER: u64 = 0x8000;

/// Physical-memory operations required by the process page-table builder.
///
/// Implementations must provide exclusive ownership of allocated frames and
/// must make page-table writes visible before a root can be activated.
pub trait ProcessFrameMemory {
    type Error;

    fn allocate_zeroed(&mut self) -> Result<PhysicalAddress, Self::Error>;
    fn release(&mut self, frame: PhysicalAddress) -> Result<(), Self::Error>;
    fn read_entry(&self, table: PhysicalAddress, index: usize) -> Result<u64, Self::Error>;
    fn write_entry(
        &mut self,
        table: PhysicalAddress,
        index: usize,
        value: u64,
    ) -> Result<(), Self::Error>;
    fn write_bytes(
        &mut self,
        frame: PhysicalAddress,
        offset: usize,
        bytes: &[u8],
    ) -> Result<(), Self::Error>;
    fn bytes_equal(
        &self,
        frame: PhysicalAddress,
        offset: usize,
        bytes: &[u8],
    ) -> Result<bool, Self::Error>;
    fn bytes_zero(
        &self,
        frame: PhysicalAddress,
        offset: usize,
        length: usize,
    ) -> Result<bool, Self::Error>;
}

/// Accesses allocator-owned RAM through Boulder's established direct map.
pub struct DirectMapFrameMemory<'allocator, 'storage> {
    allocator: &'allocator mut BitmapFrameAllocator<'storage>,
    direct_map_base: usize,
    mapped_physical_limit: u64,
}

impl<'allocator, 'storage> DirectMapFrameMemory<'allocator, 'storage> {
    /// Creates a physical-memory adapter over a live direct map.
    ///
    /// # Safety
    ///
    /// Every frame returned by `allocator` below `mapped_physical_limit` must
    /// be mapped writable at `direct_map_base + physical_address`. The mapping
    /// must remain stable and exclusively represent ordinary RAM for this
    /// adapter's lifetime.
    pub const unsafe fn new(
        allocator: &'allocator mut BitmapFrameAllocator<'storage>,
        direct_map_base: usize,
        mapped_physical_limit: u64,
        _authority: &Capability<'_, PhysicalMemoryControl>,
    ) -> Self {
        Self {
            allocator,
            direct_map_base,
            mapped_physical_limit,
        }
    }

    fn pointer(
        &self,
        frame: PhysicalAddress,
        offset: usize,
        length: usize,
    ) -> Result<*mut u8, DirectMapMemoryError> {
        if !frame.is_page_aligned()
            || offset.checked_add(length).is_none_or(|end| end > PAGE_SIZE)
            || frame
                .as_u64()
                .checked_add(PAGE_SIZE as u64)
                .is_none_or(|end| end > self.mapped_physical_limit)
        {
            return Err(DirectMapMemoryError::InvalidAccess);
        }
        let physical =
            usize::try_from(frame.as_u64()).map_err(|_| DirectMapMemoryError::AddressOverflow)?;
        let address = self
            .direct_map_base
            .checked_add(physical)
            .and_then(|base| base.checked_add(offset))
            .ok_or(DirectMapMemoryError::AddressOverflow)?;
        Ok(address as *mut u8)
    }
}

impl ProcessFrameMemory for DirectMapFrameMemory<'_, '_> {
    type Error = DirectMapMemoryError;

    fn allocate_zeroed(&mut self) -> Result<PhysicalAddress, Self::Error> {
        let frame = self
            .allocator
            .allocate()
            .ok_or(DirectMapMemoryError::OutOfFrames)?;
        let pointer = match self.pointer(frame, 0, PAGE_SIZE) {
            Ok(pointer) => pointer,
            Err(error) => {
                self.allocator
                    .deallocate(frame)
                    .map_err(DirectMapMemoryError::Allocator)?;
                return Err(error);
            }
        };
        // SAFETY: The adapter owns the allocated frame and `pointer` covers
        // exactly its stable writable direct-map alias.
        unsafe { ptr::write_bytes(pointer, 0, PAGE_SIZE) };
        Ok(frame)
    }

    fn release(&mut self, frame: PhysicalAddress) -> Result<(), Self::Error> {
        self.allocator
            .deallocate(frame)
            .map_err(DirectMapMemoryError::Allocator)
    }

    fn read_entry(&self, table: PhysicalAddress, index: usize) -> Result<u64, Self::Error> {
        if index >= TABLE_ENTRIES {
            return Err(DirectMapMemoryError::InvalidAccess);
        }
        let pointer = self.pointer(table, index * size_of::<u64>(), size_of::<u64>())?;
        // SAFETY: `pointer` identifies one aligned entry in a mapped frame.
        Ok(unsafe { pointer.cast::<u64>().read_volatile() })
    }

    fn write_entry(
        &mut self,
        table: PhysicalAddress,
        index: usize,
        value: u64,
    ) -> Result<(), Self::Error> {
        if index >= TABLE_ENTRIES {
            return Err(DirectMapMemoryError::InvalidAccess);
        }
        let pointer = self.pointer(table, index * size_of::<u64>(), size_of::<u64>())?;
        // SAFETY: The builder exclusively owns destination tables and writes
        // one naturally aligned hardware entry.
        unsafe { pointer.cast::<u64>().write_volatile(value) };
        Ok(())
    }

    fn write_bytes(
        &mut self,
        frame: PhysicalAddress,
        offset: usize,
        bytes: &[u8],
    ) -> Result<(), Self::Error> {
        let pointer = self.pointer(frame, offset, bytes.len())?;
        // SAFETY: Bounds were checked against the exclusively owned frame and
        // source and destination cannot overlap.
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len()) };
        Ok(())
    }

    fn bytes_equal(
        &self,
        frame: PhysicalAddress,
        offset: usize,
        bytes: &[u8],
    ) -> Result<bool, Self::Error> {
        let pointer = self.pointer(frame, offset, bytes.len())?;
        // SAFETY: The checked direct-map range remains readable for the
        // adapter lifetime.
        let actual = unsafe { core::slice::from_raw_parts(pointer.cast_const(), bytes.len()) };
        Ok(actual == bytes)
    }

    fn bytes_zero(
        &self,
        frame: PhysicalAddress,
        offset: usize,
        length: usize,
    ) -> Result<bool, Self::Error> {
        let pointer = self.pointer(frame, offset, length)?;
        // SAFETY: The checked direct-map range remains readable for the
        // adapter lifetime.
        let actual = unsafe { core::slice::from_raw_parts(pointer.cast_const(), length) };
        Ok(actual.iter().all(|byte| *byte == 0))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectMapMemoryError {
    OutOfFrames,
    InvalidAccess,
    AddressOverflow,
    Allocator(FrameAllocatorError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameBackedSpace {
    generation: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameBackedMapping {
    slot: u8,
    generation: u32,
}

#[derive(Clone, Copy)]
struct MappingRecord {
    occupied: bool,
    sealed: bool,
    generation: u32,
    virtual_address: u64,
    memory_size: usize,
    first_page: u8,
    page_count: u8,
    permissions: MappingPermissions,
}

impl MappingRecord {
    const EMPTY: Self = Self {
        occupied: false,
        sealed: false,
        generation: 0,
        virtual_address: 0,
        memory_size: 0,
        first_page: 0,
        page_count: 0,
        permissions: MappingPermissions {
            readable: false,
            writable: false,
            executable: false,
        },
    };
}

#[derive(Clone, Copy)]
struct PageRecord {
    frame: PhysicalAddress,
    virtual_address: u64,
}

impl PageRecord {
    const EMPTY: Self = Self {
        frame: PhysicalAddress::new(0),
        virtual_address: 0,
    };
}

/// Builds an x86_64 hardware-format user address space from owned frames.
///
/// The root inherits only PML4 entries 256..511 from the active kernel root.
/// A committed root can be switched into CR3 for a bounded validation while
/// the kernel remains entirely in its inherited higher-half mappings. Retained
/// ownership and privilege entry remain responsibilities of the scheduler.
pub struct FrameBackedAddressSpace<Memory: ProcessFrameMemory> {
    memory: Memory,
    kernel_root: PhysicalAddress,
    root: Option<PhysicalAddress>,
    generation: u32,
    active: bool,
    image_start: u64,
    image_end: u64,
    mappings: [MappingRecord; super::install::MAXIMUM_PROCESS_SEGMENTS],
    mapping_count: usize,
    pages: [PageRecord; MAXIMUM_PROCESS_PAGES],
    page_count: usize,
    owned_frames: [PhysicalAddress; MAXIMUM_OWNED_FRAMES],
    owned_frame_count: usize,
    process_live: bool,
    process_generation: u32,
    process_info: ProcessImageInfo,
}

impl<Memory: ProcessFrameMemory> FrameBackedAddressSpace<Memory> {
    pub const fn new(
        memory: Memory,
        kernel_root: PhysicalAddress,
        _authority: &Capability<'_, ProcessInstallControl>,
    ) -> Self {
        Self {
            memory,
            kernel_root,
            root: None,
            generation: 0,
            active: false,
            image_start: 0,
            image_end: 0,
            mappings: [MappingRecord::EMPTY; super::install::MAXIMUM_PROCESS_SEGMENTS],
            mapping_count: 0,
            pages: [PageRecord::EMPTY; MAXIMUM_PROCESS_PAGES],
            page_count: 0,
            owned_frames: [PhysicalAddress::new(0); MAXIMUM_OWNED_FRAMES],
            owned_frame_count: 0,
            process_live: false,
            process_generation: 0,
            process_info: ProcessImageInfo {
                entry_point: 0,
                segment_count: 0,
                address_space_root: None,
                owned_frames: 0,
                initial_stack_pointer: None,
            },
        }
    }

    pub const fn memory(&self) -> &Memory {
        &self.memory
    }

    pub fn memory_mut(&mut self) -> &mut Memory {
        &mut self.memory
    }

    pub const fn owned_frame_count(&self) -> usize {
        self.owned_frame_count
    }

    /// Adds one zeroed, writable, non-executable stack page to a committed
    /// process while retaining ownership in this backend.
    pub fn install_initial_stack(
        &mut self,
        process: &ProcessImageHandle,
        _authority: &Capability<'_, ProcessInstallControl>,
    ) -> Result<u64, FrameBackedError<Memory::Error>> {
        if self.active
            || self.process_info(process).is_none()
            || self.process_info.initial_stack_pointer.is_some()
            || self.page_count == self.pages.len()
        {
            return Err(FrameBackedError::InvalidState);
        }
        let (table, index) = self.leaf_slot(INITIAL_USER_STACK_BASE)?;
        let existing = self
            .memory
            .read_entry(table, index)
            .map_err(FrameBackedError::Memory)?;
        if existing != 0 {
            return Err(FrameBackedError::MappingConflict);
        }

        let frame = self.allocate_owned()?;
        let entry = frame.as_u64() | ENTRY_PRESENT | ENTRY_WRITABLE | ENTRY_USER | ENTRY_NO_EXECUTE;
        if let Err(error) = self.memory.write_entry(table, index, entry) {
            return match self.release_last_owned(frame) {
                Ok(()) => Err(FrameBackedError::Memory(error)),
                Err(cleanup) => Err(cleanup),
            };
        }
        self.pages[self.page_count] = PageRecord {
            frame,
            virtual_address: INITIAL_USER_STACK_BASE,
        };
        self.page_count += 1;
        self.process_info.initial_stack_pointer = Some(INITIAL_USER_STACK_POINTER);
        self.process_info.owned_frames = self.owned_frame_count;
        Ok(INITIAL_USER_STACK_POINTER)
    }

    /// Retries reclamation after a frame-memory backend reported a release
    /// failure. Failed frames remain recorded until this succeeds.
    pub fn retry_cleanup(
        &mut self,
        _authority: &Capability<'_, PhysicalMemoryControl>,
    ) -> Result<(), FrameBackedError<Memory::Error>> {
        if self.active || self.process_live {
            return Err(FrameBackedError::InvalidState);
        }
        self.release_owned()
    }

    fn allocate_owned(&mut self) -> Result<PhysicalAddress, FrameBackedError<Memory::Error>> {
        if self.owned_frame_count == self.owned_frames.len() {
            return Err(FrameBackedError::CapacityExceeded);
        }
        let frame = self
            .memory
            .allocate_zeroed()
            .map_err(FrameBackedError::Memory)?;
        if !frame.is_page_aligned() || frame.as_u64() & !PAGE_ADDRESS_MASK != 0 {
            self.memory
                .release(frame)
                .map_err(FrameBackedError::Memory)?;
            return Err(FrameBackedError::InvalidPhysicalFrame);
        }
        self.owned_frames[self.owned_frame_count] = frame;
        self.owned_frame_count += 1;
        Ok(frame)
    }

    fn release_owned(&mut self) -> Result<(), FrameBackedError<Memory::Error>> {
        let mut first_error = None;
        let mut retained = [PhysicalAddress::new(0); MAXIMUM_OWNED_FRAMES];
        let mut retained_count = 0;
        for index in (0..self.owned_frame_count).rev() {
            let frame = self.owned_frames[index];
            if let Err(error) = self.memory.release(frame) {
                retained[retained_count] = frame;
                retained_count += 1;
                if first_error.is_none() {
                    first_error = Some(FrameBackedError::Memory(error));
                }
            }
        }
        self.owned_frames = retained;
        self.owned_frame_count = retained_count;
        self.root = None;
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn release_last_owned(
        &mut self,
        frame: PhysicalAddress,
    ) -> Result<(), FrameBackedError<Memory::Error>> {
        let Some(index) = self.owned_frame_count.checked_sub(1) else {
            return Err(FrameBackedError::CorruptHierarchy);
        };
        if self.owned_frames[index] != frame {
            return Err(FrameBackedError::CorruptHierarchy);
        }
        self.memory
            .release(frame)
            .map_err(FrameBackedError::Memory)?;
        self.owned_frames[index] = PhysicalAddress::new(0);
        self.owned_frame_count = index;
        Ok(())
    }

    fn reset_records(&mut self) {
        self.mappings.fill(MappingRecord::EMPTY);
        self.mapping_count = 0;
        self.pages.fill(PageRecord::EMPTY);
        self.page_count = 0;
        self.process_info = ProcessImageInfo {
            entry_point: 0,
            segment_count: 0,
            address_space_root: None,
            owned_frames: 0,
            initial_stack_pointer: None,
        };
    }

    fn initialize_root(&mut self) -> Result<(), FrameBackedError<Memory::Error>> {
        let root = self.allocate_owned()?;
        self.root = Some(root);
        for index in USER_PML4_ENTRIES..TABLE_ENTRIES {
            let entry = self
                .memory
                .read_entry(self.kernel_root, index)
                .map_err(FrameBackedError::Memory)?;
            self.memory
                .write_entry(root, index, entry)
                .map_err(FrameBackedError::Memory)?;
        }
        Ok(())
    }

    fn mapping(
        &self,
        mapping: FrameBackedMapping,
    ) -> Result<MappingRecord, FrameBackedError<Memory::Error>> {
        self.mappings
            .get(usize::from(mapping.slot))
            .copied()
            .filter(|record| record.occupied && record.generation == mapping.generation)
            .ok_or(FrameBackedError::InvalidHandle)
    }

    fn ensure_leaf_slot(
        &mut self,
        virtual_address: u64,
    ) -> Result<(PhysicalAddress, usize), FrameBackedError<Memory::Error>> {
        let indices = page_indices(virtual_address)?;
        if indices[0] >= USER_PML4_ENTRIES {
            return Err(FrameBackedError::InvalidUserRange);
        }
        let mut table = self.root.ok_or(FrameBackedError::InvalidState)?;
        for index in &indices[..3] {
            let entry = self
                .memory
                .read_entry(table, *index)
                .map_err(FrameBackedError::Memory)?;
            if entry & ENTRY_PRESENT != 0 {
                if entry & ENTRY_HUGE != 0 {
                    return Err(FrameBackedError::MappingConflict);
                }
                table = PhysicalAddress::new(entry & PAGE_ADDRESS_MASK);
            } else {
                let next = self.allocate_owned()?;
                self.memory
                    .write_entry(
                        table,
                        *index,
                        next.as_u64() | ENTRY_PRESENT | ENTRY_WRITABLE | ENTRY_USER,
                    )
                    .map_err(FrameBackedError::Memory)?;
                table = next;
            }
        }
        let leaf_index = indices[3];
        let leaf = self
            .memory
            .read_entry(table, leaf_index)
            .map_err(FrameBackedError::Memory)?;
        if leaf != 0 {
            return Err(FrameBackedError::MappingConflict);
        }
        Ok((table, leaf_index))
    }

    fn leaf_slot(
        &self,
        virtual_address: u64,
    ) -> Result<(PhysicalAddress, usize), FrameBackedError<Memory::Error>> {
        let indices = page_indices(virtual_address)?;
        let mut table = self.root.ok_or(FrameBackedError::InvalidState)?;
        for index in &indices[..3] {
            let entry = self
                .memory
                .read_entry(table, *index)
                .map_err(FrameBackedError::Memory)?;
            if entry & ENTRY_PRESENT == 0 || entry & ENTRY_HUGE != 0 {
                return Err(FrameBackedError::CorruptHierarchy);
            }
            table = PhysicalAddress::new(entry & PAGE_ADDRESS_MASK);
        }
        Ok((table, indices[3]))
    }

    fn frame_for_mapping_page(
        &self,
        mapping: MappingRecord,
        page: usize,
    ) -> Result<PhysicalAddress, FrameBackedError<Memory::Error>> {
        if page >= usize::from(mapping.page_count) {
            return Err(FrameBackedError::InvalidRange);
        }
        self.pages
            .get(usize::from(mapping.first_page) + page)
            .map(|record| record.frame)
            .ok_or(FrameBackedError::CorruptHierarchy)
    }

    fn cleanup_transaction(&mut self) -> Result<(), FrameBackedError<Memory::Error>> {
        self.active = false;
        self.reset_records();
        self.release_owned()
    }
}

impl<Memory: ProcessFrameMemory> UserAddressSpaceBackend for FrameBackedAddressSpace<Memory> {
    type Error = FrameBackedError<Memory::Error>;
    type Space = FrameBackedSpace;
    type Mapping = FrameBackedMapping;
    type Process = ProcessImageHandle;

    fn begin(&mut self, image_start: u64, image_end: u64) -> Result<Self::Space, Self::Error> {
        if self.active
            || self.process_live
            || self.owned_frame_count != 0
            || image_start >= image_end
            || image_end > 0x0000_8000_0000_0000
            || !self.kernel_root.is_page_aligned()
        {
            return Err(FrameBackedError::InvalidState);
        }
        self.generation = next_generation(self.generation);
        self.active = true;
        self.image_start = image_start;
        self.image_end = image_end;
        self.reset_records();
        if let Err(error) = self.initialize_root() {
            return match self.cleanup_transaction() {
                Ok(()) => Err(error),
                Err(cleanup) => Err(cleanup),
            };
        }
        Ok(FrameBackedSpace {
            generation: self.generation,
        })
    }

    fn map_zeroed(
        &mut self,
        space: Self::Space,
        virtual_address: u64,
        memory_size: usize,
    ) -> Result<Self::Mapping, Self::Error> {
        let end = virtual_address
            .checked_add(memory_size as u64)
            .ok_or(FrameBackedError::InvalidRange)?;
        if !self.active
            || space.generation != self.generation
            || memory_size == 0
            || virtual_address & (PAGE_SIZE as u64 - 1) != 0
            || virtual_address < self.image_start
            || end > self.image_end
        {
            return Err(FrameBackedError::InvalidRange);
        }
        let pages_needed = memory_size.div_ceil(PAGE_SIZE);
        if pages_needed == 0
            || pages_needed > u8::MAX as usize
            || self.page_count + pages_needed > self.pages.len()
        {
            return Err(FrameBackedError::CapacityExceeded);
        }
        let mapping_index = self.mapping_count;
        if mapping_index >= self.mappings.len() {
            return Err(FrameBackedError::CapacityExceeded);
        }
        let first_page = self.page_count;
        for page in 0..pages_needed {
            let page_virtual = virtual_address
                .checked_add((page * PAGE_SIZE) as u64)
                .ok_or(FrameBackedError::InvalidRange)?;
            let frame = self.allocate_owned()?;
            let _ = self.ensure_leaf_slot(page_virtual)?;
            self.pages[self.page_count] = PageRecord {
                frame,
                virtual_address: page_virtual,
            };
            self.page_count += 1;
        }
        self.mappings[mapping_index] = MappingRecord {
            occupied: true,
            sealed: false,
            generation: self.generation,
            virtual_address,
            memory_size,
            first_page: first_page as u8,
            page_count: pages_needed as u8,
            permissions: MappingPermissions {
                readable: false,
                writable: false,
                executable: false,
            },
        };
        self.mapping_count += 1;
        Ok(FrameBackedMapping {
            slot: mapping_index as u8,
            generation: self.generation,
        })
    }

    fn copy_into(
        &mut self,
        mapping: Self::Mapping,
        offset: usize,
        bytes: &[u8],
    ) -> Result<(), Self::Error> {
        let record = self.mapping(mapping)?;
        if record.sealed
            || offset
                .checked_add(bytes.len())
                .is_none_or(|end| end > record.memory_size)
        {
            return Err(FrameBackedError::InvalidRange);
        }
        let mut copied = 0;
        while copied < bytes.len() {
            let absolute = offset + copied;
            let page = absolute / PAGE_SIZE;
            let within_page = absolute % PAGE_SIZE;
            let length = (PAGE_SIZE - within_page).min(bytes.len() - copied);
            let frame = self.frame_for_mapping_page(record, page)?;
            self.memory
                .write_bytes(frame, within_page, &bytes[copied..copied + length])
                .map_err(FrameBackedError::Memory)?;
            copied += length;
        }
        Ok(())
    }

    fn verify_contents(
        &mut self,
        mapping: Self::Mapping,
        initialized: &[u8],
        memory_size: usize,
    ) -> Result<bool, Self::Error> {
        let record = self.mapping(mapping)?;
        if record.sealed || memory_size != record.memory_size || initialized.len() > memory_size {
            return Err(FrameBackedError::InvalidRange);
        }
        let mut offset = 0;
        while offset < initialized.len() {
            let page = offset / PAGE_SIZE;
            let within_page = offset % PAGE_SIZE;
            let length = (PAGE_SIZE - within_page).min(initialized.len() - offset);
            let frame = self.frame_for_mapping_page(record, page)?;
            if !self
                .memory
                .bytes_equal(frame, within_page, &initialized[offset..offset + length])
                .map_err(FrameBackedError::Memory)?
            {
                return Ok(false);
            }
            offset += length;
        }
        while offset < memory_size {
            let page = offset / PAGE_SIZE;
            let within_page = offset % PAGE_SIZE;
            let length = (PAGE_SIZE - within_page).min(memory_size - offset);
            let frame = self.frame_for_mapping_page(record, page)?;
            if !self
                .memory
                .bytes_zero(frame, within_page, length)
                .map_err(FrameBackedError::Memory)?
            {
                return Ok(false);
            }
            offset += length;
        }
        Ok(true)
    }

    fn seal(
        &mut self,
        mapping: Self::Mapping,
        permissions: MappingPermissions,
    ) -> Result<(), Self::Error> {
        let record = self.mapping(mapping)?;
        if record.sealed
            || !permissions.readable
            || (permissions.writable && permissions.executable)
        {
            return Err(FrameBackedError::UnsupportedPermissions);
        }
        for page in 0..usize::from(record.page_count) {
            let page_record = self.pages[usize::from(record.first_page) + page];
            let (table, index) = self.leaf_slot(page_record.virtual_address)?;
            let mut entry = page_record.frame.as_u64() | ENTRY_PRESENT | ENTRY_USER;
            if permissions.writable {
                entry |= ENTRY_WRITABLE;
            }
            if !permissions.executable {
                entry |= ENTRY_NO_EXECUTE;
            }
            self.memory
                .write_entry(table, index, entry)
                .map_err(FrameBackedError::Memory)?;
        }
        self.mappings[usize::from(mapping.slot)].permissions = permissions;
        self.mappings[usize::from(mapping.slot)].sealed = true;
        Ok(())
    }

    fn commit(
        &mut self,
        space: Self::Space,
        entry_point: u64,
    ) -> Result<Self::Process, Self::Error> {
        if !self.active
            || space.generation != self.generation
            || self.mapping_count == 0
            || self.mappings[..self.mapping_count]
                .iter()
                .any(|mapping| !mapping.sealed)
            || !self.mappings[..self.mapping_count].iter().any(|mapping| {
                mapping.permissions.executable
                    && entry_point >= mapping.virtual_address
                    && entry_point < mapping.virtual_address + mapping.memory_size as u64
            })
        {
            return Err(FrameBackedError::InvalidState);
        }
        let root = self.root.ok_or(FrameBackedError::InvalidState)?;
        self.active = false;
        self.process_generation = next_generation(self.process_generation);
        self.process_live = true;
        self.process_info = ProcessImageInfo {
            entry_point,
            segment_count: self.mapping_count,
            address_space_root: Some(root.as_u64()),
            owned_frames: self.owned_frame_count,
            initial_stack_pointer: None,
        };
        Ok(ProcessImageHandle::new(0, self.process_generation))
    }

    fn abort(&mut self, space: Self::Space) -> Result<(), Self::Error> {
        if !self.active || space.generation != self.generation {
            return Err(FrameBackedError::InvalidHandle);
        }
        self.cleanup_transaction()
    }

    fn process_info(&self, process: &Self::Process) -> Option<ProcessImageInfo> {
        (self.process_live
            && process.slot() == 0
            && process.generation() == self.process_generation)
            .then_some(self.process_info)
    }

    fn process_generation(&self, process: &Self::Process) -> Option<u32> {
        self.process_info(process).map(|_| process.generation())
    }

    unsafe fn validate_activation(
        &mut self,
        process: &Self::Process,
        _authority: &Capability<'_, ProcessInstallControl>,
    ) -> Result<(), Self::Error> {
        let root = self
            .process_info(process)
            .and_then(|info| info.address_space_root)
            .ok_or(FrameBackedError::InvalidHandle)?;

        #[cfg(target_os = "none")]
        {
            let _interrupt_guard = InterruptGuard::<X86_64>::enter();
            // SAFETY: The serialized bootstrap phase owns this inactive root.
            // Its upper PML4 half was copied from the active kernel hierarchy,
            // so this code, stack, and direct map remain reachable.
            let original_root = unsafe { active_page_table_root() };
            unsafe { load_page_table_root(root) };
            if unsafe { active_page_table_root() } != root {
                unsafe { load_page_table_root(original_root) };
                return Err(FrameBackedError::ActivationFailed);
            }
            // Reaching this point while the process root is active proves the
            // inherited higher-half execution mappings are operational.
            unsafe { load_page_table_root(original_root) };
            if unsafe { active_page_table_root() } != original_root {
                return Err(FrameBackedError::RestoreFailed);
            }
        }

        #[cfg(not(target_os = "none"))]
        let _ = root;

        Ok(())
    }

    fn release_process(&mut self, process: &Self::Process) -> Result<(), Self::Error> {
        if self.process_info(process).is_none() {
            return Err(FrameBackedError::InvalidHandle);
        }
        self.process_live = false;
        self.reset_records();
        self.release_owned()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameBackedError<MemoryError> {
    Memory(MemoryError),
    InvalidState,
    InvalidHandle,
    InvalidRange,
    InvalidUserRange,
    InvalidPhysicalFrame,
    CapacityExceeded,
    MappingConflict,
    CorruptHierarchy,
    UnsupportedPermissions,
    ActivationFailed,
    RestoreFailed,
}

fn page_indices<MemoryError>(address: u64) -> Result<[usize; 4], FrameBackedError<MemoryError>> {
    if address >= 0x0000_8000_0000_0000 {
        return Err(FrameBackedError::InvalidUserRange);
    }
    Ok([
        ((address >> 39) & 0x1ff) as usize,
        ((address >> 30) & 0x1ff) as usize,
        ((address >> 21) & 0x1ff) as usize,
        ((address >> 12) & 0x1ff) as usize,
    ])
}

const fn next_generation(generation: u32) -> u32 {
    let next = generation.wrapping_add(1);
    if next == 0 { 1 } else { next }
}

#[cfg(test)]
mod tests {
    use blacklab::oureboros::{
        FractalCatalog, FractalClass, FractalRecipe, FractalSeed, MINIMAL_X86_64_ELF_BYTES,
        TargetArchitecture, measure_recipe,
    };

    use crate::capability::{Authority, ProcessInstallControl, UserlandImageControl};
    use crate::process::image::prepare_user_image;
    use crate::process::install::{InstallError, install_user_image};

    use super::*;

    struct TestMemory<const FRAMES: usize> {
        frames: [[u8; PAGE_SIZE]; FRAMES],
        allocated: [bool; FRAMES],
        fail_release_once: bool,
    }

    impl<const FRAMES: usize> TestMemory<FRAMES> {
        const fn new() -> Self {
            Self {
                frames: [[0; PAGE_SIZE]; FRAMES],
                allocated: [false; FRAMES],
                fail_release_once: false,
            }
        }

        fn in_use(&self) -> usize {
            self.allocated
                .iter()
                .filter(|allocated| **allocated)
                .count()
        }

        fn range(
            &self,
            frame: PhysicalAddress,
            offset: usize,
            length: usize,
        ) -> Result<(usize, core::ops::Range<usize>), TestMemoryError> {
            let index = usize::try_from(frame.as_u64() / PAGE_SIZE as u64)
                .map_err(|_| TestMemoryError::Invalid)?;
            let end = offset.checked_add(length).ok_or(TestMemoryError::Invalid)?;
            if index >= FRAMES || end > PAGE_SIZE {
                return Err(TestMemoryError::Invalid);
            }
            Ok((index, offset..end))
        }
    }

    impl<const FRAMES: usize> ProcessFrameMemory for TestMemory<FRAMES> {
        type Error = TestMemoryError;

        fn allocate_zeroed(&mut self) -> Result<PhysicalAddress, Self::Error> {
            let index = self
                .allocated
                .iter()
                .enumerate()
                .skip(1)
                .find_map(|(index, allocated)| (!*allocated).then_some(index))
                .ok_or(TestMemoryError::Exhausted)?;
            self.allocated[index] = true;
            self.frames[index].fill(0);
            Ok(PhysicalAddress::new((index * PAGE_SIZE) as u64))
        }

        fn release(&mut self, frame: PhysicalAddress) -> Result<(), Self::Error> {
            let (index, _) = self.range(frame, 0, PAGE_SIZE)?;
            if !self.allocated[index] {
                return Err(TestMemoryError::Invalid);
            }
            if self.fail_release_once {
                self.fail_release_once = false;
                return Err(TestMemoryError::ReleaseFailed);
            }
            self.allocated[index] = false;
            self.frames[index].fill(0);
            Ok(())
        }

        fn read_entry(&self, table: PhysicalAddress, index: usize) -> Result<u64, Self::Error> {
            let (frame, range) = self.range(table, index * 8, 8)?;
            Ok(u64::from_le_bytes(
                self.frames[frame][range]
                    .try_into()
                    .map_err(|_| TestMemoryError::Invalid)?,
            ))
        }

        fn write_entry(
            &mut self,
            table: PhysicalAddress,
            index: usize,
            value: u64,
        ) -> Result<(), Self::Error> {
            let (frame, range) = self.range(table, index * 8, 8)?;
            self.frames[frame][range].copy_from_slice(&value.to_le_bytes());
            Ok(())
        }

        fn write_bytes(
            &mut self,
            frame: PhysicalAddress,
            offset: usize,
            bytes: &[u8],
        ) -> Result<(), Self::Error> {
            let (frame, range) = self.range(frame, offset, bytes.len())?;
            self.frames[frame][range].copy_from_slice(bytes);
            Ok(())
        }

        fn bytes_equal(
            &self,
            frame: PhysicalAddress,
            offset: usize,
            bytes: &[u8],
        ) -> Result<bool, Self::Error> {
            let (frame, range) = self.range(frame, offset, bytes.len())?;
            Ok(self.frames[frame][range] == *bytes)
        }

        fn bytes_zero(
            &self,
            frame: PhysicalAddress,
            offset: usize,
            length: usize,
        ) -> Result<bool, Self::Error> {
            let (frame, range) = self.range(frame, offset, length)?;
            Ok(self.frames[frame][range].iter().all(|byte| *byte == 0))
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum TestMemoryError {
        Exhausted,
        Invalid,
        ReleaseFailed,
    }

    fn catalog() -> FractalCatalog {
        let recipe = FractalRecipe {
            algorithm_version: 2,
            base_entropy: 1,
            structural_mutator: 2,
        };
        let mut catalog = FractalCatalog::new();
        catalog
            .plant_seed(FractalSeed {
                inode_id: 1,
                class: FractalClass::Executable,
                architecture: TargetArchitecture::X86_64,
                recipe,
                unfolded_size_bytes: MINIMAL_X86_64_ELF_BYTES as u32,
                entry_offset: 128,
                expected_sha256: measure_recipe(recipe, MINIMAL_X86_64_ELF_BYTES).unwrap(),
            })
            .unwrap();
        catalog
    }

    #[test]
    fn builds_hardware_entries_and_reclaims_every_owned_frame() {
        let mut memory = TestMemory::<16>::new();
        let inherited = 0x1234_5000 | ENTRY_PRESENT | ENTRY_WRITABLE;
        memory
            .write_entry(PhysicalAddress::new(0), 256, inherited)
            .unwrap();
        let authority = unsafe { Authority::assume_root() };
        let install_control = authority.grant::<ProcessInstallControl>();
        let mut backend =
            FrameBackedAddressSpace::new(memory, PhysicalAddress::new(0), &install_control);
        let catalog = catalog();
        let mut bytes = [0_u8; MINIMAL_X86_64_ELF_BYTES];
        let image_control = authority.grant::<UserlandImageControl>();
        let artifact = catalog.materialize(1, &mut bytes).unwrap();
        let image = prepare_user_image(artifact, &image_control).unwrap();
        let installed = install_user_image(image, &mut backend, &install_control).unwrap();
        let info = backend.process_info(&installed.process).unwrap();
        // SAFETY: Host tests exercise structural activation validation only;
        // privileged CR3 switching is compiled solely for the bare-metal target.
        unsafe {
            backend
                .validate_activation(&installed.process, &install_control)
                .unwrap();
        }
        let root = PhysicalAddress::new(info.address_space_root.unwrap());
        assert_eq!(info.owned_frames, 5);
        assert_eq!(backend.memory().read_entry(root, 256), Ok(inherited));
        assert_eq!(
            backend.install_initial_stack(&installed.process, &install_control),
            Ok(INITIAL_USER_STACK_POINTER)
        );
        let stacked = backend.process_info(&installed.process).unwrap();
        assert_eq!(stacked.owned_frames, 6);
        assert_eq!(
            stacked.initial_stack_pointer,
            Some(INITIAL_USER_STACK_POINTER)
        );

        let p3 = backend.memory().read_entry(root, 0).unwrap() & PAGE_ADDRESS_MASK;
        let p2 = backend
            .memory()
            .read_entry(PhysicalAddress::new(p3), 0)
            .unwrap()
            & PAGE_ADDRESS_MASK;
        let p1 = backend
            .memory()
            .read_entry(PhysicalAddress::new(p2), 0)
            .unwrap()
            & PAGE_ADDRESS_MASK;
        let leaf = backend
            .memory()
            .read_entry(PhysicalAddress::new(p1), 1)
            .unwrap();
        assert_eq!(
            leaf & (ENTRY_PRESENT | ENTRY_USER),
            ENTRY_PRESENT | ENTRY_USER
        );
        assert_eq!(leaf & (ENTRY_WRITABLE | ENTRY_NO_EXECUTE), 0);
        let data = PhysicalAddress::new(leaf & PAGE_ADDRESS_MASK);
        assert_eq!(
            backend
                .memory()
                .bytes_equal(data, 34, b"PID1 syscall write\n"),
            Ok(true)
        );
        assert_eq!(
            backend.memory().bytes_zero(data, 53, PAGE_SIZE - 53),
            Ok(true)
        );
        let stack_leaf = backend
            .memory()
            .read_entry(PhysicalAddress::new(p1), 7)
            .unwrap();
        assert_eq!(
            stack_leaf & (ENTRY_PRESENT | ENTRY_WRITABLE | ENTRY_USER | ENTRY_NO_EXECUTE),
            ENTRY_PRESENT | ENTRY_WRITABLE | ENTRY_USER | ENTRY_NO_EXECUTE
        );
        assert_eq!(
            backend.install_initial_stack(&installed.process, &install_control),
            Err(FrameBackedError::InvalidState)
        );

        backend.release_process(&installed.process).unwrap();
        assert_eq!(backend.process_info(&installed.process), None);
        // SAFETY: This verifies that released handles cannot authorize a later
        // activation; no privileged operation is compiled into this host test.
        assert_eq!(
            unsafe { backend.validate_activation(&installed.process, &install_control) },
            Err(FrameBackedError::InvalidHandle)
        );
        assert_eq!(backend.memory().in_use(), 0);
    }

    #[test]
    fn allocation_failure_aborts_and_reclaims_partial_hierarchy() {
        let memory = TestMemory::<5>::new();
        let catalog = catalog();
        let mut bytes = [0_u8; MINIMAL_X86_64_ELF_BYTES];
        let authority = unsafe { Authority::assume_root() };
        let image_control = authority.grant::<UserlandImageControl>();
        let install_control = authority.grant::<ProcessInstallControl>();
        let mut backend =
            FrameBackedAddressSpace::new(memory, PhysicalAddress::new(0), &install_control);
        let artifact = catalog.materialize(1, &mut bytes).unwrap();
        let image = prepare_user_image(artifact, &image_control).unwrap();
        assert_eq!(
            install_user_image(image, &mut backend, &install_control),
            Err(InstallError::Backend(FrameBackedError::Memory(
                TestMemoryError::Exhausted
            )))
        );
        assert_eq!(backend.memory().in_use(), 0);
    }

    #[test]
    fn staging_is_nonpresent_before_rw_nx_sealing() {
        let memory = TestMemory::<16>::new();
        let authority = unsafe { Authority::assume_root() };
        let install_control = authority.grant::<ProcessInstallControl>();
        let mut backend =
            FrameBackedAddressSpace::new(memory, PhysicalAddress::new(0), &install_control);
        let space = backend.begin(0x1000, 0x2000).unwrap();
        let mapping = backend.map_zeroed(space, 0x1000, PAGE_SIZE).unwrap();
        let root = backend.root.unwrap();
        let p3 = backend.memory().read_entry(root, 0).unwrap() & PAGE_ADDRESS_MASK;
        let p2 = backend
            .memory()
            .read_entry(PhysicalAddress::new(p3), 0)
            .unwrap()
            & PAGE_ADDRESS_MASK;
        let p1 = backend
            .memory()
            .read_entry(PhysicalAddress::new(p2), 0)
            .unwrap()
            & PAGE_ADDRESS_MASK;
        assert_eq!(
            backend.memory().read_entry(PhysicalAddress::new(p1), 1),
            Ok(0)
        );

        backend
            .seal(
                mapping,
                MappingPermissions {
                    readable: true,
                    writable: true,
                    executable: false,
                },
            )
            .unwrap();
        let leaf = backend
            .memory()
            .read_entry(PhysicalAddress::new(p1), 1)
            .unwrap();
        assert_eq!(
            leaf & (ENTRY_PRESENT | ENTRY_WRITABLE | ENTRY_USER | ENTRY_NO_EXECUTE),
            ENTRY_PRESENT | ENTRY_WRITABLE | ENTRY_USER | ENTRY_NO_EXECUTE
        );
        backend.abort(space).unwrap();
        assert_eq!(backend.memory().in_use(), 0);
    }

    #[test]
    fn failed_frame_release_remains_owned_for_retry() {
        let memory = TestMemory::<16>::new();
        let authority = unsafe { Authority::assume_root() };
        let install_control = authority.grant::<ProcessInstallControl>();
        let physical_memory = authority.grant::<PhysicalMemoryControl>();
        let mut backend =
            FrameBackedAddressSpace::new(memory, PhysicalAddress::new(0), &install_control);
        let space = backend.begin(0x1000, 0x2000).unwrap();
        backend.map_zeroed(space, 0x1000, PAGE_SIZE).unwrap();
        backend.memory_mut().fail_release_once = true;
        assert_eq!(
            backend.abort(space),
            Err(FrameBackedError::Memory(TestMemoryError::ReleaseFailed))
        );
        assert_eq!(backend.owned_frame_count(), 1);
        backend.retry_cleanup(&physical_memory).unwrap();
        assert_eq!(backend.owned_frame_count(), 0);
        assert_eq!(backend.memory().in_use(), 0);
    }
}
