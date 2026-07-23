use crate::capability::{Capability, DmaControl, FaultPolicyControl, PolicyControl};
use crate::sync::SpinLock;

pub const CHARYBDIS_PAGE_SIZE: u64 = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct DmaAccess(u8);

impl DmaAccess {
    pub const READ: Self = Self(1 << 0);
    pub const WRITE: Self = Self(1 << 1);
    pub const EXECUTE: Self = Self(1 << 2);
    pub const READ_WRITE: Self = Self(Self::READ.0 | Self::WRITE.0);

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn contains(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }

    const fn valid(self) -> bool {
        self.0 != 0 && self.0 & !(Self::READ.0 | Self::WRITE.0 | Self::EXECUTE.0) == 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmaIntent {
    pub domain: u64,
    pub device: u64,
    pub device_address: u64,
    pub physical_address: u64,
    pub length: u64,
    pub expires_at: u64,
    pub access: DmaAccess,
    pub witness_quorum: u8,
    pub purpose: u16,
    pub policy_epoch: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmaAperture {
    pub id: u32,
    pub policy_epoch: u32,
    pub domain: u64,
    pub device_mask: u64,
    pub device_value: u64,
    pub device_address_start: u64,
    pub device_address_end: u64,
    pub physical_address_start: u64,
    pub physical_address_end: u64,
    pub maximum_mapping_bytes: u64,
    pub maximum_total_bytes: u64,
    pub allowed_access: DmaAccess,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct DmaTicket(u64);

impl DmaTicket {
    pub const INVALID: Self = Self(0);

    pub const fn raw(self) -> u64 {
        self.0
    }

    const fn new(slot: u16, generation: u16, tag: u32) -> Self {
        Self((slot as u64) | ((generation as u64) << 16) | ((tag as u64) << 32))
    }

    const fn slot(self) -> usize {
        self.0 as u16 as usize
    }

    const fn generation(self) -> u16 {
        (self.0 >> 16) as u16
    }

    const fn tag(self) -> u32 {
        (self.0 >> 32) as u32
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MappingReceipt {
    pub ticket: DmaTicket,
    pub backend_cookie: u64,
    pub device_address: u64,
    pub length: u64,
    pub access: DmaAccess,
    pub expires_at: u64,
    pub witness_mask: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MappingPhase {
    Prepared,
    Mapping,
    Active,
    Revoking,
    Poisoned,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MappingSnapshot {
    pub ticket: DmaTicket,
    pub phase: MappingPhase,
    pub aperture_id: u32,
    pub intent: DmaIntent,
    pub witness_mask: u64,
    pub backend_cookie: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CharybdisError {
    ZeroCapacity,
    InvalidIntent,
    InvalidAperture,
    ApertureCapacity,
    ApertureNotFound,
    ApertureBusy,
    NoMatchingAperture,
    AmbiguousAperture,
    QuotaExceeded,
    MappingCapacity,
    AddressOverlap,
    InvalidTicket,
    ForgedTicket,
    StaleTicket,
    WrongPhase,
    WitnessOutOfRange,
    WitnessQuorum,
    Expired,
    BackendMap,
    BackendUnmap,
    InvalidBackendCookie,
    Poisoned,
}

pub trait CharybdisBackend: Sync {
    fn map(&self, intent: DmaIntent) -> Result<u64, CharybdisError>;
    fn unmap(&self, backend_cookie: u64, intent: DmaIntent) -> Result<(), CharybdisError>;
}

#[derive(Clone, Copy)]
struct ApertureRecord {
    occupied: bool,
    aperture: DmaAperture,
    reserved_bytes: u64,
}

impl ApertureRecord {
    const EMPTY: Self = Self {
        occupied: false,
        aperture: DmaAperture {
            id: 0,
            policy_epoch: 0,
            domain: 0,
            device_mask: 0,
            device_value: 0,
            device_address_start: 0,
            device_address_end: 0,
            physical_address_start: 0,
            physical_address_end: 0,
            maximum_mapping_bytes: 0,
            maximum_total_bytes: 0,
            allowed_access: DmaAccess(0),
        },
        reserved_bytes: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum RecordPhase {
    Empty,
    Prepared,
    Mapping,
    Active,
    Revoking,
    Poisoned,
}

#[derive(Clone, Copy)]
struct MappingRecord {
    phase: RecordPhase,
    generation: u16,
    aperture_slot: u16,
    intent: DmaIntent,
    witness_mask: u64,
    backend_cookie: u64,
    tag: u32,
}

impl MappingRecord {
    const EMPTY: Self = Self {
        phase: RecordPhase::Empty,
        generation: 1,
        aperture_slot: 0,
        intent: DmaIntent {
            domain: 0,
            device: 0,
            device_address: 0,
            physical_address: 0,
            length: 0,
            expires_at: 0,
            access: DmaAccess(0),
            witness_quorum: 0,
            purpose: 0,
            policy_epoch: 0,
        },
        witness_mask: 0,
        backend_cookie: 0,
        tag: 0,
    };

    fn clear_preserving_generation(&mut self) {
        let generation = self.generation;
        *self = Self::EMPTY;
        self.generation = generation;
    }
}

struct CharybdisState<const APERTURES: usize, const MAPPINGS: usize> {
    apertures: [ApertureRecord; APERTURES],
    mappings: [MappingRecord; MAPPINGS],
    prepared: u64,
    committed: u64,
    revoked: u64,
    rejected: u64,
}

impl<const APERTURES: usize, const MAPPINGS: usize> CharybdisState<APERTURES, MAPPINGS> {
    const fn new() -> Self {
        Self {
            apertures: [ApertureRecord::EMPTY; APERTURES],
            mappings: [MappingRecord::EMPTY; MAPPINGS],
            prepared: 0,
            committed: 0,
            revoked: 0,
            rejected: 0,
        }
    }
}

pub struct CharybdisDmaFirewall<const APERTURES: usize, const MAPPINGS: usize> {
    secret: u64,
    state: SpinLock<CharybdisState<APERTURES, MAPPINGS>>,
}

impl<const APERTURES: usize, const MAPPINGS: usize> CharybdisDmaFirewall<APERTURES, MAPPINGS> {
    pub const fn new(secret: u64) -> Self {
        Self {
            secret,
            state: SpinLock::new(CharybdisState::new()),
        }
    }

    pub fn install_aperture(
        &self,
        aperture: DmaAperture,
        _authority: &Capability<'_, PolicyControl>,
    ) -> Result<(), CharybdisError> {
        if APERTURES == 0 {
            return Err(CharybdisError::ZeroCapacity);
        }
        validate_aperture(aperture)?;

        let mut state = self.state.lock();
        if state
            .apertures
            .iter()
            .any(|record| record.occupied && record.aperture.id == aperture.id)
        {
            return Err(CharybdisError::InvalidAperture);
        }

        let slot = state
            .apertures
            .iter_mut()
            .find(|record| !record.occupied)
            .ok_or(CharybdisError::ApertureCapacity)?;
        *slot = ApertureRecord {
            occupied: true,
            aperture,
            reserved_bytes: 0,
        };
        Ok(())
    }

    pub fn remove_aperture(
        &self,
        aperture_id: u32,
        _authority: &Capability<'_, PolicyControl>,
    ) -> Result<(), CharybdisError> {
        let mut state = self.state.lock();
        let slot = state
            .apertures
            .iter_mut()
            .find(|record| record.occupied && record.aperture.id == aperture_id)
            .ok_or(CharybdisError::ApertureNotFound)?;

        if slot.reserved_bytes != 0 {
            return Err(CharybdisError::ApertureBusy);
        }

        *slot = ApertureRecord::EMPTY;
        Ok(())
    }

    pub fn prepare(
        &self,
        intent: DmaIntent,
        now_tick: u64,
        _authority: &Capability<'_, DmaControl>,
    ) -> Result<DmaTicket, CharybdisError> {
        if APERTURES == 0 || MAPPINGS == 0 {
            return Err(CharybdisError::ZeroCapacity);
        }
        validate_intent(intent, now_tick)?;

        let mut state = self.state.lock();
        let mut matching_apertures = state
            .apertures
            .iter()
            .enumerate()
            .filter(|(_, record)| record.occupied && aperture_matches(record.aperture, intent));
        let aperture_slot = matching_apertures
            .next()
            .map(|(index, _)| index)
            .ok_or(CharybdisError::NoMatchingAperture)?;
        if matching_apertures.next().is_some() {
            state.rejected = state.rejected.saturating_add(1);
            return Err(CharybdisError::AmbiguousAperture);
        }
        if aperture_slot > u16::MAX as usize {
            return Err(CharybdisError::ApertureCapacity);
        }

        if state.mappings.iter().any(|record| {
            record.phase != RecordPhase::Empty
                && record.intent.domain == intent.domain
                && record.intent.device == intent.device
                && ranges_overlap(
                    record.intent.device_address,
                    record.intent.length,
                    intent.device_address,
                    intent.length,
                )
        }) {
            state.rejected = state.rejected.saturating_add(1);
            return Err(CharybdisError::AddressOverlap);
        }

        let aperture = state.apertures[aperture_slot];
        let new_reserved = aperture
            .reserved_bytes
            .checked_add(intent.length)
            .ok_or(CharybdisError::QuotaExceeded)?;
        if new_reserved > aperture.aperture.maximum_total_bytes {
            state.rejected = state.rejected.saturating_add(1);
            return Err(CharybdisError::QuotaExceeded);
        }

        let mapping_slot = state
            .mappings
            .iter()
            .position(|record| record.phase == RecordPhase::Empty)
            .ok_or(CharybdisError::MappingCapacity)?;
        if mapping_slot > u16::MAX as usize {
            return Err(CharybdisError::MappingCapacity);
        }

        let generation = state.mappings[mapping_slot]
            .generation
            .wrapping_add(1)
            .max(1);
        let tag = ticket_tag(self.secret, mapping_slot, generation, intent);

        state.mappings[mapping_slot] = MappingRecord {
            phase: RecordPhase::Prepared,
            generation,
            aperture_slot: aperture_slot as u16,
            intent,
            witness_mask: 0,
            backend_cookie: 0,
            tag,
        };
        state.apertures[aperture_slot].reserved_bytes = new_reserved;
        state.prepared = state.prepared.saturating_add(1);

        Ok(DmaTicket::new(mapping_slot as u16, generation, tag))
    }

    pub fn witness(&self, ticket: DmaTicket, witness: usize) -> Result<bool, CharybdisError> {
        if witness >= 64 {
            return Err(CharybdisError::WitnessOutOfRange);
        }

        let mut state = self.state.lock();
        let index = validate_ticket(&state, self.secret, ticket)?;
        let record = &mut state.mappings[index];

        if record.phase != RecordPhase::Prepared {
            return Err(CharybdisError::WrongPhase);
        }

        let bit = 1_u64 << witness;
        let inserted = record.witness_mask & bit == 0;
        record.witness_mask |= bit;
        Ok(inserted)
    }

    pub fn commit(
        &self,
        ticket: DmaTicket,
        now_tick: u64,
        backend: &dyn CharybdisBackend,
        _authority: &Capability<'_, DmaControl>,
    ) -> Result<MappingReceipt, CharybdisError> {
        let intent = {
            let mut state = self.state.lock();
            let index = validate_ticket(&state, self.secret, ticket)?;
            let record = state.mappings[index];

            if record.phase != RecordPhase::Prepared {
                return Err(CharybdisError::WrongPhase);
            }
            if now_tick >= record.intent.expires_at {
                release_reservation(&mut state, index);
                state.mappings[index].clear_preserving_generation();
                return Err(CharybdisError::Expired);
            }
            if record.witness_mask.count_ones() < u32::from(record.intent.witness_quorum) {
                return Err(CharybdisError::WitnessQuorum);
            }

            state.mappings[index].phase = RecordPhase::Mapping;
            record.intent
        };

        let backend_result = backend.map(intent);

        let mut state = self.state.lock();
        let index = validate_ticket(&state, self.secret, ticket)?;
        if state.mappings[index].phase != RecordPhase::Mapping {
            return Err(CharybdisError::WrongPhase);
        }

        match backend_result {
            Ok(cookie) if cookie != 0 => {
                state.mappings[index].phase = RecordPhase::Active;
                state.mappings[index].backend_cookie = cookie;
                state.committed = state.committed.saturating_add(1);
                let record = state.mappings[index];

                Ok(MappingReceipt {
                    ticket,
                    backend_cookie: cookie,
                    device_address: record.intent.device_address,
                    length: record.intent.length,
                    access: record.intent.access,
                    expires_at: record.intent.expires_at,
                    witness_mask: record.witness_mask,
                })
            }
            Ok(_) => {
                release_reservation(&mut state, index);
                state.mappings[index].clear_preserving_generation();
                state.rejected = state.rejected.saturating_add(1);
                Err(CharybdisError::InvalidBackendCookie)
            }
            Err(_) => {
                release_reservation(&mut state, index);
                state.mappings[index].clear_preserving_generation();
                state.rejected = state.rejected.saturating_add(1);
                Err(CharybdisError::BackendMap)
            }
        }
    }

    pub fn revoke(
        &self,
        ticket: DmaTicket,
        backend: &dyn CharybdisBackend,
        _authority: &Capability<'_, DmaControl>,
    ) -> Result<(), CharybdisError> {
        let (cookie, intent) = {
            let mut state = self.state.lock();
            let index = validate_ticket(&state, self.secret, ticket)?;
            let record = state.mappings[index];

            if record.phase == RecordPhase::Poisoned {
                return Err(CharybdisError::Poisoned);
            }
            if record.phase != RecordPhase::Active {
                return Err(CharybdisError::WrongPhase);
            }

            state.mappings[index].phase = RecordPhase::Revoking;
            (record.backend_cookie, record.intent)
        };

        let result = backend.unmap(cookie, intent);

        let mut state = self.state.lock();
        let index = validate_ticket(&state, self.secret, ticket)?;
        if state.mappings[index].phase != RecordPhase::Revoking {
            return Err(CharybdisError::WrongPhase);
        }

        match result {
            Ok(()) => {
                release_reservation(&mut state, index);
                state.mappings[index].clear_preserving_generation();
                state.revoked = state.revoked.saturating_add(1);
                Ok(())
            }
            Err(_) => {
                state.mappings[index].phase = RecordPhase::Poisoned;
                Err(CharybdisError::BackendUnmap)
            }
        }
    }

    pub fn abort_prepared(
        &self,
        ticket: DmaTicket,
        _authority: &Capability<'_, DmaControl>,
    ) -> Result<(), CharybdisError> {
        let mut state = self.state.lock();
        let index = validate_ticket(&state, self.secret, ticket)?;
        if state.mappings[index].phase != RecordPhase::Prepared {
            return Err(CharybdisError::WrongPhase);
        }

        release_reservation(&mut state, index);
        state.mappings[index].clear_preserving_generation();
        Ok(())
    }

    pub fn expire_one(
        &self,
        now_tick: u64,
        _authority: &Capability<'_, DmaControl>,
    ) -> Option<DmaTicket> {
        let mut state = self.state.lock();
        let index = state.mappings.iter().position(|record| {
            record.phase == RecordPhase::Prepared && now_tick >= record.intent.expires_at
        })?;

        let record = state.mappings[index];
        let ticket = DmaTicket::new(index as u16, record.generation, record.tag);
        release_reservation(&mut state, index);
        state.mappings[index].clear_preserving_generation();
        Some(ticket)
    }

    pub fn acknowledge_poison(
        &self,
        ticket: DmaTicket,
        _authority: &Capability<'_, FaultPolicyControl>,
    ) -> Result<DmaIntent, CharybdisError> {
        let mut state = self.state.lock();
        let index = validate_ticket(&state, self.secret, ticket)?;
        if state.mappings[index].phase != RecordPhase::Poisoned {
            return Err(CharybdisError::WrongPhase);
        }

        let intent = state.mappings[index].intent;
        release_reservation(&mut state, index);
        state.mappings[index].clear_preserving_generation();
        Ok(intent)
    }

    pub fn snapshot(&self, ticket: DmaTicket) -> Result<MappingSnapshot, CharybdisError> {
        let state = self.state.lock();
        let index = validate_ticket(&state, self.secret, ticket)?;
        let record = state.mappings[index];
        let phase = match record.phase {
            RecordPhase::Prepared => MappingPhase::Prepared,
            RecordPhase::Mapping => MappingPhase::Mapping,
            RecordPhase::Active => MappingPhase::Active,
            RecordPhase::Revoking => MappingPhase::Revoking,
            RecordPhase::Poisoned => MappingPhase::Poisoned,
            RecordPhase::Empty => return Err(CharybdisError::StaleTicket),
        };

        Ok(MappingSnapshot {
            ticket,
            phase,
            aperture_id: state.apertures[usize::from(record.aperture_slot)]
                .aperture
                .id,
            intent: record.intent,
            witness_mask: record.witness_mask,
            backend_cookie: record.backend_cookie,
        })
    }

    pub fn totals(&self) -> (u64, u64, u64, u64) {
        let state = self.state.lock();
        (
            state.prepared,
            state.committed,
            state.revoked,
            state.rejected,
        )
    }
}

fn validate_aperture(aperture: DmaAperture) -> Result<(), CharybdisError> {
    if aperture.id == 0
        || aperture.policy_epoch == 0
        || aperture.device_address_start >= aperture.device_address_end
        || aperture.physical_address_start >= aperture.physical_address_end
        || aperture.maximum_mapping_bytes == 0
        || aperture.maximum_total_bytes < aperture.maximum_mapping_bytes
        || !aperture.allowed_access.valid()
        || aperture.device_address_start % CHARYBDIS_PAGE_SIZE != 0
        || aperture.device_address_end % CHARYBDIS_PAGE_SIZE != 0
        || aperture.physical_address_start % CHARYBDIS_PAGE_SIZE != 0
        || aperture.physical_address_end % CHARYBDIS_PAGE_SIZE != 0
    {
        return Err(CharybdisError::InvalidAperture);
    }
    Ok(())
}

fn validate_intent(intent: DmaIntent, now_tick: u64) -> Result<(), CharybdisError> {
    if intent.domain == 0
        || intent.device == 0
        || intent.length == 0
        || intent.length % CHARYBDIS_PAGE_SIZE != 0
        || intent.device_address % CHARYBDIS_PAGE_SIZE != 0
        || intent.physical_address % CHARYBDIS_PAGE_SIZE != 0
        || intent.device_address.checked_add(intent.length).is_none()
        || intent.physical_address.checked_add(intent.length).is_none()
        || intent.expires_at <= now_tick
        || !intent.access.valid()
        || intent.witness_quorum == 0
        || intent.witness_quorum > 64
        || intent.policy_epoch == 0
    {
        return Err(CharybdisError::InvalidIntent);
    }
    Ok(())
}

fn aperture_matches(aperture: DmaAperture, intent: DmaIntent) -> bool {
    if aperture.policy_epoch != intent.policy_epoch
        || aperture.domain != intent.domain
        || intent.device & aperture.device_mask != aperture.device_value & aperture.device_mask
        || !aperture.allowed_access.contains(intent.access)
        || intent.length > aperture.maximum_mapping_bytes
    {
        return false;
    }

    let Some(device_end) = intent.device_address.checked_add(intent.length) else {
        return false;
    };
    let Some(physical_end) = intent.physical_address.checked_add(intent.length) else {
        return false;
    };

    intent.device_address >= aperture.device_address_start
        && device_end <= aperture.device_address_end
        && intent.physical_address >= aperture.physical_address_start
        && physical_end <= aperture.physical_address_end
}

fn validate_ticket<const A: usize, const M: usize>(
    state: &CharybdisState<A, M>,
    secret: u64,
    ticket: DmaTicket,
) -> Result<usize, CharybdisError> {
    if ticket == DmaTicket::INVALID {
        return Err(CharybdisError::InvalidTicket);
    }

    let index = ticket.slot();
    let record = state
        .mappings
        .get(index)
        .ok_or(CharybdisError::InvalidTicket)?;

    if record.phase == RecordPhase::Empty {
        return Err(CharybdisError::StaleTicket);
    }
    if record.generation != ticket.generation() {
        return Err(CharybdisError::StaleTicket);
    }

    let expected = ticket_tag(secret, index, record.generation, record.intent);
    if record.tag != expected || ticket.tag() != expected {
        return Err(CharybdisError::ForgedTicket);
    }

    Ok(index)
}

fn release_reservation<const A: usize, const M: usize>(
    state: &mut CharybdisState<A, M>,
    mapping_index: usize,
) {
    let record = state.mappings[mapping_index];
    if let Some(aperture) = state.apertures.get_mut(usize::from(record.aperture_slot)) {
        aperture.reserved_bytes = aperture.reserved_bytes.saturating_sub(record.intent.length);
    }
}

fn ranges_overlap(
    first_start: u64,
    first_length: u64,
    second_start: u64,
    second_length: u64,
) -> bool {
    let first_end = first_start.saturating_add(first_length);
    let second_end = second_start.saturating_add(second_length);
    first_start < second_end && second_start < first_end
}

fn ticket_tag(secret: u64, slot: usize, generation: u16, intent: DmaIntent) -> u32 {
    let mut state = mix(secret, slot as u64);
    state = mix(state, u64::from(generation));
    state = mix(state, intent.domain);
    state = mix(state, intent.device);
    state = mix(state, intent.device_address);
    state = mix(state, intent.physical_address);
    state = mix(state, intent.length);
    state = mix(state, intent.expires_at);
    state = mix(state, u64::from(intent.access.bits()));
    state = mix(state, u64::from(intent.witness_quorum));
    state = mix(state, u64::from(intent.purpose));
    state = mix(state, u64::from(intent.policy_epoch));
    (state ^ (state >> 32)) as u32
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
    use core::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::capability::Authority;

    struct TestBackend {
        next: AtomicU64,
    }

    impl CharybdisBackend for TestBackend {
        fn map(&self, _intent: DmaIntent) -> Result<u64, CharybdisError> {
            Ok(self.next.fetch_add(1, Ordering::Relaxed).max(1))
        }

        fn unmap(&self, backend_cookie: u64, _intent: DmaIntent) -> Result<(), CharybdisError> {
            if backend_cookie == 0 {
                Err(CharybdisError::BackendUnmap)
            } else {
                Ok(())
            }
        }
    }

    fn aperture() -> DmaAperture {
        DmaAperture {
            id: 1,
            policy_epoch: 7,
            domain: 9,
            device_mask: u64::MAX,
            device_value: 0x10de,
            device_address_start: 0x1000,
            device_address_end: 0x20_0000,
            physical_address_start: 0x1000_0000,
            physical_address_end: 0x1100_0000,
            maximum_mapping_bytes: 0x20_000,
            maximum_total_bytes: 0x80_000,
            allowed_access: DmaAccess::READ_WRITE,
        }
    }

    fn intent() -> DmaIntent {
        DmaIntent {
            domain: 9,
            device: 0x10de,
            device_address: 0x4000,
            physical_address: 0x1000_0000,
            length: 0x4000,
            expires_at: 100,
            access: DmaAccess::READ_WRITE,
            witness_quorum: 2,
            purpose: 1,
            policy_epoch: 7,
        }
    }

    #[test]
    fn requires_witness_quorum_before_mapping() {
        let authority = unsafe { Authority::assume_root() };
        let policy = authority.grant::<PolicyControl>();
        let dma = authority.grant::<DmaControl>();
        let firewall = CharybdisDmaFirewall::<2, 8>::new(0x1234);
        let backend = TestBackend {
            next: AtomicU64::new(1),
        };

        firewall.install_aperture(aperture(), &policy).unwrap();
        let ticket = firewall.prepare(intent(), 1, &dma).unwrap();
        firewall.witness(ticket, 0).unwrap();

        assert_eq!(
            firewall.commit(ticket, 2, &backend, &dma),
            Err(CharybdisError::WitnessQuorum)
        );

        firewall.witness(ticket, 1).unwrap();
        let receipt = firewall.commit(ticket, 2, &backend, &dma).unwrap();
        assert_eq!(receipt.witness_mask.count_ones(), 2);
        firewall.revoke(ticket, &backend, &dma).unwrap();
    }

    #[test]
    fn rejects_overlapping_device_ranges() {
        let authority = unsafe { Authority::assume_root() };
        let policy = authority.grant::<PolicyControl>();
        let dma = authority.grant::<DmaControl>();
        let firewall = CharybdisDmaFirewall::<2, 8>::new(0x5678);

        firewall.install_aperture(aperture(), &policy).unwrap();
        let _first = firewall.prepare(intent(), 1, &dma).unwrap();

        let mut second = intent();
        second.physical_address += 0x80_000;
        assert_eq!(
            firewall.prepare(second, 1, &dma),
            Err(CharybdisError::AddressOverlap)
        );
    }
}
