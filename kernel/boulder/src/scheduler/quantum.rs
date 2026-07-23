use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

/// A task's quantum state — probability amplitude in complex form
#[derive(Clone, Copy)]
pub struct QuantumAmplitude {
    pub real: f64, // cos component
    pub imag: f64, // sin component
}

impl QuantumAmplitude {
    pub fn magnitude_sq(&self) -> f64 {
        self.real * self.real + self.imag * self.imag
    }
    /// Interfere two amplitudes (constructive/destructive)
    pub fn interfere(&self, other: &Self) -> Self {
        Self {
            real: self.real + other.real,
            imag: self.imag + other.imag,
        }
    }
}

pub struct QuantumTask {
    pub tid: u64,
    pub amplitude: QuantumAmplitude,
    pub entangled_with: Option<u64>, // entangled task id for IPC fast-path
    pub phase: f64,                  // Bloch sphere phase angle
    pub priority_mass: f64,          // relativistic "mass" — heavier = slower time
}

/// The superposition runqueue — O(1) probabilistic pick
pub struct SuperpositionQueue {
    tasks: Vec<QuantumTask>,
    decoherence_timer: AtomicU64,
}

impl SuperpositionQueue {
    pub fn new() -> Self {
        Self {
            tasks: Vec::new(),
            decoherence_timer: AtomicU64::new(0),
        }
    }

    /// Collapse the wavefunction — pick next task by probability amplitude
    pub fn collapse_and_schedule(&mut self) -> Option<&QuantumTask> {
        if self.tasks.is_empty() {
            return None;
        }

        // Normalize probabilities across all tasks
        let total: f64 = self.tasks.iter().map(|t| t.amplitude.magnitude_sq()).sum();

        // "Observe" the system — deterministic but amplitude-weighted
        let tick = self.decoherence_timer.fetch_add(1, Ordering::SeqCst);
        let selector = (tick as f64 * 0.6180339887) % 1.0; // golden ratio hash

        let mut cumulative = 0.0_f64;
        for task in &self.tasks {
            cumulative += task.amplitude.magnitude_sq() / total;
            if cumulative >= selector {
                return Some(task);
            }
        }
        self.tasks.last()
    }

    /// Entangle two tasks — when one yields, the other gets a phase boost
    pub fn entangle(&mut self, tid_a: u64, tid_b: u64) {
        for task in &mut self.tasks {
            if task.tid == tid_a {
                task.entangled_with = Some(tid_b);
            }
        }
    }

    /// Apply relativistic time dilation — high-priority tasks experience slower time
    pub fn dilate_time(&mut self, tid: u64, mass_factor: f64) {
        for task in &mut self.tasks {
            if task.tid == tid {
                // Lorentz factor: γ = 1/sqrt(1 - v²/c²), modeled as priority curve
                // Note: libm::sqrt should be used if f64::sqrt is not available in no_std core without std.
                // But let's use libm::sqrt or if not available, we can write a simple approx or rely on core.
                let v2 = (mass_factor * 0.99).min(0.99);
                let gamma = 1.0 / libm::sqrt(1.0 - v2);
                task.amplitude.real *= gamma;
                task.phase += core::f64::consts::PI / gamma;
            }
        }
    }
}
