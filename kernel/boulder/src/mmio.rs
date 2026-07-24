use core::ptr::NonNull;
use core::sync::atomic::{Ordering, compiler_fence};

use sisyphus_driver_abi::{
    Handle, STATUS_BUSY, STATUS_INVALID_ARGUMENT, STATUS_NOT_FOUND, STATUS_OK, STATUS_UNSUPPORTED,
    Status,
};

use crate::arch::x86_64::invalidate_page;
use crate::shim::{MmioMapping, MmioService};
use crate::sync::SpinLock;

pub const HIGHER_HALF_DIRECT_MAP_BASE: usize = 0xffff_8000_0000_0000;
pub const KERNEL_VIRTUAL_BASE: usize = 0xffff_ffff_8000_0000;
pub const MMIO_WINDOW_BASE: usize = 0xffff_8080_0000_0000;
pub const EARLY_MAPPED_PHYSICAL_LIMIT: u64 = 1024 * 1024 * 1024;

const PAGE_SIZE: usize = 4096;
const MMIO_SLOTS: usize = 512;
const SLOT_WORDS: usize = MMIO_SLOTS / u64::BITS as usize;
const MAXIMUM_MAPPINGS: usize = 64;
const PAGE_ADDRESS_MASK: u64 = 0x000f_ffff_ffff_f000;
const PTE_PRESENT: u64 = 1 << 0;
const PTE_WRITABLE: u64 = 1 << 1;
const PTE_WRITE_THROUGH: u64 = 1 << 3;
const PTE_CACHE_DISABLE: u64 = 1 << 4;
const MMIO_PAGE_FLAGS: u64 = PTE_PRESENT | PTE_WRITABLE | PTE_WRITE_THROUGH | PTE_CACHE_DISABLE;

unsafe extern "C" {
    static mut mmio_p1_table: [u64; MMIO_SLOTS];
}

#[derive(Clone, Copy)]
struct MappingRecord {
    generation: u32,
    first_slot: u16,
    page_count: u16,
    active: bool,
}

impl MappingRecord {
    const EMPTY: Self = Self {
        generation: 0,
        first_slot: 0,
        page_count: 0,
        active: false,
    };
}

struct EarlyMmioMapper {
    occupied: [u64; SLOT_WORDS],
    records: [MappingRecord; MAXIMUM_MAPPINGS],
    next_generation: u32,
}

impl EarlyMmioMapper {
    const fn new() -> Self {
        Self {
            occupied: [0; SLOT_WORDS],
            records: [MappingRecord::EMPTY; MAXIMUM_MAPPINGS],
            next_generation: 1,
        }
    }

