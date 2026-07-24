use abyss::memory::{MemoryMap, MemoryMapError, MemoryRegion, MemoryRegionKind};
use abyss::paging::PhysicalAddress;

use super::acpi::Rsdp;

const TAG_END: u32 = 0;
const TAG_MODULE: u32 = 3;
const TAG_MEMORY_MAP: u32 = 6;
const TAG_FRAMEBUFFER: u32 = 8;
const TAG_ACPI_OLD: u32 = 14;
const TAG_ACPI_NEW: u32 = 15;
const HEADER_SIZE: usize = 8;
const TAG_HEADER_SIZE: usize = 8;
const MEMORY_MAP_HEADER_SIZE: usize = 16;
const MEMORY_MAP_ENTRY_MINIMUM_SIZE: usize = 24;
const MAXIMUM_BOOT_INFORMATION_SIZE: usize = 16 * 1024 * 1024;
const MODULE_HEADER_SIZE: usize = 16;
const FRAMEBUFFER_HEADER_SIZE: usize = 32;
const FRAMEBUFFER_DIRECT_COLOR_SIZE: usize = 38;

pub const FRAMEBUFFER_FORMAT_XRGB8888: u32 = 1;
pub const FRAMEBUFFER_FORMAT_XBGR8888: u32 = 2;
pub const FRAMEBUFFER_FORMAT_RGB565: u32 = 3;

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

    pub fn rsdp(self) -> Result<Rsdp, BootError> {
        let tag = match self.find_tag(TAG_ACPI_NEW)? {
            Some(tag) => tag,
            None => self
                .find_tag(TAG_ACPI_OLD)?
                .ok_or(BootError::MissingAcpiRsdp)?,
        };
        let payload_length = tag.size - TAG_HEADER_SIZE;
        // SAFETY: find_tag verified the complete tag and its payload against
        // the bounds of the Multiboot2 information structure.
        let payload = unsafe {
            core::slice::from_raw_parts(
                (tag.address + TAG_HEADER_SIZE) as *const u8,
                payload_length,
            )
        };
        Rsdp::parse(payload).map_err(|_| BootError::MalformedAcpiRsdp)
    }


    pub fn framebuffer(self) -> Result<Option<BootFramebuffer>, BootError> {
        let Some(tag) = self.find_tag(TAG_FRAMEBUFFER)? else {
            return Ok(None);
        };
        if tag.size < FRAMEBUFFER_HEADER_SIZE {
            return Err(BootError::MalformedFramebuffer);
        }

        // SAFETY: find_tag validated every fixed field used below.
        let physical_address = unsafe { read_u64(tag.address + 8) };
        let pitch = unsafe { read_u32(tag.address + 16) };
        let width = unsafe { read_u32(tag.address + 20) };
        let height = unsafe { read_u32(tag.address + 24) };
        let bits_per_pixel = unsafe { read_u8(tag.address + 28) };
        let framebuffer_type = unsafe { read_u8(tag.address + 29) };

        if physical_address == 0
            || pitch == 0
            || width == 0
            || height == 0
            || bits_per_pixel == 0
        {
            return Err(BootError::MalformedFramebuffer);
        }

        let format = match framebuffer_type {
            1 => {
                if tag.size < FRAMEBUFFER_DIRECT_COLOR_SIZE {
                    return Err(BootError::MalformedFramebuffer);
                }
                // SAFETY: The complete direct-color payload is present.
                let red_position = unsafe { read_u8(tag.address + 32) };
                let red_mask = unsafe { read_u8(tag.address + 33) };
                let green_position = unsafe { read_u8(tag.address + 34) };
                let green_mask = unsafe { read_u8(tag.address + 35) };
                let blue_position = unsafe { read_u8(tag.address + 36) };
                let blue_mask = unsafe { read_u8(tag.address + 37) };

                match (
                    bits_per_pixel,
                    red_position,
                    red_mask,
                    green_position,
                    green_mask,
                    blue_position,
                    blue_mask,
                ) {
                    (32, 16, 8, 8, 8, 0, 8) => FRAMEBUFFER_FORMAT_XRGB8888,
                    (32, 0, 8, 8, 8, 16, 8) => FRAMEBUFFER_FORMAT_XBGR8888,
                    (16, 11, 5, 5, 6, 0, 5) => FRAMEBUFFER_FORMAT_RGB565,
                    _ => return Err(BootError::UnsupportedFramebuffer),
                }
            }
            _ => return Err(BootError::UnsupportedFramebuffer),
        };

        let minimum_pitch = width
            .checked_mul(u32::from(bits_per_pixel).div_ceil(8))
            .ok_or(BootError::AddressOverflow)?;
        if pitch < minimum_pitch {
            return Err(BootError::MalformedFramebuffer);
        }

        let byte_length = u64::from(pitch)
            .checked_mul(u64::from(height))
            .ok_or(BootError::AddressOverflow)?;
        physical_address
            .checked_add(byte_length)
            .ok_or(BootError::AddressOverflow)?;

        Ok(Some(BootFramebuffer {
            physical_address,
            byte_length,
            width,
            height,
            pitch,
            bits_per_pixel,
            format,
        }))
    }

    pub fn module(self, command_line: &[u8]) -> Result<BootModule, BootError> {
        let end = self.address + self.total_size;
        let mut address = self.address + HEADER_SIZE;
        while address < end {
            if end - address < TAG_HEADER_SIZE {
                return Err(BootError::MalformedTag);
            }
            // SAFETY: The fixed header lies in the validated boot structure.
            let tag_type = unsafe { read_u32(address) };
            let size = unsafe { read_u32(address + 4) } as usize;
            if size < TAG_HEADER_SIZE || address.checked_add(size).is_none_or(|next| next > end) {
                return Err(BootError::MalformedTag);
            }
            if tag_type == TAG_MODULE {
                if size < MODULE_HEADER_SIZE {
                    return Err(BootError::MalformedModule);
                }
                let string_length = size - MODULE_HEADER_SIZE;
                // SAFETY: The complete module tag was bounds-checked above.
                let string = unsafe {
                    core::slice::from_raw_parts(
                        (address + MODULE_HEADER_SIZE) as *const u8,
                        string_length,
                    )
                };
                let Some(nul) = string.iter().position(|byte| *byte == 0) else {
                    return Err(BootError::MalformedModule);
                };
                if &string[..nul] == command_line {
                    // SAFETY: The module tag's fixed fields are present.
                    let start = u64::from(unsafe { read_u32(address + 8) });
                    let finish = u64::from(unsafe { read_u32(address + 12) });
                    if start >= finish {
                        return Err(BootError::MalformedModule);
                    }
                    return Ok(BootModule {
                        start: PhysicalAddress::new(start),
                        end: PhysicalAddress::new(finish),
                    });
                }
            }
            if tag_type == TAG_END {
                break;
            }
            address = align_up_8(address + size).ok_or(BootError::AddressOverflow)?;
        }
        Err(BootError::MissingModule)
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
pub struct BootFramebuffer {
    pub physical_address: u64,
    pub byte_length: u64,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bits_per_pixel: u8,
    pub format: u32,
}

impl BootFramebuffer {
    pub const fn end(self) -> Option<u64> {
        self.physical_address.checked_add(self.byte_length)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BootModule {
    pub start: PhysicalAddress,
    pub end: PhysicalAddress,
}

impl BootModule {
    pub const fn length(self) -> u64 {
        self.end.as_u64() - self.start.as_u64()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootError {
    InvalidAddress,
    InvalidTotalSize,
    MalformedTag,
    MissingEndTag,
    MissingMemoryMap,
    MalformedMemoryMap,
    MalformedFramebuffer,
    UnsupportedFramebuffer,
    MissingAcpiRsdp,
    MalformedAcpiRsdp,
    MissingModule,
    MalformedModule,
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

unsafe fn read_u8(address: usize) -> u8 {
    unsafe { (address as *const u8).read_unaligned() }
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

    #[test]
    fn parses_a_direct_color_framebuffer() {
        #[repr(align(8))]
        struct FramebufferBootInformation([u8; 56]);

        let mut bytes = FramebufferBootInformation([0; 56]);
        bytes.0[0..4].copy_from_slice(&(56_u32).to_le_bytes());
        bytes.0[8..12].copy_from_slice(&TAG_FRAMEBUFFER.to_le_bytes());
        bytes.0[12..16].copy_from_slice(&(38_u32).to_le_bytes());
        bytes.0[16..24].copy_from_slice(&(0xe000_0000_u64).to_le_bytes());
        bytes.0[24..28].copy_from_slice(&(4096_u32).to_le_bytes());
        bytes.0[28..32].copy_from_slice(&(1024_u32).to_le_bytes());
        bytes.0[32..36].copy_from_slice(&(768_u32).to_le_bytes());
        bytes.0[36] = 32;
        bytes.0[37] = 1;
        bytes.0[40] = 16;
        bytes.0[41] = 8;
        bytes.0[42] = 8;
        bytes.0[43] = 8;
        bytes.0[44] = 0;
        bytes.0[45] = 8;
        bytes.0[48..52].copy_from_slice(&TAG_END.to_le_bytes());
        bytes.0[52..56].copy_from_slice(&(8_u32).to_le_bytes());

        let boot = unsafe { BootInformation::load(bytes.0.as_ptr() as usize) }.unwrap();
        let framebuffer = boot.framebuffer().unwrap().unwrap();
        assert_eq!(framebuffer.physical_address, 0xe000_0000);
        assert_eq!(framebuffer.byte_length, 4096 * 768);
        assert_eq!(framebuffer.format, FRAMEBUFFER_FORMAT_XRGB8888);
    }

    #[test]
    fn locates_a_named_boot_module() {
        #[repr(align(8))]
        struct ModuleBootInformation([u8; 40]);

        let mut bytes = ModuleBootInformation([0; 40]);
        bytes.0[0..4].copy_from_slice(&(40_u32).to_le_bytes());
        bytes.0[8..12].copy_from_slice(&TAG_MODULE.to_le_bytes());
        bytes.0[12..16].copy_from_slice(&(21_u32).to_le_bytes());
        bytes.0[16..20].copy_from_slice(&(0x40_0000_u32).to_le_bytes());
        bytes.0[20..24].copy_from_slice(&(0x40_2000_u32).to_le_bytes());
        bytes.0[24..29].copy_from_slice(b"push\0");
        bytes.0[32..36].copy_from_slice(&TAG_END.to_le_bytes());
        bytes.0[36..40].copy_from_slice(&(8_u32).to_le_bytes());

        let boot = unsafe { BootInformation::load(bytes.0.as_ptr() as usize) }.unwrap();
        let module = boot.module(b"push").unwrap();
        assert_eq!(module.start.as_u64(), 0x40_0000);
        assert_eq!(module.end.as_u64(), 0x40_2000);
        assert_eq!(module.length(), 0x2000);
        assert_eq!(boot.module(b"crest"), Err(BootError::MissingModule));
    }
}
