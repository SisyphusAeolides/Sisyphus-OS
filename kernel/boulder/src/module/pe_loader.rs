const DOS_HEADER_MINIMUM: usize = 64;
const PE_SIGNATURE: u32 = 0x0000_4550;
const MACHINE_AMD64: u16 = 0x8664;
const OPTIONAL_HEADER_PE32_PLUS: u16 = 0x20b;
const COFF_HEADER_LENGTH: usize = 20;
const OPTIONAL_HEADER_MINIMUM: usize = 112;
const SECTION_HEADER_LENGTH: usize = 40;
const MAXIMUM_SECTIONS: usize = 96;
const SECTION_EXECUTABLE: u32 = 0x2000_0000;
const SECTION_READABLE: u32 = 0x4000_0000;
const SECTION_WRITABLE: u32 = 0x8000_0000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeSection {
    pub name: [u8; 8],
    pub virtual_address: u32,
    pub virtual_size: u32,
    pub raw_data_offset: usize,
    pub raw_data_size: usize,
    pub readable: bool,
    pub writable: bool,
    pub executable: bool,
}

impl PeSection {
    const EMPTY: Self = Self {
        name: [0; 8],
        virtual_address: 0,
        virtual_size: 0,
        raw_data_offset: 0,
        raw_data_size: 0,
        readable: false,
        writable: false,
        executable: false,
    };

    fn contains_rva(self, rva: u32) -> bool {
        self.virtual_address <= rva
            && self
                .virtual_address
                .checked_add(self.virtual_size)
                .is_some_and(|end| rva < end)
    }
}

pub struct PeLoadPlan {
    sections: [PeSection; MAXIMUM_SECTIONS],
    section_count: usize,
    pub image_base: u64,
    pub image_size: u32,
    pub headers_size: u32,
    pub entry_point_rva: u32,
    pub section_alignment: u32,
    pub file_alignment: u32,
}

impl PeLoadPlan {
    pub fn parse(bytes: &[u8]) -> Result<Self, PeError> {
        if bytes.len() < DOS_HEADER_MINIMUM {
            return Err(PeError::Truncated);
        }
        if read_u16(bytes, 0) != Some(0x5a4d) {
            return Err(PeError::InvalidDosSignature);
        }
        let pe_offset = read_u32(bytes, 0x3c).ok_or(PeError::Truncated)? as usize;
        if pe_offset
            .checked_add(4 + COFF_HEADER_LENGTH)
            .is_none_or(|end| end > bytes.len())
            || read_u32(bytes, pe_offset) != Some(PE_SIGNATURE)
        {
            return Err(PeError::InvalidPeSignature);
        }
        let coff = pe_offset + 4;
        if read_u16(bytes, coff) != Some(MACHINE_AMD64) {
            return Err(PeError::UnsupportedMachine);
        }
        let section_count = read_u16(bytes, coff + 2).ok_or(PeError::Truncated)? as usize;
        let optional_size = read_u16(bytes, coff + 16).ok_or(PeError::Truncated)? as usize;
        if section_count == 0
            || section_count > MAXIMUM_SECTIONS
            || optional_size < OPTIONAL_HEADER_MINIMUM
        {
            return Err(PeError::InvalidHeaders);
        }
        let optional = coff + COFF_HEADER_LENGTH;
        let section_table = optional
            .checked_add(optional_size)
            .ok_or(PeError::InvalidHeaders)?;
        let table_size = section_count
            .checked_mul(SECTION_HEADER_LENGTH)
            .ok_or(PeError::InvalidHeaders)?;
        if section_table
            .checked_add(table_size)
            .is_none_or(|end| end > bytes.len())
            || read_u16(bytes, optional) != Some(OPTIONAL_HEADER_PE32_PLUS)
        {
            return Err(PeError::InvalidHeaders);
        }

        let entry_point_rva = read_u32(bytes, optional + 16).ok_or(PeError::Truncated)?;
        let image_base = read_u64(bytes, optional + 24).ok_or(PeError::Truncated)?;
        let section_alignment = read_u32(bytes, optional + 32).ok_or(PeError::Truncated)?;
        let file_alignment = read_u32(bytes, optional + 36).ok_or(PeError::Truncated)?;
        let image_size = read_u32(bytes, optional + 56).ok_or(PeError::Truncated)?;
        let headers_size = read_u32(bytes, optional + 60).ok_or(PeError::Truncated)?;
        if image_base == 0
            || image_size == 0
            || headers_size == 0
            || headers_size > image_size
            || headers_size as usize > bytes.len()
            || !section_alignment.is_power_of_two()
            || !file_alignment.is_power_of_two()
        {
            return Err(PeError::InvalidImageLayout);
        }

        let mut plan = Self {
            sections: [PeSection::EMPTY; MAXIMUM_SECTIONS],
            section_count: 0,
            image_base,
            image_size,
            headers_size,
            entry_point_rva,
            section_alignment,
            file_alignment,
        };
        for index in 0..section_count {
            let offset = section_table + index * SECTION_HEADER_LENGTH;
            let header = &bytes[offset..offset + SECTION_HEADER_LENGTH];
            let mut name = [0_u8; 8];
            name.copy_from_slice(&header[..8]);
            let virtual_size = read_u32(header, 8).ok_or(PeError::InvalidSection)?;
            let virtual_address = read_u32(header, 12).ok_or(PeError::InvalidSection)?;
            let raw_data_size = read_u32(header, 16).ok_or(PeError::InvalidSection)? as usize;
            let raw_data_offset = read_u32(header, 20).ok_or(PeError::InvalidSection)? as usize;
            let characteristics = read_u32(header, 36).ok_or(PeError::InvalidSection)?;
            let writable = characteristics & SECTION_WRITABLE != 0;
            let executable = characteristics & SECTION_EXECUTABLE != 0;
            if writable && executable
                || virtual_address % section_alignment != 0
                || raw_data_offset % file_alignment as usize != 0
                || raw_data_offset
                    .checked_add(raw_data_size)
                    .is_none_or(|end| end > bytes.len())
                || virtual_address
                    .checked_add(virtual_size.max(raw_data_size as u32))
                    .is_none_or(|end| end > image_size)
            {
                return Err(if writable && executable {
                    PeError::WriteExecuteSection
                } else {
                    PeError::InvalidSection
                });
            }
            let section = PeSection {
                name,
                virtual_address,
                virtual_size,
                raw_data_offset,
                raw_data_size,
                readable: characteristics & SECTION_READABLE != 0,
                writable,
                executable,
            };
            if plan.sections[..plan.section_count]
                .iter()
                .any(|existing| sections_overlap(*existing, section))
            {
                return Err(PeError::OverlappingSections);
            }
            plan.sections[plan.section_count] = section;
            plan.section_count += 1;
        }
        if !plan
            .sections()
            .iter()
            .any(|section| section.executable && section.contains_rva(entry_point_rva))
        {
            return Err(PeError::InvalidEntryPoint);
        }
        Ok(plan)
    }