    fn map(
        &mut self,
        physical_address: u64,
        length: usize,
        flags: u64,
    ) -> Result<MmioMapping, Status> {
        if length == 0 {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        if flags != 0 {
            return Err(STATUS_UNSUPPORTED);
        }
        let page_offset = (physical_address as usize) & (PAGE_SIZE - 1);
        let span = page_offset
            .checked_add(length)
            .ok_or(STATUS_INVALID_ARGUMENT)?;
        let page_count = span.div_ceil(PAGE_SIZE);
        if page_count == 0 || page_count > MMIO_SLOTS {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        let physical_base = physical_address & !(PAGE_SIZE as u64 - 1);
        let physical_end = physical_base
            .checked_add((page_count * PAGE_SIZE) as u64)
            .ok_or(STATUS_INVALID_ARGUMENT)?;
        if physical_end - 1 > PAGE_ADDRESS_MASK {
            return Err(STATUS_INVALID_ARGUMENT);
        }

        let record_index = self
            .records
            .iter()
            .position(|record| !record.active)
            .ok_or(STATUS_BUSY)?;
        let first_slot = self.find_slots(page_count).ok_or(STATUS_BUSY)?;
        let generation = self.next_generation.max(1);
        self.next_generation = self.next_generation.wrapping_add(1).max(1);

        for offset in 0..page_count {
            self.set_slot(first_slot + offset, true);
            let physical = physical_base + (offset * PAGE_SIZE) as u64;
            let virtual_address = MMIO_WINDOW_BASE + (first_slot + offset) * PAGE_SIZE;
            // SAFETY: Bootstrap assembly installed this aligned P1 table in the
            // active hierarchy. The lock gives exclusive updates to each slot.
            unsafe {
                page_table()
                    .add(first_slot + offset)
                    .write_volatile(physical | MMIO_PAGE_FLAGS);
            }
            compiler_fence(Ordering::SeqCst);
            // SAFETY: The PTE write is complete and this address belongs to the
            // dedicated MMIO window.
            unsafe { invalidate_page(virtual_address) };
        }

        self.records[record_index] = MappingRecord {
            generation,
            first_slot: first_slot as u16,
            page_count: page_count as u16,
            active: true,
        };
        let handle = (u64::from(generation) << 32) | (record_index as u64 + 1);
        let pointer =
            NonNull::new((MMIO_WINDOW_BASE + first_slot * PAGE_SIZE + page_offset) as *mut u8)
                .ok_or(STATUS_INVALID_ARGUMENT)?;
        Ok(MmioMapping { handle, pointer })
    }

    fn unmap(&mut self, handle: Handle) -> Status {
        let record_number = (handle & 0xffff_ffff) as usize;
        let generation = (handle >> 32) as u32;
        if record_number == 0 || record_number > self.records.len() {
            return STATUS_NOT_FOUND;
        }
        let record_index = record_number - 1;
        let record = self.records[record_index];
        if !record.active || record.generation != generation {
            return STATUS_NOT_FOUND;
        }

        for offset in 0..record.page_count as usize {
            let slot = record.first_slot as usize + offset;
            let virtual_address = MMIO_WINDOW_BASE + slot * PAGE_SIZE;
            // SAFETY: This active record owns every listed page-table slot.
            unsafe { page_table().add(slot).write_volatile(0) };
            compiler_fence(Ordering::SeqCst);
            // SAFETY: The PTE was cleared before invalidating its translation.
            unsafe { invalidate_page(virtual_address) };
            self.set_slot(slot, false);
        }
        self.records[record_index] = MappingRecord::EMPTY;
        STATUS_OK
    }

    fn find_slots(&self, page_count: usize) -> Option<usize> {
        let mut run_start = 0;
        let mut run_length = 0;
        for slot in 0..MMIO_SLOTS {
            if self.slot_is_set(slot) {
                run_length = 0;
            } else {
                if run_length == 0 {
                    run_start = slot;
                }
                run_length += 1;
                if run_length == page_count {
                    return Some(run_start);
                }
            }
        }
        None
    }

    fn slot_is_set(&self, slot: usize) -> bool {
        self.occupied[slot / 64] & (1_u64 << (slot % 64)) != 0
    }

    fn set_slot(&mut self, slot: usize, occupied: bool) {
        let word = &mut self.occupied[slot / 64];
        let mask = 1_u64 << (slot % 64);
        if occupied {
            *word |= mask;
        } else {
            *word &= !mask;
        }
    }
}

unsafe fn page_table() -> *mut u64 {
    core::ptr::addr_of_mut!(mmio_p1_table).cast::<u64>()
}

pub struct KernelMmio {
    mapper: SpinLock<EarlyMmioMapper>,
}

impl KernelMmio {
    const fn new() -> Self {
        Self {
            mapper: SpinLock::new(EarlyMmioMapper::new()),
        }
    }
}

impl MmioService for KernelMmio {
    fn map(&self, physical_address: u64, length: usize, flags: u64) -> Result<MmioMapping, Status> {
        self.mapper.lock().map(physical_address, length, flags)
    }

    fn unmap(&self, mapping: Handle) -> Status {
        self.mapper.lock().unmap(mapping)
    }
}

static KERNEL_MMIO: KernelMmio = KernelMmio::new();

pub fn kernel_mmio() -> &'static KernelMmio {
    &KERNEL_MMIO
}

pub fn direct_map_address(physical_address: u64) -> Option<usize> {
    if physical_address >= EARLY_MAPPED_PHYSICAL_LIMIT {
        return None;
    }
    HIGHER_HALF_DIRECT_MAP_BASE.checked_add(physical_address as usize)
}

#[cfg(target_os = "none")]
unsafe extern "C" {
    static __kernel_start: u8;
    static __kernel_end: u8;
}

/// Converts a retained kernel or direct-map virtual span to its physical base.
///
/// The complete span must lie inside one established linear mapping. This
/// function never guesses a load offset from a pointer.
#[cfg(target_os = "none")]
pub fn kernel_virtual_to_physical(
    virtual_address: usize,
    length: usize,
) -> Option<u64> {
    let end = virtual_address.checked_add(length)?;

    let direct_start = HIGHER_HALF_DIRECT_MAP_BASE;
    let direct_end = direct_start.checked_add(
        usize::try_from(EARLY_MAPPED_PHYSICAL_LIMIT).ok()?,
    )?;
    if virtual_address >= direct_start && end <= direct_end {
        return u64::try_from(virtual_address - direct_start).ok();
    }

    let kernel_start = core::ptr::addr_of!(__kernel_start) as usize;
    let kernel_end = core::ptr::addr_of!(__kernel_end) as usize;
    if virtual_address >= kernel_start && end <= kernel_end {
        return u64::try_from(
            virtual_address.checked_sub(KERNEL_VIRTUAL_BASE)?,
        )
        .ok();
    }

    None
}

#[cfg(not(target_os = "none"))]
pub fn kernel_virtual_to_physical(
    virtual_address: usize,
    length: usize,
) -> Option<u64> {
    let _ = virtual_address.checked_add(length);
    None
}

// ─── TYPED DEVICE WINDOW ────────────────────────────────────────────────────

use crate::capability::{Capability, DeviceMemoryRight};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct WindowId(Handle);

impl WindowId {
    pub const fn raw(self) -> Handle {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MmioAccessError {
    Map(Status),
    OutOfBounds,
    Misaligned,
}

pub struct MmioWindow {
    id: WindowId,
    base: NonNull<u8>,
    length: usize,
}

impl MmioWindow {
    pub fn map(
        physical_address: u64,
        length: usize,
        _authority: &Capability<'_, DeviceMemoryRight>,
    ) -> Result<Self, MmioAccessError> {
        let mapping = kernel_mmio()
            .map(physical_address, length, 0)
            .map_err(MmioAccessError::Map)?;

        Ok(Self {
            id: WindowId(mapping.handle),
            base: mapping.pointer,
            length,
        })
    }

    pub const fn id(&self) -> WindowId {
        self.id
    }

    pub const fn length(&self) -> usize {
        self.length
    }

    pub fn read_u16(&self, offset: usize) -> Result<u16, MmioAccessError> {
        let pointer = self.checked_pointer::<u16>(offset)?;
        compiler_fence(Ordering::SeqCst);

        // SAFETY: checked_pointer verified range and alignment, and the mapping
        // remains owned by this non-Copy MmioWindow.
        let value = unsafe { pointer.read_volatile() };

        compiler_fence(Ordering::SeqCst);
        Ok(value)
    }

    pub fn write_u16(&self, offset: usize, value: u16) -> Result<(), MmioAccessError> {
        let pointer = self.checked_pointer::<u16>(offset)?;
        compiler_fence(Ordering::SeqCst);

        // SAFETY: checked_pointer verified range and alignment, and the mapping
        // remains owned by this non-Copy MmioWindow.
        unsafe { pointer.write_volatile(value) };

        compiler_fence(Ordering::SeqCst);
        Ok(())
    }

    pub fn read_u32(&self, offset: usize) -> Result<u32, MmioAccessError> {
        let pointer = self.checked_pointer::<u32>(offset)?;
        compiler_fence(Ordering::SeqCst);

        // SAFETY: checked_pointer verified range and alignment, and the mapping
        // remains owned by this non-Copy MmioWindow.
        let value = unsafe { pointer.read_volatile() };

        compiler_fence(Ordering::SeqCst);
        Ok(value)
    }

    pub fn write_u32(&self, offset: usize, value: u32) -> Result<(), MmioAccessError> {
        let pointer = self.checked_pointer::<u32>(offset)?;
        compiler_fence(Ordering::SeqCst);

        // SAFETY: checked_pointer verified range and alignment, and the mapping
        // remains owned by this non-Copy MmioWindow.
        unsafe { pointer.write_volatile(value) };

        compiler_fence(Ordering::SeqCst);
        Ok(())
    }

    pub fn close(self, _authority: &Capability<'_, DeviceMemoryRight>) -> Status {
        kernel_mmio().unmap(self.id.0)
    }

    fn checked_pointer<T>(&self, offset: usize) -> Result<*mut T, MmioAccessError> {
        let end = offset
            .checked_add(core::mem::size_of::<T>())
            .ok_or(MmioAccessError::OutOfBounds)?;

        if end > self.length {
            return Err(MmioAccessError::OutOfBounds);
        }

        let address = self.base.as_ptr() as usize + offset;
        if address % core::mem::align_of::<T>() != 0 {
            return Err(MmioAccessError::Misaligned);
        }

        Ok(address as *mut T)
    }
}
