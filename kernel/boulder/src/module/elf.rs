const ELF_HEADER_LENGTH: usize = 64;
const SECTION_HEADER_LENGTH: usize = 64;
const ELF_CLASS_64: u8 = 2;
const ELF_DATA_LITTLE_ENDIAN: u8 = 1;
const ELF_VERSION_CURRENT: u8 = 1;
const ELF_TYPE_RELOCATABLE: u16 = 1;
const MACHINE_X86_64: u16 = 62;
const SECTION_TYPE_NULL: u32 = 0;
const SECTION_TYPE_NOBITS: u32 = 8;
const UNDEFINED_SECTION: u16 = 0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ElfError {
    Truncated,
    InvalidMagic,
    UnsupportedClass,
    UnsupportedByteOrder,
    UnsupportedVersion,
    UnsupportedFileType,
    UnsupportedMachine,
    InvalidHeaderSize,
    InvalidSectionTable,
    InvalidSection,
    InvalidStringTable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SectionHeader {
    pub name_offset: u32,
    pub section_type: u32,
    pub flags: u64,
    pub address: u64,
    pub offset: u64,
    pub size: u64,
    pub link: u32,
    pub info: u32,
    pub alignment: u64,
    pub entry_size: u64,
}

pub struct ElfModule<'a> {
    bytes: &'a [u8],
    section_table_offset: usize,
    section_count: usize,
    section_name_table: usize,
}

