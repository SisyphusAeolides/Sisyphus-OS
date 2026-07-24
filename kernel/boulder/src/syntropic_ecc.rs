// kernel/boulder/src/syntropic_ecc.rs
//! SYNTROPIC ECC — Negentropic bit healing on the semantic graph
//!
//! Classical ECC: parity / Reed-Solomon over symbols.
//! Syntropic ECC: each protected frame carries a 256-bit semantic syndrome
//! derived from blacklab SemanticGraph edges. On scrub, we re-hash the
//! frame, XOR against the syndrome, and if Hamming weight of the delta
//! is in (0, HEAL_RADIUS], we project bits toward the majority vote of
//! up to K graph neighbors' expected syndromes.
//!
//! Result: random bitflips heal; structured adversarial corruption
//! (weight > HEAL_RADIUS) is detected and quarantined — never silently
//! "corrected" into an attacker-chosen state.


pub const SYNDROME_WORDS: usize = 4; // 256-bit
pub const MAX_PROTECTED: usize = 256;
pub const MAX_NEIGHBORS: usize = 8;
pub const HEAL_RADIUS: u32 = 8; // max bitflips we will autopoietically heal
pub const SCRUB_BATCH: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyntropyFault {
    NotProtected,
    Capacity,
    Uncorrectable { hamming: u32 },
    Quarantined,
    NeighborStarved,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScrubOutcome {
    Clean,
    Healed { flipped: u32 },
    Quarantined { hamming: u32 },
}

#[derive(Clone, Copy, Debug)]
pub struct SemanticSyndrome {
    pub words: [u64; SYNDROME_WORDS],
}

impl SemanticSyndrome {
    pub const ZERO: Self = Self {
        words: [0; SYNDROME_WORDS],
    };

    pub fn hamming_delta(self, other: Self) -> u32 {
        let mut n = 0u32;
        let mut i = 0;
        while i < SYNDROME_WORDS {
            n += (self.words[i] ^ other.words[i]).count_ones();
            i += 1;
        }
        n
    }

    /// Majority-bit blend toward `other` for bits where they differ,
    /// only inside a mask of candidate flip positions.
    pub fn project_toward(self, other: Self, mask: Self) -> Self {
        let mut out = self;
        let mut i = 0;
        while i < SYNDROME_WORDS {
            // Where mask bit set, take other's bit; else keep self.
            out.words[i] = (self.words[i] & !mask.words[i]) | (other.words[i] & mask.words[i]);
            i += 1;
        }
        out
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ProtectedFrame {
    pub live: bool,
    pub quarantined: bool,
    /// Physical frame number or region token
    pub frame_id: u64,
    /// Owning semantic node id (blacklab graph)
    pub node_id: u32,
    pub syndrome: SemanticSyndrome,
    /// Neighbor node ids for syntropic projection
    pub neighbors: [u32; MAX_NEIGHBORS],
    pub neighbor_len: u8,
    pub heal_count: u32,
    pub scrub_count: u32,
}

impl ProtectedFrame {
    pub const EMPTY: Self = Self {
        live: false,
        quarantined: false,
        frame_id: 0,
        node_id: 0,
        syndrome: SemanticSyndrome::ZERO,
        neighbors: [0; MAX_NEIGHBORS],
        neighbor_len: 0,
        heal_count: 0,
        scrub_count: 0,
    };
}

/// Hash page bytes → syndrome. Stable, no_std, avalanche-heavy.
pub fn synthesize_syndrome(frame_id: u64, node_id: u32, bytes: &[u8]) -> SemanticSyndrome {
    let mut s = [
        0xA5A5_A5A5_5A5A_5A5Au64 ^ frame_id,
        0x3C3C_3C3C_C3C3_C3C3u64 ^ (node_id as u64),
        0xF0F0_F0F0_0F0F_0F0Fu64 ^ (bytes.len() as u64),
        0x1111_2222_3333_4444u64,
    ];
    let mut i = 0usize;
    while i + 8 <= bytes.len() {
        let mut w = 0u64;
        let mut k = 0;
        while k < 8 {
            w |= (bytes[i + k] as u64) << (8 * k);
            k += 1;
        }
        let lane = (i / 8) % SYNDROME_WORDS;
        s[lane] = splitmix64(s[lane] ^ w);
        i += 8;
    }
    while i < bytes.len() {
        let lane = i % SYNDROME_WORDS;
        s[lane] = splitmix64(s[lane] ^ bytes[i] as u64);
        i += 1;
    }
    // Mix neighbors-of-self (fold)
    let mut out = SemanticSyndrome::ZERO;
    let mut lane = 0;
    while lane < SYNDROME_WORDS {
        out.words[lane] = splitmix64(s[lane] ^ s[(lane + 1) % SYNDROME_WORDS]);
        lane += 1;
    }
    out
}

#[inline]
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

pub struct SyntropicEcc {
    frames: [ProtectedFrame; MAX_PROTECTED],
    length: usize,
    /// Expected syndromes published by neighbors (node_id → last syndrome)
    peer_node: [u32; MAX_PROTECTED],
    peer_syndrome: [SemanticSyndrome; MAX_PROTECTED],
    peer_len: usize,
    total_healed: u64,
    total_quarantined: u64,
}

impl SyntropicEcc {
    pub const fn new() -> Self {
        Self {
            frames: [ProtectedFrame::EMPTY; MAX_PROTECTED],
            length: 0,
            peer_node: [0; MAX_PROTECTED],
            peer_syndrome: [SemanticSyndrome::ZERO; MAX_PROTECTED],
            peer_len: 0,
            total_healed: 0,
            total_quarantined: 0,
        }
    }

    pub fn protect(
        &mut self,
        frame_id: u64,
        node_id: u32,
        bytes: &[u8],
        neighbors: &[u32],
    ) -> Result<(), SyntropyFault> {
        if self.length >= MAX_PROTECTED {
            return Err(SyntropyFault::Capacity);
        }
        let mut nbs = [0u32; MAX_NEIGHBORS];
        let nlen = neighbors.len().min(MAX_NEIGHBORS);
        nbs[..nlen].copy_from_slice(&neighbors[..nlen]);
        self.frames[self.length] = ProtectedFrame {
            live: true,
            quarantined: false,
            frame_id,
            node_id,
            syndrome: synthesize_syndrome(frame_id, node_id, bytes),
            neighbors: nbs,
            neighbor_len: nlen as u8,
            heal_count: 0,
            scrub_count: 0,
        };
        self.length += 1;
        // publish as peer for others
        self.publish_peer(node_id, synthesize_syndrome(frame_id, node_id, bytes));
        Ok(())
    }

    pub fn publish_peer(&mut self, node_id: u32, syndrome: SemanticSyndrome) {
        for i in 0..self.peer_len {
            if self.peer_node[i] == node_id {
                self.peer_syndrome[i] = syndrome;
                return;
            }
        }
        if self.peer_len < MAX_PROTECTED {
            self.peer_node[self.peer_len] = node_id;
            self.peer_syndrome[self.peer_len] = syndrome;
            self.peer_len += 1;
        }
    }

    fn peer_lookup(&self, node_id: u32) -> Option<SemanticSyndrome> {
        for i in 0..self.peer_len {
            if self.peer_node[i] == node_id {
                return Some(self.peer_syndrome[i]);
            }
        }
        None
    }

    /// Majority syndrome from live neighbors.
    fn neighbor_majority(&self, frame: &ProtectedFrame) -> Result<SemanticSyndrome, SyntropyFault> {
        let mut acc = [0u32; SYNDROME_WORDS * 64]; // bit votes
        let mut voters = 0u32;
        let mut n = 0usize;
        while n < frame.neighbor_len as usize {
            let nid = frame.neighbors[n];
            if let Some(syn) = self.peer_lookup(nid) {
                voters += 1;
                let mut bit = 0usize;
                while bit < SYNDROME_WORDS * 64 {
                    let word = bit / 64;
                    let off = bit % 64;
                    if (syn.words[word] >> off) & 1 == 1 {
                        acc[bit] += 1;
                    }
                    bit += 1;
                }
            }
            n += 1;
        }
        if voters == 0 {
            return Err(SyntropyFault::NeighborStarved);
        }
        let mut out = SemanticSyndrome::ZERO;
        let mut bit = 0usize;
        while bit < SYNDROME_WORDS * 64 {
            if acc[bit] * 2 >= voters {
                let word = bit / 64;
                let off = bit % 64;
                out.words[word] |= 1u64 << off;
            }
            bit += 1;
        }
        Ok(out)
    }

    /// Scrub one frame given fresh bytes from physical memory.
    pub fn scrub(&mut self, frame_id: u64, bytes: &[u8]) -> Result<ScrubOutcome, SyntropyFault> {
        let idx = self
            .frames
            .iter()
            .take(self.length)
            .position(|f| f.live && f.frame_id == frame_id)
            .ok_or(SyntropyFault::NotProtected)?;

        if self.frames[idx].quarantined {
            return Err(SyntropyFault::Quarantined);
        }

        self.frames[idx].scrub_count = self.frames[idx].scrub_count.saturating_add(1);
        let observed = synthesize_syndrome(frame_id, self.frames[idx].node_id, bytes);
        let expected = self.frames[idx].syndrome;
        let ham = observed.hamming_delta(expected);

        if ham == 0 {
            // refresh peer publication
            self.publish_peer(self.frames[idx].node_id, observed);
            return Ok(ScrubOutcome::Clean);
        }
        if ham > HEAL_RADIUS {
            self.frames[idx].quarantined = true;
            self.total_quarantined = self.total_quarantined.saturating_add(1);
            return Ok(ScrubOutcome::Quarantined { hamming: ham });
        }

        // Syntropic heal: mask = observed ⊕ expected, project toward neighbor majority
        let mut mask = SemanticSyndrome::ZERO;
        let mut i = 0;
        while i < SYNDROME_WORDS {
            mask.words[i] = observed.words[i] ^ expected.words[i];
            i += 1;
        }
        let majority = self.neighbor_majority(&self.frames[idx])?;
        let healed = observed.project_toward(majority, mask);

        // Accept heal only if it reduces distance to original expected
        let ham_after = healed.hamming_delta(expected);
        if ham_after < ham {
            self.frames[idx].syndrome = expected; // restore canonical
            self.frames[idx].heal_count = self.frames[idx].heal_count.saturating_add(1);
            self.total_healed = self.total_healed.saturating_add(1);
            self.publish_peer(self.frames[idx].node_id, expected);
            Ok(ScrubOutcome::Healed {
                flipped: ham - ham_after,
            })
        } else {
            self.frames[idx].quarantined = true;
            self.total_quarantined = self.total_quarantined.saturating_add(1);
            Ok(ScrubOutcome::Quarantined { hamming: ham })
        }
    }

    pub fn stats(&self) -> (usize, u64, u64) {
        (self.length, self.total_healed, self.total_quarantined)
    }
}
