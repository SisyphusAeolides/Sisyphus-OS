const ELF_HEADER_LENGTH: usize = 64;
const PROGRAM_HEADER_LENGTH: usize = 56;
const ELF_TYPE_SHARED_OBJECT: u16 = 3;
const MACHINE_X86_64: u16 = 62;
const SEGMENT_LOAD: u32 = 1;
const SEGMENT_EXECUTABLE: u32 = 1 << 0;
const SEGMENT_WRITABLE: u32 = 1 << 1;
const SEGMENT_READABLE: u32 = 1 << 2;
const SEGMENT_FLAGS: u32 = SEGMENT_EXECUTABLE | SEGMENT_WRITABLE | SEGMENT_READABLE;
const MAXIMUM_LOAD_SEGMENTS: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoadSegment {
    pub file_offset: usize,
    pub file_size: usize,
    pub virtual_address: u64,
    pub memory_size: u64,
    pub alignment: u64,
    pub readable: bool,
    pub writable: bool,
    pub executable: bool,
}

impl LoadSegment {
    const EMPTY: Self = Self {
        file_offset: 0,
        file_size: 0,
        virtual_address: 0,
        memory_size: 0,
        alignment: 0,
        readable: false,
        writable: false,
        executable: false,
    };

    fn end(self) -> Option<u64> {
        self.virtual_address.checked_add(self.memory_size)
    }