impl<'a> ElfModule<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, ElfError> {
        if bytes.len() < ELF_HEADER_LENGTH {
            return Err(ElfError::Truncated);
        }
        if bytes.get(..4) != Some(b"\x7fELF") {
            return Err(ElfError::InvalidMagic);
        }
        if bytes[4] != ELF_CLASS_64 {
            return Err(ElfError::UnsupportedClass);
        }
        if bytes[5] != ELF_DATA_LITTLE_ENDIAN {
            return Err(ElfError::UnsupportedByteOrder);
        }
        if bytes[6] != ELF_VERSION_CURRENT
            || read_u32(bytes, 20).ok_or(ElfError::Truncated)? != u32::from(ELF_VERSION_CURRENT)
        {
            return Err(ElfError::UnsupportedVersion);
        }
        if read_u16(bytes, 16).ok_or(ElfError::Truncated)? != ELF_TYPE_RELOCATABLE {
            return Err(ElfError::UnsupportedFileType);
        }
        if read_u16(bytes, 18).ok_or(ElfError::Truncated)? != MACHINE_X86_64 {
            return Err(ElfError::UnsupportedMachine);
        }
        if read_u16(bytes, 52).ok_or(ElfError::Truncated)? as usize != ELF_HEADER_LENGTH {
            return Err(ElfError::InvalidHeaderSize);
        }

        let section_table_offset = usize::try_from(read_u64(bytes, 40).ok_or(ElfError::Truncated)?)
            .map_err(|_| ElfError::InvalidSectionTable)?;
        let section_header_size = read_u16(bytes, 58).ok_or(ElfError::Truncated)? as usize;
        let section_count = read_u16(bytes, 60).ok_or(ElfError::Truncated)? as usize;
        let section_name_table = read_u16(bytes, 62).ok_or(ElfError::Truncated)? as usize;
        if section_header_size != SECTION_HEADER_LENGTH || section_count == 0 {
            return Err(ElfError::InvalidSectionTable);
        }
        let table_length = section_count
            .checked_mul(section_header_size)
            .ok_or(ElfError::InvalidSectionTable)?;
        let table_end = section_table_offset
            .checked_add(table_length)
            .ok_or(ElfError::InvalidSectionTable)?;
        if table_end > bytes.len() {
            return Err(ElfError::InvalidSectionTable);
        }
        if section_name_table != UNDEFINED_SECTION as usize && section_name_table >= section_count {
            return Err(ElfError::InvalidStringTable);
        }

        let module = Self {
            bytes,
            section_table_offset,
            section_count,
            section_name_table,
        };
        for index in 0..section_count {
            let section = module.section(index).ok_or(ElfError::InvalidSection)?;
            if section.alignment != 0 && !section.alignment.is_power_of_two() {
                return Err(ElfError::InvalidSection);
            }
            if section.section_type != SECTION_TYPE_NOBITS {
                module.section_data(section)?;
            }
            if index == 0 && section.section_type != SECTION_TYPE_NULL {
                return Err(ElfError::InvalidSectionTable);
            }
        }
        if section_name_table != UNDEFINED_SECTION as usize {
            let names = module
                .section(section_name_table)
                .ok_or(ElfError::InvalidStringTable)?;
            module
                .section_data(names)
                .map_err(|_| ElfError::InvalidStringTable)?;
        }
        Ok(module)
    }

    pub const fn section_count(&self) -> usize {
        self.section_count
    }

    pub fn section(&self, index: usize) -> Option<SectionHeader> {
        if index >= self.section_count {
            return None;
        }
        let offset = self.section_table_offset + index * SECTION_HEADER_LENGTH;
        let bytes = self.bytes.get(offset..offset + SECTION_HEADER_LENGTH)?;
        Some(SectionHeader {
            name_offset: read_u32(bytes, 0)?,
            section_type: read_u32(bytes, 4)?,
            flags: read_u64(bytes, 8)?,
            address: read_u64(bytes, 16)?,
            offset: read_u64(bytes, 24)?,
            size: read_u64(bytes, 32)?,
            link: read_u32(bytes, 40)?,
            info: read_u32(bytes, 44)?,
            alignment: read_u64(bytes, 48)?,
            entry_size: read_u64(bytes, 56)?,
        })
    }

    pub fn section_data(&self, section: SectionHeader) -> Result<&'a [u8], ElfError> {
        if section.section_type == SECTION_TYPE_NOBITS {
            return Ok(&[]);
        }
        let offset = usize::try_from(section.offset).map_err(|_| ElfError::InvalidSection)?;
        let size = usize::try_from(section.size).map_err(|_| ElfError::InvalidSection)?;
        let end = offset.checked_add(size).ok_or(ElfError::InvalidSection)?;
        self.bytes.get(offset..end).ok_or(ElfError::InvalidSection)
    }

    pub fn section_name(&self, section: SectionHeader) -> Result<&'a [u8], ElfError> {
        if self.section_name_table == UNDEFINED_SECTION as usize {
            return Err(ElfError::InvalidStringTable);
        }
        let names = self
            .section(self.section_name_table)
            .ok_or(ElfError::InvalidStringTable)?;
        let bytes = self
            .section_data(names)
            .map_err(|_| ElfError::InvalidStringTable)?;
        let start = section.name_offset as usize;
        let suffix = bytes.get(start..).ok_or(ElfError::InvalidStringTable)?;
        let length = suffix
            .iter()
            .position(|byte| *byte == 0)
            .ok_or(ElfError::InvalidStringTable)?;
        Ok(&suffix[..length])
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_module() -> [u8; 128] {
        let mut bytes = [0_u8; 128];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = ELF_CLASS_64;
        bytes[5] = ELF_DATA_LITTLE_ENDIAN;
        bytes[6] = ELF_VERSION_CURRENT;
        bytes[16..18].copy_from_slice(&ELF_TYPE_RELOCATABLE.to_le_bytes());
        bytes[18..20].copy_from_slice(&MACHINE_X86_64.to_le_bytes());
        bytes[20..24].copy_from_slice(&(ELF_VERSION_CURRENT as u32).to_le_bytes());
        bytes[40..48].copy_from_slice(&(ELF_HEADER_LENGTH as u64).to_le_bytes());
        bytes[52..54].copy_from_slice(&(ELF_HEADER_LENGTH as u16).to_le_bytes());
        bytes[58..60].copy_from_slice(&(SECTION_HEADER_LENGTH as u16).to_le_bytes());
        bytes[60..62].copy_from_slice(&(1_u16).to_le_bytes());
        bytes
    }

    #[test]
    fn accepts_a_bounded_x86_64_relocatable_object() {
        let bytes = minimal_module();
        let module = ElfModule::parse(&bytes).unwrap();
        assert_eq!(module.section_count(), 1);
        assert_eq!(module.section(0).unwrap().section_type, SECTION_TYPE_NULL);
    }

    #[test]
    fn rejects_truncated_and_executable_images() {
        assert_eq!(ElfModule::parse(&[0; 8]).err(), Some(ElfError::Truncated));

        let mut bytes = minimal_module();
        bytes[16..18].copy_from_slice(&(2_u16).to_le_bytes());
        assert_eq!(
            ElfModule::parse(&bytes).err(),
            Some(ElfError::UnsupportedFileType)
        );
    }

    #[test]
    fn rejects_sections_outside_the_image() {
        let mut bytes = minimal_module();
        bytes[64 + 4..64 + 8].copy_from_slice(&(1_u32).to_le_bytes());
        bytes[64 + 24..64 + 32].copy_from_slice(&(120_u64).to_le_bytes());
        bytes[64 + 32..64 + 40].copy_from_slice(&(16_u64).to_le_bytes());
        assert_eq!(
            ElfModule::parse(&bytes).err(),
            Some(ElfError::InvalidSection)
        );
    }
}
