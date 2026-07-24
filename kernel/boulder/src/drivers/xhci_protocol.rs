//! Lease-bounded xHCI Supported Protocol capability decoding.
//!
//! Every production read is derived from an exact BAR0 aperture lease before
//! it reaches the caller-provided transport.  The resulting evidence describes
//! controller protocol/port coverage only; it deliberately makes no claims
//! about attached USB children.

use crate::hw::pci::{Bar0ApertureLease, Bar0ApertureRange, BarApertureBoundsError};

pub const MAXIMUM_SUPPORTED_PROTOCOLS: usize = 16;
pub const MAXIMUM_EXTENDED_CAPABILITY_HOPS: usize = 64;

const EXTENDED_CAPABILITY_MINIMUM_OFFSET: u32 = 0x20;
const SUPPORTED_PROTOCOL_CAPABILITY_ID: u8 = 2;
const SUPPORTED_PROTOCOL_FIXED_BYTES: u32 = 16;
const USB_PROTOCOL_NAME: u32 = u32::from_le_bytes(*b"USB ");
const ROOT_DOMAIN: u64 = 0x5848_4349_5052_4f54;
const SPEED_ROOT_DOMAIN: u64 = 0x5350_4545_445f_4944;

/// A transport which can consume only a range already checked against the
/// retained BAR0 lease.  Implementations may map, read, and unmap one dword.
pub trait CheckedSupportedProtocolRead {
    type Error;

