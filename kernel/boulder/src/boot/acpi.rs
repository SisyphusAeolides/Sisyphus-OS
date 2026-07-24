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
const DMAR_SIGNATURE: &[u8; 4] = b"DMAR";
const DMAR_HEADER_LENGTH: usize = 48;
const DMAR_DRHD_TYPE: u16 = 0;
const DMAR_DRHD_HEADER_LENGTH: usize = 16;
const DMAR_SCOPE_HEADER_LENGTH: usize = 6;
const DMAR_SCOPE_ENDPOINT: u8 = 1;
const DMAR_INCLUDE_ALL: u8 = 1;

pub const MAXIMUM_IO_APICS: usize = 8;
pub const MAXIMUM_INTERRUPT_OVERRIDES: usize = 24;
pub const MAXIMUM_PROCESSORS: usize = 256;
pub const MAXIMUM_DMAR_UNITS: usize = 16;
pub const MAXIMUM_DMAR_ENDPOINTS: usize = 64;

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
pub struct DmarEndpoint {
    pub segment: u16,
    pub bus: u8,
    pub slot: u8,
    pub function: u8,
}

impl DmarEndpoint {
    const EMPTY: Self = Self {
        segment: 0,
        bus: 0,
        slot: 0,
        function: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmarRemappingUnit {
    pub segment: u16,
    pub register_base: u64,
    pub include_all: bool,
    endpoint_start: u16,
    endpoint_count: u16,
    unresolved_scope_count: u16,
}

impl DmarRemappingUnit {
    const EMPTY: Self = Self {
        segment: 0,
        register_base: 0,
        include_all: false,
        endpoint_start: 0,
        endpoint_count: 0,
        unresolved_scope_count: 0,
    };

    /// True when firmware attached a scope that cannot be reduced to an exact
    /// requester without live PCI bridge-topology resolution.
    pub const fn has_unresolved_scopes(self) -> bool {
        self.unresolved_scope_count != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmarInfo {
    pub host_address_width: u8,
    pub flags: u8,
    units: [DmarRemappingUnit; MAXIMUM_DMAR_UNITS],
    unit_count: usize,
    endpoints: [DmarEndpoint; MAXIMUM_DMAR_ENDPOINTS],
    endpoint_count: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DmarRouteError {
    AmbiguousExplicitScope,
    AmbiguousIncludeAll,
}

impl DmarInfo {
    fn new(host_address_width: u8, flags: u8) -> Self {
        Self {
            host_address_width,
            flags,
            units: [DmarRemappingUnit::EMPTY; MAXIMUM_DMAR_UNITS],
            unit_count: 0,
            endpoints: [DmarEndpoint::EMPTY; MAXIMUM_DMAR_ENDPOINTS],
            endpoint_count: 0,
        }
    }

    pub fn remapping_units(&self) -> &[DmarRemappingUnit] {
        &self.units[..self.unit_count]
    }

    pub fn explicit_endpoints_for(&self, unit: DmarRemappingUnit) -> Option<&[DmarEndpoint]> {
        let unit = self
            .remapping_units()
            .iter()
            .find(|known| **known == unit)?;
        let start = usize::from(unit.endpoint_start);
        let end = start.checked_add(usize::from(unit.endpoint_count))?;
        self.endpoints.get(start..end)
    }

    /// Resolves the exact remapping unit for a requester. An explicit device
    /// scope outranks an include-all unit, matching DMAR routing semantics.
    /// Duplicate explicit owners or include-all fallbacks are rejected rather
    /// than guessed.
    pub fn remapping_unit_for(
        &self,
        endpoint: DmarEndpoint,
    ) -> Result<Option<DmarRemappingUnit>, DmarRouteError> {
        let mut explicit = None;
        let mut include_all = None;
        for unit in self
            .remapping_units()
            .iter()
            .copied()
            .filter(|unit| unit.segment == endpoint.segment)
        {
            let start = usize::from(unit.endpoint_start);
            let end = start + usize::from(unit.endpoint_count);
            if self.endpoints[start..end].contains(&endpoint) {
                if explicit.replace(unit).is_some() {
                    return Err(DmarRouteError::AmbiguousExplicitScope);
                }
            } else if unit.include_all && include_all.replace(unit).is_some() {
                return Err(DmarRouteError::AmbiguousIncludeAll);
            }
        }
        Ok(explicit.or(include_all))
    }

    /// Presence evidence only; this never implies active translation or an
    /// isolated domain. Ambiguous firmware descriptions fail closed.
    pub fn covers_endpoint(&self, endpoint: DmarEndpoint) -> bool {
        self.remapping_unit_for(endpoint)
            .is_ok_and(|unit| unit.is_some())
    }

    fn push_unit(&mut self, unit: DmarRemappingUnit) -> Result<(), AcpiError> {
        let slot = self
            .units
            .get_mut(self.unit_count)
            .ok_or(AcpiError::CapacityExceeded)?;
        *slot = unit;
        self.unit_count += 1;
        Ok(())
    }

    fn push_endpoint(&mut self, endpoint: DmarEndpoint) -> Result<(), AcpiError> {
        let slot = self
            .endpoints
            .get_mut(self.endpoint_count)
            .ok_or(AcpiError::CapacityExceeded)?;
        *slot = endpoint;
        self.endpoint_count += 1;
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
    MalformedDmar,
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

/// Finds and parses an optional Intel DMA-remapping description.
///
/// A returned table proves only that firmware described remapping hardware.
/// Callers must initialize a unit and create a live domain before treating any
/// endpoint as isolated.
///
/// # Safety
///
/// `map` has the same stability and readability requirements as
/// [`discover_madt`].
pub unsafe fn discover_dmar<F>(rsdp: Rsdp, map: F) -> Result<Option<DmarInfo>, AcpiError>
where
    F: Fn(u64, usize) -> Option<*const u8> + Copy,
{
    if let Some(xsdt_address) = rsdp.xsdt_address {
        unsafe { find_dmar(xsdt_address, XSDT_SIGNATURE, 8, map) }
    } else {
        unsafe { find_dmar(u64::from(rsdp.rsdt_address), RSDT_SIGNATURE, 4, map) }
    }
}

unsafe fn find_dmar<F>(
    root_address: u64,
    expected_signature: &[u8; 4],
    entry_width: usize,
    map: F,
) -> Result<Option<DmarInfo>, AcpiError>
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
        if header.get(..4) == Some(DMAR_SIGNATURE) {
            let table = unsafe { validated_table(address, map)? };
            return parse_dmar(table).map(Some);
        }
    }
    Ok(None)
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

fn parse_dmar(table: &[u8]) -> Result<DmarInfo, AcpiError> {
    if table.len() < DMAR_HEADER_LENGTH || table.get(..4) != Some(DMAR_SIGNATURE) {
        return Err(AcpiError::MalformedDmar);
    }
    let host_address_width = table[36]
        .checked_add(1)
        .filter(|width| *width <= 64)
        .ok_or(AcpiError::MalformedDmar)?;
    if table[37] & !0x07 != 0 || table[38..DMAR_HEADER_LENGTH].iter().any(|byte| *byte != 0) {
        return Err(AcpiError::MalformedDmar);
    }
    let mut dmar = DmarInfo::new(host_address_width, table[37]);

    let mut offset = DMAR_HEADER_LENGTH;
    while offset < table.len() {
        if table.len() - offset < 4 {
            return Err(AcpiError::MalformedDmar);
        }
        let structure_type = read_u16(table, offset).ok_or(AcpiError::MalformedDmar)?;
        let length = usize::from(read_u16(table, offset + 2).ok_or(AcpiError::MalformedDmar)?);
        if length < 4
            || offset
                .checked_add(length)
                .is_none_or(|end| end > table.len())
        {
            return Err(AcpiError::MalformedDmar);
        }
        let structure = &table[offset..offset + length];
        if structure_type == DMAR_DRHD_TYPE {
            parse_drhd(structure, &mut dmar)?;
        }
        offset += length;
    }
    if dmar.remapping_units().is_empty() {
        return Err(AcpiError::MalformedDmar);
    }
    Ok(dmar)
}

fn parse_drhd(structure: &[u8], dmar: &mut DmarInfo) -> Result<(), AcpiError> {
    if structure.len() < DMAR_DRHD_HEADER_LENGTH {
        return Err(AcpiError::MalformedDmar);
    }
    let flags = structure[4];
    if flags & !DMAR_INCLUDE_ALL != 0 || structure[5] != 0 {
        return Err(AcpiError::MalformedDmar);
    }
    let segment = read_u16(structure, 6).ok_or(AcpiError::MalformedDmar)?;
    let register_base = read_u64(structure, 8).ok_or(AcpiError::MalformedDmar)?;
    if register_base == 0 || register_base & 0xfff != 0 {
        return Err(AcpiError::MalformedDmar);
    }

    let endpoint_start = dmar.endpoint_count;
    let mut unresolved_scope_count = 0_u16;
    let mut offset = DMAR_DRHD_HEADER_LENGTH;
    while offset < structure.len() {
        if structure.len() - offset < DMAR_SCOPE_HEADER_LENGTH {
            return Err(AcpiError::MalformedDmar);
        }
        let scope_length = usize::from(structure[offset + 1]);
        if scope_length < DMAR_SCOPE_HEADER_LENGTH + 2
            || (scope_length - DMAR_SCOPE_HEADER_LENGTH) % 2 != 0
            || offset
                .checked_add(scope_length)
                .is_none_or(|end| end > structure.len())
        {
            return Err(AcpiError::MalformedDmar);
        }
        let scope = &structure[offset..offset + scope_length];
        if scope[2] != 0 || scope[3] != 0 {
            return Err(AcpiError::MalformedDmar);
        }
        let path_count = (scope_length - DMAR_SCOPE_HEADER_LENGTH) / 2;
        for path in scope[DMAR_SCOPE_HEADER_LENGTH..].chunks_exact(2) {
            if path[0] >= 32 || path[1] >= 8 {
                return Err(AcpiError::MalformedDmar);
            }
        }
        // A multi-hop path needs live bridge topology to resolve its terminal
        // bus. Retaining it as a direct endpoint would manufacture coverage,
        // so only exact one-hop endpoint scopes enter the evidence set.
        if scope[0] == DMAR_SCOPE_ENDPOINT && path_count == 1 {
            if scope[4] != 0 {
                return Err(AcpiError::MalformedDmar);
            }
            let endpoint = DmarEndpoint {
                segment,
                bus: scope[5],
                slot: scope[6],
                function: scope[7],
            };
            if dmar.endpoints[endpoint_start..dmar.endpoint_count].contains(&endpoint) {
                return Err(AcpiError::MalformedDmar);
            }
            dmar.push_endpoint(endpoint)?;
        } else {
            unresolved_scope_count = unresolved_scope_count
                .checked_add(1)
                .ok_or(AcpiError::CapacityExceeded)?;
        }
        offset += scope_length;
    }

    let endpoint_count = dmar
        .endpoint_count
        .checked_sub(endpoint_start)
        .ok_or(AcpiError::MalformedDmar)?;
    dmar.push_unit(DmarRemappingUnit {
        segment,
        register_base,
        include_all: flags & DMAR_INCLUDE_ALL != 0,
        endpoint_start: u16::try_from(endpoint_start).map_err(|_| AcpiError::CapacityExceeded)?,
        endpoint_count: u16::try_from(endpoint_count).map_err(|_| AcpiError::CapacityExceeded)?,
        unresolved_scope_count,
    })
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
    const DMAR_OFFSET: usize = 0x300;

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

        let xsdt_length = SDT_HEADER_LENGTH + 16;
        let xsdt = &mut memory[XSDT_OFFSET..XSDT_OFFSET + xsdt_length];
        xsdt[..4].copy_from_slice(XSDT_SIGNATURE);
        xsdt[4..8].copy_from_slice(&(xsdt_length as u32).to_le_bytes());
        xsdt[8] = 1;
        xsdt[SDT_HEADER_LENGTH..SDT_HEADER_LENGTH + 8]
            .copy_from_slice(&(MEMORY_BASE + MADT_OFFSET as u64).to_le_bytes());
        xsdt[SDT_HEADER_LENGTH + 8..]
            .copy_from_slice(&(MEMORY_BASE + DMAR_OFFSET as u64).to_le_bytes());
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

        let mut dmar = dmar_table();
        set_checksum(&mut dmar, 9);
        memory[DMAR_OFFSET..DMAR_OFFSET + dmar.len()].copy_from_slice(&dmar);
        memory
    }

    fn dmar_table() -> [u8; 88] {
        let mut table = [0_u8; 88];
        table[..4].copy_from_slice(DMAR_SIGNATURE);
        table[4..8].copy_from_slice(&(88_u32).to_le_bytes());
        table[36] = 47;
        table[37] = 1;

        let include_all = &mut table[48..64];
        include_all[..2].copy_from_slice(&DMAR_DRHD_TYPE.to_le_bytes());
        include_all[2..4].copy_from_slice(&(16_u16).to_le_bytes());
        include_all[4] = DMAR_INCLUDE_ALL;
        include_all[6..8].copy_from_slice(&(0_u16).to_le_bytes());
        include_all[8..16].copy_from_slice(&(0xfeda_0000_u64).to_le_bytes());

        let scoped = &mut table[64..88];
        scoped[..2].copy_from_slice(&DMAR_DRHD_TYPE.to_le_bytes());
        scoped[2..4].copy_from_slice(&(24_u16).to_le_bytes());
        scoped[6..8].copy_from_slice(&(2_u16).to_le_bytes());
        scoped[8..16].copy_from_slice(&(0xfedb_0000_u64).to_le_bytes());
        scoped[16] = DMAR_SCOPE_ENDPOINT;
        scoped[17] = 8;
        scoped[21] = 4;
        scoped[22] = 3;
        scoped[23] = 1;
        table
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
    fn discovers_validated_dmar_without_claiming_isolation() {
        let memory = acpi_memory();
        let rsdp = Rsdp::parse(&rsdp(MEMORY_BASE + XSDT_OFFSET as u64)).unwrap();
        let map = |address: u64, length: usize| {
            let offset = address.checked_sub(MEMORY_BASE)? as usize;
            Some(memory.get(offset..offset.checked_add(length)?)?.as_ptr())
        };

        let dmar = unsafe { discover_dmar(rsdp, map) }.unwrap().unwrap();
        assert_eq!(dmar.remapping_units().len(), 2);
        assert!(dmar.covers_endpoint(DmarEndpoint {
            segment: 2,
            bus: 4,
            slot: 3,
            function: 1,
        }));
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

    #[test]
    fn parses_conservative_dma_remapping_coverage() {
        let dmar = parse_dmar(&dmar_table()).unwrap();

        assert_eq!(dmar.host_address_width, 48);
        assert_eq!(dmar.flags, 1);
        assert_eq!(dmar.remapping_units().len(), 2);
        assert!(
            dmar.explicit_endpoints_for(dmar.remapping_units()[0])
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            dmar.explicit_endpoints_for(dmar.remapping_units()[1]),
            Some(
                &[DmarEndpoint {
                    segment: 2,
                    bus: 4,
                    slot: 3,
                    function: 1,
                }][..]
            )
        );
        assert!(dmar.covers_endpoint(DmarEndpoint {
            segment: 0,
            bus: 255,
            slot: 31,
            function: 7,
        }));
        assert!(dmar.covers_endpoint(DmarEndpoint {
            segment: 2,
            bus: 4,
            slot: 3,
            function: 1,
        }));
        assert!(!dmar.covers_endpoint(DmarEndpoint {
            segment: 2,
            bus: 4,
            slot: 3,
            function: 0,
        }));
    }

    #[test]
    fn routes_explicit_dmar_scope_before_include_all_and_rejects_ambiguity() {
        let endpoint = DmarEndpoint {
            segment: 0,
            bus: 4,
            slot: 3,
            function: 1,
        };
        let include_all = DmarRemappingUnit {
            segment: 0,
            register_base: 0xfeda_0000,
            include_all: true,
            endpoint_start: 0,
            endpoint_count: 0,
            unresolved_scope_count: 0,
        };
        let explicit = DmarRemappingUnit {
            segment: 0,
            register_base: 0xfedb_0000,
            include_all: false,
            endpoint_start: 0,
            endpoint_count: 1,
            unresolved_scope_count: 0,
        };

        let mut dmar = DmarInfo::new(48, 0);
        dmar.push_endpoint(endpoint).unwrap();
        dmar.push_unit(include_all).unwrap();
        dmar.push_unit(explicit).unwrap();
        assert_eq!(dmar.remapping_unit_for(endpoint), Ok(Some(explicit)));

        let duplicate = DmarRemappingUnit {
            register_base: 0xfedc_0000,
            ..explicit
        };
        dmar.push_unit(duplicate).unwrap();
        assert_eq!(
            dmar.remapping_unit_for(endpoint),
            Err(DmarRouteError::AmbiguousExplicitScope)
        );

        let mut fallback = DmarInfo::new(48, 0);
        fallback.push_unit(include_all).unwrap();
        fallback
            .push_unit(DmarRemappingUnit {
                register_base: 0xfedd_0000,
                ..include_all
            })
            .unwrap();
        assert_eq!(
            fallback.remapping_unit_for(endpoint),
            Err(DmarRouteError::AmbiguousIncludeAll)
        );
        assert!(!fallback.covers_endpoint(endpoint));
    }

    #[test]
    fn rejects_invalid_dmar_endpoint_paths() {
        let mut table = dmar_table();
        table[86] = 32;
        assert_eq!(parse_dmar(&table), Err(AcpiError::MalformedDmar));
    }

    #[test]
    fn retains_unresolved_bridge_scope_evidence_instead_of_claiming_exclusivity() {
        let base = dmar_table();
        let mut table = [0_u8; 90];
        table[..base.len()].copy_from_slice(&base);
        table[4..8].copy_from_slice(&(90_u32).to_le_bytes());
        table[66..68].copy_from_slice(&(26_u16).to_le_bytes());
        table[81] = 10;
        table[88] = 4;
        table[89] = 0;

        let dmar = parse_dmar(&table).unwrap();
        let scoped = dmar.remapping_units()[1];
        assert!(scoped.has_unresolved_scopes());
        assert!(dmar.explicit_endpoints_for(scoped).unwrap().is_empty());
    }
}
