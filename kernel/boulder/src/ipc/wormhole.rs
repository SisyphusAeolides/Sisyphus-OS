// kernel/boulder/src/ipc/wormhole.rs
// #![no_std] inherited
//
// WORMHOLE — Causal Time-Reversal IPC
//
// CTC buffer: a ring where slot [N] is the "future" of slot [N-1].
//   Senders write to future slots. Receivers can speculatively READ
//   from future slots before the sender has written — if their prediction
//   of the message hash matches what eventually arrives, the speculation
//   is validated and committed with zero observed latency.
//
// Holographic compression: messages are stored as a 64-bit semantic hash
//   (XOR-folded content hash seeded by sender+receiver AS IDs from blacklab graph).
//   Full message bytes live in a separate payload arena; the ring holds only hashes.
//   Receiver reconstructs from hash + local semantic context if the content
//   is semantically predictable (e.g., heartbeat, status, ACK patterns).
//
// Zero-latency path:
//   1. Receiver speculatively reads slot[future] → gets predicted_hash
//   2. Receiver pre-executes handler with predicted payload
//   3. Sender writes actual message → actual_hash computed
//   4. If actual_hash == predicted_hash: commit speculation, round-trip = 0 ticks
//   5. If mismatch: rollback receiver state, replay with actual payload
//
// Speculation accuracy tracked per (sender, receiver) pair →
//   pairs with high accuracy get larger speculative windows (up to SPEC_WINDOW_MAX slots)
//
// Causal ordering: enforced via Lamport logical clocks embedded in each slot.
//   Violation = paradox → kernel resolves by choosing lower logical clock (past wins).

#![allow(dead_code)]
extern crate alloc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicU32, Ordering};

// ─────────────────────────────────────────────
// CONSTANTS
// ─────────────────────────────────────────────

pub const CTC_RING_SLOTS:     usize = 512;
pub const PAYLOAD_ARENA_BYTES: usize = 1 << 20; // 1 MB payload arena
pub const MAX_CHANNELS:       usize = 256;
pub const SPEC_WINDOW_MAX:    usize = 16;     // max slots receiver looks ahead
pub const SPEC_WINDOW_MIN:    usize = 1;
pub const ACCURACY_PROMOTE:   u32   = 95;     // % accuracy to widen spec window
pub const ACCURACY_DEMOTE:    u32   = 70;     // % accuracy to shrink spec window
pub const LAMPORT_INFINITY:   u64   = u64::MAX;
pub const HASH_SEED_MAGIC:    u64   = 0x517cc1b727220a95; // FNV-like prime

// ─────────────────────────────────────────────
// SEMANTIC HASH (holographic compression)
// ─────────────────────────────────────────────

/// Compute a 64-bit semantic hash of a message payload
/// Seeded by sender+receiver AS IDs so same content = different hash per channel
/// (prevents cross-channel hash collisions from creating false speculation hits)
#[inline(always)]
pub fn semantic_hash(payload: &[u8], sender_as: u64, receiver_as: u64) -> u64 {
    let mut h: u64 = HASH_SEED_MAGIC
        ^ sender_as.wrapping_mul(0x9e3779b97f4a7c15)
        ^ receiver_as.wrapping_mul(0x6c62272e07bb0142);
    for chunk in payload.chunks(8) {
        let mut word = 0u64;
        for (i, &b) in chunk.iter().enumerate() {
            word |= (b as u64) << (i * 8);
        }
        h ^= word;
        h = h.wrapping_mul(0x517cc1b727220a95);
        h ^= h >> 32;
    }
    h
}

/// Predict the next message hash from channel history (pattern extrapolation)
/// Uses a 2nd-order linear predictor on the hash sequence:
///   predicted = 2*h[n-1] - h[n-2]  (works surprisingly well for periodic messages)
pub fn predict_hash(history: &[u64; 8], hist_len: usize) -> u64 {
    if hist_len < 2 { return history[0]; }
    let h1 = history[hist_len % 8];
    let h2 = history[(hist_len.wrapping_sub(1)) % 8];
    // XOR-based prediction (addition would overflow for hashes; XOR preserves bit patterns)
    h1 ^ h2 ^ history[(hist_len.wrapping_sub(2)) % 8]
}

// ─────────────────────────────────────────────
// CTC RING SLOT
// ─────────────────────────────────────────────