    pub fn sections(&self) -> &[PeSection] {
        &self.sections[..self.section_count]
    }

    pub fn section_data<'a>(
        &self,
        bytes: &'a [u8],
        section: PeSection,
    ) -> Result<&'a [u8], PeError> {
        bytes
            .get(section.raw_data_offset..section.raw_data_offset + section.raw_data_size)
            .ok_or(PeError::InvalidSection)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeError {
    Truncated,
    InvalidDosSignature,
    InvalidPeSignature,
    UnsupportedMachine,
    InvalidHeaders,
    InvalidImageLayout,
    InvalidSection,
    WriteExecuteSection,
    OverlappingSections,
    InvalidEntryPoint,
}

fn sections_overlap(left: PeSection, right: PeSection) -> bool {
    let left_end = left
        .virtual_address
        .saturating_add(left.virtual_size.max(left.raw_data_size as u32));
    let right_end = right
        .virtual_address
        .saturating_add(right.virtual_size.max(right.raw_data_size as u32));
    left.virtual_address < right_end && right.virtual_address < left_end
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

    fn image(characteristics: u32) -> [u8; 516] {
        let mut bytes = [0_u8; 516];
        bytes[..2].copy_from_slice(&(0x5a4d_u16).to_le_bytes());
        bytes[0x3c..0x40].copy_from_slice(&(0x80_u32).to_le_bytes());
        bytes[0x80..0x84].copy_from_slice(&PE_SIGNATURE.to_le_bytes());
        let coff = 0x84;
        bytes[coff..coff + 2].copy_from_slice(&MACHINE_AMD64.to_le_bytes());
        bytes[coff + 2..coff + 4].copy_from_slice(&(1_u16).to_le_bytes());
        bytes[coff + 16..coff + 18]
            .copy_from_slice(&(OPTIONAL_HEADER_MINIMUM as u16).to_le_bytes());
        let optional = coff + COFF_HEADER_LENGTH;
        bytes[optional..optional + 2].copy_from_slice(&OPTIONAL_HEADER_PE32_PLUS.to_le_bytes());
        bytes[optional + 16..optional + 20].copy_from_slice(&(0x1000_u32).to_le_bytes());
        bytes[optional + 24..optional + 32].copy_from_slice(&(0x1_4000_0000_u64).to_le_bytes());
        bytes[optional + 32..optional + 36].copy_from_slice(&(0x1000_u32).to_le_bytes());
        bytes[optional + 36..optional + 40].copy_from_slice(&(0x200_u32).to_le_bytes());
        bytes[optional + 56..optional + 60].copy_from_slice(&(0x2000_u32).to_le_bytes());
        bytes[optional + 60..optional + 64].copy_from_slice(&(0x200_u32).to_le_bytes());

        let section = optional + OPTIONAL_HEADER_MINIMUM;
        bytes[section..section + 5].copy_from_slice(b".text");
        bytes[section + 8..section + 12].copy_from_slice(&(0x1000_u32).to_le_bytes());
        bytes[section + 12..section + 16].copy_from_slice(&(0x1000_u32).to_le_bytes());
        bytes[section + 16..section + 20].copy_from_slice(&(4_u32).to_le_bytes());
        bytes[section + 20..section + 24].copy_from_slice(&(0x200_u32).to_le_bytes());
        bytes[section + 36..section + 40].copy_from_slice(&characteristics.to_le_bytes());
        bytes[0x200..].copy_from_slice(&[0x90, 0x90, 0x90, 0xc3]);
        bytes
    }

    #[test]
    fn validates_a_bounded_amd64_pe32_plus_plan() {
        let bytes = image(SECTION_READABLE | SECTION_EXECUTABLE);
        let plan = PeLoadPlan::parse(&bytes).unwrap();
        assert_eq!(plan.sections().len(), 1);
        assert_eq!(plan.entry_point_rva, 0x1000);
        assert_eq!(
            plan.section_data(&bytes, plan.sections()[0]).unwrap().len(),
            4
        );
    }

    #[test]
    fn rejects_write_execute_sections() {
        let bytes = image(SECTION_READABLE | SECTION_WRITABLE | SECTION_EXECUTABLE);
        assert_eq!(
            PeLoadPlan::parse(&bytes).err(),
            Some(PeError::WriteExecuteSection)
        );
    }
}
