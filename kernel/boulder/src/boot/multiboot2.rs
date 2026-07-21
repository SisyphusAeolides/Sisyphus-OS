use abyss::memory::{MemoryMap, MemoryMapError, MemoryRegion, MemoryRegionKind};
use abyss::paging::PhysicalAddress;

const TAG_END: u32 = 0;
const TAG_MEMORY_MAP: u32 = 6;
const HEADER_SIZE: usize = 8;
const TAG_HEADER_SIZE: usize = 8;
const MEMORY_MAP_HEADER_SIZE: usize = 16;
const MEMORY_MAP_ENTRY_MINIMUM_SIZE: usize = 24;
const MAXIMUM_BOOT_INFORMATION_SIZE: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy)]
pub struct BootInformation {
    address: usize,
    total_size: usize,
}

impl BootInformation {
    /// Validates a Multiboot2 information structure supplied by the bootloader.
    ///
    /// # Safety
    ///
    /// `address` must point to readable identity-mapped memory containing the
    /// complete Multiboot2 information structure for the duration of its use.
    pub unsafe fn load(address: usize) -> Result<Self, BootError> {
        if address == 0 || address & 7 != 0 {
            return Err(BootError::InvalidAddress);
        }
        // SAFETY: The caller guarantees at least the Multiboot2 header is
        // readable at this address.
        let total_size = unsafe { read_u32(address) } as usize;
        if !(HEADER_SIZE..=MAXIMUM_BOOT_INFORMATION_SIZE).contains(&total_size)
            || total_size & 7 != 0
            || address.checked_add(total_size).is_none()
        {
            return Err(BootError::InvalidTotalSize);
        }
        Ok(Self {
            address,
            total_size,
        })
    }

    pub const fn address(self) -> usize {
        self.address
    }

    pub const fn total_size(self) -> usize {
        self.total_size
    }

    pub fn memory_map(self) -> Result<MemoryMap, BootError> {
        let tag = self
            .find_tag(TAG_MEMORY_MAP)?
            .ok_or(BootError::MissingMemoryMap)?;
        if tag.size < MEMORY_MAP_HEADER_SIZE {
            return Err(BootError::MalformedMemoryMap);
        }

        // SAFETY: find_tag validated that the complete tag lies inside the boot
        // information structure.
        let entry_size = unsafe { read_u32(tag.address + 8) } as usize;
        let entries_size = tag.size - MEMORY_MAP_HEADER_SIZE;
        if entry_size < MEMORY_MAP_ENTRY_MINIMUM_SIZE || entries_size % entry_size != 0 {
            return Err(BootError::MalformedMemoryMap);
        }

        let mut map = MemoryMap::new();
        let mut entry = tag.address + MEMORY_MAP_HEADER_SIZE;
        let end = tag.address + tag.size;
        while entry < end {
            // SAFETY: Entry bounds and minimum field size were checked above.
            let base = unsafe { read_u64(entry) };
            let length = unsafe { read_u64(entry + 8) };
            let entry_type = unsafe { read_u32(entry + 16) };
            let region_end = base.checked_add(length).ok_or(BootError::AddressOverflow)?;
            if length != 0 {
                map.push(MemoryRegion::new(
                    PhysicalAddress::new(base),
                    PhysicalAddress::new(region_end),
                    region_kind(entry_type),
                ))?;
            }
            entry += entry_size;
        }
        if map.is_empty() {
            return Err(BootError::MissingMemoryMap);
        }
        Ok(map)
    }

    fn find_tag(self, wanted_type: u32) -> Result<Option<Tag>, BootError> {
        let end = self.address + self.total_size;
        let mut address = self.address + HEADER_SIZE;
        while address < end {
            if end - address < TAG_HEADER_SIZE {
                return Err(BootError::MalformedTag);
            }
            // SAFETY: The fixed tag header lies inside the validated structure.
            let tag_type = unsafe { read_u32(address) };
            let size = unsafe { read_u32(address + 4) } as usize;
            if size < TAG_HEADER_SIZE || address.checked_add(size).is_none_or(|next| next > end) {
                return Err(BootError::MalformedTag);
            }
            if tag_type == wanted_type {
                return Ok(Some(Tag { address, size }));
            }
            if tag_type == TAG_END {
                return Ok(None);
            }
            address = align_up_8(address + size).ok_or(BootError::AddressOverflow)?;
        }
        Err(BootError::MissingEndTag)
    }
}