    fn contains(self, address: u64) -> bool {
        self.virtual_address <= address && self.end().is_some_and(|end| address < end)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoadPlan {
    segments: [LoadSegment; MAXIMUM_LOAD_SEGMENTS],
    segment_count: usize,
    pub image_start: u64,
    pub image_end: u64,
    pub entry_point: u64,
}

impl LoadPlan {
    pub fn parse(bytes: &[u8]) -> Result<Self, LoaderError> {
        validate_header(bytes)?;
        let program_header_offset =
            usize::try_from(read_u64(bytes, 32).ok_or(LoaderError::Truncated)?)
                .map_err(|_| LoaderError::InvalidProgramHeaders)?;
        let program_header_size = read_u16(bytes, 54).ok_or(LoaderError::Truncated)? as usize;
        let program_header_count = read_u16(bytes, 56).ok_or(LoaderError::Truncated)? as usize;
        if program_header_size != PROGRAM_HEADER_LENGTH || program_header_count == 0 {
            return Err(LoaderError::InvalidProgramHeaders);
        }
        let table_size = program_header_count
            .checked_mul(program_header_size)
            .ok_or(LoaderError::InvalidProgramHeaders)?;
        if program_header_offset
            .checked_add(table_size)
            .is_none_or(|end| end > bytes.len())
        {
            return Err(LoaderError::InvalidProgramHeaders);
        }

        let mut plan = Self {
            segments: [LoadSegment::EMPTY; MAXIMUM_LOAD_SEGMENTS],
            segment_count: 0,
            image_start: u64::MAX,
            image_end: 0,
            entry_point: read_u64(bytes, 24).ok_or(LoaderError::Truncated)?,
        };
        for index in 0..program_header_count {
            let offset = program_header_offset + index * program_header_size;
            let header = &bytes[offset..offset + program_header_size];
            if read_u32(header, 0).ok_or(LoaderError::InvalidProgramHeaders)? != SEGMENT_LOAD {
                continue;
            }
            let flags = read_u32(header, 4).ok_or(LoaderError::InvalidSegment)?;
            let file_offset = read_u64(header, 8).ok_or(LoaderError::InvalidSegment)?;
            let virtual_address = read_u64(header, 16).ok_or(LoaderError::InvalidSegment)?;
            let file_size = read_u64(header, 32).ok_or(LoaderError::InvalidSegment)?;
            let memory_size = read_u64(header, 40).ok_or(LoaderError::InvalidSegment)?;
            let alignment = read_u64(header, 48).ok_or(LoaderError::InvalidSegment)?;
            if memory_size == 0 {
                continue;
            }
            if flags & !SEGMENT_FLAGS != 0
                || flags & (SEGMENT_WRITABLE | SEGMENT_EXECUTABLE)
                    == (SEGMENT_WRITABLE | SEGMENT_EXECUTABLE)
            {
                return Err(LoaderError::WriteExecuteSegment);
            }
            if file_size > memory_size
                || (alignment > 1 && !alignment.is_power_of_two())
                || (alignment > 1 && file_offset % alignment != virtual_address % alignment)
            {
                return Err(LoaderError::InvalidSegment);
            }
            let file_offset =
                usize::try_from(file_offset).map_err(|_| LoaderError::InvalidSegment)?;
            let file_size = usize::try_from(file_size).map_err(|_| LoaderError::InvalidSegment)?;
            if file_offset
                .checked_add(file_size)
                .is_none_or(|end| end > bytes.len())
                || virtual_address.checked_add(memory_size).is_none()
            {
                return Err(LoaderError::InvalidSegment);
            }
            let segment = LoadSegment {
                file_offset,
                file_size,
                virtual_address,
                memory_size,
                alignment,
                readable: flags & SEGMENT_READABLE != 0,
                writable: flags & SEGMENT_WRITABLE != 0,
                executable: flags & SEGMENT_EXECUTABLE != 0,
            };
            if plan.segments[..plan.segment_count]
                .iter()
                .any(|existing| ranges_overlap(*existing, segment))
            {
                return Err(LoaderError::OverlappingSegments);
            }
            let slot = plan
                .segments
                .get_mut(plan.segment_count)
                .ok_or(LoaderError::TooManySegments)?;
            *slot = segment;
            plan.segment_count += 1;
            plan.image_start = plan.image_start.min(segment.virtual_address);
            plan.image_end = plan
                .image_end
                .max(segment.end().ok_or(LoaderError::InvalidSegment)?);
        }
        if plan.segment_count == 0 {
            return Err(LoaderError::MissingLoadSegment);
        }
        if !plan
            .segments()
            .iter()
            .any(|segment| segment.executable && segment.contains(plan.entry_point))
        {
            return Err(LoaderError::InvalidEntryPoint);
        }
        Ok(plan)
    }

    pub fn segments(&self) -> &[LoadSegment] {
        &self.segments[..self.segment_count]
    }

    pub fn segment_data<'a>(
        &self,
        bytes: &'a [u8],
        segment: LoadSegment,
    ) -> Result<&'a [u8], LoaderError> {
        bytes
            .get(segment.file_offset..segment.file_offset + segment.file_size)
            .ok_or(LoaderError::InvalidSegment)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoaderError {
    Truncated,
    InvalidMagic,
    UnsupportedFormat,
    InvalidHeader,
    InvalidProgramHeaders,
    InvalidSegment,
    WriteExecuteSegment,
    OverlappingSegments,
    TooManySegments,
    MissingLoadSegment,
    InvalidEntryPoint,
}

fn validate_header(bytes: &[u8]) -> Result<(), LoaderError> {
    if bytes.len() < ELF_HEADER_LENGTH {
        return Err(LoaderError::Truncated);
    }
    if bytes.get(..4) != Some(b"\x7fELF") {
        return Err(LoaderError::InvalidMagic);
    }
    if bytes[4] != 2
        || bytes[5] != 1
        || bytes[6] != 1
        || read_u16(bytes, 16) != Some(ELF_TYPE_SHARED_OBJECT)
        || read_u16(bytes, 18) != Some(MACHINE_X86_64)
        || read_u32(bytes, 20) != Some(1)
    {
        return Err(LoaderError::UnsupportedFormat);
    }
    if read_u16(bytes, 52) != Some(ELF_HEADER_LENGTH as u16) {
        return Err(LoaderError::InvalidHeader);
    }
    Ok(())
}

fn ranges_overlap(left: LoadSegment, right: LoadSegment) -> bool {
    let Some(left_end) = left.end() else {
        return true;
    };
    let Some(right_end) = right.end() else {
        return true;
    };
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

    fn shared_object(flags: u32) -> [u8; 132] {
        let mut bytes = [0_u8; 132];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[6] = 1;
        bytes[16..18].copy_from_slice(&ELF_TYPE_SHARED_OBJECT.to_le_bytes());
        bytes[18..20].copy_from_slice(&MACHINE_X86_64.to_le_bytes());
        bytes[20..24].copy_from_slice(&(1_u32).to_le_bytes());
        bytes[24..32].copy_from_slice(&(0x1000_u64).to_le_bytes());
        bytes[32..40].copy_from_slice(&(64_u64).to_le_bytes());
        bytes[52..54].copy_from_slice(&(64_u16).to_le_bytes());
        bytes[54..56].copy_from_slice(&(56_u16).to_le_bytes());
        bytes[56..58].copy_from_slice(&(1_u16).to_le_bytes());

        let header = &mut bytes[64..120];
        header[0..4].copy_from_slice(&SEGMENT_LOAD.to_le_bytes());
        header[4..8].copy_from_slice(&flags.to_le_bytes());
        header[8..16].copy_from_slice(&(128_u64).to_le_bytes());
        header[16..24].copy_from_slice(&(0x1000_u64).to_le_bytes());
        header[32..40].copy_from_slice(&(4_u64).to_le_bytes());
        header[40..48].copy_from_slice(&(0x1000_u64).to_le_bytes());
        header[48..56].copy_from_slice(&(1_u64).to_le_bytes());
        bytes[128..].copy_from_slice(&[1, 2, 3, 4]);
        bytes
    }

    #[test]
    fn builds_a_bounded_read_execute_plan() {
        let bytes = shared_object(SEGMENT_READABLE | SEGMENT_EXECUTABLE);
        let plan = LoadPlan::parse(&bytes).unwrap();
        assert_eq!(plan.image_start, 0x1000);
        assert_eq!(plan.image_end, 0x2000);
        assert_eq!(plan.segments().len(), 1);
        assert_eq!(
            plan.segment_data(&bytes, plan.segments()[0]).unwrap(),
            &[1, 2, 3, 4]
        );
    }

    #[test]
    fn rejects_write_execute_segments() {
        let bytes = shared_object(SEGMENT_READABLE | SEGMENT_WRITABLE | SEGMENT_EXECUTABLE);
        assert_eq!(
            LoadPlan::parse(&bytes),
            Err(LoaderError::WriteExecuteSegment)
        );
    }
}
