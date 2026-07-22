use core::convert::TryFrom;

pub const NEXUS_WIRE_MAGIC: u32 = 0x4e58_5331; // NXS1
pub const NEXUS_WIRE_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum NexusOpcode {
    QueryStats = 1,
    QueryTelemetry = 3,

    AttachTask = 16,
    Entangle = 18,
    SetCollapseThreshold = 20,
    SetPriorityMass = 22,
    OfferKairos = 24,
}

impl TryFrom<u16> for NexusOpcode {
    type Error = WireError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::QueryStats),
            3 => Ok(Self::QueryTelemetry),
            16 => Ok(Self::AttachTask),
            18 => Ok(Self::Entangle),
            20 => Ok(Self::SetCollapseThreshold),
            22 => Ok(Self::SetPriorityMass),
            24 => Ok(Self::OfferKairos),
            _ => Err(WireError::UnknownOpcode),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum NexusStatus {
    Ok = 0,
    BadFrame = 1,
    Denied = 2,
    Expired = 3,
    InvalidArgument = 4,
    Capacity = 5,
    ThermalThrottle = 6,
    NotReady = 7,
    InternalFault = 8,
}

impl TryFrom<u16> for NexusStatus {
    type Error = WireError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Ok),
            1 => Ok(Self::BadFrame),
            2 => Ok(Self::Denied),
            3 => Ok(Self::Expired),
            4 => Ok(Self::InvalidArgument),
            5 => Ok(Self::Capacity),
            6 => Ok(Self::ThermalThrottle),
            7 => Ok(Self::NotReady),
            8 => Ok(Self::InternalFault),
            _ => Err(WireError::UnknownStatus),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WireError {
    BadMagic,
    BadVersion,
    BadChecksum,
    UnknownOpcode,
    UnknownStatus,
    SequenceMismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C, align(64))]
pub struct NexusCommand {
    pub magic: u32,
    pub version: u16,
    pub opcode: u16,

    pub sequence: u64,
    pub capability: u64,

    pub arguments: [u64; 4],
    pub checksum: u64,
}

const _: () = assert!(core::mem::size_of::<NexusCommand>() == 64);

impl NexusCommand {
    pub const ZERO: Self = Self {
        magic: 0,
        version: 0,
        opcode: 0,
        sequence: 0,
        capability: 0,
        arguments: [0; 4],
        checksum: 0,
    };

    pub fn new(
        opcode: NexusOpcode,
        sequence: u64,
        capability: u64,
        arguments: [u64; 4],
    ) -> Self {
        let mut command = Self {
            magic: NEXUS_WIRE_MAGIC,
            version: NEXUS_WIRE_VERSION,
            opcode: opcode as u16,
            sequence,
            capability,
            arguments,
            checksum: 0,
        };

        command.checksum = command.compute_checksum();
        command
    }

    pub fn validate(&self) -> Result<NexusOpcode, WireError> {
        if self.magic != NEXUS_WIRE_MAGIC {
            return Err(WireError::BadMagic);
        }

        if self.version != NEXUS_WIRE_VERSION {
            return Err(WireError::BadVersion);
        }

        if self.checksum != self.compute_checksum() {
            return Err(WireError::BadChecksum);
        }

        NexusOpcode::try_from(self.opcode)
    }

    fn compute_checksum(&self) -> u64 {
        let header = u64::from(self.magic)
            | (u64::from(self.version) << 32)
            | (u64::from(self.opcode) << 48);

        let mut digest = fold(0x6a09_e667_f3bc_c909, header);
        digest = fold(digest, self.sequence);
        digest = fold(digest, self.capability);

        for argument in self.arguments {
            digest = fold(digest, argument);
        }

        digest
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C, align(64))]
pub struct NexusReply {
    pub magic: u32,
    pub version: u16,
    pub status: u16,

    pub sequence: u64,
    pub kernel_tick: u64,

    pub generation: u32,
    pub payload_kind: u16,
    pub flags: u16,

    pub values: [u64; 3],
    pub checksum: u64,
}

const _: () = assert!(core::mem::size_of::<NexusReply>() == 64);

impl NexusReply {
    pub const ZERO: Self = Self {
        magic: 0,
        version: 0,
        status: 0,
        sequence: 0,
        kernel_tick: 0,
        generation: 0,
        payload_kind: 0,
        flags: 0,
        values: [0; 3],
        checksum: 0,
    };

