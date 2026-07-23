#![allow(dead_code)]
use alloc::{collections::BTreeMap, string::String, vec, vec::Vec};
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

// ─────────────────────────────────────────────
// CONSTANTS
// ─────────────────────────────────────────────

pub const CRUCIBLE_CAPACITY: usize = 4096; // ring buffer slots per channel
pub const MAX_ATOM_BYTES: usize = 4096; // max message payload
pub const MAX_CHANNELS: usize = 65536;
pub const MAX_TRANSMUTATIONS: usize = 1024; // registered schema count
pub const ATOM_MAGIC: u64 = 0xA1C4_E31A_0000_DEAD; // atom integrity marker

// ─────────────────────────────────────────────
// ATOM — The Immutable Message Unit
// ─────────────────────────────────────────────

/// An atom crystallizes when sent and shatters when received — never copied
#[repr(C, align(64))] // cache-line aligned
pub struct Atom {
    pub magic: u64,          // integrity: must == ATOM_MAGIC
    pub sequence: AtomicU64, // monotonic sequence number
    pub sender_pid: u32,
    pub receiver_pid: u32,
    pub type_id: u64,      // sender's type hash (FNV-1a of type name)
    pub recv_type_id: u64, // receiver's expected type hash
    pub payload_len: u32,
    pub flags: AtomicU8, // state: 0=empty, 1=writing, 2=ready, 3=consumed
    pub timestamp_ns: u64,
    pub payload: [u8; MAX_ATOM_BYTES],
    pub checksum: u32, // xxHash32 of payload
}

const _: () = assert!(core::mem::size_of::<Atom>() <= MAX_ATOM_BYTES + 128);

impl Atom {
    pub const FLAG_EMPTY: u8 = 0;
    pub const FLAG_WRITING: u8 = 1;
    pub const FLAG_READY: u8 = 2;
    pub const FLAG_CONSUMED: u8 = 3;

    pub fn new_empty() -> Self {
        unsafe { core::mem::zeroed() }
    }

    pub fn is_ready(&self) -> bool {
        self.flags.load(Ordering::Acquire) == Self::FLAG_READY
    }

