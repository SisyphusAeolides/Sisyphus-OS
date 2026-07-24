//! Bounded preflight and commit support for the explicitly implemented ELF64
//! relocatable-object subset. This is not a general Linux `.ko` loader.

use alloc::vec::Vec;
use core::mem::size_of;

use crate::mirage::personality::{
    CallingConvention, MirageEnclave, ObjectFormat, OsPersonality, PersonalityError,
};
use crate::module::elf::{ElfError, ElfModule, SectionHeader};
use crate::module::elf_headers::{RelocationEntry, SymbolEntry};
use crate::module::relocator::ExternalSymbolResolver;

const MAXIMUM_IMAGE_SIZE: usize = 16 * 1024 * 1024;
const MAXIMUM_RELOCATIONS: usize = 4096;
const PAGE_ALIGNMENT: usize = 4096;
const ELF_HEADER_LENGTH: usize = 64;
const SECTION_HEADER_LENGTH: usize = 64;

const SECTION_TYPE_NULL: u32 = 0;
const SECTION_TYPE_PROGRAM_BITS: u32 = 1;
const SECTION_TYPE_SYMBOL_TABLE: u32 = 2;
const SECTION_TYPE_STRING_TABLE: u32 = 3;
const SECTION_TYPE_RELA: u32 = 4;
const SECTION_TYPE_NOTE: u32 = 7;
const SECTION_TYPE_NOBITS: u32 = 8;

const SECTION_WRITE: u64 = 1 << 0;
const SECTION_ALLOCATE: u64 = 1 << 1;
const SECTION_EXECUTE: u64 = 1 << 2;
const SECTION_MERGE: u64 = 1 << 4;
const SECTION_STRINGS: u64 = 1 << 5;
const SUPPORTED_SECTION_FLAGS: u64 =
    SECTION_WRITE | SECTION_ALLOCATE | SECTION_EXECUTE | SECTION_MERGE | SECTION_STRINGS;

const SECTION_UNDEFINED: u16 = 0;
const SECTION_ABSOLUTE: u16 = 0xfff1;
const RESERVED_SECTION_START: u16 = 0xff00;

const SYMBOL_BINDING_GLOBAL: u8 = 1;
const SYMBOL_TYPE_FUNCTION: u8 = 2;

const R_X86_64_64: u32 = 1;
const R_X86_64_PC32: u32 = 2;
const R_X86_64_PLT32: u32 = 4;
const R_X86_64_GLOB_DAT: u32 = 6;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ElfSectionPlacement {
    pub section_index: usize,
    pub image_offset: usize,
    pub memory_size: usize,
    pub writable: bool,
    pub executable: bool,
    file_offset: usize,
    file_size: usize,
}

#[derive(Clone, Copy)]
enum PreparedValue {
    Absolute(u64),
    Relative(i32),
}

#[derive(Clone, Copy)]
struct PreparedPatch {
    image_offset: usize,
    value: PreparedValue,
}

impl PreparedPatch {
    const fn width(self) -> usize {
        match self.value {
            PreparedValue::Absolute(_) => size_of::<u64>(),
            PreparedValue::Relative(_) => size_of::<i32>(),
        }
    }
}

/// A fully validated load transaction for one bounded ELF64 ET_REL object.
///
/// External addresses and relocation values are frozen during construction.
/// Once returned, allocating `image_size()` bytes at `image_virtual_address()`
/// and calling `commit()` requires no metadata allocation or symbol lookup.
pub struct ForeignElfLoadPlan<'a> {
    bytes: &'a [u8],
    personality: OsPersonality,
    image_virtual_address: u64,
    image_size: usize,
    image_alignment: usize,
    entry_address: u64,
    placements: Vec<ElfSectionPlacement>,
    patches: Vec<PreparedPatch>,
}

impl<'a> ForeignElfLoadPlan<'a> {
    pub fn preflight(
        bytes: &'a [u8],
        personality: OsPersonality,
        image_virtual_address: u64,
        entry_symbol: &[u8],
    ) -> Result<Self, ForeignElfError> {
        if image_virtual_address == 0 || entry_symbol.is_empty() {
            return Err(ForeignElfError::InvalidLoadRequest);
        }
        let module = ElfModule::parse(bytes).map_err(ForeignElfError::Elf)?;
        validate_relocatable_header(bytes)?;
        validate_file_layout(&module, bytes)?;
        validate_section_kinds(&module)?;

        let (placements, image_size, image_alignment) =
            plan_allocated_sections(&module, image_virtual_address)?;
        let (symbol_table_index, symbol_table_header, symbol_table, strings) =
            find_symbol_table(&module)?;
        validate_symbols(&module, symbol_table_header, symbol_table, strings)?;

        let enclave =
            MirageEnclave::materialize(personality).map_err(ForeignElfError::Personality)?;
        if enclave.object_format() != ObjectFormat::ElfRelocatable
            || enclave.calling_convention() != CallingConvention::SystemV64
        {
            return Err(ForeignElfError::PersonalityFormatMismatch);
        }

        let entry_address = resolve_entry_symbol(
            &module,
            symbol_table,
            strings,
            &placements,
            image_virtual_address,
            entry_symbol,
        )?;
        let patches = prepare_relocations(
            &module,
            symbol_table_index,
            symbol_table,
            strings,
            &placements,
            &enclave,
            image_virtual_address,
        )?;

        Ok(Self {
            bytes,
            personality,
            image_virtual_address,
            image_size,
            image_alignment,
            entry_address,
            placements,
            patches,
        })
    }