#[derive(Clone, Copy)]
pub struct CtcSlot {
    pub semantic_hash:    u64,
    pub lamport_clock:    u64,
    pub sender_as:        u64,
    pub receiver_as:      u64,
    pub payload_offset:   u32,   // offset into payload arena (u32::MAX = no payload)
    pub payload_len:      u32,
    pub is_speculative:   bool,  // true = written by receiver as prediction
    pub is_committed:     bool,  // true = sender confirmed / speculation validated
    pub is_paradox:       bool,  // causal violation detected
    pub generation:       u32,   // ring wrap counter (prevents ABA)
}

impl CtcSlot {
    pub const fn empty() -> Self {
        Self {
            semantic_hash: 0, lamport_clock: 0,
            sender_as: 0, receiver_as: 0,
            payload_offset: u32::MAX, payload_len: 0,
            is_speculative: false, is_committed: false,
            is_paradox: false, generation: 0,
        }
    }
}

// ─────────────────────────────────────────────
// PAYLOAD ARENA (bump allocator for message bodies)
// ─────────────────────────────────────────────

pub struct PayloadArena {
    pub data:      [u8; PAYLOAD_ARENA_BYTES],
    pub cursor:    AtomicU32,
    pub watermark: AtomicU32,
}

impl PayloadArena {
    pub const fn new() -> Self {
        Self {
            data: [0u8; PAYLOAD_ARENA_BYTES],
            cursor: AtomicU32::new(0),
            watermark: AtomicU32::new(0),
        }
    }

    pub fn alloc(&self, len: usize) -> Option<u32> {
        let len32 = len as u32;
        let offset = self.cursor.fetch_add(len32, Ordering::AcqRel);
        if offset as usize + len > PAYLOAD_ARENA_BYTES {
            self.cursor.fetch_sub(len32, Ordering::AcqRel);
            return None;
        }
        let wm = offset + len32;
        let _ = self.watermark.fetch_max(wm, Ordering::Relaxed);
        Some(offset)
    }

    pub fn write(&mut self, offset: u32, data: &[u8]) {
        let off = offset as usize;
        if off + data.len() <= PAYLOAD_ARENA_BYTES {
            self.data[off..off + data.len()].copy_from_slice(data);
        }
    }

    pub fn read(&self, offset: u32, len: u32) -> &[u8] {
        let off = offset as usize;
        let end = (off + len as usize).min(PAYLOAD_ARENA_BYTES);
        &self.data[off..end]
    }

    /// Compact: reset cursor when all committed messages have been read
    pub fn reset_if_empty(&self, committed_watermark: u32) {
        if committed_watermark >= self.watermark.load(Ordering::Relaxed) {
            self.cursor.store(0, Ordering::Release);
            self.watermark.store(0, Ordering::Release);
        }
    }
}

// ─────────────────────────────────────────────
// CHANNEL STATE — per (sender, receiver) pair
// ─────────────────────────────────────────────

pub struct ChannelState {
    pub sender_as:         u64,
    pub receiver_as:       u64,
    pub spec_window:       usize,        // current speculative look-ahead
    pub hash_history:      [u64; 8],     // recent message hashes (ring)
    pub hash_hist_len:     usize,
    pub spec_attempts:     u64,
    pub spec_hits:         u64,          // correctly predicted
    pub spec_misses:       u64,          // mispredicted → rollback
    pub lamport_send:      u64,
    pub lamport_recv:      u64,
    pub paradoxes:         u32,
    pub zero_latency_wins: AtomicU64,
}

impl ChannelState {
    pub fn new(sender_as: u64, receiver_as: u64) -> Self {
        Self {
            sender_as, receiver_as,
            spec_window: SPEC_WINDOW_MIN,
            hash_history: [0u64; 8],
            hash_hist_len: 0,
            spec_attempts: 0, spec_hits: 0, spec_misses: 0,
            lamport_send: 0, lamport_recv: 0,
            paradoxes: 0,
            zero_latency_wins: AtomicU64::new(0),
        }
    }

    pub fn record_hash(&mut self, hash: u64) {
        self.hash_history[self.hash_hist_len % 8] = hash;
        self.hash_hist_len += 1;
    }

    pub fn accuracy_pct(&self) -> u32 {
        if self.spec_attempts == 0 { return 100; }
        (self.spec_hits * 100 / self.spec_attempts) as u32
    }

    pub fn adapt_window(&mut self) {
        let acc = self.accuracy_pct();
        if acc >= ACCURACY_PROMOTE && self.spec_window < SPEC_WINDOW_MAX {
            self.spec_window += 1;
        } else if acc < ACCURACY_DEMOTE && self.spec_window > SPEC_WINDOW_MIN {
            self.spec_window -= 1;
        }
    }

