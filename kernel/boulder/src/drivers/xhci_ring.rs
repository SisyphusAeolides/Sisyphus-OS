//! Bounded xHCI command and event rings over explicitly owned DMA storage.
//!
//! Publication is a small protocol rather than a raw memory write: a TRB is
//! first made invisible to the controller, its payload is replaced, and its
//! cycle bit is committed last with release ordering. Completion advances the
//! event dequeue state only after an exact command-pointer correlation.

use abyss::paging::PAGE_SIZE;

use super::xhci_dma::{
    XhciDmaArena, XhciDmaError, XhciDmaPurpose, XhciDmaRegionPhase, XhciDmaRegionRecord,
};

const TRB_BYTES: usize = 16;
const TRBS_PER_PAGE: usize = PAGE_SIZE / TRB_BYTES;
const COMMAND_LINK_INDEX: usize = TRBS_PER_PAGE - 1;
const COMMAND_CAPACITY: usize = COMMAND_LINK_INDEX;
const EVENT_CAPACITY: usize = TRBS_PER_PAGE;
const TRB_CYCLE: u32 = 1 << 0;
const LINK_TOGGLE_CYCLE: u32 = 1 << 1;
const TRB_TYPE_SHIFT: u32 = 10;
const TRB_TYPE_MASK: u32 = 0x3f << TRB_TYPE_SHIFT;
const TRB_TYPE_LINK: u8 = 6;
const TRB_TYPE_NO_OP_COMMAND: u8 = 23;
const TRB_TYPE_COMMAND_COMPLETION_EVENT: u8 = 33;
const COMPLETION_CODE_SUCCESS: u8 = 1;
const RING_ROOT_DOMAIN: u64 = 0x5848_4349_5249_4e47;
const RECEIPT_ROOT_DOMAIN: u64 = 0x5848_4349_434d_4452;
const COMPLETION_ROOT_DOMAIN: u64 = 0x5848_4349_4556_4e54;

/// Volatile storage contract used by the ring state machine.
///
/// Implementations must make a successful write visible in byte order and
/// must return an error before modifying storage when validation fails.
pub trait XhciRingStorage {
    type Error;

    fn region(&self, purpose: XhciDmaPurpose) -> Option<XhciDmaRegionRecord>;

    fn write(
        &self,
        purpose: XhciDmaPurpose,
        offset: usize,
        bytes: &[u8],
    ) -> Result<(), Self::Error>;

    fn read(
        &self,
        purpose: XhciDmaPurpose,
        offset: usize,
        output: &mut [u8],
    ) -> Result<(), Self::Error>;
}

impl<'arena, 'storage, 'authority> XhciRingStorage for XhciDmaArena<'arena, 'storage, 'authority> {
    type Error = XhciDmaError;

    fn region(&self, purpose: XhciDmaPurpose) -> Option<XhciDmaRegionRecord> {
        XhciDmaArena::region(self, purpose)
    }

    fn write(
        &self,
        purpose: XhciDmaPurpose,
        offset: usize,
        bytes: &[u8],
    ) -> Result<(), Self::Error> {
        let token = self
            .token(purpose)
            .ok_or(XhciDmaError::MissingRegion(purpose))?;
        XhciDmaArena::write(self, token, offset, bytes)
    }

