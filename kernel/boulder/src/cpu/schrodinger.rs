// kernel/boulder/src/cpu/schrodinger.rs
// #![no_std] inherited
//
// SCHRÖDINGER — Quantum Speculative Execution Sandbox
//
// Concept: When a branch outcome is uncertain (e.g. waiting on slow I/O or
//   a cache miss), instead of guessing one path and suffering pipeline flushes,
//   the kernel physically splits the execution state and runs BOTH branches
//   concurrently in isolated "superposition contexts".
//
// State representation: |ψ⟩ = α|True⟩ + β|False⟩
//
// Isolation: Each path gets a Shadow Register File and a Quantum Write Buffer.
//   Reads come from main memory unless overwritten in the buffer.
//   Writes to memory are trapped and kept in the local quantum buffer.
//
// Collapse (Observation): Once the actual branch condition resolves (the I/O
//   returns or memory fetch completes), the wave function collapses. The
//   correct path's quantum buffer is atomically committed to main memory,
//   and the incorrect path's state is instantly destroyed.
//
// Entanglement: If two speculative threads interact via IPC (e.g., Wormhole),
//   their wave functions become entangled. Collapsing one collapses the other.

#![allow(dead_code)]
extern crate alloc;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

// ─────────────────────────────────────────────
// CONSTANTS
// ─────────────────────────────────────────────

pub const MAX_SUPERPOSITIONS: usize = 4;
pub const QUANTUM_BUFFER_SIZE: usize = 1024; // max writes per speculative path

// ─────────────────────────────────────────────
// QUANTUM WRITE BUFFER (Shadow Memory)
// ─────────────────────────────────────────────

#[derive(Clone)]
pub struct QuantumBuffer {
    // Maps physical address to uncommitted speculative byte
    pub writes: BTreeMap<u64, u8>,
}

impl QuantumBuffer {
    pub fn new() -> Self {
        Self {
            writes: BTreeMap::new(),
        }
    }
    pub fn read(&self, paddr: u64) -> Option<u8> {
        self.writes.get(&paddr).copied()
    }
    pub fn write(&mut self, paddr: u64, val: u8) -> bool {
        if self.writes.len() < QUANTUM_BUFFER_SIZE {
            self.writes.insert(paddr, val);
            true
        } else {
            false // buffer overflow -> decoherence
        }
    }
}

// ─────────────────────────────────────────────
// SUPERPOSITION STATE
// ─────────────────────────────────────────────

#[derive(Clone)]
pub struct RegisterFile {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub rsp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rflags: u64,
}

pub struct SuperpositionState {
    pub id: u32,
    pub branch_cond: bool, // Which reality this represents (True or False)
    pub regs: RegisterFile,
    pub memory: QuantumBuffer,
    pub cycles_run: u64,
    pub decohered: bool, // True if the state failed (e.g., buffer overflow or crash)
}

// ─────────────────────────────────────────────
// WAVE FUNCTION (The Thread)
// ─────────────────────────────────────────────

pub struct WaveFunction {
    pub thread_id: u32,
    pub base_regs: RegisterFile,
    pub states: Vec<SuperpositionState>,
    pub observation: Option<bool>, // When set, wave collapses
    pub entangled_with: Vec<u32>,  // Other thread IDs
}

impl WaveFunction {
    pub fn new(tid: u32, regs: RegisterFile) -> Self {
        Self {
            thread_id: tid,
            base_regs: regs,
            states: Vec::new(),
            observation: None,
            entangled_with: Vec::new(),
        }
    }

    /// Split execution into two parallel realities
    pub fn bifurcate(&mut self) -> bool {
        if self.states.len() >= MAX_SUPERPOSITIONS {
            return false;
        }

        let s_true = SuperpositionState {
            id: 1,
            branch_cond: true,
            regs: self.base_regs.clone(),
            memory: QuantumBuffer::new(),
            cycles_run: 0,
            decohered: false,
        };

        let s_false = SuperpositionState {
            id: 0,
            branch_cond: false,
            regs: self.base_regs.clone(),
            memory: QuantumBuffer::new(),
            cycles_run: 0,
            decohered: false,
        };

        self.states.push(s_true);
        self.states.push(s_false);
        true
    }

    /// Trap a speculative write
    pub fn speculative_write(&mut self, state_id: u32, paddr: u64, val: u8) {
        if let Some(state) = self.states.iter_mut().find(|s| s.id == state_id) {
            if !state.memory.write(paddr, val) {
                state.decohered = true; // Buffer full -> this universe dies
            }
        }
    }

    /// The hardware finally observed the truth. Collapse the wave function!
    pub fn collapse(&mut self, truth: bool) -> Option<QuantumBuffer> {
        self.observation = Some(truth);

        // Find the surviving universe
        let mut survivor_buffer = None;
        while let Some(state) = self.states.pop() {
            if state.branch_cond == truth && !state.decohered {
                // This is the chosen reality. Extract its write buffer.
                survivor_buffer = Some(state.memory);
                self.base_regs = state.regs;
            }
        }

        survivor_buffer
    }
}