#[derive(Clone, Copy)]
struct Tag {
    address: usize,
    size: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootError {
    InvalidAddress,
    InvalidTotalSize,
    MalformedTag,
    MissingEndTag,
    MissingMemoryMap,
    MalformedMemoryMap,
    AddressOverflow,
    TooManyMemoryRegions,
}

impl From<MemoryMapError> for BootError {
    fn from(error: MemoryMapError) -> Self {
        match error {
            MemoryMapError::InvalidRegion => Self::MalformedMemoryMap,
            MemoryMapError::CapacityExceeded => Self::TooManyMemoryRegions,
        }
    }
}

const fn region_kind(entry_type: u32) -> MemoryRegionKind {
    match entry_type {
        1 => MemoryRegionKind::Usable,
        3 => MemoryRegionKind::AcpiReclaimable,
        4 => MemoryRegionKind::AcpiNonVolatile,
        5 => MemoryRegionKind::Defective,
        _ => MemoryRegionKind::Reserved,
    }
}

fn align_up_8(value: usize) -> Option<usize> {
    value.checked_add(7).map(|rounded| rounded & !7)
}

unsafe fn read_u32(address: usize) -> u32 {
    unsafe { (address as *const u32).read_unaligned() }
}

unsafe fn read_u64(address: usize) -> u64 {
    unsafe { (address as *const u64).read_unaligned() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(align(8))]
    struct AlignedBootInformation([u8; 56]);

    fn valid_boot_information() -> AlignedBootInformation {
        let mut bytes = AlignedBootInformation([0; 56]);
        bytes.0[0..4].copy_from_slice(&(56_u32).to_le_bytes());

        bytes.0[8..12].copy_from_slice(&TAG_MEMORY_MAP.to_le_bytes());
        bytes.0[12..16].copy_from_slice(&(40_u32).to_le_bytes());
        bytes.0[16..20].copy_from_slice(&(24_u32).to_le_bytes());
        bytes.0[20..24].copy_from_slice(&(0_u32).to_le_bytes());
        bytes.0[24..32].copy_from_slice(&(0x10_0000_u64).to_le_bytes());
        bytes.0[32..40].copy_from_slice(&(0x40_0000_u64).to_le_bytes());
        bytes.0[40..44].copy_from_slice(&(1_u32).to_le_bytes());

        bytes.0[48..52].copy_from_slice(&TAG_END.to_le_bytes());
        bytes.0[52..56].copy_from_slice(&(8_u32).to_le_bytes());
        bytes
    }

    #[test]
    fn parses_a_valid_memory_map() {
        let bytes = valid_boot_information();
        let address = bytes.0.as_ptr() as usize;
        let boot = unsafe { BootInformation::load(address) }.unwrap();
        let map = boot.memory_map().unwrap();

        assert_eq!(boot.total_size(), 56);
        assert_eq!(map.regions().len(), 1);
        assert_eq!(map.regions()[0].start.as_u64(), 0x10_0000);
        assert_eq!(map.regions()[0].end.as_u64(), 0x50_0000);
        assert_eq!(map.regions()[0].kind, MemoryRegionKind::Usable);
    }

    #[test]
    fn rejects_a_misaligned_address() {
        let bytes = valid_boot_information();
        let address = bytes.0.as_ptr() as usize + 1;
        assert_eq!(
            unsafe { BootInformation::load(address) }.err(),
            Some(BootError::InvalidAddress)
        );
    }

    #[test]
    fn rejects_an_invalid_entry_size() {
        let mut bytes = valid_boot_information();
        bytes.0[16..20].copy_from_slice(&(16_u32).to_le_bytes());
        let boot = unsafe { BootInformation::load(bytes.0.as_ptr() as usize) }.unwrap();
        assert_eq!(boot.memory_map().err(), Some(BootError::MalformedMemoryMap));
    }
}