    fn read(
        &self,
        purpose: XhciDmaPurpose,
        offset: usize,
        output: &mut [u8],
    ) -> Result<(), Self::Error> {
        let token = self
            .token(purpose)
            .ok_or(XhciDmaError::MissingRegion(purpose))?;
        XhciDmaArena::read(self, token, offset, output)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XhciRingGeometryError {
    MissingRegion(XhciDmaPurpose),
    RegionNotReady(XhciDmaPurpose),
    RegionGenerationMismatch(XhciDmaPurpose),
    RegionMustBeOnePage(XhciDmaPurpose),
    RegionAddressUnaligned(XhciDmaPurpose),
    RegionAddressOverflow(XhciDmaPurpose),
    RegionsOverlap,
}

#[derive(Debug, Eq, PartialEq)]
pub enum XhciRingError<E> {
    InvalidSecret,
    Geometry(XhciRingGeometryError),
    Storage(E),
    CommandBusy,
    CommandSequenceExhausted,
    NoOutstandingCommand,
    ReceiptMismatch,
    EventPointerReservedBits(u8),
    UnexpectedEventType(u8),
    CompletionPointerMismatch { expected: u64, observed: u64 },
    CompletionVirtualFunctionMismatch(u8),
    CompletionSlotMismatch(u8),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
struct RawTrb {
    parameter: u64,
    status: u32,
    control: u32,
}

impl RawTrb {
    const ZERO: Self = Self {
        parameter: 0,
        status: 0,
        control: 0,
    };

    const fn trb_type(self) -> u8 {
        ((self.control & TRB_TYPE_MASK) >> TRB_TYPE_SHIFT) as u8
    }

    const fn cycle(self) -> bool {
        self.control & TRB_CYCLE != 0
    }

    fn encode(self) -> [u8; TRB_BYTES] {
        let mut bytes = [0_u8; TRB_BYTES];
        bytes[0..8].copy_from_slice(&self.parameter.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.status.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.control.to_le_bytes());
        bytes
    }

    fn decode(bytes: [u8; TRB_BYTES]) -> Self {
        let mut parameter = [0_u8; 8];
        parameter.copy_from_slice(&bytes[0..8]);
        let mut status = [0_u8; 4];
        status.copy_from_slice(&bytes[8..12]);
        let mut control = [0_u8; 4];
        control.copy_from_slice(&bytes[12..16]);
        Self {
            parameter: u64::from_le_bytes(parameter),
            status: u32::from_le_bytes(status),
            control: u32::from_le_bytes(control),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XhciRingRegisterProgram {
    pub command_ring_control: u64,
    pub event_ring_segment_table_size: u16,
    pub event_ring_segment_table_base: u64,
    pub event_ring_dequeue_pointer: u64,
}

#[derive(Debug, Eq, PartialEq)]
pub struct XhciNoOpCommandReceipt {
    generation: u32,
    sequence: u32,
    command_index: u16,
    producer_cycle: bool,
    command_device_address: u64,
    receipt_root: u64,
}

impl XhciNoOpCommandReceipt {
    pub const fn generation(&self) -> u32 {
        self.generation
    }

    pub const fn sequence(&self) -> u32 {
        self.sequence
    }

    pub const fn command_index(&self) -> u16 {
        self.command_index
    }

    pub const fn producer_cycle(&self) -> bool {
        self.producer_cycle
    }

    pub const fn command_device_address(&self) -> u64 {
        self.command_device_address
    }

    pub const fn receipt_root(&self) -> u64 {
        self.receipt_root
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OutstandingCommand {
    generation: u32,
    sequence: u32,
    command_index: u16,
    producer_cycle: bool,
    command_device_address: u64,
    receipt_root: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XhciCommandCompletionEvidence {
    pub generation: u32,
    pub sequence: u32,
    pub completion_code: u8,
    pub completion_parameter: u32,
    pub consumed_event_index: u16,
    pub consumed_event_cycle: bool,
    pub next_event_dequeue_pointer: u64,
    pub completion_root: u64,
}

impl XhciCommandCompletionEvidence {
    pub const fn successful(self) -> bool {
        self.completion_code == COMPLETION_CODE_SUCCESS
    }
}

/// A one-command-at-a-time ring machine for the first xHCI bootstrap epoch.
///
/// Serial admission deliberately trades throughput for an exact relation
/// between a command receipt and the next consumed event. It can still cross
/// both ring boundaries without allocation or identity reuse.
pub struct XhciRingMachine {
    generation: u32,
    command_device_base: u64,
    event_device_base: u64,
    erst_device_base: u64,
    producer_index: u16,
    producer_cycle: bool,
    event_index: u16,
    event_cycle: bool,
    next_sequence: u32,
    outstanding: Option<OutstandingCommand>,
    ring_root: u64,
}

impl XhciRingMachine {
    pub fn initialize<S: XhciRingStorage>(
        storage: &S,
        secret: u64,
    ) -> Result<Self, XhciRingError<S::Error>> {
        if secret == 0 {
            return Err(XhciRingError::InvalidSecret);
        }
        let command = validated_region(storage, XhciDmaPurpose::CommandRing, None)?;
        let generation = command.generation;
        let event = validated_region(storage, XhciDmaPurpose::EventRing, Some(generation))?;
        let erst = validated_region(
            storage,
            XhciDmaPurpose::EventRingSegmentTable,
            Some(generation),
        )?;
        if overlaps(command, event) || overlaps(command, erst) || overlaps(event, erst) {
            return Err(XhciRingError::Geometry(
                XhciRingGeometryError::RegionsOverlap,
            ));
        }

        zero_ring(storage, XhciDmaPurpose::CommandRing)?;
        zero_ring(storage, XhciDmaPurpose::EventRing)?;
        write_link(storage, command.device_address_start, true)?;
        write_erst(storage, event.device_address_start)?;

        let mut root = mix(secret ^ RING_ROOT_DOMAIN, u64::from(generation));
        root = mix(root, command.region_root);
        root = mix(root, event.region_root);
        root = mix(root, erst.region_root);
        root = mix(root, command.device_address_start);
        root = mix(root, event.device_address_start);
        root = mix(root, erst.device_address_start);
        root = mix(root, COMMAND_CAPACITY as u64);
        root = mix(root, EVENT_CAPACITY as u64);
        let ring_root = canonical_root(root, RING_ROOT_DOMAIN);

        Ok(Self {
            generation,
            command_device_base: command.device_address_start,
            event_device_base: event.device_address_start,
            erst_device_base: erst.device_address_start,
            producer_index: 0,
            producer_cycle: true,
            event_index: 0,
            event_cycle: true,
            next_sequence: 1,
            outstanding: None,
            ring_root,
        })
    }

    pub const fn generation(&self) -> u32 {
        self.generation
    }

    pub const fn ring_root(&self) -> u64 {
        self.ring_root
    }

    pub const fn command_capacity(&self) -> usize {
        COMMAND_CAPACITY
    }

    pub const fn event_capacity(&self) -> usize {
        EVENT_CAPACITY
    }

    pub const fn event_index(&self) -> u16 {
        self.event_index
    }

    pub const fn event_cycle(&self) -> bool {
        self.event_cycle
    }

    pub const fn register_program(&self) -> XhciRingRegisterProgram {
        XhciRingRegisterProgram {
            command_ring_control: self.command_device_base | TRB_CYCLE as u64,
            event_ring_segment_table_size: 1,
            event_ring_segment_table_base: self.erst_device_base,
            event_ring_dequeue_pointer: self.event_device_base,
        }
    }

    pub fn submit_no_op<S: XhciRingStorage>(
        &mut self,
        storage: &S,
    ) -> Result<XhciNoOpCommandReceipt, XhciRingError<S::Error>> {
        if self.outstanding.is_some() {
            return Err(XhciRingError::CommandBusy);
        }
        if self.next_sequence == u32::MAX {
            return Err(XhciRingError::CommandSequenceExhausted);
        }

        let index = usize::from(self.producer_index);
        if index + 1 == COMMAND_LINK_INDEX {
            write_link(storage, self.command_device_base, self.producer_cycle)?;
        }
        let trb = RawTrb {
            parameter: 0,
            status: 0,
            control: u32::from(self.producer_cycle)
                | (u32::from(TRB_TYPE_NO_OP_COMMAND) << TRB_TYPE_SHIFT),
        };
        publish_trb(
            storage,
            XhciDmaPurpose::CommandRing,
            index * TRB_BYTES,
            trb,
            self.producer_cycle,
        )?;
        let command_device_address = self
            .command_device_base
            .checked_add((index * TRB_BYTES) as u64)
            .ok_or(XhciRingError::Geometry(
                XhciRingGeometryError::RegionAddressOverflow(XhciDmaPurpose::CommandRing),
            ))?;
        let sequence = self.next_sequence;
        let receipt_root = receipt_root(
            self.ring_root,
            self.generation,
            sequence,
            self.producer_index,
            self.producer_cycle,
            command_device_address,
        );
        let outstanding = OutstandingCommand {
            generation: self.generation,
            sequence,
            command_index: self.producer_index,
            producer_cycle: self.producer_cycle,
            command_device_address,
            receipt_root,
        };
        self.outstanding = Some(outstanding);
        self.next_sequence += 1;
        self.producer_index += 1;
        if usize::from(self.producer_index) == COMMAND_LINK_INDEX {
            self.producer_index = 0;
            self.producer_cycle = !self.producer_cycle;
        }
        Ok(XhciNoOpCommandReceipt {
            generation: outstanding.generation,
            sequence: outstanding.sequence,
            command_index: outstanding.command_index,
            producer_cycle: outstanding.producer_cycle,
            command_device_address: outstanding.command_device_address,
            receipt_root: outstanding.receipt_root,
        })
    }

    pub fn poll_no_op_completion<S: XhciRingStorage>(
        &mut self,
        storage: &S,
        receipt: &XhciNoOpCommandReceipt,
    ) -> Result<Option<XhciCommandCompletionEvidence>, XhciRingError<S::Error>> {
        let expected = self
            .outstanding
            .ok_or(XhciRingError::NoOutstandingCommand)?;
        if !receipt_matches(expected, receipt) {
            return Err(XhciRingError::ReceiptMismatch);
        }
        let event_offset = usize::from(self.event_index) * TRB_BYTES;
        let event = read_trb(storage, XhciDmaPurpose::EventRing, event_offset)?;
        if event.cycle() != self.event_cycle {
            return Ok(None);
        }
        let event_type = event.trb_type();
        if event_type != TRB_TYPE_COMMAND_COMPLETION_EVENT {
            return Err(XhciRingError::UnexpectedEventType(event_type));
        }
        if event.parameter & 0xf != 0 {
            return Err(XhciRingError::EventPointerReservedBits(
                (event.parameter & 0xf) as u8,
            ));
        }
        if event.parameter != expected.command_device_address {
            return Err(XhciRingError::CompletionPointerMismatch {
                expected: expected.command_device_address,
                observed: event.parameter,
            });
        }
        let virtual_function = ((event.control >> 16) & 0xff) as u8;
        if virtual_function != 0 {
            return Err(XhciRingError::CompletionVirtualFunctionMismatch(
                virtual_function,
            ));
        }
        let slot = (event.control >> 24) as u8;
        if slot != 0 {
            return Err(XhciRingError::CompletionSlotMismatch(slot));
        }

        let consumed_index = self.event_index;
        let consumed_cycle = self.event_cycle;
        self.event_index += 1;
        if usize::from(self.event_index) == EVENT_CAPACITY {
            self.event_index = 0;
            self.event_cycle = !self.event_cycle;
        }
        self.outstanding = None;
        let next_event_dequeue_pointer =
            self.event_device_base + u64::from(self.event_index) * TRB_BYTES as u64;
        let completion_code = (event.status >> 24) as u8;
        let completion_parameter = event.status & 0x00ff_ffff;
        let mut root = mix(
            self.ring_root ^ COMPLETION_ROOT_DOMAIN,
            expected.receipt_root,
        );
        root = mix(root, event.parameter);
        root = mix(root, u64::from(event.status));
        root = mix(root, u64::from(event.control));
        root = mix(root, u64::from(consumed_index));
        root = mix(root, consumed_cycle as u64);
        root = mix(root, next_event_dequeue_pointer);
        Ok(Some(XhciCommandCompletionEvidence {
            generation: self.generation,
            sequence: expected.sequence,
            completion_code,
            completion_parameter,
            consumed_event_index: consumed_index,
            consumed_event_cycle: consumed_cycle,
            next_event_dequeue_pointer,
            completion_root: canonical_root(root, COMPLETION_ROOT_DOMAIN),
        }))
    }
}

fn validated_region<S: XhciRingStorage>(
    storage: &S,
    purpose: XhciDmaPurpose,
    expected_generation: Option<u32>,
) -> Result<XhciDmaRegionRecord, XhciRingError<S::Error>> {
    let record = storage.region(purpose).ok_or(XhciRingError::Geometry(
        XhciRingGeometryError::MissingRegion(purpose),
    ))?;
    if record.phase != XhciDmaRegionPhase::Ready {
        return Err(XhciRingError::Geometry(
            XhciRingGeometryError::RegionNotReady(purpose),
        ));
    }
    if record.purpose != purpose
        || expected_generation.is_some_and(|value| value != record.generation)
    {
        return Err(XhciRingError::Geometry(
            XhciRingGeometryError::RegionGenerationMismatch(purpose),
        ));
    }
    if record.page_count != 1 || record.byte_length() != PAGE_SIZE {
        return Err(XhciRingError::Geometry(
            XhciRingGeometryError::RegionMustBeOnePage(purpose),
        ));
    }
    if record.device_address_start % PAGE_SIZE as u64 != 0 {
        return Err(XhciRingError::Geometry(
            XhciRingGeometryError::RegionAddressUnaligned(purpose),
        ));
    }
    let expected_end = record
        .device_address_start
        .checked_add(PAGE_SIZE as u64)
        .ok_or(XhciRingError::Geometry(
            XhciRingGeometryError::RegionAddressOverflow(purpose),
        ))?;
    if expected_end != record.device_address_end {
        return Err(XhciRingError::Geometry(
            XhciRingGeometryError::RegionMustBeOnePage(purpose),
        ));
    }
    Ok(record)
}

const fn overlaps(left: XhciDmaRegionRecord, right: XhciDmaRegionRecord) -> bool {
    left.device_address_start < right.device_address_end
        && right.device_address_start < left.device_address_end
}

fn zero_ring<S: XhciRingStorage>(
    storage: &S,
    purpose: XhciDmaPurpose,
) -> Result<(), XhciRingError<S::Error>> {
    let zero = RawTrb::ZERO.encode();
    for index in 0..TRBS_PER_PAGE {
        storage
            .write(purpose, index * TRB_BYTES, &zero)
            .map_err(XhciRingError::Storage)?;
    }
    Ok(())
}

fn write_link<S: XhciRingStorage>(
    storage: &S,
    command_device_base: u64,
    cycle: bool,
) -> Result<(), XhciRingError<S::Error>> {
    let link = RawTrb {
        parameter: command_device_base,
        status: 0,
        control: u32::from(cycle)
            | LINK_TOGGLE_CYCLE
            | (u32::from(TRB_TYPE_LINK) << TRB_TYPE_SHIFT),
    };
    publish_trb(
        storage,
        XhciDmaPurpose::CommandRing,
        COMMAND_LINK_INDEX * TRB_BYTES,
        link,
        cycle,
    )
}

fn write_erst<S: XhciRingStorage>(
    storage: &S,
    event_device_base: u64,
) -> Result<(), XhciRingError<S::Error>> {
    let mut entry = [0_u8; TRB_BYTES];
    entry[0..8].copy_from_slice(&event_device_base.to_le_bytes());
    entry[8..12].copy_from_slice(&(EVENT_CAPACITY as u32).to_le_bytes());
    storage
        .write(XhciDmaPurpose::EventRingSegmentTable, 0, &entry)
        .map_err(XhciRingError::Storage)
}

fn publish_trb<S: XhciRingStorage>(
    storage: &S,
    purpose: XhciDmaPurpose,
    offset: usize,
    trb: RawTrb,
    publish_cycle: bool,
) -> Result<(), XhciRingError<S::Error>> {
    let bytes = trb.encode();
    let invisible_control = if publish_cycle {
        trb.control & !TRB_CYCLE
    } else {
        trb.control | TRB_CYCLE
    };
    storage
        .write(purpose, offset + 12, &invisible_control.to_le_bytes())
        .map_err(XhciRingError::Storage)?;
    storage
        .write(purpose, offset, &bytes[..12])
        .map_err(XhciRingError::Storage)?;
    storage
        .write(purpose, offset + 12, &trb.control.to_le_bytes())
        .map_err(XhciRingError::Storage)
}

fn read_trb<S: XhciRingStorage>(
    storage: &S,
    purpose: XhciDmaPurpose,
    offset: usize,
) -> Result<RawTrb, XhciRingError<S::Error>> {
    let mut bytes = [0_u8; TRB_BYTES];
    storage
        .read(purpose, offset, &mut bytes)
        .map_err(XhciRingError::Storage)?;
    Ok(RawTrb::decode(bytes))
}

fn receipt_matches(expected: OutstandingCommand, receipt: &XhciNoOpCommandReceipt) -> bool {
    expected.generation == receipt.generation
        && expected.sequence == receipt.sequence
        && expected.command_index == receipt.command_index
        && expected.producer_cycle == receipt.producer_cycle
        && expected.command_device_address == receipt.command_device_address
        && expected.receipt_root == receipt.receipt_root
}

fn receipt_root(
    ring_root: u64,
    generation: u32,
    sequence: u32,
    index: u16,
    cycle: bool,
    address: u64,
) -> u64 {
    let mut root = mix(ring_root ^ RECEIPT_ROOT_DOMAIN, u64::from(generation));
    root = mix(root, u64::from(sequence));
    root = mix(root, u64::from(index));
    root = mix(root, cycle as u64);
    root = mix(root, address);
    canonical_root(root, RECEIPT_ROOT_DOMAIN)
}

const fn canonical_root(root: u64, domain: u64) -> u64 {
    if root == 0 { domain } else { root }
}

fn mix(mut state: u64, word: u64) -> u64 {
    state ^= word.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    state ^= state >> 30;
    state = state.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    state ^= state >> 27;
    state = state.wrapping_mul(0x94d0_49bb_1331_11eb);
    state ^ (state >> 31)
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use super::*;
    use crate::hw::pci::PciAddress;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum MockError {
        Bounds,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct WriteRecord {
        purpose: XhciDmaPurpose,
        offset: usize,
        bytes: Vec<u8>,
    }

    struct MockStorage {
        command: RefCell<[u8; PAGE_SIZE]>,
        event: RefCell<[u8; PAGE_SIZE]>,
        erst: RefCell<[u8; PAGE_SIZE]>,
        writes: RefCell<Vec<WriteRecord>>,
        command_record: XhciDmaRegionRecord,
        event_record: XhciDmaRegionRecord,
        erst_record: XhciDmaRegionRecord,
    }

    impl MockStorage {
        fn new() -> Self {
            Self {
                command: RefCell::new([0xa5; PAGE_SIZE]),
                event: RefCell::new([0xa5; PAGE_SIZE]),
                erst: RefCell::new([0xa5; PAGE_SIZE]),
                writes: RefCell::new(Vec::new()),
                command_record: record(XhciDmaPurpose::CommandRing, 0x1000, 11),
                event_record: record(XhciDmaPurpose::EventRing, 0x2000, 12),
                erst_record: record(XhciDmaPurpose::EventRingSegmentTable, 0x3000, 13),
            }
        }

        fn bytes(&self, purpose: XhciDmaPurpose, offset: usize) -> [u8; TRB_BYTES] {
            let source = match purpose {
                XhciDmaPurpose::CommandRing => self.command.borrow(),
                XhciDmaPurpose::EventRing => self.event.borrow(),
                XhciDmaPurpose::EventRingSegmentTable => self.erst.borrow(),
                _ => unreachable!(),
            };
            source[offset..offset + TRB_BYTES].try_into().unwrap()
        }

        fn install_event(&self, index: usize, event: RawTrb) {
            let offset = index * TRB_BYTES;
            self.event.borrow_mut()[offset..offset + TRB_BYTES].copy_from_slice(&event.encode());
        }
    }

    impl XhciRingStorage for MockStorage {
        type Error = MockError;

        fn region(&self, purpose: XhciDmaPurpose) -> Option<XhciDmaRegionRecord> {
            match purpose {
                XhciDmaPurpose::CommandRing => Some(self.command_record),
                XhciDmaPurpose::EventRing => Some(self.event_record),
                XhciDmaPurpose::EventRingSegmentTable => Some(self.erst_record),
                _ => None,
            }
        }

        fn write(
            &self,
            purpose: XhciDmaPurpose,
            offset: usize,
            bytes: &[u8],
        ) -> Result<(), Self::Error> {
            let end = offset.checked_add(bytes.len()).ok_or(MockError::Bounds)?;
            if end > PAGE_SIZE || bytes.is_empty() {
                return Err(MockError::Bounds);
            }
            let target = match purpose {
                XhciDmaPurpose::CommandRing => &self.command,
                XhciDmaPurpose::EventRing => &self.event,
                XhciDmaPurpose::EventRingSegmentTable => &self.erst,
                _ => return Err(MockError::Bounds),
            };
            target.borrow_mut()[offset..end].copy_from_slice(bytes);
            self.writes.borrow_mut().push(WriteRecord {
                purpose,
                offset,
                bytes: bytes.to_vec(),
            });
            Ok(())
        }

        fn read(
            &self,
            purpose: XhciDmaPurpose,
            offset: usize,
            output: &mut [u8],
        ) -> Result<(), Self::Error> {
            let end = offset.checked_add(output.len()).ok_or(MockError::Bounds)?;
            if end > PAGE_SIZE || output.is_empty() {
                return Err(MockError::Bounds);
            }
            let source = match purpose {
                XhciDmaPurpose::CommandRing => self.command.borrow(),
                XhciDmaPurpose::EventRing => self.event.borrow(),
                XhciDmaPurpose::EventRingSegmentTable => self.erst.borrow(),
                _ => return Err(MockError::Bounds),
            };
            output.copy_from_slice(&source[offset..end]);
            Ok(())
        }
    }

    fn record(purpose: XhciDmaPurpose, address: u64, root: u64) -> XhciDmaRegionRecord {
        XhciDmaRegionRecord {
            phase: XhciDmaRegionPhase::Ready,
            generation: 7,
            purpose,
            device: PciAddress::new(0, 4, 0).unwrap(),
            physical_start: address,
            physical_end: address + PAGE_SIZE as u64,
            device_address_start: address,
            device_address_end: address + PAGE_SIZE as u64,
            cpu_start: address as usize,
            cpu_end: address as usize + PAGE_SIZE,
            page_count: 1,
            region_root: root,
        }
    }

    fn completion(receipt: &XhciNoOpCommandReceipt, cycle: bool, code: u8) -> RawTrb {
        RawTrb {
            parameter: receipt.command_device_address(),
            status: u32::from(code) << 24,
            control: u32::from(cycle)
                | (u32::from(TRB_TYPE_COMMAND_COMPLETION_EVENT) << TRB_TYPE_SHIFT),
        }
    }

    #[test]
    fn initialization_builds_a_self_link_and_one_entry_erst() {
        let storage = MockStorage::new();
        let machine = XhciRingMachine::initialize(&storage, 0x55aa).unwrap();
        let link = RawTrb::decode(
            storage.bytes(XhciDmaPurpose::CommandRing, COMMAND_LINK_INDEX * TRB_BYTES),
        );
        assert_eq!(link.parameter, 0x1000);
        assert_eq!(link.trb_type(), TRB_TYPE_LINK);
        assert!(link.cycle());
        assert_ne!(link.control & LINK_TOGGLE_CYCLE, 0);
        let erst = storage.bytes(XhciDmaPurpose::EventRingSegmentTable, 0);
        assert_eq!(u64::from_le_bytes(erst[0..8].try_into().unwrap()), 0x2000);
        assert_eq!(
            u32::from_le_bytes(erst[8..12].try_into().unwrap()),
            EVENT_CAPACITY as u32
        );
        assert_eq!(machine.register_program().command_ring_control, 0x1001);
        assert_ne!(machine.ring_root(), 0);
    }

    #[test]
    fn no_op_publication_commits_the_cycle_bit_last() {
        let storage = MockStorage::new();
        let mut machine = XhciRingMachine::initialize(&storage, 1).unwrap();
        storage.writes.borrow_mut().clear();
        let receipt = machine.submit_no_op(&storage).unwrap();
        let writes = storage.writes.borrow();
        assert_eq!(writes.len(), 3);
        assert_eq!(writes[0].offset, 12);
        assert_eq!(writes[1].offset, 0);
        assert_eq!(writes[2].offset, 12);
        assert_eq!(
            u32::from_le_bytes(writes[0].bytes.clone().try_into().unwrap()) & 1,
            0
        );
        assert_eq!(
            u32::from_le_bytes(writes[2].bytes.clone().try_into().unwrap()) & 1,
            1
        );
        assert_eq!(receipt.command_device_address(), 0x1000);
        assert_eq!(
            machine.submit_no_op(&storage),
            Err(XhciRingError::CommandBusy)
        );
    }

    #[test]
    fn empty_event_cycle_is_pending_without_state_change() {
        let storage = MockStorage::new();
        let mut machine = XhciRingMachine::initialize(&storage, 2).unwrap();
        let receipt = machine.submit_no_op(&storage).unwrap();
        assert_eq!(machine.poll_no_op_completion(&storage, &receipt), Ok(None));
        assert_eq!(
            machine.submit_no_op(&storage),
            Err(XhciRingError::CommandBusy)
        );
    }

    #[test]
    fn exact_completion_consumes_once_and_returns_erdp_evidence() {
        let storage = MockStorage::new();
        let mut machine = XhciRingMachine::initialize(&storage, 3).unwrap();
        let receipt = machine.submit_no_op(&storage).unwrap();
        storage.install_event(0, completion(&receipt, true, COMPLETION_CODE_SUCCESS));
        let evidence = machine
            .poll_no_op_completion(&storage, &receipt)
            .unwrap()
            .unwrap();
        assert!(evidence.successful());
        assert_eq!(evidence.next_event_dequeue_pointer, 0x2010);
        assert_ne!(evidence.completion_root, 0);
        assert_eq!(
            machine.poll_no_op_completion(&storage, &receipt),
            Err(XhciRingError::NoOutstandingCommand)
        );
    }

    #[test]
    fn mismatched_pointer_does_not_consume_or_admit_another_command() {
        let storage = MockStorage::new();
        let mut machine = XhciRingMachine::initialize(&storage, 4).unwrap();
        let receipt = machine.submit_no_op(&storage).unwrap();
        let mut event = completion(&receipt, true, COMPLETION_CODE_SUCCESS);
        event.parameter += TRB_BYTES as u64;
        storage.install_event(0, event);
        assert_eq!(
            machine.poll_no_op_completion(&storage, &receipt),
            Err(XhciRingError::CompletionPointerMismatch {
                expected: receipt.command_device_address(),
                observed: receipt.command_device_address() + TRB_BYTES as u64,
            })
        );
        assert_eq!(
            machine.submit_no_op(&storage),
            Err(XhciRingError::CommandBusy)
        );
    }

    #[test]
    fn non_successful_completion_is_terminal_measured_evidence() {
        let storage = MockStorage::new();
        let mut machine = XhciRingMachine::initialize(&storage, 5).unwrap();
        let receipt = machine.submit_no_op(&storage).unwrap();
        storage.install_event(0, completion(&receipt, true, 5));
        let evidence = machine
            .poll_no_op_completion(&storage, &receipt)
            .unwrap()
            .unwrap();
        assert!(!evidence.successful());
        assert_eq!(evidence.completion_code, 5);
        assert!(machine.submit_no_op(&storage).is_ok());
    }

    #[test]
    fn command_and_event_cycles_cross_their_boundaries_without_aliasing() {
        let storage = MockStorage::new();
        let mut machine = XhciRingMachine::initialize(&storage, 6).unwrap();
        for sequence in 0..COMMAND_CAPACITY {
            let receipt = machine.submit_no_op(&storage).unwrap();
            assert_eq!(receipt.command_index(), sequence as u16);
            storage.install_event(sequence, completion(&receipt, true, 1));
            assert!(
                machine
                    .poll_no_op_completion(&storage, &receipt)
                    .unwrap()
                    .unwrap()
                    .successful()
            );
        }
        let wrapped = machine.submit_no_op(&storage).unwrap();
        assert_eq!(wrapped.command_index(), 0);
        assert!(!wrapped.producer_cycle());
        storage.install_event(COMMAND_CAPACITY, completion(&wrapped, true, 1));
        assert_eq!(
            machine
                .poll_no_op_completion(&storage, &wrapped)
                .unwrap()
                .unwrap()
                .consumed_event_index,
            COMMAND_CAPACITY as u16
        );
        assert_eq!(machine.event_index(), 0);
        assert!(!machine.event_cycle());
    }

    #[test]
    fn geometry_rejects_generation_drift_and_overlap_before_writes() {
        let mut storage = MockStorage::new();
        storage.event_record.generation = 8;
        assert!(matches!(
            XhciRingMachine::initialize(&storage, 7),
            Err(XhciRingError::Geometry(
                XhciRingGeometryError::RegionGenerationMismatch(XhciDmaPurpose::EventRing)
            ))
        ));
        assert!(storage.writes.borrow().is_empty());

        let mut storage = MockStorage::new();
        storage.event_record.device_address_start = 0x1000;
        storage.event_record.device_address_end = 0x2000;
        assert!(matches!(
            XhciRingMachine::initialize(&storage, 8),
            Err(XhciRingError::Geometry(
                XhciRingGeometryError::RegionsOverlap
            ))
        ));
        assert!(storage.writes.borrow().is_empty());
    }
}