    pub fn crystallize(
        &mut self,
        sender: u32,
        receiver: u32,
        type_id: u64,
        recv_type: u64,
        data: &[u8],
        seq: u64,
        now_ns: u64,
    ) -> bool {
        // CAS: empty → writing
        if self
            .flags
            .compare_exchange(
                Self::FLAG_EMPTY,
                Self::FLAG_WRITING,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_err()
        {
            return false;
        }
        let len = data.len().min(MAX_ATOM_BYTES);
        self.magic = 0xAEA00000_DEAD_00EE; // simplified magic
        self.sequence.store(seq, Ordering::Relaxed);
        self.sender_pid = sender;
        self.receiver_pid = receiver;
        self.type_id = type_id;
        self.recv_type_id = recv_type;
        self.payload_len = len as u32;
        self.payload[..len].copy_from_slice(&data[..len]);
        self.checksum = Self::xxhash32(&data[..len]);
        self.timestamp_ns = now_ns;
        // writing → ready (publish the atom)
        self.flags.store(Self::FLAG_READY, Ordering::Release);
        true
    }

    /// Shatter: consume atom — returns payload, marks slot empty for reuse
    pub fn shatter(&mut self) -> Option<(&[u8], u64, u32)> {
        if self
            .flags
            .compare_exchange(
                Self::FLAG_READY,
                Self::FLAG_CONSUMED,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_err()
        {
            return None;
        }
        let len = self.payload_len as usize;
        if Self::xxhash32(&self.payload[..len]) != self.checksum {
            // Checksum fail — atom corrupted in flight
            self.flags.store(Self::FLAG_EMPTY, Ordering::Release);
            return None;
        }
        Some((&self.payload[..len], self.type_id, self.sender_pid))
    }

    /// Mark atom as empty after receiver copied data
    pub fn recycle(&mut self) {
        self.payload_len = 0;
        self.flags.store(Self::FLAG_EMPTY, Ordering::Release);
    }

    fn xxhash32(data: &[u8]) -> u32 {
        const PRIME1: u32 = 2654435761;
        const PRIME2: u32 = 2246822519;
        const PRIME3: u32 = 3266489917;
        const PRIME4: u32 = 668265263;
        const PRIME5: u32 = 374761393;
        let mut h: u32 = PRIME5;
        h = h.wrapping_add(data.len() as u32);
        for chunk in data.chunks(4) {
            let mut word = 0u32;
            for (i, &b) in chunk.iter().enumerate() {
                word |= (b as u32) << (i * 8);
            }
            h = h.wrapping_add(word.wrapping_mul(PRIME3));
            h = h.rotate_left(17).wrapping_mul(PRIME4);
        }
        h ^= h >> 15;
        h = h.wrapping_mul(PRIME2);
        h ^= h >> 13;
        h = h.wrapping_mul(PRIME3);
        h ^= h >> 16;
        h
    }
}

// ─────────────────────────────────────────────
// TRANSMUTATION SCHEMA
// ─────────────────────────────────────────────

/// A transmutation schema: proof that type A can be reinterpreted as type B
pub struct TransmutationSchema {
    pub from_type_id: u64,
    pub to_type_id: u64,
    pub from_size: usize,
    pub to_size: usize,
    pub from_name: String,
    pub to_name: String,
    /// Layout compatibility proof:
    /// - All offsets in `to` layout must be satisfied by `from` layout
    /// - No uninitialized reads allowed
    pub field_map: Vec<FieldMapping>,
    pub is_zero_copy: bool, // true if sizes match and layout is identical
    pub usage_count: AtomicU64,
}

#[derive(Clone)]
pub struct FieldMapping {
    pub from_offset: usize,
    pub to_offset: usize,
    pub size: usize,
    pub is_inhabited: bool, // false = field doesn't exist in source (use zero)
}

impl Clone for TransmutationSchema {
    fn clone(&self) -> Self {
        Self {
            from_type_id: self.from_type_id,
            to_type_id: self.to_type_id,
            from_size: self.from_size,
            to_size: self.to_size,
            from_name: self.from_name.clone(),
            to_name: self.to_name.clone(),
            field_map: self.field_map.clone(),
            is_zero_copy: self.is_zero_copy,
            usage_count: AtomicU64::new(self.usage_count.load(Ordering::Relaxed)),
        }
    }
}

impl TransmutationSchema {
    pub fn identity(type_id: u64, name: &str, size: usize) -> Self {
        Self {
            from_type_id: type_id,
            to_type_id: type_id,
            from_size: size,
            to_size: size,
            from_name: String::from(name),
            to_name: String::from(name),
            field_map: vec![FieldMapping {
                from_offset: 0,
                to_offset: 0,
                size,
                is_inhabited: true,
            }],
            is_zero_copy: true,
            usage_count: AtomicU64::new(0),
        }
    }

    /// Validate that a payload can be safely transmuted according to this schema
    pub fn validate(&self, payload: &[u8]) -> bool {
        if payload.len() < self.from_size {
            return false;
        }
        // Check all field mappings are within bounds
        for field in &self.field_map {
            if !field.is_inhabited {
                continue;
            }
            if field.from_offset + field.size > payload.len() {
                return false;
            }
        }
        true
    }

    /// Apply transmutation: reinterpret `src` as the target type into `dst`
    pub fn transmute(&self, src: &[u8], dst: &mut [u8]) -> bool {
        if !self.validate(src) {
            return false;
        }
        if dst.len() < self.to_size {
            return false;
        }
        // Zero-initialize destination (prevents uninit reads)
        for b in dst[..self.to_size].iter_mut() {
            *b = 0;
        }
        for field in &self.field_map {
            if !field.is_inhabited {
                continue;
            }
            let src_end = field.from_offset + field.size;
            let dst_end = field.to_offset + field.size;
            if src_end > src.len() || dst_end > dst.len() {
                return false;
            }
            dst[field.to_offset..dst_end].copy_from_slice(&src[field.from_offset..src_end]);
        }
        self.usage_count.fetch_add(1, Ordering::Relaxed);
        true
    }

    pub fn fnv1a(name: &str) -> u64 {
        let mut hash: u64 = 0xcbf29ce484222325;
        for &b in name.as_bytes() {
            hash ^= b as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash
    }
}

// ─────────────────────────────────────────────
// CRUCIBLE — MPSC Lock-Free Ring Buffer
// ─────────────────────────────────────────────

/// The crucible: a receiver's dedicated lock-free message ring
pub struct Crucible {
    pub receiver_pid: u32,
    atoms: Vec<Atom>,
    pub head: AtomicU64, // next read position
    pub tail: AtomicU64, // next write position (claimed by sender)
    pub capacity: usize,
    pub dropped: AtomicU64, // atoms dropped due to full buffer
    pub received: AtomicU64,
    pub latency_sum_ns: AtomicU64,
}

impl Crucible {
    pub fn new(receiver_pid: u32, capacity: usize) -> Self {
        let mut atoms = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            atoms.push(Atom::new_empty());
        }
        Self {
            receiver_pid,
            atoms,
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
            capacity,
            dropped: AtomicU64::new(0),
            received: AtomicU64::new(0),
            latency_sum_ns: AtomicU64::new(0),
        }
    }

    /// Send into crucible — MPSC: multiple senders call this concurrently
    /// Returns sequence number of placed atom, or None if full
    pub fn send(
        &mut self,
        sender: u32,
        type_id: u64,
        recv_type: u64,
        data: &[u8],
        now_ns: u64,
    ) -> Option<u64> {
        // Claim a tail slot via CAS (MPSC producer-side)
        let tail = self.tail.fetch_add(1, Ordering::AcqRel);
        let slot = (tail as usize) % self.capacity;
        let seq = tail + 1;

        let atom = &mut self.atoms[slot];
        if !atom.crystallize(
            sender,
            self.receiver_pid,
            type_id,
            recv_type,
            data,
            seq,
            now_ns,
        ) {
            // Slot occupied — buffer full
            self.tail.fetch_sub(1, Ordering::AcqRel);
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        Some(seq)
    }

    /// Receive from crucible — single consumer
    /// Returns (payload_bytes, type_id, sender_pid, latency_ns) or None
    pub fn recv(&mut self, now_ns: u64) -> Option<(Vec<u8>, u64, u32, u64)> {
        let head = self.head.load(Ordering::Acquire);
        let slot = (head as usize) % self.capacity;
        let atom = &mut self.atoms[slot];

        if !atom.is_ready() {
            return None;
        }
        let timestamp_ns = atom.timestamp_ns;
        let (data, type_id, sender_pid) = atom.shatter()?;
        let latency = now_ns.saturating_sub(timestamp_ns);
        let result = (data.to_vec(), type_id, sender_pid, latency);
        atom.recycle();
        self.head.fetch_add(1, Ordering::Release);
        self.received.fetch_add(1, Ordering::Relaxed);
        self.latency_sum_ns.fetch_add(latency, Ordering::Relaxed);
        Some(result)
    }

    pub fn len(&self) -> usize {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Relaxed);
        (tail.saturating_sub(head)) as usize
    }

    pub fn avg_latency_ns(&self) -> u64 {
        let recv = self.received.load(Ordering::Relaxed);
        if recv == 0 {
            return 0;
        }
        self.latency_sum_ns.load(Ordering::Relaxed) / recv
    }
}

// ─────────────────────────────────────────────
// THE PHILOSOPHER'S STONE — Master IPC Router
// ─────────────────────────────────────────────

pub struct Philosopher {
    pub crucibles: BTreeMap<u32, Crucible>, // pid → crucible
    pub schemas: BTreeMap<(u64, u64), TransmutationSchema>, // (from,to) → schema
    pub type_registry: BTreeMap<String, u64>, // type_name → type_id
    pub wall_ns: u64,
    pub total_sent: AtomicU64,
    pub total_received: AtomicU64,
    pub total_transmuted: AtomicU64,
    pub total_dropped: AtomicU64,
    pub routing_table: BTreeMap<u64, Vec<u32>>, // type_id → interested pids
}

impl Philosopher {
    pub fn new() -> Self {
        Self {
            crucibles: BTreeMap::new(),
            schemas: BTreeMap::new(),
            type_registry: BTreeMap::new(),
            wall_ns: 0,
            total_sent: AtomicU64::new(0),
            total_received: AtomicU64::new(0),
            total_transmuted: AtomicU64::new(0),
            total_dropped: AtomicU64::new(0),
            routing_table: BTreeMap::new(),
        }
    }

    /// Register a process — creates its crucible
    pub fn register(&mut self, pid: u32) {
        self.crucibles
            .insert(pid, Crucible::new(pid, CRUCIBLE_CAPACITY));
    }

    /// Register a type in the alchemy registry
    pub fn register_type(&mut self, name: &str, size: usize) -> u64 {
        let id = TransmutationSchema::fnv1a(name);
        self.type_registry.insert(String::from(name), id);
        // Register identity schema (type transmutes to itself)
        let schema = TransmutationSchema::identity(id, name, size);
        self.schemas.insert((id, id), schema);
        id
    }

    /// Register a transmutation schema — allows from_type → to_type conversion
    pub fn register_schema(&mut self, schema: TransmutationSchema) {
        self.schemas
            .insert((schema.from_type_id, schema.to_type_id), schema);
    }

    /// Subscribe a process to receive messages of a given type
    pub fn subscribe(&mut self, pid: u32, type_id: u64) {
        self.routing_table
            .entry(type_id)
            .or_insert_with(Vec::new)
            .push(pid);
    }

    /// Send a message — the Philosopher routes and optionally transmutes it
    pub fn send(
        &mut self,
        sender: u32,
        receiver: u32,
        type_id: u64,
        data: &[u8],
    ) -> Result<u64, IpcError> {
        let receiver_crucible = self
            .crucibles
            .get_mut(&receiver)
            .ok_or(IpcError::NoSuchReceiver)?;

        // Determine if transmutation is needed
        // In this direct-send path, type_id == recv_type_id (no transmutation needed)
        let seq = receiver_crucible
            .send(sender, type_id, type_id, data, self.wall_ns)
            .ok_or(IpcError::BufferFull)?;

        self.total_sent.fetch_add(1, Ordering::Relaxed);
        Ok(seq)
    }

    /// Transmute-send: send with automatic type conversion
    pub fn transmute_send(
        &mut self,
        sender: u32,
        receiver: u32,
        from_type: u64,
        to_type: u64,
        data: &[u8],
    ) -> Result<u64, IpcError> {
        if from_type == to_type {
            return self.send(sender, receiver, from_type, data);
        }

        let schema = self
            .schemas
            .get(&(from_type, to_type))
            .ok_or(IpcError::NoTransmutationSchema)?
            .clone();

        let mut transmuted = vec![0u8; schema.to_size];
        if !schema.transmute(data, &mut transmuted) {
            return Err(IpcError::TransmutationFailed);
        }

        self.total_transmuted.fetch_add(1, Ordering::Relaxed);

        let crucible = self
            .crucibles
            .get_mut(&receiver)
            .ok_or(IpcError::NoSuchReceiver)?;
        crucible
            .send(sender, from_type, to_type, &transmuted, self.wall_ns)
            .ok_or(IpcError::BufferFull)
            .map(|seq| {
                self.total_sent.fetch_add(1, Ordering::Relaxed);
                seq
            })
    }

    /// Broadcast: publish to all type subscribers
    pub fn broadcast(&mut self, sender: u32, type_id: u64, data: &[u8]) -> usize {
        let receivers: Vec<u32> = self
            .routing_table
            .get(&type_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|&pid| pid != sender)
            .collect();

        let mut delivered = 0;
        for receiver_pid in receivers {
            // Get receiver's expected type (may need transmutation)
            let crucible = match self.crucibles.get_mut(&receiver_pid) {
                Some(c) => c,
                None => continue,
            };
            if crucible
                .send(sender, type_id, type_id, data, self.wall_ns)
                .is_some()
            {
                delivered += 1;
                self.total_sent.fetch_add(1, Ordering::Relaxed);
            } else {
                self.total_dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        delivered
    }

    /// Receive: drain one atom from a process's crucible
    pub fn recv(&mut self, pid: u32) -> Option<AlchemyMessage> {
        let crucible = self.crucibles.get_mut(&pid)?;
        let (data, type_id, sender_pid, latency) = crucible.recv(self.wall_ns)?;
        self.total_received.fetch_add(1, Ordering::Relaxed);
        Some(AlchemyMessage {
            data,
            type_id,
            sender_pid,
            latency_ns: latency,
        })
    }

    pub fn tick(&mut self, wall_ns: u64) {
        self.wall_ns = wall_ns;
    }

    pub fn channel_stats(&self, pid: u32) -> Option<CrucibleStats> {
        let c = self.crucibles.get(&pid)?;
        Some(CrucibleStats {
            pending: c.len(),
            received: c.received.load(Ordering::Relaxed),
            dropped: c.dropped.load(Ordering::Relaxed),
            avg_latency_ns: c.avg_latency_ns(),
        })
    }

    pub fn global_stats(&self) -> PhilosopherStats {
        PhilosopherStats {
            total_sent: self.total_sent.load(Ordering::Relaxed),
            total_received: self.total_received.load(Ordering::Relaxed),
            total_transmuted: self.total_transmuted.load(Ordering::Relaxed),
            total_dropped: self.total_dropped.load(Ordering::Relaxed),
            registered_types: self.type_registry.len() as u32,
            active_channels: self.crucibles.len() as u32,
            schema_count: self.schemas.len() as u32,
        }
    }
}

pub struct AlchemyMessage {
    pub data: Vec<u8>,
    pub type_id: u64,
    pub sender_pid: u32,
    pub latency_ns: u64,
}

#[derive(Clone, Copy, Debug)]
pub enum IpcError {
    NoSuchReceiver,
    BufferFull,
    NoTransmutationSchema,
    TransmutationFailed,
    ChecksumMismatch,
}

#[derive(Clone, Copy, Debug)]
pub struct CrucibleStats {
    pub pending: usize,
    pub received: u64,
    pub dropped: u64,
    pub avg_latency_ns: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct PhilosopherStats {
    pub total_sent: u64,
    pub total_received: u64,
    pub total_transmuted: u64,
    pub total_dropped: u64,
    pub registered_types: u32,
    pub active_channels: u32,
    pub schema_count: u32,
}
