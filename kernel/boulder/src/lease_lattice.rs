use crate::capability::{
    Capability, ResonanceRight,
};
use crate::sync::SpinLock;

const ROOT_PARENT: u16 = u16::MAX;
const MAXIMUM_DELEGATION_DEPTH: u8 = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct LeaseRights(u32);

impl LeaseRights {
    pub const OBSERVE: Self = Self(1 << 0);
    pub const SCHEDULE: Self = Self(1 << 1);
    pub const RESONANCE: Self = Self(1 << 2);
    pub const CONTROL: Self = Self(1 << 3);
    pub const DELEGATE: Self = Self(1 << 31);

    pub const ALL: Self = Self(
        Self::OBSERVE.0
            | Self::SCHEDULE.0
            | Self::RESONANCE.0
            | Self::CONTROL.0
            | Self::DELEGATE.0,
    );

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn contains(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }

    pub const fn intersect(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct LeaseToken(u64);

impl LeaseToken {
    pub const INVALID: Self = Self(0);

    pub const fn raw(self) -> u64 {
        self.0
    }

    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    const fn new(slot: u16, generation: u16, tag: u32) -> Self {
        Self(
            u64::from(slot)
                | (u64::from(generation) << 16)
                | (u64::from(tag) << 32),
        )
    }

    const fn slot(self) -> usize {
        (self.0 as u16) as usize
    }

    const fn generation(self) -> u16 {
        (self.0 >> 16) as u16
    }

    const fn tag(self) -> u32 {
        (self.0 >> 32) as u32
    }
}

#[derive(Clone, Copy)]
struct LeaseRecord {
    active: bool,
    generation: u16,
    depth: u8,
    reserved: u8,

    rights: LeaseRights,

    parent_slot: u16,
    parent_generation: u16,

    not_before: u64,
    expires_at: u64,

    quota_limit: u32,
    uses: u32,

    tag: u32,
}

impl LeaseRecord {
    const EMPTY: Self = Self {
        active: false,
        generation: 1,
        depth: 0,
        reserved: 0,
        rights: LeaseRights(0),
        parent_slot: ROOT_PARENT,
        parent_generation: 0,
        not_before: 0,
        expires_at: 0,
        quota_limit: 0,
        uses: 0,
        tag: 0,
    };
}

struct LeaseState<const N: usize> {
    records: [LeaseRecord; N],
    issuance_nonce: u64,
}

impl<const N: usize> LeaseState<N> {
    const fn new() -> Self {
        Self {
            records: [LeaseRecord::EMPTY; N],
            issuance_nonce: 1,
        }
    }
}

pub struct LeaseLattice<const N: usize> {
    secret: u64,
    state: SpinLock<LeaseState<N>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LeaseError {
    Capacity,
    Invalid,
    Forged,
    NotYetValid,
    Expired,
    Exhausted,
    MissingRight,
    CannotDelegate,
    RightsAmplification,
    InvalidLifetime,
    InvalidQuota,
    DelegationDepth,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LeaseAdmission {
    pub rights: LeaseRights,
    pub remaining_uses: u32,
    pub expires_at: u64,
    pub depth: u8,
}

impl<const N: usize> LeaseLattice<N> {
    pub const fn new(secret: u64) -> Self {
        Self {
            secret,
            state: SpinLock::new(LeaseState::new()),
        }
    }

    pub fn issue_root(
        &self,
        rights: LeaseRights,
        not_before: u64,
        expires_at: u64,
        quota: u32,
        _authority: &Capability<'_, ResonanceRight>,
    ) -> Result<LeaseToken, LeaseError> {
        validate_lifetime(not_before, expires_at)?;

        if quota == 0 {
            return Err(LeaseError::InvalidQuota);
        }

        let mut state = self.state.lock();

        let slot = state
            .records
            .iter()
            .position(|record| !record.active)
            .ok_or(LeaseError::Capacity)?;

        let generation =
            state.records[slot].generation.wrapping_add(1).max(1);

        let nonce = state.issuance_nonce;
        state.issuance_nonce =
            state.issuance_nonce.wrapping_add(1).max(1);

        let mut record = LeaseRecord {
            active: true,
            generation,
            depth: 0,
            reserved: 0,
            rights,
            parent_slot: ROOT_PARENT,
            parent_generation: 0,
            not_before,
            expires_at,
            quota_limit: quota,
            uses: 0,
            tag: 0,
        };

        record.tag = seal(self.secret, slot, record, nonce);
        state.records[slot] = record;

        Ok(LeaseToken::new(
            slot as u16,
            generation,
            record.tag,
        ))
    }

    pub fn attenuate(
        &self,
        parent_token: LeaseToken,
        rights: LeaseRights,
        expires_at: u64,
        quota: u32,
        now_tick: u64,
    ) -> Result<LeaseToken, LeaseError> {
        if quota == 0 {
            return Err(LeaseError::InvalidQuota);
        }

        let mut state = self.state.lock();
        let parent_index =
            validate_token(&state, self.secret, parent_token, now_tick)?;

        validate_ancestry(&state, parent_index)?;

        let parent = state.records[parent_index];

        if !parent.rights.contains(LeaseRights::DELEGATE) {
            return Err(LeaseError::CannotDelegate);
        }

        if parent.depth >= MAXIMUM_DELEGATION_DEPTH {
            return Err(LeaseError::DelegationDepth);
        }

        if !parent.rights.contains(rights) {
            return Err(LeaseError::RightsAmplification);
        }

        if expires_at > parent.expires_at || expires_at <= now_tick {
            return Err(LeaseError::InvalidLifetime);
        }

        let available =
            parent.quota_limit.saturating_sub(parent.uses);

        if quota > available {
            return Err(LeaseError::InvalidQuota);
        }

        let child_index = state
            .records
            .iter()
            .position(|record| !record.active)
            .ok_or(LeaseError::Capacity)?;

        let generation = state.records[child_index]
            .generation
            .wrapping_add(1)
            .max(1);

        let nonce = state.issuance_nonce;
        state.issuance_nonce =
            state.issuance_nonce.wrapping_add(1).max(1);

        state.records[parent_index].uses =
            state.records[parent_index]
                .uses
                .saturating_add(quota);

        let mut child = LeaseRecord {
            active: true,
            generation,
            depth: parent.depth.saturating_add(1),
            reserved: 0,
            rights: parent.rights.intersect(rights),
            parent_slot: parent_index as u16,
            parent_generation: parent.generation,
            not_before: now_tick,
            expires_at,
            quota_limit: quota,
            uses: 0,
            tag: 0,
        };

        child.tag =
            seal(self.secret, child_index, child, nonce);

        state.records[child_index] = child;

        Ok(LeaseToken::new(
            child_index as u16,
            generation,
            child.tag,
        ))
    }

    pub fn admit(
        &self,
        token: LeaseToken,
        required: LeaseRights,
        now_tick: u64,
    ) -> Result<LeaseAdmission, LeaseError> {
        let mut state = self.state.lock();

        let index =
            validate_token(&state, self.secret, token, now_tick)?;

        validate_ancestry(&state, index)?;

        let record = &mut state.records[index];

        if !record.rights.contains(required) {
            return Err(LeaseError::MissingRight);
        }

        if record.uses >= record.quota_limit {
            return Err(LeaseError::Exhausted);
        }

        record.uses = record.uses.saturating_add(1);

        Ok(LeaseAdmission {
            rights: record.rights,
            remaining_uses:
                record.quota_limit.saturating_sub(record.uses),
            expires_at: record.expires_at,
            depth: record.depth,
        })
    }

    pub fn revoke(
        &self,
        token: LeaseToken,
        _authority: &Capability<'_, ResonanceRight>,
    ) -> Result<(), LeaseError> {
        let mut state = self.state.lock();

        let index = validate_token_without_time(
            &state,
            self.secret,
            token,
        )?;

        let generation =
            state.records[index].generation.wrapping_add(1).max(1);

        state.records[index] = LeaseRecord {
            generation,
            ..LeaseRecord::EMPTY
        };

        Ok(())
    }
}

fn validate_lifetime(
    not_before: u64,
    expires_at: u64,
) -> Result<(), LeaseError> {
    if expires_at == 0 || expires_at <= not_before {
        Err(LeaseError::InvalidLifetime)
    } else {
        Ok(())
    }
}

fn validate_token<const N: usize>(
    state: &LeaseState<N>,
    secret: u64,
    token: LeaseToken,
    now_tick: u64,
) -> Result<usize, LeaseError> {
    let index =
        validate_token_without_time(state, secret, token)?;

    let record = state.records[index];

    if now_tick < record.not_before {
        return Err(LeaseError::NotYetValid);
    }

    if now_tick > record.expires_at {
        return Err(LeaseError::Expired);
    }

    Ok(index)
}

fn validate_token_without_time<const N: usize>(
    state: &LeaseState<N>,
    secret: u64,
    token: LeaseToken,
) -> Result<usize, LeaseError> {
    let index = token.slot();
    let record = state.records.get(index).ok_or(LeaseError::Invalid)?;

    if !record.active || record.generation != token.generation() {
        return Err(LeaseError::Invalid);
    }

    if record.tag != token.tag() {
        return Err(LeaseError::Forged);
    }

    // The table is authoritative. The keyed tag is an additional fabrication
    // barrier, not a replacement for the table lookup.
    let _ = secret;

    Ok(index)
}

fn validate_ancestry<const N: usize>(
    state: &LeaseState<N>,
    start: usize,
) -> Result<(), LeaseError> {
    let mut current = start;
    let mut traversed = 0_u8;

    loop {
        let record = state.records[current];

        if record.parent_slot == ROOT_PARENT {
            return Ok(());
        }

        traversed = traversed.saturating_add(1);

        if traversed > MAXIMUM_DELEGATION_DEPTH {
            return Err(LeaseError::DelegationDepth);
        }

        let parent_index = usize::from(record.parent_slot);
        let parent = state
            .records
            .get(parent_index)
            .ok_or(LeaseError::Invalid)?;

        if !parent.active
            || parent.generation != record.parent_generation
        {
            return Err(LeaseError::Invalid);
        }

        current = parent_index;
    }
}

fn seal(
    secret: u64,
    slot: usize,
    record: LeaseRecord,
    nonce: u64,
) -> u32 {
    let mut state = secret ^ nonce.rotate_left(17);

    state = mix(state, slot as u64);
    state = mix(state, u64::from(record.generation));
    state = mix(state, u64::from(record.rights.bits()));
    state = mix(state, record.not_before);
    state = mix(state, record.expires_at);
    state = mix(state, u64::from(record.quota_limit));
    state = mix(
        state,
        u64::from(record.parent_slot)
            | (u64::from(record.parent_generation) << 16),
    );

    (state ^ (state >> 32)) as u32
}

fn mix(mut state: u64, value: u64) -> u64 {
    state ^= value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    state = state.rotate_left(29);
    state = state.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    state ^ (state >> 31)
}
