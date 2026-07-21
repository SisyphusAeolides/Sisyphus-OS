use core::slice;

const RSDP_SIGNATURE: &[u8; 8] = b"RSD PTR ";
const RSDP_V1_LENGTH: usize = 20;
const RSDP_V2_LENGTH: usize = 36;
const SDT_HEADER_LENGTH: usize = 36;
const MADT_HEADER_LENGTH: usize = 44;
const MAXIMUM_SDT_LENGTH: usize = 1024 * 1024;
const XSDT_SIGNATURE: &[u8; 4] = b"XSDT";
const RSDT_SIGNATURE: &[u8; 4] = b"RSDT";
const MADT_SIGNATURE: &[u8; 4] = b"APIC";

pub const MAXIMUM_IO_APICS: usize = 8;
pub const MAXIMUM_INTERRUPT_OVERRIDES: usize = 24;
pub const MAXIMUM_PROCESSORS: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rsdp {
    pub revision: u8,
    pub rsdt_address: u32,
    pub xsdt_address: Option<u64>,
}

impl Rsdp {
    pub fn parse(bytes: &[u8]) -> Result<Self, AcpiError> {
        if bytes.len() < RSDP_V1_LENGTH {
            return Err(AcpiError::Truncated);
        }
        if bytes.get(..8) != Some(RSDP_SIGNATURE) {
            return Err(AcpiError::InvalidSignature);
        }
        if !checksum_is_valid(&bytes[..RSDP_V1_LENGTH]) {
            return Err(AcpiError::InvalidChecksum);
        }

        let revision = bytes[15];
        let rsdt_address = read_u32(bytes, 16).ok_or(AcpiError::Truncated)?;
        let xsdt_address = if revision >= 2 {
            if bytes.len() < RSDP_V2_LENGTH {
                return Err(AcpiError::Truncated);
            }
            let length = read_u32(bytes, 20).ok_or(AcpiError::Truncated)? as usize;
            if !(RSDP_V2_LENGTH..=bytes.len()).contains(&length) {
                return Err(AcpiError::InvalidLength);
            }
            if !checksum_is_valid(&bytes[..length]) {
                return Err(AcpiError::InvalidChecksum);
            }
            let address = read_u64(bytes, 24).ok_or(AcpiError::Truncated)?;
            (address != 0).then_some(address)
        } else {
            None
        };

        if xsdt_address.is_none() && rsdt_address == 0 {
            return Err(AcpiError::InvalidAddress);
        }
        Ok(Self {
            revision,
            rsdt_address,
            xsdt_address,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoApicDescriptor {
    pub id: u8,
    pub address: u32,
    pub global_system_interrupt_base: u32,
}

impl IoApicDescriptor {
    const EMPTY: Self = Self {
        id: 0,
        address: 0,
        global_system_interrupt_base: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterruptPolarity {
    Conforms,
    ActiveHigh,
    ActiveLow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterruptTriggerMode {
    Conforms,
    Edge,
    Level,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InterruptSourceOverride {
    pub bus: u8,
    pub source: u8,
    pub global_system_interrupt: u32,
    pub polarity: InterruptPolarity,
    pub trigger_mode: InterruptTriggerMode,
}

impl InterruptSourceOverride {
    const EMPTY: Self = Self {
        bus: 0,
        source: 0,
        global_system_interrupt: 0,
        polarity: InterruptPolarity::Conforms,
        trigger_mode: InterruptTriggerMode::Conforms,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessorDescriptor {
    pub firmware_uid: u32,
    pub apic_id: u32,
    pub enabled: bool,
    pub online_capable: bool,
    pub uses_x2apic: bool,
}

impl ProcessorDescriptor {
    const EMPTY: Self = Self {
        firmware_uid: 0,
        apic_id: 0,
        enabled: false,
        online_capable: false,
        uses_x2apic: false,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MadtInfo {
    pub local_apic_address: u64,
    pub flags: u32,
    io_apics: [IoApicDescriptor; MAXIMUM_IO_APICS],
    io_apic_count: usize,
    overrides: [InterruptSourceOverride; MAXIMUM_INTERRUPT_OVERRIDES],
    override_count: usize,
    processors: [ProcessorDescriptor; MAXIMUM_PROCESSORS],
    processor_count: usize,
}

impl MadtInfo {
    fn new(local_apic_address: u64, flags: u32) -> Self {
        Self {
            local_apic_address,
            flags,
            io_apics: [IoApicDescriptor::EMPTY; MAXIMUM_IO_APICS],
            io_apic_count: 0,
            overrides: [InterruptSourceOverride::EMPTY; MAXIMUM_INTERRUPT_OVERRIDES],
            override_count: 0,
            processors: [ProcessorDescriptor::EMPTY; MAXIMUM_PROCESSORS],
            processor_count: 0,
        }
    }

    pub fn io_apics(&self) -> &[IoApicDescriptor] {
        &self.io_apics[..self.io_apic_count]
    }

    pub fn interrupt_source_overrides(&self) -> &[InterruptSourceOverride] {
        &self.overrides[..self.override_count]
    }

    pub fn processors(&self) -> &[ProcessorDescriptor] {
        &self.processors[..self.processor_count]
    }

    fn push_io_apic(&mut self, descriptor: IoApicDescriptor) -> Result<(), AcpiError> {
        let slot = self
            .io_apics
            .get_mut(self.io_apic_count)
            .ok_or(AcpiError::CapacityExceeded)?;
        *slot = descriptor;
        self.io_apic_count += 1;
        Ok(())
    }

    fn push_override(&mut self, entry: InterruptSourceOverride) -> Result<(), AcpiError> {
        let slot = self
            .overrides
            .get_mut(self.override_count)
            .ok_or(AcpiError::CapacityExceeded)?;
        *slot = entry;
        self.override_count += 1;
        Ok(())
    }

    fn push_processor(&mut self, processor: ProcessorDescriptor) -> Result<(), AcpiError> {
        if self
            .processors()
            .iter()
            .any(|existing| existing.apic_id == processor.apic_id)
        {
            return Err(AcpiError::DuplicateProcessor);
        }
        let slot = self
            .processors
            .get_mut(self.processor_count)
            .ok_or(AcpiError::CapacityExceeded)?;
        *slot = processor;
        self.processor_count += 1;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AcpiError {
    Truncated,
    InvalidSignature,
    InvalidChecksum,
    InvalidLength,
    InvalidAddress,
    AddressUnavailable,
    MissingMadt,
    MalformedMadt,
    InvalidInterruptFlags,
    DuplicateProcessor,
    CapacityExceeded,
}

/// Finds and parses the MADT referenced by an already validated RSDP.
///
/// # Safety
///
/// `map` must return a readable, stable virtual mapping for every complete
/// physical range it accepts. Those mappings must remain valid until this
/// function returns and must not be mutated concurrently.
pub unsafe fn discover_madt<F>(rsdp: Rsdp, map: F) -> Result<MadtInfo, AcpiError>
where
    F: Fn(u64, usize) -> Option<*const u8> + Copy,
{
    if let Some(xsdt_address) = rsdp.xsdt_address {
        unsafe { find_madt(xsdt_address, XSDT_SIGNATURE, 8, map) }
    } else {
        unsafe { find_madt(u64::from(rsdp.rsdt_address), RSDT_SIGNATURE, 4, map) }
    }
}

unsafe fn find_madt<F>(
    root_address: u64,
    expected_signature: &[u8; 4],
    entry_width: usize,
    map: F,
) -> Result<MadtInfo, AcpiError>
where
    F: Fn(u64, usize) -> Option<*const u8> + Copy,
{
    let root = unsafe { validated_table(root_address, map)? };
    if root.get(..4) != Some(expected_signature) {
        return Err(AcpiError::InvalidSignature);
    }
    let entries = &root[SDT_HEADER_LENGTH..];
    if entries.len() % entry_width != 0 {
        return Err(AcpiError::InvalidLength);
    }

    for entry in entries.chunks_exact(entry_width) {
        let address = if entry_width == 8 {
            read_u64(entry, 0).ok_or(AcpiError::Truncated)?
        } else {
            u64::from(read_u32(entry, 0).ok_or(AcpiError::Truncated)?)
        };
        let header = unsafe { mapped_bytes(address, SDT_HEADER_LENGTH, map)? };
        if header.get(..4) == Some(MADT_SIGNATURE) {
            let table = unsafe { validated_table(address, map)? };
            return parse_madt(table);
        }
    }
    Err(AcpiError::MissingMadt)
}

unsafe fn validated_table<F>(address: u64, map: F) -> Result<&'static [u8], AcpiError>
where
    F: Fn(u64, usize) -> Option<*const u8> + Copy,
{
    let header = unsafe { mapped_bytes(address, SDT_HEADER_LENGTH, map)? };
    let length = read_u32(header, 4).ok_or(AcpiError::Truncated)? as usize;
    if !(SDT_HEADER_LENGTH..=MAXIMUM_SDT_LENGTH).contains(&length) {
        return Err(AcpiError::InvalidLength);
    }
    let table = unsafe { mapped_bytes(address, length, map)? };
    if !checksum_is_valid(table) {
        return Err(AcpiError::InvalidChecksum);
    }
    Ok(table)
}

unsafe fn mapped_bytes<F>(address: u64, length: usize, map: F) -> Result<&'static [u8], AcpiError>
where
    F: Fn(u64, usize) -> Option<*const u8>,
{
    if address == 0 || length == 0 || address.checked_add(length as u64).is_none() {
        return Err(AcpiError::InvalidAddress);
    }
    let pointer = map(address, length).ok_or(AcpiError::AddressUnavailable)?;
    if pointer.is_null() {
        return Err(AcpiError::AddressUnavailable);
    }
    Ok(unsafe { slice::from_raw_parts(pointer, length) })
}

fn parse_madt(table: &[u8]) -> Result<MadtInfo, AcpiError> {
    if table.len() < MADT_HEADER_LENGTH || table.get(..4) != Some(MADT_SIGNATURE) {
        return Err(AcpiError::MalformedMadt);
    }
    let local_apic_address = u64::from(read_u32(table, 36).ok_or(AcpiError::MalformedMadt)?);
    let flags = read_u32(table, 40).ok_or(AcpiError::MalformedMadt)?;
    let mut madt = MadtInfo::new(local_apic_address, flags);

    let mut offset = MADT_HEADER_LENGTH;
    while offset < table.len() {
        if table.len() - offset < 2 {
            return Err(AcpiError::MalformedMadt);
        }
        let entry_type = table[offset];
        let entry_length = table[offset + 1] as usize;
        if entry_length < 2 || offset + entry_length > table.len() {
            return Err(AcpiError::MalformedMadt);
        }
        let entry = &table[offset..offset + entry_length];
        match entry_type {
            0 => {
                if entry.len() < 8 {
                    return Err(AcpiError::MalformedMadt);
                }
                let processor_flags = read_u32(entry, 4).ok_or(AcpiError::MalformedMadt)?;
                madt.push_processor(ProcessorDescriptor {
                    firmware_uid: u32::from(entry[2]),
                    apic_id: u32::from(entry[3]),
                    enabled: processor_flags & 1 != 0,
                    online_capable: processor_flags & 2 != 0,
                    uses_x2apic: false,
                })?;
            }
            1 => {
                if entry.len() < 12 {
                    return Err(AcpiError::MalformedMadt);
                }
                madt.push_io_apic(IoApicDescriptor {
                    id: entry[2],
                    address: read_u32(entry, 4).ok_or(AcpiError::MalformedMadt)?,
                    global_system_interrupt_base: read_u32(entry, 8)
                        .ok_or(AcpiError::MalformedMadt)?,
                })?;
            }
            2 => {
                if entry.len() < 10 {
                    return Err(AcpiError::MalformedMadt);
                }
                let interrupt_flags = read_u16(entry, 8).ok_or(AcpiError::MalformedMadt)?;
                madt.push_override(InterruptSourceOverride {
                    bus: entry[2],
                    source: entry[3],
                    global_system_interrupt: read_u32(entry, 4).ok_or(AcpiError::MalformedMadt)?,
                    polarity: decode_polarity(interrupt_flags)?,
                    trigger_mode: decode_trigger_mode(interrupt_flags)?,
                })?;
            }
            5 => {
                if entry.len() < 12 {
                    return Err(AcpiError::MalformedMadt);
                }
                madt.local_apic_address = read_u64(entry, 4).ok_or(AcpiError::MalformedMadt)?;
            }
            9 => {
                if entry.len() < 16 {
                    return Err(AcpiError::MalformedMadt);
                }
                let processor_flags = read_u32(entry, 8).ok_or(AcpiError::MalformedMadt)?;
                madt.push_processor(ProcessorDescriptor {
                    firmware_uid: read_u32(entry, 12).ok_or(AcpiError::MalformedMadt)?,
                    apic_id: read_u32(entry, 4).ok_or(AcpiError::MalformedMadt)?,
                    enabled: processor_flags & 1 != 0,
                    online_capable: processor_flags & 2 != 0,
                    uses_x2apic: true,
                })?;
            }
            _ => {}
        }
        offset += entry_length;
    }
    if madt.io_apics().is_empty() {
        return Err(AcpiError::MalformedMadt);
    }
    Ok(madt)
}

fn decode_polarity(flags: u16) -> Result<InterruptPolarity, AcpiError> {
    match flags & 0b11 {
        0 => Ok(InterruptPolarity::Conforms),
        1 => Ok(InterruptPolarity::ActiveHigh),
        3 => Ok(InterruptPolarity::ActiveLow),
        _ => Err(AcpiError::InvalidInterruptFlags),
    }
}

fn decode_trigger_mode(flags: u16) -> Result<InterruptTriggerMode, AcpiError> {
    match (flags >> 2) & 0b11 {
        0 => Ok(InterruptTriggerMode::Conforms),
        1 => Ok(InterruptTriggerMode::Edge),
        3 => Ok(InterruptTriggerMode::Level),
        _ => Err(AcpiError::InvalidInterruptFlags),
    }
}

fn checksum_is_valid(bytes: &[u8]) -> bool {
    bytes.iter().copied().fold(0_u8, u8::wrapping_add) == 0
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

    const MEMORY_BASE: u64 = 0x1000;
    const XSDT_OFFSET: usize = 0x100;
    const MADT_OFFSET: usize = 0x200;

    fn set_checksum(bytes: &mut [u8], checksum_offset: usize) {
        bytes[checksum_offset] = 0;
        let sum = bytes.iter().copied().fold(0_u8, u8::wrapping_add);
        bytes[checksum_offset] = 0_u8.wrapping_sub(sum);
    }

    fn rsdp(xsdt_address: u64) -> [u8; RSDP_V2_LENGTH] {
        let mut bytes = [0_u8; RSDP_V2_LENGTH];
        bytes[..8].copy_from_slice(RSDP_SIGNATURE);
        bytes[9..15].copy_from_slice(b"SISYPH");
        bytes[15] = 2;
        bytes[16..20].copy_from_slice(&(0x1234_u32).to_le_bytes());
        bytes[20..24].copy_from_slice(&(RSDP_V2_LENGTH as u32).to_le_bytes());
        bytes[24..32].copy_from_slice(&xsdt_address.to_le_bytes());
        set_checksum(&mut bytes[..RSDP_V1_LENGTH], 8);
        set_checksum(&mut bytes, 32);
        bytes
    }

    fn acpi_memory() -> [u8; 0x400] {
        let mut memory = [0_u8; 0x400];

        let xsdt_length = SDT_HEADER_LENGTH + 8;
        let xsdt = &mut memory[XSDT_OFFSET..XSDT_OFFSET + xsdt_length];
        xsdt[..4].copy_from_slice(XSDT_SIGNATURE);
        xsdt[4..8].copy_from_slice(&(xsdt_length as u32).to_le_bytes());
        xsdt[8] = 1;
        xsdt[SDT_HEADER_LENGTH..]
            .copy_from_slice(&(MEMORY_BASE + MADT_OFFSET as u64).to_le_bytes());
        set_checksum(xsdt, 9);

        let madt_length = MADT_HEADER_LENGTH + 12 + 10 + 12 + 8 + 16;
        let madt = &mut memory[MADT_OFFSET..MADT_OFFSET + madt_length];
        madt[..4].copy_from_slice(MADT_SIGNATURE);
        madt[4..8].copy_from_slice(&(madt_length as u32).to_le_bytes());
        madt[8] = 5;
        madt[36..40].copy_from_slice(&(0xfee0_0000_u32).to_le_bytes());
        madt[40..44].copy_from_slice(&(1_u32).to_le_bytes());

        let io_apic = &mut madt[44..56];
        io_apic[0] = 1;
        io_apic[1] = 12;
        io_apic[2] = 2;
        io_apic[4..8].copy_from_slice(&(0xfec0_0000_u32).to_le_bytes());

        let source_override = &mut madt[56..66];
        source_override[0] = 2;
        source_override[1] = 10;
        source_override[3] = 9;
        source_override[4..8].copy_from_slice(&(20_u32).to_le_bytes());
        source_override[8..10].copy_from_slice(&(0b1111_u16).to_le_bytes());

        let address_override = &mut madt[66..78];
        address_override[0] = 5;
        address_override[1] = 12;
        address_override[4..12].copy_from_slice(&(0xfee0_1000_u64).to_le_bytes());

        let local_apic = &mut madt[78..86];
        local_apic[0] = 0;
        local_apic[1] = 8;
        local_apic[2] = 3;
        local_apic[3] = 7;
        local_apic[4..8].copy_from_slice(&(1_u32).to_le_bytes());

        let x2apic = &mut madt[86..102];
        x2apic[0] = 9;
        x2apic[1] = 16;
        x2apic[4..8].copy_from_slice(&(0x123_u32).to_le_bytes());
        x2apic[8..12].copy_from_slice(&(2_u32).to_le_bytes());
        x2apic[12..16].copy_from_slice(&(42_u32).to_le_bytes());
        set_checksum(madt, 9);
        memory
    }

    #[test]
    fn parses_extended_rsdp_and_rejects_corruption() {
        let mut bytes = rsdp(MEMORY_BASE + XSDT_OFFSET as u64);
        let parsed = Rsdp::parse(&bytes).unwrap();
        assert_eq!(parsed.revision, 2);
        assert_eq!(parsed.xsdt_address, Some(0x1100));

        bytes[33] ^= 1;
        assert_eq!(Rsdp::parse(&bytes), Err(AcpiError::InvalidChecksum));
    }

    #[test]
    fn discovers_io_apic_overrides_and_local_apic_override() {
        let memory = acpi_memory();
        let rsdp = Rsdp::parse(&rsdp(MEMORY_BASE + XSDT_OFFSET as u64)).unwrap();
        let map = |address: u64, length: usize| {
            let offset = address.checked_sub(MEMORY_BASE)? as usize;
            let bytes = memory.get(offset..offset.checked_add(length)?)?;
            Some(bytes.as_ptr())
        };
        let madt = unsafe { discover_madt(rsdp, map) }.unwrap();

        assert_eq!(madt.local_apic_address, 0xfee0_1000);
        assert_eq!(madt.flags, 1);
        assert_eq!(
            madt.io_apics(),
            &[IoApicDescriptor {
                id: 2,
                address: 0xfec0_0000,
                global_system_interrupt_base: 0,
            }]
        );
        assert_eq!(
            madt.interrupt_source_overrides(),
            &[InterruptSourceOverride {
                bus: 0,
                source: 9,
                global_system_interrupt: 20,
                polarity: InterruptPolarity::ActiveLow,
                trigger_mode: InterruptTriggerMode::Level,
            }]
        );
        assert_eq!(
            madt.processors(),
            &[
                ProcessorDescriptor {
                    firmware_uid: 3,
                    apic_id: 7,
                    enabled: true,
                    online_capable: false,
                    uses_x2apic: false,
                },
                ProcessorDescriptor {
                    firmware_uid: 42,
                    apic_id: 0x123,
                    enabled: false,
                    online_capable: true,
                    uses_x2apic: true,
                },
            ]
        );
    }

    #[test]
    fn rejects_malformed_madt_entries() {
        let mut memory = acpi_memory();
        memory[MADT_OFFSET + 45] = 1;
        let madt_length = read_u32(&memory[MADT_OFFSET..], 4).unwrap() as usize;
        set_checksum(&mut memory[MADT_OFFSET..MADT_OFFSET + madt_length], 9);
        let rsdp = Rsdp::parse(&rsdp(MEMORY_BASE + XSDT_OFFSET as u64)).unwrap();
        let map = |address: u64, length: usize| {
            let offset = address.checked_sub(MEMORY_BASE)? as usize;
            Some(memory.get(offset..offset.checked_add(length)?)?.as_ptr())
        };

        assert_eq!(
            unsafe { discover_madt(rsdp, map) },
            Err(AcpiError::MalformedMadt)
        );
    }
}