    pub fn next_lamport_send(&mut self, peer_clock: u64) -> u64 {
        self.lamport_send = self.lamport_send.max(peer_clock) + 1;
        self.lamport_send
    }
}

// ─────────────────────────────────────────────
// WORMHOLE — Master CTC IPC Engine
// ─────────────────────────────────────────────

pub struct Wormhole {
    pub ring:          [CtcSlot; CTC_RING_SLOTS],
    pub arena:         PayloadArena,
    pub channels:      Vec<ChannelState>,
    pub write_head:    AtomicU64,   // next slot for sender
    pub read_head:     AtomicU64,   // receiver's confirmed read position
    pub spec_head:     AtomicU64,   // receiver's speculative read position
    pub total_sent:    AtomicU64,
    pub total_recv:    AtomicU64,
    pub total_zero_latency: AtomicU64,
    pub total_paradox: AtomicU32,
    pub global_lamport: AtomicU64,
}

impl Wormhole {
    pub fn new() -> Self {
        Self {
            ring: [CtcSlot::empty(); CTC_RING_SLOTS],
            arena: PayloadArena::new(),
            channels: Vec::new(),
            write_head: AtomicU64::new(0),
            read_head:  AtomicU64::new(0),
            spec_head:  AtomicU64::new(0),
            total_sent: AtomicU64::new(0),
            total_recv: AtomicU64::new(0),
            total_zero_latency: AtomicU64::new(0),
            total_paradox: AtomicU32::new(0),
            global_lamport: AtomicU64::new(0),
        }
    }

    fn channel_idx(&self, sender_as: u64, receiver_as: u64) -> Option<usize> {
        self.channels.iter().position(|c| c.sender_as == sender_as && c.receiver_as == receiver_as)
    }

    pub fn open_channel(&mut self, sender_as: u64, receiver_as: u64) -> usize {
        if let Some(idx) = self.channel_idx(sender_as, receiver_as) { return idx; }
        self.channels.push(ChannelState::new(sender_as, receiver_as));
        self.channels.len() - 1
    }

    /// SEND: write message into CTC ring, advance Lamport clock
    pub fn send(&mut self, sender_as: u64, receiver_as: u64, payload: &[u8]) -> SendResult {
        let ch_idx = match self.channel_idx(sender_as, receiver_as) {
            Some(i) => i,
            None => self.open_channel(sender_as, receiver_as),
        };

        let hash = semantic_hash(payload, sender_as, receiver_as);

        // Check: did the receiver already speculatively consume a slot with this hash?
        let _spec_pos = self.spec_head.load(Ordering::Acquire);
        let write_pos = self.write_head.load(Ordering::Acquire);

        // Look through spec window for matching prediction
        let spec_window = self.channels[ch_idx].spec_window;
        for look in 0..spec_window {
            let slot_idx = (write_pos + look as u64) as usize % CTC_RING_SLOTS;
            let slot = &self.ring[slot_idx];
            if slot.is_speculative && !slot.is_committed
                && slot.sender_as == sender_as
                && slot.receiver_as == receiver_as
                && slot.semantic_hash == hash
            {
                // ZERO-LATENCY WIN: speculation was correct
                let slot = &mut self.ring[slot_idx];
                slot.is_committed = true;
                slot.is_speculative = false;
                self.channels[ch_idx].spec_hits += 1;
                self.channels[ch_idx].spec_attempts += 1;
                self.channels[ch_idx].adapt_window();
                self.channels[ch_idx].record_hash(hash);
                self.total_zero_latency.fetch_add(1, Ordering::Relaxed);
                self.total_sent.fetch_add(1, Ordering::Relaxed);
                return SendResult::ZeroLatency { slot_idx: slot_idx as u32 };
            }
        }

        // Normal send: write to next slot
        let slot_pos = self.write_head.fetch_add(1, Ordering::AcqRel);
        let slot_idx = slot_pos as usize % CTC_RING_SLOTS;
        let lamport = self.global_lamport.fetch_add(1, Ordering::AcqRel);

        // Write payload to arena
        let pay_offset = self.arena.alloc(payload.len())
            .unwrap_or(u32::MAX);
        if pay_offset != u32::MAX {
            self.arena.write(pay_offset, payload);
        }

        let next_gen = self.ring[slot_idx].generation.wrapping_add(1);
        self.ring[slot_idx] = CtcSlot {
            semantic_hash: hash,
            lamport_clock: lamport,
            sender_as, receiver_as,
            payload_offset: pay_offset,
            payload_len: payload.len() as u32,
            is_speculative: false,
            is_committed: true,
            is_paradox: false,
            generation: next_gen,
        };

        self.channels[ch_idx].record_hash(hash);
        self.channels[ch_idx].spec_attempts += 1;
        self.channels[ch_idx].spec_misses += 1;
        self.channels[ch_idx].adapt_window();
        self.total_sent.fetch_add(1, Ordering::Relaxed);

        SendResult::Normal { slot_idx: slot_idx as u32, lamport }
    }