    fn read_u32(&mut self, range: Bar0ApertureRange<'_>) -> Result<u32, Self::Error>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsbProtocolKind {
    Usb2,
    Usb3,
}

impl UsbProtocolKind {
    const fn code(self) -> u64 {
        match self {
            Self::Usb2 => 2,
            Self::Usb3 => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SupportedProtocol {
    pub capability_offset: u32,
    pub next_capability_offset: Option<u32>,
    pub kind: UsbProtocolKind,
    pub revision_bcd: u16,
    /// xHCI port identifiers are one-based.
    pub first_port: u8,
    pub port_count: u8,
    pub protocol_defined: u16,
    pub protocol_speed_id_count: u8,
    pub protocol_slot_type: u8,
    pub speed_id_root: u64,
    pub capability_root: u64,
}

impl SupportedProtocol {
    const EMPTY: Self = Self {
        capability_offset: 0,
        next_capability_offset: None,
        kind: UsbProtocolKind::Usb2,
        revision_bcd: 0,
        first_port: 0,
        port_count: 0,
        protocol_defined: 0,
        protocol_speed_id_count: 0,
        protocol_slot_type: 0,
        speed_id_root: 0,
        capability_root: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SupportedProtocolEvidence {
    protocols: [SupportedProtocol; MAXIMUM_SUPPORTED_PROTOCOLS],
    protocol_count: u8,
    pub extended_capability_count: u8,
    pub initial_offset: u32,
    pub maximum_ports: u8,
    pub aperture_base: u64,
    pub aperture_bytes: u64,
    pub root: u64,
}

impl SupportedProtocolEvidence {
    pub fn protocols(&self) -> &[SupportedProtocol] {
        &self.protocols[..usize::from(self.protocol_count)]
    }

    pub const fn protocol_count(&self) -> u8 {
        self.protocol_count
    }

    pub fn usb2_protocols(&self) -> impl Iterator<Item = &SupportedProtocol> {
        self.protocols()
            .iter()
            .filter(|protocol| protocol.kind == UsbProtocolKind::Usb2)
    }

    pub fn usb3_protocols(&self) -> impl Iterator<Item = &SupportedProtocol> {
        self.protocols()
            .iter()
            .filter(|protocol| protocol.kind == UsbProtocolKind::Usb3)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum SupportedProtocolError<ReadError> {
    InvalidSecret,
    InvalidMaximumPorts,
    InvalidCapabilityOffset(u32),
    CapabilityOutsideAperture {
        offset: u32,
        bytes: u32,
        aperture_bytes: u64,
    },
    ApertureLease {
        offset: u32,
        source: BarApertureBoundsError,
    },
    Read(ReadError),
    MissingSupportedProtocol,
    MissingPortOneCoverage,
    ExtendedCapabilityCapacity,
    SupportedProtocolCapacity,
    CapabilityCycle {
        from: u32,
        to: u32,
    },
    BodyOverlapsNextCapability {
        offset: u32,
        body_end: u32,
        next: u32,
    },
    InvalidProtocolName {
        offset: u32,
        name: u32,
    },
    InvalidBcdRevision {
        offset: u32,
        major: u8,
        minor: u8,
    },
    UnsupportedMajorRevision {
        offset: u32,
        major: u8,
    },
    UnsupportedMinorRevision {
        offset: u32,
        major: u8,
        minor: u8,
    },
    InvalidCompatiblePortRange {
        offset: u32,
        first_port: u8,
        port_count: u8,
        maximum_ports: u8,
    },
    DuplicateProtocolCoverage {
        offset: u32,
        prior_offset: u32,
    },
    OverlappingProtocolCoverage {
        offset: u32,
        prior_offset: u32,
        port: u8,
    },
    InvalidProtocolSpeedId {
        offset: u32,
        speed_id: u8,
    },
    DuplicateProtocolSpeedId {
        offset: u32,
        speed_id: u8,
    },
    ReservedProtocolSpeedType {
        offset: u32,
    },
    UnpairedAsymmetricProtocolSpeed {
        offset: u32,
        speed_id: u8,
    },
}

/// Decode the xHCI extended-capability chain rooted at `initial_offset`.
///
/// The non-copy BAR lease remains borrowed for the entire walk.  Every offset
/// and complete Supported Protocol body must fit inside the measured aperture;
/// the resulting map must contain a Supported Protocol range covering port 1.
pub fn decode_supported_protocols<Reader: CheckedSupportedProtocolRead>(
    aperture: &Bar0ApertureLease,
    initial_offset: u32,
    maximum_ports: u8,
    secret: u64,
    reader: &mut Reader,
) -> Result<SupportedProtocolEvidence, SupportedProtocolError<Reader::Error>> {
    let mut leased = MeasuredApertureReader { aperture, reader };
    decode_with_reader(&mut leased, initial_offset, maximum_ports, secret)
}

trait LeasedDwordReader {
    type Error;

    fn aperture_base(&self) -> u64;
    fn aperture_bytes(&self) -> u64;
    fn read_u32(&mut self, offset: u32) -> Result<u32, SupportedProtocolError<Self::Error>>;
}

struct MeasuredApertureReader<'lease, 'reader, Reader> {
    aperture: &'lease Bar0ApertureLease,
    reader: &'reader mut Reader,
}

impl<Reader: CheckedSupportedProtocolRead> LeasedDwordReader
    for MeasuredApertureReader<'_, '_, Reader>
{
    type Error = Reader::Error;

    fn aperture_base(&self) -> u64 {
        self.aperture.physical_base()
    }

    fn aperture_bytes(&self) -> u64 {
        self.aperture.length()
    }

    fn read_u32(&mut self, offset: u32) -> Result<u32, SupportedProtocolError<Self::Error>> {
        let range = self
            .aperture
            .checked_range(u64::from(offset), core::mem::size_of::<u32>())
            .map_err(|source| SupportedProtocolError::ApertureLease { offset, source })?;
        self.reader
            .read_u32(range)
            .map_err(SupportedProtocolError::Read)
    }
}

fn decode_with_reader<Reader: LeasedDwordReader>(
    reader: &mut Reader,
    initial_offset: u32,
    maximum_ports: u8,
    secret: u64,
) -> Result<SupportedProtocolEvidence, SupportedProtocolError<Reader::Error>> {
    if secret == 0 {
        return Err(SupportedProtocolError::InvalidSecret);
    }
    if maximum_ports == 0 {
        return Err(SupportedProtocolError::InvalidMaximumPorts);
    }
    let aperture_base = reader.aperture_base();
    let aperture_bytes = reader.aperture_bytes();
    let mut evidence = SupportedProtocolEvidence {
        protocols: [SupportedProtocol::EMPTY; MAXIMUM_SUPPORTED_PROTOCOLS],
        protocol_count: 0,
        extended_capability_count: 0,
        initial_offset,
        maximum_ports,
        aperture_base,
        aperture_bytes,
        root: 0,
    };
    let mut visited = [0_u32; MAXIMUM_EXTENDED_CAPABILITY_HOPS];
    let mut visited_count = 0_usize;
    let mut covered_ports = [0_u64; 4];
    let mut root = mix(secret ^ ROOT_DOMAIN, aperture_base);
    root = mix(root, aperture_bytes);
    root = mix(root, u64::from(initial_offset));
    root = mix(root, u64::from(maximum_ports));

    let mut offset = initial_offset;
    while offset != 0 {
        validate_capability_offset(offset, aperture_bytes)?;
        if visited[..visited_count].contains(&offset) {
            return Err(SupportedProtocolError::CapabilityCycle {
                from: offset,
                to: offset,
            });
        }
        let destination = visited
            .get_mut(visited_count)
            .ok_or(SupportedProtocolError::ExtendedCapabilityCapacity)?;
        *destination = offset;
        visited_count += 1;

        let header = reader.read_u32(offset)?;
        let capability_id = header as u8;
        let next_dwords = ((header >> 8) & 0xff) as u8;
        let next = relative_next(offset, next_dwords, aperture_bytes)?;
        root = mix(root, u64::from(offset));
        root = mix(root, u64::from(header));

        if capability_id == SUPPORTED_PROTOCOL_CAPABILITY_ID {
            if usize::from(evidence.protocol_count) == MAXIMUM_SUPPORTED_PROTOCOLS {
                return Err(SupportedProtocolError::SupportedProtocolCapacity);
            }
            let protocol =
                read_supported_protocol(reader, offset, header, next, maximum_ports, secret)?;
            validate_protocol_coverage(&evidence, protocol, &mut covered_ports)?;
            let index = usize::from(evidence.protocol_count);
            evidence.protocols[index] = protocol;
            evidence.protocol_count += 1;
            root = mix(root, protocol.capability_root);
        }
        offset = next.unwrap_or(0);
    }

    if evidence.protocol_count == 0 {
        return Err(SupportedProtocolError::MissingSupportedProtocol);
    }
    if !evidence
        .protocols()
        .iter()
        .any(|protocol| protocol.first_port == 1)
    {
        return Err(SupportedProtocolError::MissingPortOneCoverage);
    }
    evidence.extended_capability_count = u8::try_from(visited_count)
        .map_err(|_| SupportedProtocolError::ExtendedCapabilityCapacity)?;
    root = mix(root, u64::from(evidence.protocol_count));
    root = mix(root, u64::from(evidence.extended_capability_count));
    for word in covered_ports {
        root = mix(root, word);
    }
    evidence.root = canonical_root(root);
    Ok(evidence)
}

fn read_supported_protocol<Reader: LeasedDwordReader>(
    reader: &mut Reader,
    offset: u32,
    header: u32,
    next: Option<u32>,
    maximum_ports: u8,
    secret: u64,
) -> Result<SupportedProtocol, SupportedProtocolError<Reader::Error>> {
    ensure_range(
        offset,
        SUPPORTED_PROTOCOL_FIXED_BYTES,
        reader.aperture_bytes(),
    )?;
    let name_offset = checked_relative_offset(offset, 4, reader.aperture_bytes())?;
    let ports_offset = checked_relative_offset(offset, 8, reader.aperture_bytes())?;
    let slot_offset = checked_relative_offset(offset, 12, reader.aperture_bytes())?;
    let name = reader.read_u32(name_offset)?;
    let ports = reader.read_u32(ports_offset)?;
    let slot = reader.read_u32(slot_offset)?;
    if name != USB_PROTOCOL_NAME {
        return Err(SupportedProtocolError::InvalidProtocolName { offset, name });
    }

    let minor = (header >> 16) as u8;
    let major = (header >> 24) as u8;
    if !is_packed_bcd_byte(major) || !is_packed_bcd_byte(minor) {
        return Err(SupportedProtocolError::InvalidBcdRevision {
            offset,
            major,
            minor,
        });
    }
    let kind = match major {
        0x02 => UsbProtocolKind::Usb2,
        0x03 => UsbProtocolKind::Usb3,
        _ => {
            return Err(SupportedProtocolError::UnsupportedMajorRevision { offset, major });
        }
    };
    let supported_minor = match kind {
        UsbProtocolKind::Usb2 => minor == 0,
        UsbProtocolKind::Usb3 => matches!(minor, 0 | 0x10 | 0x20),
    };
    if !supported_minor {
        return Err(SupportedProtocolError::UnsupportedMinorRevision {
            offset,
            major,
            minor,
        });
    }

    let first_port = ports as u8;
    let port_count = (ports >> 8) as u8;
    let last_port = first_port
        .checked_add(port_count.saturating_sub(1))
        .filter(|last| first_port != 0 && port_count != 0 && *last <= maximum_ports)
        .ok_or(SupportedProtocolError::InvalidCompatiblePortRange {
            offset,
            first_port,
            port_count,
            maximum_ports,
        })?;
    let _ = last_port;

    let speed_count = ((ports >> 28) & 0x0f) as u8;
    let body_bytes = SUPPORTED_PROTOCOL_FIXED_BYTES + u32::from(speed_count) * 4;
    ensure_range(offset, body_bytes, reader.aperture_bytes())?;
    let body_end = offset.checked_add(body_bytes).ok_or(
        SupportedProtocolError::CapabilityOutsideAperture {
            offset,
            bytes: body_bytes,
            aperture_bytes: reader.aperture_bytes(),
        },
    )?;
    if let Some(next_offset) = next
        && next_offset < body_end
    {
        return Err(SupportedProtocolError::BodyOverlapsNextCapability {
            offset,
            body_end,
            next: next_offset,
        });
    }

    let mut speed_id_root = mix(secret ^ SPEED_ROOT_DOMAIN, u64::from(offset));
    let mut speed_ids = 0_u16;
    let mut speed_words = [0_u32; 15];
    for index in 0..speed_count {
        let relative = SUPPORTED_PROTOCOL_FIXED_BYTES + u32::from(index) * 4;
        let speed_offset = checked_relative_offset(offset, relative, reader.aperture_bytes())?;
        let speed = reader.read_u32(speed_offset)?;
        let speed_id = (speed & 0x0f) as u8;
        if speed_id == 0 {
            return Err(SupportedProtocolError::InvalidProtocolSpeedId {
                offset: speed_offset,
                speed_id,
            });
        }
        let speed_bit = 1_u16 << speed_id;
        if speed_ids & speed_bit != 0 {
            return Err(SupportedProtocolError::DuplicateProtocolSpeedId {
                offset: speed_offset,
                speed_id,
            });
        }
        if (speed >> 6) & 0x03 == 1 {
            return Err(SupportedProtocolError::ReservedProtocolSpeedType {
                offset: speed_offset,
            });
        }
        speed_ids |= speed_bit;
        speed_words[usize::from(index)] = speed;
        speed_id_root = mix(speed_id_root, u64::from(speed_offset));
        speed_id_root = mix(speed_id_root, u64::from(speed));
    }
    let mut index = 0_usize;
    while index < usize::from(speed_count) {
        let speed = speed_words[index];
        let protocol_speed_type = ((speed >> 6) & 0x03) as u8;
        if protocol_speed_type == 2 {
            let comparable = speed & !0xcf;
            let paired = speed_words
                .get(index + 1)
                .is_some_and(|peer| ((*peer >> 6) & 0x03) == 3 && (*peer & !0xcf) == comparable);
            if !paired {
                return Err(SupportedProtocolError::UnpairedAsymmetricProtocolSpeed {
                    offset: checked_relative_offset(
                        offset,
                        SUPPORTED_PROTOCOL_FIXED_BYTES + index as u32 * 4,
                        reader.aperture_bytes(),
                    )?,
                    speed_id: (speed & 0x0f) as u8,
                });
            }
            index += 2;
        } else if protocol_speed_type == 3 {
            return Err(SupportedProtocolError::UnpairedAsymmetricProtocolSpeed {
                offset: checked_relative_offset(
                    offset,
                    SUPPORTED_PROTOCOL_FIXED_BYTES + index as u32 * 4,
                    reader.aperture_bytes(),
                )?,
                speed_id: (speed & 0x0f) as u8,
            });
        } else {
            index += 1;
        }
    }
    speed_id_root = canonical_root(speed_id_root);

    let mut capability_root = mix(secret ^ ROOT_DOMAIN, u64::from(offset));
    capability_root = mix(capability_root, u64::from(header));
    capability_root = mix(capability_root, u64::from(name));
    capability_root = mix(capability_root, u64::from(ports));
    capability_root = mix(capability_root, u64::from(slot));
    capability_root = mix(capability_root, kind.code());
    capability_root = mix(capability_root, speed_id_root);
    capability_root = canonical_root(capability_root);

    Ok(SupportedProtocol {
        capability_offset: offset,
        next_capability_offset: next,
        kind,
        revision_bcd: (u16::from(major) << 8) | u16::from(minor),
        first_port,
        port_count,
        protocol_defined: ((ports >> 16) & 0x0fff) as u16,
        protocol_speed_id_count: speed_count,
        protocol_slot_type: (slot & 0x1f) as u8,
        speed_id_root,
        capability_root,
    })
}

fn validate_protocol_coverage<ReadError>(
    evidence: &SupportedProtocolEvidence,
    protocol: SupportedProtocol,
    covered_ports: &mut [u64; 4],
) -> Result<(), SupportedProtocolError<ReadError>> {
    for prior in evidence.protocols() {
        if prior.first_port == protocol.first_port && prior.port_count == protocol.port_count {
            return Err(SupportedProtocolError::DuplicateProtocolCoverage {
                offset: protocol.capability_offset,
                prior_offset: prior.capability_offset,
            });
        }
    }
    let last_port = u16::from(protocol.first_port) + u16::from(protocol.port_count) - 1;
    for port_number in u16::from(protocol.first_port)..=last_port {
        let port = port_number as u8;
        let bit_index = usize::from(port - 1);
        let word = bit_index / 64;
        let bit = 1_u64 << (bit_index % 64);
        if covered_ports[word] & bit != 0 {
            let prior_offset = evidence
                .protocols()
                .iter()
                .find(|prior| {
                    port_number >= u16::from(prior.first_port)
                        && port_number < u16::from(prior.first_port) + u16::from(prior.port_count)
                })
                .map_or(0, |prior| prior.capability_offset);
            return Err(SupportedProtocolError::OverlappingProtocolCoverage {
                offset: protocol.capability_offset,
                prior_offset,
                port,
            });
        }
        covered_ports[word] |= bit;
    }
    Ok(())
}

fn relative_next<ReadError>(
    current: u32,
    next_dwords: u8,
    aperture_bytes: u64,
) -> Result<Option<u32>, SupportedProtocolError<ReadError>> {
    if next_dwords == 0 {
        return Ok(None);
    }
    let displacement = u32::from(next_dwords) * 4;
    let next = current.wrapping_add(displacement);
    if next <= current {
        return Err(SupportedProtocolError::CapabilityCycle {
            from: current,
            to: next,
        });
    }
    validate_capability_offset(next, aperture_bytes)?;
    Ok(Some(next))
}

fn validate_capability_offset<ReadError>(
    offset: u32,
    aperture_bytes: u64,
) -> Result<(), SupportedProtocolError<ReadError>> {
    if offset < EXTENDED_CAPABILITY_MINIMUM_OFFSET || offset & 3 != 0 {
        return Err(SupportedProtocolError::InvalidCapabilityOffset(offset));
    }
    ensure_range(offset, 4, aperture_bytes)
}

fn ensure_range<ReadError>(
    offset: u32,
    bytes: u32,
    aperture_bytes: u64,
) -> Result<(), SupportedProtocolError<ReadError>> {
    let end = u64::from(offset) + u64::from(bytes);
    if bytes == 0 || end > aperture_bytes {
        Err(SupportedProtocolError::CapabilityOutsideAperture {
            offset,
            bytes,
            aperture_bytes,
        })
    } else {
        Ok(())
    }
}

fn checked_relative_offset<ReadError>(
    offset: u32,
    relative: u32,
    aperture_bytes: u64,
) -> Result<u32, SupportedProtocolError<ReadError>> {
    offset
        .checked_add(relative)
        .ok_or(SupportedProtocolError::CapabilityOutsideAperture {
            offset,
            bytes: relative.saturating_add(4),
            aperture_bytes,
        })
}

const fn is_packed_bcd_byte(value: u8) -> bool {
    value & 0x0f <= 9 && value >> 4 <= 9
}

const fn canonical_root(root: u64) -> u64 {
    if root == 0 { ROOT_DOMAIN } else { root }
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

    use super::*;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum ReadFault {
        Injected,
        Missing(u32),
    }

    struct ImageReader {
        aperture_base: u64,
        aperture_bytes: u64,
        words: Vec<(u32, u32)>,
        reads: Vec<u32>,
        fail_at: Option<u32>,
    }

    impl ImageReader {
        fn new(aperture_bytes: u64, words: Vec<(u32, u32)>) -> Self {
            Self {
                aperture_base: 0x8000_0000,
                aperture_bytes,
                words,
                reads: Vec::new(),
                fail_at: None,
            }
        }
    }

    impl LeasedDwordReader for ImageReader {
        type Error = ReadFault;

        fn aperture_base(&self) -> u64 {
            self.aperture_base
        }

        fn aperture_bytes(&self) -> u64 {
            self.aperture_bytes
        }

        fn read_u32(&mut self, offset: u32) -> Result<u32, SupportedProtocolError<Self::Error>> {
            ensure_range(offset, 4, self.aperture_bytes)?;
            self.reads.push(offset);
            if self.fail_at == Some(offset) {
                return Err(SupportedProtocolError::Read(ReadFault::Injected));
            }
            self.words
                .iter()
                .find_map(|(known, value)| (*known == offset).then_some(*value))
                .ok_or(SupportedProtocolError::Read(ReadFault::Missing(offset)))
        }
    }

    fn header(next_dwords: u8, minor: u8, major: u8) -> u32 {
        u32::from(SUPPORTED_PROTOCOL_CAPABILITY_ID)
            | (u32::from(next_dwords) << 8)
            | (u32::from(minor) << 16)
            | (u32::from(major) << 24)
    }

    fn protocol(
        offset: u32,
        next_dwords: u8,
        major: u8,
        minor: u8,
        first_port: u8,
        port_count: u8,
        speeds: &[u32],
    ) -> Vec<(u32, u32)> {
        let mut words = Vec::new();
        words.push((offset, header(next_dwords, minor, major)));
        words.push((offset + 4, USB_PROTOCOL_NAME));
        words.push((
            offset + 8,
            u32::from(first_port) | (u32::from(port_count) << 8) | ((speeds.len() as u32) << 28),
        ));
        words.push((offset + 12, 1));
        for (index, speed) in speeds.iter().enumerate() {
            words.push((offset + 16 + index as u32 * 4, *speed));
        }
        words
    }

    fn decode_image(
        reader: &mut ImageReader,
        initial_offset: u32,
        maximum_ports: u8,
    ) -> Result<SupportedProtocolEvidence, SupportedProtocolError<ReadFault>> {
        decode_with_reader(reader, initial_offset, maximum_ports, 0x1234_5678_9abc_def0)
    }

    #[test]
    fn empty_chain_cannot_masquerade_as_protocol_evidence() {
        let mut reader = ImageReader::new(0x100, Vec::new());
        assert_eq!(
            decode_image(&mut reader, 0, 8),
            Err(SupportedProtocolError::MissingSupportedProtocol)
        );
        assert!(reader.reads.is_empty());
    }

    #[test]
    fn decodes_usb2_and_usb3_with_current_relative_next_offsets() {
        let mut words = protocol(0x40, 8, 0x02, 0x00, 1, 4, &[0x0004_0001]);
        words.extend(protocol(
            0x60,
            0,
            0x03,
            0x20,
            5,
            4,
            &[0x0005_0001, 0x000a_0002],
        ));
        let mut reader = ImageReader::new(0x100, words);
        let evidence = decode_image(&mut reader, 0x40, 8).unwrap();
        assert_eq!(evidence.protocol_count(), 2);
        assert_eq!(evidence.extended_capability_count, 2);
        assert_eq!(evidence.protocols()[0].kind, UsbProtocolKind::Usb2);
        assert_eq!(evidence.protocols()[0].revision_bcd, 0x0200);
        assert_eq!(evidence.protocols()[0].next_capability_offset, Some(0x60));
        assert_eq!(evidence.protocols()[1].kind, UsbProtocolKind::Usb3);
        assert_eq!(evidence.protocols()[1].revision_bcd, 0x0320);
        assert_eq!(evidence.usb2_protocols().count(), 1);
        assert_eq!(evidence.usb3_protocols().count(), 1);
        assert_eq!(
            reader.reads,
            [
                0x40, 0x44, 0x48, 0x4c, 0x50, 0x60, 0x64, 0x68, 0x6c, 0x70, 0x74
            ]
        );
    }

    #[test]
    fn unknown_capabilities_are_rooted_but_not_interpreted_as_children() {
        let unknown_header = 0x99_u32 | (4 << 8);
        let words = [
            (0x20, unknown_header),
            (0x30, header(0, 0, 3)),
            (0x34, USB_PROTOCOL_NAME),
            (0x38, 1 | (2 << 8)),
            (0x3c, 0),
        ];
        let mut reader = ImageReader::new(0x80, words.into());
        let evidence = decode_image(&mut reader, 0x20, 2).unwrap();
        assert_eq!(evidence.extended_capability_count, 2);
        assert_eq!(evidence.protocol_count(), 1);
        assert_eq!(reader.reads, [0x20, 0x30, 0x34, 0x38, 0x3c]);
    }

    #[test]
    fn deterministic_root_binds_lease_and_every_protocol_body() {
        let words = protocol(0x40, 0, 3, 0x10, 1, 2, &[0x0005_0001]);
        let mut first = ImageReader::new(0x100, words.clone());
        let first_root = decode_image(&mut first, 0x40, 2).unwrap().root;
        let mut replay = ImageReader::new(0x100, words.clone());
        assert_eq!(decode_image(&mut replay, 0x40, 2).unwrap().root, first_root);

        let mut changed_words = words;
        changed_words
            .iter_mut()
            .find(|(offset, _)| *offset == 0x50)
            .unwrap()
            .1 ^= 1 << 16;
        let mut changed = ImageReader::new(0x100, changed_words);
        assert_ne!(
            decode_image(&mut changed, 0x40, 2).unwrap().root,
            first_root
        );

        let mut moved = ImageReader::new(0x100, protocol(0x40, 0, 3, 0x10, 1, 2, &[0x0005_0001]));
        moved.aperture_base += 0x1000;
        assert_ne!(decode_image(&mut moved, 0x40, 2).unwrap().root, first_root);
    }

    #[test]
    fn rejects_zero_secret_and_zero_maximum_ports_without_reads() {
        let mut reader = ImageReader::new(0x100, Vec::new());
        assert_eq!(
            decode_with_reader(&mut reader, 0, 8, 0),
            Err(SupportedProtocolError::InvalidSecret)
        );
        assert_eq!(
            decode_image(&mut reader, 0, 0),
            Err(SupportedProtocolError::InvalidMaximumPorts)
        );
        assert!(reader.reads.is_empty());
    }

    #[test]
    fn rejects_misaligned_low_and_out_of_aperture_offsets_before_reads() {
        for (offset, expected) in [
            (0x1c, SupportedProtocolError::InvalidCapabilityOffset(0x1c)),
            (0x22, SupportedProtocolError::InvalidCapabilityOffset(0x22)),
            (
                0x80,
                SupportedProtocolError::CapabilityOutsideAperture {
                    offset: 0x80,
                    bytes: 4,
                    aperture_bytes: 0x80,
                },
            ),
        ] {
            let mut reader = ImageReader::new(0x80, Vec::new());
            assert_eq!(decode_image(&mut reader, offset, 8), Err(expected));
            assert!(reader.reads.is_empty());
        }
    }

    #[test]
    fn propagates_checked_reader_failures() {
        let mut reader = ImageReader::new(0x100, protocol(0x40, 0, 2, 0, 1, 1, &[]));
        reader.fail_at = Some(0x44);
        assert_eq!(
            decode_image(&mut reader, 0x40, 1),
            Err(SupportedProtocolError::Read(ReadFault::Injected))
        );
    }

    #[test]
    fn rejects_non_bcd_revisions_and_unknown_major_versions() {
        let mut bad_major = ImageReader::new(0x100, protocol(0x40, 0, 0x2a, 0, 1, 1, &[]));
        assert_eq!(
            decode_image(&mut bad_major, 0x40, 1),
            Err(SupportedProtocolError::InvalidBcdRevision {
                offset: 0x40,
                major: 0x2a,
                minor: 0
            })
        );
        let mut bad_minor = ImageReader::new(0x100, protocol(0x40, 0, 2, 0x1a, 1, 1, &[]));
        assert_eq!(
            decode_image(&mut bad_minor, 0x40, 1),
            Err(SupportedProtocolError::InvalidBcdRevision {
                offset: 0x40,
                major: 2,
                minor: 0x1a
            })
        );
        let mut unknown = ImageReader::new(0x100, protocol(0x40, 0, 4, 0, 1, 1, &[]));
        assert_eq!(
            decode_image(&mut unknown, 0x40, 1),
            Err(SupportedProtocolError::UnsupportedMajorRevision {
                offset: 0x40,
                major: 4
            })
        );
        let mut unsupported_usb2 = ImageReader::new(0x100, protocol(0x40, 0, 2, 0x10, 1, 1, &[]));
        assert_eq!(
            decode_image(&mut unsupported_usb2, 0x40, 1),
            Err(SupportedProtocolError::UnsupportedMinorRevision {
                offset: 0x40,
                major: 2,
                minor: 0x10
            })
        );
        let mut unsupported_usb3 = ImageReader::new(0x100, protocol(0x40, 0, 3, 0x30, 1, 1, &[]));
        assert_eq!(
            decode_image(&mut unsupported_usb3, 0x40, 1),
            Err(SupportedProtocolError::UnsupportedMinorRevision {
                offset: 0x40,
                major: 3,
                minor: 0x30
            })
        );
    }

    #[test]
    fn rejects_non_usb_protocol_name() {
        let mut words = protocol(0x40, 0, 2, 0, 1, 1, &[]);
        words[1].1 = u32::from_le_bytes(*b"PCI ");
        let mut reader = ImageReader::new(0x100, words);
        assert_eq!(
            decode_image(&mut reader, 0x40, 1),
            Err(SupportedProtocolError::InvalidProtocolName {
                offset: 0x40,
                name: u32::from_le_bytes(*b"PCI ")
            })
        );
    }

    #[test]
    fn rejects_empty_wrapping_and_out_of_maxports_ranges() {
        for (first, count, maximum) in [(0, 1, 8), (1, 0, 8), (250, 10, 255), (8, 2, 8)] {
            let mut reader = ImageReader::new(0x100, protocol(0x40, 0, 2, 0, first, count, &[]));
            assert_eq!(
                decode_image(&mut reader, 0x40, maximum),
                Err(SupportedProtocolError::InvalidCompatiblePortRange {
                    offset: 0x40,
                    first_port: first,
                    port_count: count,
                    maximum_ports: maximum
                })
            );
        }
    }

    #[test]
    fn rejects_duplicate_and_partially_overlapping_port_coverage() {
        let mut duplicate_words = protocol(0x40, 4, 2, 0, 1, 2, &[]);
        duplicate_words.extend(protocol(0x50, 0, 3, 0, 1, 2, &[]));
        let mut duplicate = ImageReader::new(0x100, duplicate_words);
        assert_eq!(
            decode_image(&mut duplicate, 0x40, 4),
            Err(SupportedProtocolError::DuplicateProtocolCoverage {
                offset: 0x50,
                prior_offset: 0x40
            })
        );

        let mut overlap_words = protocol(0x40, 4, 2, 0, 1, 3, &[]);
        overlap_words.extend(protocol(0x50, 0, 3, 0, 3, 2, &[]));
        let mut overlap = ImageReader::new(0x100, overlap_words);
        assert_eq!(
            decode_image(&mut overlap, 0x40, 4),
            Err(SupportedProtocolError::OverlappingProtocolCoverage {
                offset: 0x50,
                prior_offset: 0x40,
                port: 3
            })
        );
    }

    #[test]
    fn requires_a_supported_protocol_range_anchored_at_port_one() {
        let mut reader = ImageReader::new(0x100, protocol(0x40, 0, 3, 0, 2, 2, &[]));
        assert_eq!(
            decode_image(&mut reader, 0x40, 4),
            Err(SupportedProtocolError::MissingPortOneCoverage)
        );
    }

    #[test]
    fn rejects_next_pointer_inside_supported_protocol_body() {
        let mut reader = ImageReader::new(0x100, protocol(0x40, 4, 2, 0, 1, 1, &[1]));
        assert_eq!(
            decode_image(&mut reader, 0x40, 1),
            Err(SupportedProtocolError::BodyOverlapsNextCapability {
                offset: 0x40,
                body_end: 0x54,
                next: 0x50
            })
        );
    }

    #[test]
    fn rejects_body_and_next_ranges_outside_the_aperture() {
        let mut body = ImageReader::new(0x50, protocol(0x40, 0, 2, 0, 1, 1, &[1]));
        assert_eq!(
            decode_image(&mut body, 0x40, 1),
            Err(SupportedProtocolError::CapabilityOutsideAperture {
                offset: 0x40,
                bytes: 20,
                aperture_bytes: 0x50
            })
        );

        let words = [(0x40, 0xff00_u32 | 0x99)];
        let mut next = ImageReader::new(0x100, words.into());
        assert_eq!(
            decode_image(&mut next, 0x40, 1),
            Err(SupportedProtocolError::CapabilityOutsideAperture {
                offset: 0x43c,
                bytes: 4,
                aperture_bytes: 0x100
            })
        );
    }

    #[test]
    fn rejects_wrapping_relative_next_as_a_cycle() {
        let current = 0xffff_fffc;
        let mut reader = ImageReader::new(
            u64::from(u32::MAX) + 1,
            [(current, 0x0100_u32 | 0x99)].into(),
        );
        assert_eq!(
            decode_image(&mut reader, current, 1),
            Err(SupportedProtocolError::CapabilityCycle {
                from: current,
                to: 0
            })
        );
    }

    #[test]
    fn rejects_zero_and_duplicate_protocol_speed_ids() {
        let mut zero = ImageReader::new(0x100, protocol(0x40, 0, 3, 0, 1, 1, &[0]));
        assert_eq!(
            decode_image(&mut zero, 0x40, 1),
            Err(SupportedProtocolError::InvalidProtocolSpeedId {
                offset: 0x50,
                speed_id: 0
            })
        );

        let mut duplicate = ImageReader::new(0x100, protocol(0x40, 0, 3, 0, 1, 1, &[1, 0x10001]));
        assert_eq!(
            decode_image(&mut duplicate, 0x40, 1),
            Err(SupportedProtocolError::DuplicateProtocolSpeedId {
                offset: 0x54,
                speed_id: 1
            })
        );
    }

    #[test]
    fn rejects_reserved_and_unpaired_asymmetric_speed_types() {
        let mut reserved = ImageReader::new(0x100, protocol(0x40, 0, 3, 0, 1, 1, &[1 | (1 << 6)]));
        assert_eq!(
            decode_image(&mut reserved, 0x40, 1),
            Err(SupportedProtocolError::ReservedProtocolSpeedType { offset: 0x50 })
        );

        let mut unpaired = ImageReader::new(0x100, protocol(0x40, 0, 3, 0, 1, 1, &[1 | (2 << 6)]));
        assert_eq!(
            decode_image(&mut unpaired, 0x40, 1),
            Err(SupportedProtocolError::UnpairedAsymmetricProtocolSpeed {
                offset: 0x50,
                speed_id: 1
            })
        );

        let mut paired = ImageReader::new(
            0x100,
            protocol(0x40, 0, 3, 0, 1, 1, &[1 | (2 << 6), 2 | (3 << 6)]),
        );
        assert!(decode_image(&mut paired, 0x40, 1).is_ok());

        let mut separated = ImageReader::new(
            0x100,
            protocol(0x40, 0, 3, 0, 1, 1, &[1 | (2 << 6), 3, 2 | (3 << 6)]),
        );
        assert_eq!(
            decode_image(&mut separated, 0x40, 1),
            Err(SupportedProtocolError::UnpairedAsymmetricProtocolSpeed {
                offset: 0x50,
                speed_id: 1
            })
        );

        let mut transmit_first = ImageReader::new(
            0x100,
            protocol(0x40, 0, 3, 0, 1, 1, &[1 | (3 << 6), 2 | (2 << 6)]),
        );
        assert_eq!(
            decode_image(&mut transmit_first, 0x40, 1),
            Err(SupportedProtocolError::UnpairedAsymmetricProtocolSpeed {
                offset: 0x50,
                speed_id: 1
            })
        );
    }

    #[test]
    fn fixed_protocol_capacity_is_enforced() {
        let mut words = Vec::new();
        for index in 0..=MAXIMUM_SUPPORTED_PROTOCOLS {
            let offset = 0x40 + index as u32 * 0x10;
            let next = if index == MAXIMUM_SUPPORTED_PROTOCOLS {
                0
            } else {
                4
            };
            words.extend(protocol(offset, next, 2, 0, index as u8 + 1, 1, &[]));
        }
        let mut reader = ImageReader::new(0x200, words);
        assert_eq!(
            decode_image(&mut reader, 0x40, 32),
            Err(SupportedProtocolError::SupportedProtocolCapacity)
        );
    }

    #[test]
    fn bounded_chain_capacity_is_enforced() {
        let mut words = Vec::new();
        for index in 0..=MAXIMUM_EXTENDED_CAPABILITY_HOPS {
            let offset = 0x40 + index as u32 * 4;
            let next = if index == MAXIMUM_EXTENDED_CAPABILITY_HOPS {
                0
            } else {
                1
            };
            words.push((offset, 0x99 | (next << 8)));
        }
        let mut reader = ImageReader::new(0x200, words);
        assert_eq!(
            decode_image(&mut reader, 0x40, 1),
            Err(SupportedProtocolError::ExtendedCapabilityCapacity)
        );
        assert_eq!(reader.reads.len(), MAXIMUM_EXTENDED_CAPABILITY_HOPS);
    }
}