    pub fn new(
        status: NexusStatus,
        sequence: u64,
        kernel_tick: u64,
        generation: u32,
        payload_kind: u16,
        values: [u64; 3],
    ) -> Self {
        let mut reply = Self {
            magic: NEXUS_WIRE_MAGIC,
            version: NEXUS_WIRE_VERSION,
            status: status as u16,
            sequence,
            kernel_tick,
            generation,
            payload_kind,
            flags: 0,
            values,
            checksum: 0,
        };

        reply.checksum = reply.compute_checksum();
        reply
    }

    pub fn validate(
        &self,
        expected_sequence: u64,
    ) -> Result<NexusStatus, WireError> {
        if self.magic != NEXUS_WIRE_MAGIC {
            return Err(WireError::BadMagic);
        }

        if self.version != NEXUS_WIRE_VERSION {
            return Err(WireError::BadVersion);
        }

        if self.sequence != expected_sequence {
            return Err(WireError::SequenceMismatch);
        }

        if self.checksum != self.compute_checksum() {
            return Err(WireError::BadChecksum);
        }

        NexusStatus::try_from(self.status)
    }

    fn compute_checksum(&self) -> u64 {
        let header = u64::from(self.magic)
            | (u64::from(self.version) << 32)
            | (u64::from(self.status) << 48);

        let metadata = u64::from(self.generation)
            | (u64::from(self.payload_kind) << 32)
            | (u64::from(self.flags) << 48);

        let mut digest = fold(0xbb67_ae85_84ca_a73b, header);
        digest = fold(digest, self.sequence);
        digest = fold(digest, self.kernel_tick);
        digest = fold(digest, metadata);

        for value in self.values {
            digest = fold(digest, value);
        }

        digest
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C, align(64))]
pub struct NexusTelemetry {
    pub magic: u32,
    pub version: u16,
    pub flags: u16,

    pub sequence: u64,
    pub logical_tick: u64,
    pub global_phase: u64,

    pub pairs_live: u32,
    pub generation: u32,

    pub heat: u64,
    pub collapses: u64,
    pub checksum: u64,
}

const _: () = assert!(core::mem::size_of::<NexusTelemetry>() == 64);

impl NexusTelemetry {
    pub const ZERO: Self = Self {
        magic: 0,
        version: 0,
        flags: 0,
        sequence: 0,
        logical_tick: 0,
        global_phase: 0,
        pairs_live: 0,
        generation: 0,
        heat: 0,
        collapses: 0,
        checksum: 0,
    };

    pub fn new(
        sequence: u64,
        logical_tick: u64,
        global_phase: u64,
        pairs_live: u32,
        generation: u32,
        heat: u64,
        collapses: u64,
    ) -> Self {
        let mut telemetry = Self {
            magic: NEXUS_WIRE_MAGIC,
            version: NEXUS_WIRE_VERSION,
            flags: 0,
            sequence,
            logical_tick,
            global_phase,
            pairs_live,
            generation,
            heat,
            collapses,
            checksum: 0,
        };

        telemetry.checksum = telemetry.compute_checksum();
        telemetry
    }

    pub fn validate(&self) -> Result<(), WireError> {
        if self.magic != NEXUS_WIRE_MAGIC {
            return Err(WireError::BadMagic);
        }

        if self.version != NEXUS_WIRE_VERSION {
            return Err(WireError::BadVersion);
        }

        if self.checksum != self.compute_checksum() {
            return Err(WireError::BadChecksum);
        }

        Ok(())
    }

    fn compute_checksum(&self) -> u64 {
        let header = u64::from(self.magic)
            | (u64::from(self.version) << 32)
            | (u64::from(self.flags) << 48);

        let counts =
            u64::from(self.pairs_live) | (u64::from(self.generation) << 32);

        let mut digest = fold(0x3c6e_f372_fe94_f82b, header);
        digest = fold(digest, self.sequence);
        digest = fold(digest, self.logical_tick);
        digest = fold(digest, self.global_phase);
        digest = fold(digest, counts);
        digest = fold(digest, self.heat);
        fold(digest, self.collapses)
    }
}

/// Corruption detector, not an authenticity primitive.
#[inline(always)]
fn fold(mut state: u64, value: u64) -> u64 {
    state ^= value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    state = state.rotate_left(27);
    state = state.wrapping_mul(0x94d0_49bb_1331_11eb);
    state ^ (state >> 31)
}
