// kernel/boulder/src/bootstrap_chronal.rs
//! BOOTSTRAP CHRONAL LINKER — Novikov self-consistency for module load
//!
//! A module image may only become resident if the post-load universe
//! is consistent with a predeclared ContinuityVault hologram:
//!
//!   H_before = commit(expected_image_hash, expected_exports, expected_caps)
//!   load + relocate
//!   H_after  = measure(actual)
//!   require H_after == H_before   (Novikov fixed point)
//!
//! If Prometheus mutates calling convention thunks, those thunks are
//! included in the hologram so the CTC closes.
//!
//! Paradox → reject load, ghost chronicle PARADOX_REJECT, no partial state.

pub const MAX_PENDING: usize = 16;
pub const HASH_WORDS: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChronalFault {
    Capacity,
    Paradox,
    NotPending,
    ExportMismatch,
    CapMismatch,
    EmptyImage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImageHologram {
    pub image_hash: [u64; HASH_WORDS],
    pub export_hash: [u64; HASH_WORDS],
    pub cap_bits: u64,
    pub size_bytes: u32,
    pub personality: u64, // prometheus / hermes personality id
}

impl ImageHologram {
    pub const ZERO: Self = Self {
        image_hash: [0; HASH_WORDS],
        export_hash: [0; HASH_WORDS],
        cap_bits: 0,
        size_bytes: 0,
        personality: 0,
    };

    pub fn matches(self, other: Self) -> bool {
        self.image_hash == other.image_hash
            && self.export_hash == other.export_hash
            && self.cap_bits == other.cap_bits
            && self.size_bytes == other.size_bytes
            && self.personality == other.personality
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ChronalTicket {
    pub live: bool,
    pub id: u64,
    pub committed: ImageHologram,
    pub src_pid: u32,
}

impl ChronalTicket {
    pub const EMPTY: Self = Self {
        live: false,
        id: 0,
        committed: ImageHologram::ZERO,
        src_pid: 0,
    };
}

pub struct BootstrapChronal {
    pending: [ChronalTicket; MAX_PENDING],
    length: usize,
    next_id: u64,
    accepted: u64,
    paradoxes: u64,
}

impl BootstrapChronal {
    pub const fn new() -> Self {
        Self {
            pending: [ChronalTicket::EMPTY; MAX_PENDING],
            length: 0,
            next_id: 1,
            accepted: 0,
            paradoxes: 0,
        }
    }

    /// Precommit the future (close the CTC from the past end).
    pub fn precommit(
        &mut self,
        src_pid: u32,
        hologram: ImageHologram,
    ) -> Result<u64, ChronalFault> {
        if hologram.size_bytes == 0 {
            return Err(ChronalFault::EmptyImage);
        }
        if self.length >= MAX_PENDING {
            return Err(ChronalFault::Capacity);
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.pending[self.length] = ChronalTicket {
            live: true,
            id,
            committed: hologram,
            src_pid,
        };
        self.length += 1;
        Ok(id)
    }

    /// After load+relocate+prometheus thunk gen, measure and force fixed point.
    pub fn collapse(
        &mut self,
        ticket_id: u64,
        measured: ImageHologram,
    ) -> Result<(), ChronalFault> {
        let idx = self
            .pending
            .iter()
            .position(|t| t.live && t.id == ticket_id)
            .ok_or(ChronalFault::NotPending)?;
        let committed = self.pending[idx].committed;
        if !committed.matches(measured) {
            self.pending[idx].live = false;
            self.paradoxes = self.paradoxes.saturating_add(1);
            // compact
            self.pending[idx] = self.pending[self.length - 1];
            self.length -= 1;
            return Err(ChronalFault::Paradox);
        }
        self.pending[idx].live = false;
        self.pending[idx] = self.pending[self.length - 1];
        self.length -= 1;
        self.accepted = self.accepted.saturating_add(1);
        Ok(())
    }

    pub fn measure_image(
        bytes: &[u8],
        exports: &[u64],
        cap_bits: u64,
        personality: u64,
    ) -> ImageHologram {
        let mut image_hash = [0u64; HASH_WORDS];
        let mut s = 0xC0FF_EE00_D15C_AFEEu64;
        let mut i = 0usize;
        while i < bytes.len() {
            s = splitmix64(s ^ bytes[i] as u64);
            image_hash[i % HASH_WORDS] ^= s;
            i += 1;
        }
        let mut export_hash = [0u64; HASH_WORDS];
        let mut j = 0usize;
        while j < exports.len() {
            let w = splitmix64(exports[j] ^ 0xE8B0_97A5_5000_0000);
            export_hash[j % HASH_WORDS] ^= w;
            j += 1;
        }
        ImageHologram {
            image_hash,
            export_hash,
            cap_bits,
            size_bytes: bytes.len() as u32,
            personality,
        }
    }

    pub fn stats(&self) -> (u64, u64) {
        (self.accepted, self.paradoxes)
    }
}

#[inline]
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}
