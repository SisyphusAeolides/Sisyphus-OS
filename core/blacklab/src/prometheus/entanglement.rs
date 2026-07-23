use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

/// Bell state encoding for a service pair
/// |Φ+⟩ = (|00⟩ + |11⟩)/√2 → both alive or both dead together
/// |Ψ+⟩ = (|01⟩ + |10⟩)/√2 → one alive means other is backup
#[derive(Clone, Copy, PartialEq)]
pub enum BellState {
    PhiPlus,  // correlated — live/die together (HA pair)
    PhiMinus, // anti-correlated — one lives while other is standby
    PsiPlus,  // entangled backup — state inversions propagate
    PsiMinus, // isolated — entanglement broken
}

/// Encoded service state as a 2-qubit word in a single AtomicU64
/// Bits [0] = service A alive, [1] = service B alive
/// Bits [2..9] = Bell state type, Bits [16..63] = health data
pub struct EntangledPair {
    pub state_word: AtomicU64,
    pub bell_class: BellState,
    pub pid_a: u32,
    pub pid_b: u32,
}

impl EntangledPair {
    pub fn new(pid_a: u32, pid_b: u32, bell: BellState) -> Self {
        // Both start alive: bits 0,1 = 1
        let initial = 0b11u64 | ((bell as u64) << 2) | ((100u64) << 16);
        Self {
            state_word: AtomicU64::new(initial),
            bell_class: bell,
            pid_a,
            pid_b,
        }
    }

    /// Collapse: service A changes state — propagate to B via Bell correlation
    pub fn collapse_a(&self, a_alive: bool) -> bool {
        let word = self.state_word.load(Ordering::Acquire);
        let b_alive = (word & 0b10) != 0;

        let new_b_alive = match self.bell_class {
            BellState::PhiPlus => a_alive,   // correlated — B mirrors A
            BellState::PhiMinus => !a_alive, // anti-correlated — B inverts
            BellState::PsiPlus => !b_alive,  // always flip B
            BellState::PsiMinus => b_alive,  // isolated — B unchanged
        };

        let new_word = (word & !0b11u64) | (a_alive as u64) | ((new_b_alive as u64) << 1);

        // CAS — atomic single-word state collapse, like measuring a qubit
        let _ =
            self.state_word
                .compare_exchange(word, new_word, Ordering::AcqRel, Ordering::Relaxed);
        new_b_alive
    }

    pub fn a_alive(&self) -> bool {
        (self.state_word.load(Ordering::Acquire) & 0b01) != 0
    }
    pub fn b_alive(&self) -> bool {
        (self.state_word.load(Ordering::Acquire) & 0b10) != 0
    }
}

/// The entanglement registry — PID 1's non-local service correlation engine
pub struct EntanglementRegistry {
    pairs: Vec<EntangledPair>,
}

impl EntanglementRegistry {
    pub fn new() -> Self {
        Self { pairs: Vec::new() }
    }

    pub fn entangle(&mut self, pid_a: u32, pid_b: u32, bell: BellState) {
        self.pairs.push(EntangledPair::new(pid_a, pid_b, bell));
    }

    /// Service state change — cascade through all entangled pairs
    pub fn propagate_collapse(&self, pid: u32, alive: bool) -> Vec<(u32, bool)> {
        let mut cascades = Vec::new();
        for pair in &self.pairs {
            if pair.pid_a == pid {
                let b_new = pair.collapse_a(alive);
                cascades.push((pair.pid_b, b_new));
            } else if pair.pid_b == pid {
                // Mirror operation for B→A
                let current = pair.state_word.load(Ordering::Acquire);
                let a_alive = (current & 0b01) != 0;
                let new_a = match pair.bell_class {
                    BellState::PhiPlus => alive,
                    BellState::PhiMinus => !alive,
                    BellState::PsiPlus => !a_alive,
                    BellState::PsiMinus => a_alive,
                };
                cascades.push((pair.pid_a, new_a));
            }
        }
        cascades
    }
}