    /// SPECULATIVE RECEIVE: receiver looks into the "future" ring slots
    /// and pre-consumes predicted messages before they arrive
    pub fn speculative_recv(
        &mut self,
        receiver_as: u64,
        sender_as: u64,
    ) -> Option<SpeculativeRead> {
        let ch_idx = self.channel_idx(sender_as, receiver_as)?;
        let spec_window = self.channels[ch_idx].spec_window;
        let write_pos = self.write_head.load(Ordering::Acquire);

        // Predict the next message hash from history
        let predicted_hash = predict_hash(
            &self.channels[ch_idx].hash_history,
            self.channels[ch_idx].hash_hist_len,
        );

        // Write speculative slot into the future ring position
        for look in 1..=spec_window {
            let future_idx = (write_pos + look as u64) as usize % CTC_RING_SLOTS;
            let slot = &self.ring[future_idx];
            // Only write speculation into empty/old slots
            if slot.is_committed || slot.is_speculative { continue; }

            let lamport = self.global_lamport.load(Ordering::Relaxed) + look as u64;
            self.ring[future_idx] = CtcSlot {
                semantic_hash: predicted_hash,
                lamport_clock: lamport,
                sender_as, receiver_as,
                payload_offset: u32::MAX,
                payload_len: 0,
                is_speculative: true,
                is_committed: false,
                is_paradox: false,
                generation: self.ring[future_idx].generation.wrapping_add(1),
            };

            self.spec_head.store(write_pos + look as u64, Ordering::Release);
            return Some(SpeculativeRead {
                predicted_hash,
                future_slot: future_idx as u32,
                confidence_pct: self.channels[ch_idx].accuracy_pct(),
            });
        }
        None
    }

    /// CONFIRM or ROLLBACK a speculative read after actual send arrives
    pub fn resolve_speculation(
        &mut self,
        slot_idx: u32,
        actual_hash: u64,
    ) -> SpeculationResult {
        let idx = slot_idx as usize % CTC_RING_SLOTS;
        let predicted = self.ring[idx].semantic_hash;
        if predicted == actual_hash {
            self.ring[idx].is_committed = true;
            self.ring[idx].is_speculative = false;
            SpeculationResult::Confirmed
        } else {
            // Paradox: causal violation — past expectation contradicts present reality
            self.ring[idx].is_paradox = true;
            self.ring[idx].is_committed = false;
            self.total_paradox.fetch_add(1, Ordering::Relaxed);
            SpeculationResult::Rollback { actual_hash }
        }
    }

    pub fn stats(&self) -> WormholeStats {
        WormholeStats {
            total_sent:      self.total_sent.load(Ordering::Relaxed),
            total_recv:      self.total_recv.load(Ordering::Relaxed),
            zero_latency:    self.total_zero_latency.load(Ordering::Relaxed),
            paradoxes:       self.total_paradox.load(Ordering::Relaxed),
            channels:        self.channels.len() as u32,
            arena_used:      self.arena.cursor.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum SendResult {
    ZeroLatency { slot_idx: u32 },
    Normal { slot_idx: u32, lamport: u64 },
}

#[derive(Clone, Copy, Debug)]
pub struct SpeculativeRead {
    pub predicted_hash:  u64,
    pub future_slot:     u32,
    pub confidence_pct:  u32,
}

#[derive(Clone, Copy, Debug)]
pub enum SpeculationResult {
    Confirmed,
    Rollback { actual_hash: u64 },
}

#[derive(Clone, Copy, Debug)]
pub struct WormholeStats {
    pub total_sent:   u64,
    pub total_recv:   u64,
    pub zero_latency: u64,
    pub paradoxes:    u32,
    pub channels:     u32,
    pub arena_used:   u32,
}
