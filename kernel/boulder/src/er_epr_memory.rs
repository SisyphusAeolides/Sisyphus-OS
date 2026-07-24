// kernel/boulder/src/er_epr_memory.rs
//! ER=EPR MEMORY BRIDGES
//!
//! Shared memory is not a double mapping. It is a non-traversable wormhole
//! between two ASID mouths. Operations:
//!   entangle(pid_a, va_a, pid_b, va_b, frame) → Bridge
//!   pulse(bridge) — coherence touch (both mouths heat together)
//!   pinch(bridge) — destroy entanglement; optional COW split
//!
//! Kardashev Type 0 forbids new bridges.
//! Syntropic ECC can protect the bulk frame once.
//! AdS boundary gate admits the IPC that requests entanglement.

pub const MAX_BRIDGES: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BridgeFault {
    Disabled,
    Capacity,
    NotFound,
    AsidMismatch,
    AlreadyEntangled,
    InvalidMouth,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mouth {
    pub pid: u32,
    pub asid: u16,
    pub va_page: u64, // page-aligned VA
}

#[derive(Clone, Copy, Debug)]
pub struct ErBridge {
    pub live: bool,
    pub id: u64,
    pub mouth_a: Mouth,
    pub mouth_b: Mouth,
    pub frame_id: u64,
    /// Bell-ish correlation counter (coherent touches)
    pub correlation: u64,
    pub pinched: bool,
    pub created_tsc: u64,
}

impl ErBridge {
    pub const EMPTY: Self = Self {
        live: false,
        id: 0,
        mouth_a: Mouth {
            pid: 0,
            asid: 0,
            va_page: 0,
        },
        mouth_b: Mouth {
            pid: 0,
            asid: 0,
            va_page: 0,
        },
        frame_id: 0,
        correlation: 0,
        pinched: false,
        created_tsc: 0,
    };
}

pub struct ErEprFabric {
    bridges: [ErBridge; MAX_BRIDGES],
    length: usize,
    next_id: u64,
    allow: bool,
    entangle_count: u64,
    pinch_count: u64,
}

impl ErEprFabric {
    pub const fn new() -> Self {
        Self {
            bridges: [ErBridge::EMPTY; MAX_BRIDGES],
            length: 0,
            next_id: 1,
            allow: true,
            entangle_count: 0,
            pinch_count: 0,
        }
    }

    pub fn set_allowed(&mut self, allow: bool) {
        self.allow = allow;
    }

    pub fn entangle(
        &mut self,
        a: Mouth,
        b: Mouth,
        frame_id: u64,
        now_tsc: u64,
    ) -> Result<u64, BridgeFault> {
        if !self.allow {
            return Err(BridgeFault::Disabled);
        }

        // Validate PIDs (must be non-zero) and VA pages (must be in user space, i.e., < 0x0000_8000_0000_0000)
        let is_valid_va = |va: u64| va < 0x0000_8000_0000_0000;
        if a.pid == 0 || b.pid == 0 || !is_valid_va(a.va_page) || !is_valid_va(b.va_page) {
            return Err(BridgeFault::InvalidMouth);
        }
        if a.pid == b.pid && a.va_page == b.va_page {
            return Err(BridgeFault::AlreadyEntangled);
        }
        // Refuse if either mouth already in a live bridge
        for br in self.bridges.iter().take(self.length) {
            if !br.live || br.pinched {
                continue;
            }
            if mouth_eq(br.mouth_a, a)
                || mouth_eq(br.mouth_b, a)
                || mouth_eq(br.mouth_a, b)
                || mouth_eq(br.mouth_b, b)
            {
                return Err(BridgeFault::AlreadyEntangled);
            }
        }
        if self.length >= MAX_BRIDGES {
            return Err(BridgeFault::Capacity);
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.bridges[self.length] = ErBridge {
            live: true,
            id,
            mouth_a: a,
            mouth_b: b,
            frame_id,
            correlation: 0,
            pinched: false,
            created_tsc: now_tsc,
        };
        self.length += 1;
        self.entangle_count = self.entangle_count.saturating_add(1);
        Ok(id)
    }

    pub fn pulse(&mut self, id: u64) -> Result<u64, BridgeFault> {
        let br = self
            .bridges
            .iter_mut()
            .find(|b| b.live && b.id == id)
            .ok_or(BridgeFault::NotFound)?;
        if br.pinched {
            return Err(BridgeFault::NotFound);
        }
        br.correlation = br.correlation.saturating_add(1);
        Ok(br.correlation)
    }

    pub fn pinch(&mut self, id: u64) -> Result<u64, BridgeFault> {
        let br = self
            .bridges
            .iter_mut()
            .find(|b| b.live && b.id == id)
            .ok_or(BridgeFault::NotFound)?;
        br.pinched = true;
        br.live = false;
        self.pinch_count = self.pinch_count.saturating_add(1);
        Ok(br.frame_id)
    }

    pub fn lookup_by_mouth(&self, m: Mouth) -> Option<&ErBridge> {
        self.bridges
            .iter()
            .take(self.length)
            .find(|b| b.live && !b.pinched && (mouth_eq(b.mouth_a, m) || mouth_eq(b.mouth_b, m)))
    }
}

fn mouth_eq(x: Mouth, y: Mouth) -> bool {
    x.pid == y.pid && x.asid == y.asid && x.va_page == y.va_page
}
