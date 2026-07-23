use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

pub struct IpcMessage {
    pub sender_pid: u32,
    pub semantic_hash: u64,
    pub payload: [u8; 256],
    pub payload_len: usize,
}

pub struct ImmuneRegistry {
    antigens: [AtomicU32; 128],
    index: AtomicUsize,
}

impl ImmuneRegistry {
    #[allow(clippy::declare_interior_mutable_const)]
    const ATOMIC_ZERO: AtomicU32 = AtomicU32::new(0);

    pub const fn new() -> Self {
        Self {
            antigens: [Self::ATOMIC_ZERO; 128],
            index: AtomicUsize::new(0),
        }
    }

    pub fn broadcast_antigen(&self, antigen: u32) {
        let idx = self.index.fetch_add(1, Ordering::Relaxed) % 128;
        self.antigens[idx].store(antigen, Ordering::Relaxed);
    }
}

pub static IMMUNE_REGISTRY: ImmuneRegistry = ImmuneRegistry::new();

pub fn trigger_apoptosis(pid: u32) {
    let _ = pid;
}

pub struct Macrophage {
    patrol_window: [u64; 64],
    window_index: usize,
}

impl Macrophage {
    pub const fn new() -> Self {
        Self {
            patrol_window: [0; 64],
            window_index: 0,
        }
    }

    pub fn patrol(&mut self, message: &mut IpcMessage) {
        self.patrol_window[self.window_index] = message.semantic_hash;
        self.window_index = (self.window_index + 1) % self.patrol_window.len();

        if self.is_malicious(message.semantic_hash) {
            let antigen = self.extract_antigen(message);
            self.phagocytize(message);

            IMMUNE_REGISTRY.broadcast_antigen(antigen);
            trigger_apoptosis(message.sender_pid);
        }
    }

    fn is_malicious(&self, hash: u64) -> bool {
        hash.count_ones() > 48 || (hash & 0xFFFF) == 0xDEAD
    }

    fn extract_antigen(&self, message: &IpcMessage) -> u32 {
        let mut sig = 0u32;
        let len = if message.payload_len > 4 {
            4
        } else {
            message.payload_len
        };
        for i in 0..len {
            sig |= (message.payload[i] as u32) << (i * 8);
        }
        sig
    }

    fn phagocytize(&self, message: &mut IpcMessage) {
        message.semantic_hash = 0;
        message.payload_len = 0;
        message.payload.fill(0);
    }
}
