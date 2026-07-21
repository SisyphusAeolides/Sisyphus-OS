#[repr(C)]
#[derive(Clone, Copy)]
pub struct Elf64Header {
    pub identification: [u8; 16],
    pub object_type: u16,
    pub machine: u16,
    pub version: u32,
    pub entry_point: u64,
    pub program_header_offset: u64,
    pub section_header_offset: u64,
    pub flags: u32,
    pub header_size: u16,
    pub program_header_entry_size: u16,
    pub program_header_count: u16,
    pub section_header_entry_size: u16,
    pub section_header_count: u16,
    pub section_name_table_index: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ProgramHeader {
    pub segment_type: u32,
    pub flags: u32,
    pub file_offset: u64,
    pub virtual_address: u64,
    pub physical_address: u64,
    pub file_size: u64,
    pub memory_size: u64,
    pub alignment: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RelocationEntry {
    pub offset: u64,
    pub information: u64,
    pub addend: i64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SymbolEntry {
    pub name_offset: u32,
    pub information: u8,
    pub visibility: u8,
    pub section_index: u16,
    pub value: u64,
    pub size: u64,
}

const _: () = assert!(core::mem::size_of::<Elf64Header>() == 64);
const _: () = assert!(core::mem::size_of::<ProgramHeader>() == 56);
const _: () = assert!(core::mem::size_of::<RelocationEntry>() == 24);
const _: () = assert!(core::mem::size_of::<SymbolEntry>() == 24);