    pub const fn personality(&self) -> OsPersonality {
        self.personality
    }

    pub const fn image_virtual_address(&self) -> u64 {
        self.image_virtual_address
    }

    pub const fn image_size(&self) -> usize {
        self.image_size
    }

    pub const fn image_alignment(&self) -> usize {
        self.image_alignment
    }

    pub const fn entry_address(&self) -> u64 {
        self.entry_address
    }

    pub fn sections(&self) -> &[ElfSectionPlacement] {
        &self.placements
    }

    pub fn relocation_count(&self) -> usize {
        self.patches.len()
    }

    /// Copies and relocates the preflighted object into its exact target image.
    /// No fallible validation, allocation, or external lookup occurs after the
    /// destination is cleared.
    pub fn commit(&self, image: &mut [u8]) -> Result<(), ForeignElfError> {
        if image.len() != self.image_size {
            return Err(ForeignElfError::ImageSizeMismatch);
        }

        image.fill(0);
        for placement in &self.placements {
            let source =
                &self.bytes[placement.file_offset..placement.file_offset + placement.file_size];
            let target =
                &mut image[placement.image_offset..placement.image_offset + placement.file_size];
            target.copy_from_slice(source);
        }
        for patch in &self.patches {
            match patch.value {
                PreparedValue::Absolute(value) => {
                    image[patch.image_offset..patch.image_offset + size_of::<u64>()]
                        .copy_from_slice(&value.to_le_bytes());
                }
                PreparedValue::Relative(value) => {
                    image[patch.image_offset..patch.image_offset + size_of::<i32>()]
                        .copy_from_slice(&value.to_le_bytes());
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForeignElfError {
    Elf(ElfError),
    Personality(PersonalityError),
    InvalidLoadRequest,
    UnsupportedObjectMetadata,
    UnsupportedSection,
    WriteExecuteSection,
    InvalidSectionLayout,
    ImageTooLarge,
    MissingSymbolTable,
    DuplicateSymbolTable,
    InvalidSymbolTable,
    InvalidSymbol,
    MissingEntrySymbol,
    DuplicateEntrySymbol,
    InvalidEntrySymbol,
    InvalidRelocationSection,
    InvalidRelocation,
    UnsupportedRelocation(u32),
    UnresolvedExternal,
    RelocationValueOutOfRange,
    OverlappingRelocations,
    PersonalityFormatMismatch,
    PlanAllocationFailed,
    ImageSizeMismatch,
}

fn validate_relocatable_header(bytes: &[u8]) -> Result<(), ForeignElfError> {
    let entry = read_u64(bytes, 24).ok_or(ForeignElfError::UnsupportedObjectMetadata)?;
    let program_offset = read_u64(bytes, 32).ok_or(ForeignElfError::UnsupportedObjectMetadata)?;
    let flags = read_u32(bytes, 48).ok_or(ForeignElfError::UnsupportedObjectMetadata)?;
    let program_entry_size =
        read_u16(bytes, 54).ok_or(ForeignElfError::UnsupportedObjectMetadata)?;
    let program_count = read_u16(bytes, 56).ok_or(ForeignElfError::UnsupportedObjectMetadata)?;
    let section_name_table =
        read_u16(bytes, 62).ok_or(ForeignElfError::UnsupportedObjectMetadata)?;
    if entry != 0
        || program_offset != 0
        || flags != 0
        || program_entry_size != 0
        || program_count != 0
        || section_name_table == 0
    {
        return Err(ForeignElfError::UnsupportedObjectMetadata);
    }
    Ok(())
}

fn validate_file_layout(module: &ElfModule<'_>, bytes: &[u8]) -> Result<(), ForeignElfError> {
    let table_start =
        usize::try_from(read_u64(bytes, 40).ok_or(ForeignElfError::InvalidSectionLayout)?)
            .map_err(|_| ForeignElfError::InvalidSectionLayout)?;
    let table_end = table_start
        .checked_add(
            module
                .section_count()
                .checked_mul(SECTION_HEADER_LENGTH)
                .ok_or(ForeignElfError::InvalidSectionLayout)?,
        )
        .ok_or(ForeignElfError::InvalidSectionLayout)?;
    if ranges_overlap(0, ELF_HEADER_LENGTH, table_start, table_end) {
        return Err(ForeignElfError::InvalidSectionLayout);
    }

    for index in 0..module.section_count() {
        let section = module
            .section(index)
            .ok_or(ForeignElfError::InvalidSectionLayout)?;
        if index == 0 {
            if section
                != (SectionHeader {
                    name_offset: 0,
                    section_type: SECTION_TYPE_NULL,
                    flags: 0,
                    address: 0,
                    offset: 0,
                    size: 0,
                    link: 0,
                    info: 0,
                    alignment: 0,
                    entry_size: 0,
                })
            {
                return Err(ForeignElfError::InvalidSectionLayout);
            }
            continue;
        }
        if section.section_type == SECTION_TYPE_NOBITS || section.size == 0 {
            continue;
        }
        let start =
            usize::try_from(section.offset).map_err(|_| ForeignElfError::InvalidSectionLayout)?;
        let size =
            usize::try_from(section.size).map_err(|_| ForeignElfError::InvalidSectionLayout)?;
        let end = start
            .checked_add(size)
            .ok_or(ForeignElfError::InvalidSectionLayout)?;
        if ranges_overlap(start, end, 0, ELF_HEADER_LENGTH)
            || ranges_overlap(start, end, table_start, table_end)
        {
            return Err(ForeignElfError::InvalidSectionLayout);
        }
        for previous_index in 1..index {
            let previous = module
                .section(previous_index)
                .ok_or(ForeignElfError::InvalidSectionLayout)?;
            if previous.section_type == SECTION_TYPE_NOBITS || previous.size == 0 {
                continue;
            }
            let previous_start = usize::try_from(previous.offset)
                .map_err(|_| ForeignElfError::InvalidSectionLayout)?;
            let previous_size = usize::try_from(previous.size)
                .map_err(|_| ForeignElfError::InvalidSectionLayout)?;
            let previous_end = previous_start
                .checked_add(previous_size)
                .ok_or(ForeignElfError::InvalidSectionLayout)?;
            if ranges_overlap(start, end, previous_start, previous_end) {
                return Err(ForeignElfError::InvalidSectionLayout);
            }
        }
    }
    Ok(())
}

fn validate_section_kinds(module: &ElfModule<'_>) -> Result<(), ForeignElfError> {
    for index in 0..module.section_count() {
        let section = module
            .section(index)
            .ok_or(ForeignElfError::InvalidSectionLayout)?;
        if section.flags & !SUPPORTED_SECTION_FLAGS != 0 || section.address != 0 {
            return Err(ForeignElfError::UnsupportedSection);
        }
        let allocated = section.flags & SECTION_ALLOCATE != 0;
        if allocated
            && !matches!(
                section.section_type,
                SECTION_TYPE_PROGRAM_BITS | SECTION_TYPE_NOBITS
            )
        {
            return Err(ForeignElfError::UnsupportedSection);
        }
        if allocated
            && section.flags & (SECTION_WRITE | SECTION_EXECUTE)
                == (SECTION_WRITE | SECTION_EXECUTE)
        {
            return Err(ForeignElfError::WriteExecuteSection);
        }
        if !allocated
            && !matches!(
                section.section_type,
                SECTION_TYPE_NULL
                    | SECTION_TYPE_PROGRAM_BITS
                    | SECTION_TYPE_SYMBOL_TABLE
                    | SECTION_TYPE_STRING_TABLE
                    | SECTION_TYPE_RELA
                    | SECTION_TYPE_NOTE
            )
        {
            return Err(ForeignElfError::UnsupportedSection);
        }
        if section.flags & (SECTION_MERGE | SECTION_STRINGS) != 0
            && (section.section_type != SECTION_TYPE_PROGRAM_BITS || section.entry_size == 0)
        {
            return Err(ForeignElfError::InvalidSectionLayout);
        }
    }
    Ok(())
}

fn plan_allocated_sections(
    module: &ElfModule<'_>,
    image_virtual_address: u64,
) -> Result<(Vec<ElfSectionPlacement>, usize, usize), ForeignElfError> {
    let mut placements = Vec::new();
    placements
        .try_reserve_exact(module.section_count())
        .map_err(|_| ForeignElfError::PlanAllocationFailed)?;
    let mut image_size = 0_usize;
    let mut image_alignment = PAGE_ALIGNMENT;

    for section_index in 1..module.section_count() {
        let section = module
            .section(section_index)
            .ok_or(ForeignElfError::InvalidSectionLayout)?;
        if section.flags & SECTION_ALLOCATE == 0 {
            continue;
        }
        let memory_size =
            usize::try_from(section.size).map_err(|_| ForeignElfError::InvalidSectionLayout)?;
        if memory_size == 0 {
            return Err(ForeignElfError::InvalidSectionLayout);
        }
        let requested_alignment = usize::try_from(section.alignment.max(1))
            .map_err(|_| ForeignElfError::InvalidSectionLayout)?;
        let placement_alignment = requested_alignment.max(PAGE_ALIGNMENT);
        if placement_alignment > MAXIMUM_IMAGE_SIZE {
            return Err(ForeignElfError::InvalidSectionLayout);
        }
        image_alignment = image_alignment.max(placement_alignment);
        image_size =
            align_up(image_size, placement_alignment).ok_or(ForeignElfError::ImageTooLarge)?;
        let file_size = if section.section_type == SECTION_TYPE_NOBITS {
            0
        } else {
            memory_size
        };
        let file_offset =
            usize::try_from(section.offset).map_err(|_| ForeignElfError::InvalidSectionLayout)?;
        image_size = image_size
            .checked_add(memory_size)
            .filter(|size| *size <= MAXIMUM_IMAGE_SIZE)
            .ok_or(ForeignElfError::ImageTooLarge)?;
        placements.push(ElfSectionPlacement {
            section_index,
            image_offset: image_size - memory_size,
            memory_size,
            writable: section.flags & SECTION_WRITE != 0,
            executable: section.flags & SECTION_EXECUTE != 0,
            file_offset,
            file_size,
        });
    }
    if placements.is_empty() || !placements.iter().any(|section| section.executable) {
        return Err(ForeignElfError::InvalidSectionLayout);
    }
    image_size = align_up(image_size, PAGE_ALIGNMENT).ok_or(ForeignElfError::ImageTooLarge)?;
    if image_size > MAXIMUM_IMAGE_SIZE
        || image_virtual_address % image_alignment as u64 != 0
        || image_virtual_address
            .checked_add(image_size as u64)
            .is_none()
    {
        return Err(ForeignElfError::InvalidSectionLayout);
    }
    Ok((placements, image_size, image_alignment))
}

fn find_symbol_table<'a>(
    module: &ElfModule<'a>,
) -> Result<(usize, SectionHeader, &'a [u8], &'a [u8]), ForeignElfError> {
    let mut found = None;
    for index in 1..module.section_count() {
        let section = module
            .section(index)
            .ok_or(ForeignElfError::InvalidSymbolTable)?;
        if section.section_type != SECTION_TYPE_SYMBOL_TABLE {
            continue;
        }
        if found.is_some() {
            return Err(ForeignElfError::DuplicateSymbolTable);
        }
        if section.entry_size != size_of::<SymbolEntry>() as u64
            || section.size == 0
            || section.size % section.entry_size != 0
            || section.flags != 0
            || section.address != 0
            || section.alignment != 8
        {
            return Err(ForeignElfError::InvalidSymbolTable);
        }
        let strings_section = module
            .section(section.link as usize)
            .filter(|linked| linked.section_type == SECTION_TYPE_STRING_TABLE)
            .ok_or(ForeignElfError::InvalidSymbolTable)?;
        let symbols = module
            .section_data(section)
            .map_err(|_| ForeignElfError::InvalidSymbolTable)?;
        let strings = module
            .section_data(strings_section)
            .map_err(|_| ForeignElfError::InvalidSymbolTable)?;
        if strings.first() != Some(&0)
            || section.info as usize > symbols.len() / size_of::<SymbolEntry>()
        {
            return Err(ForeignElfError::InvalidSymbolTable);
        }
        found = Some((index, section, symbols, strings));
    }
    found.ok_or(ForeignElfError::MissingSymbolTable)
}

fn validate_symbols(
    module: &ElfModule<'_>,
    symbol_table: SectionHeader,
    symbols: &[u8],
    strings: &[u8],
) -> Result<(), ForeignElfError> {
    let symbol_count = symbols.len() / size_of::<SymbolEntry>();
    for index in 0..symbol_count {
        let symbol = parse_symbol(symbols, index).ok_or(ForeignElfError::InvalidSymbol)?;
        let _ = string_at(strings, symbol.name_offset as usize)
            .ok_or(ForeignElfError::InvalidSymbol)?;
        if symbol.visibility & !0x3 != 0 {
            return Err(ForeignElfError::InvalidSymbol);
        }
        let binding = symbol.information >> 4;
        if (index < symbol_table.info as usize && binding != 0)
            || (index >= symbol_table.info as usize && binding == 0)
        {
            return Err(ForeignElfError::InvalidSymbolTable);
        }
        if index == 0 {
            if symbol.name_offset != 0
                || symbol.information != 0
                || symbol.visibility != 0
                || symbol.section_index != SECTION_UNDEFINED
                || symbol.value != 0
                || symbol.size != 0
            {
                return Err(ForeignElfError::InvalidSymbol);
            }
            continue;
        }
        match symbol.section_index {
            SECTION_UNDEFINED => {
                if symbol.value != 0 || symbol.size != 0 {
                    return Err(ForeignElfError::InvalidSymbol);
                }
            }
            SECTION_ABSOLUTE => {}
            reserved if reserved >= RESERVED_SECTION_START => {
                return Err(ForeignElfError::InvalidSymbol);
            }
            section_index => {
                let section = module
                    .section(section_index as usize)
                    .ok_or(ForeignElfError::InvalidSymbol)?;
                if symbol
                    .value
                    .checked_add(symbol.size)
                    .is_none_or(|end| end > section.size)
                {
                    return Err(ForeignElfError::InvalidSymbol);
                }
            }
        }
    }
    Ok(())
}

fn resolve_entry_symbol(
    module: &ElfModule<'_>,
    symbols: &[u8],
    strings: &[u8],
    placements: &[ElfSectionPlacement],
    image_virtual_address: u64,
    requested_name: &[u8],
) -> Result<u64, ForeignElfError> {
    let mut entry = None;
    for index in 1..symbols.len() / size_of::<SymbolEntry>() {
        let symbol = parse_symbol(symbols, index).ok_or(ForeignElfError::InvalidSymbol)?;
        let name = string_at(strings, symbol.name_offset as usize)
            .ok_or(ForeignElfError::InvalidSymbol)?;
        if name != requested_name {
            continue;
        }
        if entry.is_some() {
            return Err(ForeignElfError::DuplicateEntrySymbol);
        }
        if symbol.information >> 4 != SYMBOL_BINDING_GLOBAL
            || symbol.information & 0xf != SYMBOL_TYPE_FUNCTION
            || symbol.section_index == SECTION_UNDEFINED
            || symbol.section_index == SECTION_ABSOLUTE
        {
            return Err(ForeignElfError::InvalidEntrySymbol);
        }
        let placement = placement_for(placements, symbol.section_index as usize)
            .filter(|placement| placement.executable)
            .ok_or(ForeignElfError::InvalidEntrySymbol)?;
        let section = module
            .section(symbol.section_index as usize)
            .ok_or(ForeignElfError::InvalidEntrySymbol)?;
        if symbol.value >= section.size || symbol.size == 0 {
            return Err(ForeignElfError::InvalidEntrySymbol);
        }
        let address = image_virtual_address
            .checked_add(placement.image_offset as u64)
            .and_then(|value| value.checked_add(symbol.value))
            .ok_or(ForeignElfError::InvalidEntrySymbol)?;
        entry = Some(address);
    }
    entry.ok_or(ForeignElfError::MissingEntrySymbol)
}

fn prepare_relocations(
    module: &ElfModule<'_>,
    symbol_table_index: usize,
    symbols: &[u8],
    strings: &[u8],
    placements: &[ElfSectionPlacement],
    resolver: &dyn ExternalSymbolResolver,
    image_virtual_address: u64,
) -> Result<Vec<PreparedPatch>, ForeignElfError> {
    let mut relocation_count = 0_usize;
    for index in 1..module.section_count() {
        let section = module
            .section(index)
            .ok_or(ForeignElfError::InvalidRelocationSection)?;
        if section.section_type == SECTION_TYPE_RELA {
            validate_relocation_section(module, section, symbol_table_index, placements)?;
            let count = usize::try_from(section.size / size_of::<RelocationEntry>() as u64)
                .map_err(|_| ForeignElfError::InvalidRelocationSection)?;
            relocation_count = relocation_count
                .checked_add(count)
                .filter(|count| *count <= MAXIMUM_RELOCATIONS)
                .ok_or(ForeignElfError::InvalidRelocationSection)?;
        }
    }

    let mut patches = Vec::new();
    patches
        .try_reserve_exact(relocation_count)
        .map_err(|_| ForeignElfError::PlanAllocationFailed)?;
    for index in 1..module.section_count() {
        let relocation_section = module
            .section(index)
            .ok_or(ForeignElfError::InvalidRelocationSection)?;
        if relocation_section.section_type != SECTION_TYPE_RELA {
            continue;
        }
        let target = placement_for(placements, relocation_section.info as usize)
            .ok_or(ForeignElfError::InvalidRelocationSection)?;
        let relocation_bytes = module
            .section_data(relocation_section)
            .map_err(|_| ForeignElfError::InvalidRelocationSection)?;
        for relocation_index in 0..relocation_bytes.len() / size_of::<RelocationEntry>() {
            let relocation = parse_relocation(relocation_bytes, relocation_index)
                .ok_or(ForeignElfError::InvalidRelocation)?;
            patches.push(prepare_patch(
                relocation,
                target,
                symbols,
                strings,
                placements,
                resolver,
                image_virtual_address,
            )?);
        }
    }
    patches.sort_unstable_by_key(|patch| patch.image_offset);
    if patches.windows(2).any(|pair| {
        pair[0]
            .image_offset
            .checked_add(pair[0].width())
            .is_none_or(|end| end > pair[1].image_offset)
    }) {
        return Err(ForeignElfError::OverlappingRelocations);
    }
    Ok(patches)
}

fn validate_relocation_section(
    module: &ElfModule<'_>,
    section: SectionHeader,
    symbol_table_index: usize,
    placements: &[ElfSectionPlacement],
) -> Result<(), ForeignElfError> {
    if section.entry_size != size_of::<RelocationEntry>() as u64
        || section.size % section.entry_size != 0
        || section.flags != 0
        || section.address != 0
        || section.alignment != 8
        || section.link as usize != symbol_table_index
        || placement_for(placements, section.info as usize).is_none()
        || module.section(section.info as usize).is_none()
    {
        return Err(ForeignElfError::InvalidRelocationSection);
    }
    Ok(())
}

fn prepare_patch(
    relocation: RelocationEntry,
    target: &ElfSectionPlacement,
    symbols: &[u8],
    strings: &[u8],
    placements: &[ElfSectionPlacement],
    resolver: &dyn ExternalSymbolResolver,
    image_virtual_address: u64,
) -> Result<PreparedPatch, ForeignElfError> {
    let symbol_index = usize::try_from(relocation.information >> 32)
        .map_err(|_| ForeignElfError::InvalidRelocation)?;
    let relocation_type = relocation.information as u32;
    let symbol = parse_symbol(symbols, symbol_index).ok_or(ForeignElfError::InvalidRelocation)?;
    let symbol_address = match symbol.section_index {
        SECTION_UNDEFINED => {
            if symbol.information >> 4 != SYMBOL_BINDING_GLOBAL || symbol.visibility != 0 {
                return Err(ForeignElfError::InvalidRelocation);
            }
            let name = string_at(strings, symbol.name_offset as usize)
                .filter(|name| !name.is_empty())
                .ok_or(ForeignElfError::InvalidRelocation)?;
            resolver
                .resolve(name)
                .ok_or(ForeignElfError::UnresolvedExternal)?
        }
        SECTION_ABSOLUTE => symbol.value,
        section_index => {
            let placement = placement_for(placements, section_index as usize)
                .ok_or(ForeignElfError::InvalidRelocation)?;
            if symbol.value >= placement.memory_size as u64 {
                return Err(ForeignElfError::InvalidRelocation);
            }
            image_virtual_address
                .checked_add(placement.image_offset as u64)
                .and_then(|address| address.checked_add(symbol.value))
                .ok_or(ForeignElfError::RelocationValueOutOfRange)?
        }
    };
    let relative_offset =
        usize::try_from(relocation.offset).map_err(|_| ForeignElfError::InvalidRelocation)?;
    let image_offset = target
        .image_offset
        .checked_add(relative_offset)
        .ok_or(ForeignElfError::InvalidRelocation)?;
    let place = image_virtual_address
        .checked_add(image_offset as u64)
        .ok_or(ForeignElfError::RelocationValueOutOfRange)?;
    let value = i128::from(symbol_address) + i128::from(relocation.addend);
    let prepared = match relocation_type {
        R_X86_64_64 | R_X86_64_GLOB_DAT => {
            if relative_offset
                .checked_add(size_of::<u64>())
                .is_none_or(|end| end > target.memory_size)
            {
                return Err(ForeignElfError::InvalidRelocation);
            }
            PreparedValue::Absolute(
                u64::try_from(value).map_err(|_| ForeignElfError::RelocationValueOutOfRange)?,
            )
        }
        R_X86_64_PC32 | R_X86_64_PLT32 => {
            if relative_offset
                .checked_add(size_of::<i32>())
                .is_none_or(|end| end > target.memory_size)
            {
                return Err(ForeignElfError::InvalidRelocation);
            }
            let relative = value - i128::from(place);
            PreparedValue::Relative(
                i32::try_from(relative).map_err(|_| ForeignElfError::RelocationValueOutOfRange)?,
            )
        }
        unsupported => return Err(ForeignElfError::UnsupportedRelocation(unsupported)),
    };
    Ok(PreparedPatch {
        image_offset,
        value: prepared,
    })
}

fn placement_for(
    placements: &[ElfSectionPlacement],
    section_index: usize,
) -> Option<&ElfSectionPlacement> {
    placements
        .iter()
        .find(|placement| placement.section_index == section_index)
}

fn parse_symbol(bytes: &[u8], index: usize) -> Option<SymbolEntry> {
    let offset = index.checked_mul(size_of::<SymbolEntry>())?;
    Some(SymbolEntry {
        name_offset: read_u32(bytes, offset)?,
        information: *bytes.get(offset + 4)?,
        visibility: *bytes.get(offset + 5)?,
        section_index: read_u16(bytes, offset + 6)?,
        value: read_u64(bytes, offset + 8)?,
        size: read_u64(bytes, offset + 16)?,
    })
}

fn parse_relocation(bytes: &[u8], index: usize) -> Option<RelocationEntry> {
    let offset = index.checked_mul(size_of::<RelocationEntry>())?;
    Some(RelocationEntry {
        offset: read_u64(bytes, offset)?,
        information: read_u64(bytes, offset + 8)?,
        addend: read_i64(bytes, offset + 16)?,
    })
}

fn string_at(bytes: &[u8], offset: usize) -> Option<&[u8]> {
    let suffix = bytes.get(offset..)?;
    let length = suffix.iter().position(|byte| *byte == 0)?;
    Some(&suffix[..length])
}

fn align_up(value: usize, alignment: usize) -> Option<usize> {
    debug_assert!(alignment.is_power_of_two());
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
}

fn ranges_overlap(
    left_start: usize,
    left_end: usize,
    right_start: usize,
    right_end: usize,
) -> bool {
    left_start < right_end && right_start < left_end
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

fn read_i64(bytes: &[u8], offset: usize) -> Option<i64> {
    Some(i64::from_le_bytes(
        bytes.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;
    use crate::mirage::personality::{LinuxVersion, NtVersion};
    use crate::shim::linux_kpi;

    const IMAGE_BASE: u64 = 0x20_0000;
    const SECTION_TABLE_OFFSET: usize = 64;
    const TEXT_OFFSET: usize = 448;
    const RELA_OFFSET: usize = 464;
    const SYMBOL_OFFSET: usize = 488;
    const STRING_OFFSET: usize = 560;
    const SECTION_STRING_OFFSET: usize = 576;

    struct InstalledApi;

    impl Drop for InstalledApi {
        fn drop(&mut self) {
            let _ = unsafe { linux_kpi::uninstall() };
        }
    }

    fn write_section(bytes: &mut [u8], index: usize, section: SectionHeader) {
        let offset = SECTION_TABLE_OFFSET + index * 64;
        bytes[offset..offset + 4].copy_from_slice(&section.name_offset.to_le_bytes());
        bytes[offset + 4..offset + 8].copy_from_slice(&section.section_type.to_le_bytes());
        bytes[offset + 8..offset + 16].copy_from_slice(&section.flags.to_le_bytes());
        bytes[offset + 16..offset + 24].copy_from_slice(&section.address.to_le_bytes());
        bytes[offset + 24..offset + 32].copy_from_slice(&section.offset.to_le_bytes());
        bytes[offset + 32..offset + 40].copy_from_slice(&section.size.to_le_bytes());
        bytes[offset + 40..offset + 44].copy_from_slice(&section.link.to_le_bytes());
        bytes[offset + 44..offset + 48].copy_from_slice(&section.info.to_le_bytes());
        bytes[offset + 48..offset + 56].copy_from_slice(&section.alignment.to_le_bytes());
        bytes[offset + 56..offset + 64].copy_from_slice(&section.entry_size.to_le_bytes());
    }

    fn write_symbol(bytes: &mut [u8], index: usize, symbol: SymbolEntry) {
        let offset = SYMBOL_OFFSET + index * size_of::<SymbolEntry>();
        bytes[offset..offset + 4].copy_from_slice(&symbol.name_offset.to_le_bytes());
        bytes[offset + 4] = symbol.information;
        bytes[offset + 5] = symbol.visibility;
        bytes[offset + 6..offset + 8].copy_from_slice(&symbol.section_index.to_le_bytes());
        bytes[offset + 8..offset + 16].copy_from_slice(&symbol.value.to_le_bytes());
        bytes[offset + 16..offset + 24].copy_from_slice(&symbol.size.to_le_bytes());
    }

    fn relocatable_module(relocation_type: u32) -> [u8; 640] {
        let mut bytes = [0_u8; 640];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[6] = 1;
        bytes[16..18].copy_from_slice(&(1_u16).to_le_bytes());
        bytes[18..20].copy_from_slice(&(62_u16).to_le_bytes());
        bytes[20..24].copy_from_slice(&(1_u32).to_le_bytes());
        bytes[40..48].copy_from_slice(&(SECTION_TABLE_OFFSET as u64).to_le_bytes());
        bytes[52..54].copy_from_slice(&(64_u16).to_le_bytes());
        bytes[58..60].copy_from_slice(&(64_u16).to_le_bytes());
        bytes[60..62].copy_from_slice(&(6_u16).to_le_bytes());
        bytes[62..64].copy_from_slice(&(5_u16).to_le_bytes());

        write_section(
            &mut bytes,
            1,
            SectionHeader {
                name_offset: 1,
                section_type: SECTION_TYPE_PROGRAM_BITS,
                flags: SECTION_ALLOCATE | SECTION_EXECUTE,
                address: 0,
                offset: TEXT_OFFSET as u64,
                size: 16,
                link: 0,
                info: 0,
                alignment: 16,
                entry_size: 0,
            },
        );
        write_section(
            &mut bytes,
            2,
            SectionHeader {
                name_offset: 7,
                section_type: SECTION_TYPE_RELA,
                flags: 0,
                address: 0,
                offset: RELA_OFFSET as u64,
                size: size_of::<RelocationEntry>() as u64,
                link: 3,
                info: 1,
                alignment: 8,
                entry_size: size_of::<RelocationEntry>() as u64,
            },
        );
        write_section(
            &mut bytes,
            3,
            SectionHeader {
                name_offset: 18,
                section_type: SECTION_TYPE_SYMBOL_TABLE,
                flags: 0,
                address: 0,
                offset: SYMBOL_OFFSET as u64,
                size: (3 * size_of::<SymbolEntry>()) as u64,
                link: 4,
                info: 1,
                alignment: 8,
                entry_size: size_of::<SymbolEntry>() as u64,
            },
        );
        write_section(
            &mut bytes,
            4,
            SectionHeader {
                name_offset: 26,
                section_type: SECTION_TYPE_STRING_TABLE,
                flags: 0,
                address: 0,
                offset: STRING_OFFSET as u64,
                size: 15,
                link: 0,
                info: 0,
                alignment: 1,
                entry_size: 0,
            },
        );
        write_section(
            &mut bytes,
            5,
            SectionHeader {
                name_offset: 34,
                section_type: SECTION_TYPE_STRING_TABLE,
                flags: 0,
                address: 0,
                offset: SECTION_STRING_OFFSET as u64,
                size: 44,
                link: 0,
                info: 0,
                alignment: 1,
                entry_size: 0,
            },
        );

        bytes[TEXT_OFFSET..TEXT_OFFSET + 8].fill(0x90);
        bytes[RELA_OFFSET..RELA_OFFSET + 8].copy_from_slice(&(8_u64).to_le_bytes());
        let relocation_information = (2_u64 << 32) | u64::from(relocation_type);
        bytes[RELA_OFFSET + 8..RELA_OFFSET + 16]
            .copy_from_slice(&relocation_information.to_le_bytes());

        write_symbol(
            &mut bytes,
            1,
            SymbolEntry {
                name_offset: 1,
                information: (SYMBOL_BINDING_GLOBAL << 4) | SYMBOL_TYPE_FUNCTION,
                visibility: 0,
                section_index: 1,
                value: 0,
                size: 8,
            },
        );
        write_symbol(
            &mut bytes,
            2,
            SymbolEntry {
                name_offset: 7,
                information: SYMBOL_BINDING_GLOBAL << 4,
                visibility: 0,
                section_index: SECTION_UNDEFINED,
                value: 0,
                size: 0,
            },
        );
        bytes[STRING_OFFSET..STRING_OFFSET + 15].copy_from_slice(b"\0entry\0kmalloc\0");
        bytes[SECTION_STRING_OFFSET..SECTION_STRING_OFFSET + 44]
            .copy_from_slice(b"\0.text\0.rela.text\0.symtab\0.strtab\0.shstrtab\0");
        bytes
    }

    #[test]
    fn preflights_and_commits_the_exact_linux_elf_subset() {
        let _lock = linux_kpi::TEST_INSTALL_LOCK.lock();
        let _ = unsafe { linux_kpi::uninstall() };
        assert_eq!(
            unsafe { linux_kpi::install(&linux_kpi::TEST_KERNEL_API) },
            Ok(())
        );
        let _installed = InstalledApi;
        let bytes = relocatable_module(R_X86_64_64);

        let plan = ForeignElfLoadPlan::preflight(
            &bytes,
            OsPersonality::Linux(LinuxVersion::V6_1),
            IMAGE_BASE,
            b"entry",
        )
        .unwrap();
        assert_eq!(plan.image_virtual_address(), IMAGE_BASE);
        assert_eq!(plan.image_alignment(), PAGE_ALIGNMENT);
        assert_eq!(plan.entry_address(), IMAGE_BASE);
        assert_eq!(plan.relocation_count(), 1);
        assert_eq!(plan.sections().len(), 1);

        let mut wrong_size = vec![0xa5; plan.image_size() - 1];
        assert_eq!(
            plan.commit(&mut wrong_size),
            Err(ForeignElfError::ImageSizeMismatch)
        );
        assert!(wrong_size.iter().all(|byte| *byte == 0xa5));

        let mut image = vec![0xa5; plan.image_size()];
        plan.commit(&mut image).unwrap();
        assert_eq!(&image[..8], &[0x90; 8]);
        assert_eq!(
            u64::from_le_bytes(image[8..16].try_into().unwrap()),
            linux_kpi::kmalloc as *const () as usize as u64
        );
        assert!(image[16..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn rejects_metadata_before_a_destination_can_be_mutated() {
        let _lock = linux_kpi::TEST_INSTALL_LOCK.lock();
        let _ = unsafe { linux_kpi::uninstall() };
        assert_eq!(
            unsafe { linux_kpi::install(&linux_kpi::TEST_KERNEL_API) },
            Ok(())
        );
        let _installed = InstalledApi;

        let unsupported = relocatable_module(42);
        assert!(matches!(
            ForeignElfLoadPlan::preflight(
                &unsupported,
                OsPersonality::Linux(LinuxVersion::V6_1),
                IMAGE_BASE,
                b"entry"
            ),
            Err(ForeignElfError::UnsupportedRelocation(42))
        ));

        let mut unresolved = relocatable_module(R_X86_64_64);
        unresolved[STRING_OFFSET + 7..STRING_OFFSET + 14].copy_from_slice(b"missing");
        assert!(matches!(
            ForeignElfLoadPlan::preflight(
                &unresolved,
                OsPersonality::Linux(LinuxVersion::V6_1),
                IMAGE_BASE,
                b"entry"
            ),
            Err(ForeignElfError::UnresolvedExternal)
        ));

        let mut bad_symbol = relocatable_module(R_X86_64_64);
        bad_symbol[RELA_OFFSET + 8..RELA_OFFSET + 16]
            .copy_from_slice(&((9_u64 << 32) | u64::from(R_X86_64_64)).to_le_bytes());
        assert!(matches!(
            ForeignElfLoadPlan::preflight(
                &bad_symbol,
                OsPersonality::Linux(LinuxVersion::V6_1),
                IMAGE_BASE,
                b"entry"
            ),
            Err(ForeignElfError::InvalidRelocation)
        ));

        let mut overlapping_sections = relocatable_module(R_X86_64_64);
        let text_header = SECTION_TABLE_OFFSET + SECTION_HEADER_LENGTH;
        overlapping_sections[text_header + 24..text_header + 32]
            .copy_from_slice(&(SYMBOL_OFFSET as u64).to_le_bytes());
        assert!(matches!(
            ForeignElfLoadPlan::preflight(
                &overlapping_sections,
                OsPersonality::Linux(LinuxVersion::V6_1),
                IMAGE_BASE,
                b"entry"
            ),
            Err(ForeignElfError::InvalidSectionLayout)
        ));
    }

    #[test]
    fn rejects_unavailable_or_non_elf_personalities() {
        let bytes = relocatable_module(R_X86_64_64);
        assert!(matches!(
            ForeignElfLoadPlan::preflight(
                &bytes,
                OsPersonality::WindowsNt(NtVersion::Windows11),
                IMAGE_BASE,
                b"entry"
            ),
            Err(ForeignElfError::PersonalityFormatMismatch)
        ));

        let _lock = linux_kpi::TEST_INSTALL_LOCK.lock();
        let _ = unsafe { linux_kpi::uninstall() };
        assert!(matches!(
            ForeignElfLoadPlan::preflight(
                &bytes,
                OsPersonality::Linux(LinuxVersion::V6_1),
                IMAGE_BASE,
                b"entry"
            ),
            Err(ForeignElfError::Personality(
                PersonalityError::RuntimeUnavailable
            ))
        ));
    }
}
