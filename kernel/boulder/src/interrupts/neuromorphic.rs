use core::sync::atomic::{AtomicI32, AtomicU64, Ordering};

const MAX_IRQ: usize = 256;
const SYNAPSE_DECAY: i32 = 1; // per tick weight decay

/// A synaptic weight between two IRQ lines
/// Positive = excitatory (fire together → wire together)
/// Negative = inhibitory (fire apart → suppress each other)
#[derive(Default)]
pub struct Synapse {
    weight: AtomicI32,
    last_pre_fire: AtomicU64,
    last_post_fire: AtomicU64,
}

impl Synapse {
    /// STDP rule: Δw = A+ * exp(-Δt/τ+) if pre before post
    ///                  -A- * exp(-Δt/τ-) if post before pre
    pub fn update_stdp(&self, pre_tick: u64, post_tick: u64) {
        let delta = if post_tick >= pre_tick {
            // causal: strengthen
            let dt = (post_tick - pre_tick).min(100) as i32;
            20 - dt / 5 // A+ * exp(-dt/tau+) approximated
        } else {
            // anticausal: weaken
            let dt = (pre_tick - post_tick).min(100) as i32;
            -(20 - dt / 5)
        };
        self.weight
            .fetch_add(delta.max(-50).min(50), Ordering::Relaxed);
        self.last_pre_fire.store(pre_tick, Ordering::Relaxed);
        self.last_post_fire.store(post_tick, Ordering::Relaxed);
    }

    pub fn get_weight(&self) -> i32 {
        self.weight.load(Ordering::Relaxed)
    }

    pub fn decay(&self) {
        // Passive forgetting — weights decay toward zero
        let w = self.weight.load(Ordering::Relaxed);
        if w > 0 {
            self.weight.fetch_sub(SYNAPSE_DECAY, Ordering::Relaxed);
        } else if w < 0 {
            self.weight.fetch_add(SYNAPSE_DECAY, Ordering::Relaxed);
        }
    }
}

/// The neuromorphic IRQ mesh — a 256×256 synaptic weight matrix
pub struct NeuromorphicIDT {
    synapses: [[Synapse; MAX_IRQ]; MAX_IRQ], // won't fit stack, needs static/heap
    fire_times: [AtomicU64; MAX_IRQ],
    tick: AtomicU64,
}

impl NeuromorphicIDT {
    /// Record IRQ firing — updates all synapses connected to this neuron
    pub fn fire(&self, irq: usize) {
        let now = self.tick.fetch_add(1, Ordering::SeqCst);
        let _prev = self.fire_times[irq].swap(now, Ordering::SeqCst);

        // Update synapses with all recently-fired IRQs (within τ window)
        for other in 0..MAX_IRQ {
            if other == irq {
                continue;
            }
            let other_time = self.fire_times[other].load(Ordering::Relaxed);
            if now.saturating_sub(other_time) < 50 {
                // They fired close together — apply STDP
                self.synapses[irq][other].update_stdp(other_time, now);
            }
        }
    }

    /// Predict next likely IRQ based on current firing pattern
    /// Returns the IRQ most likely to fire next (highest excitatory weight sum)
    pub fn predict_next(&self, recently_fired: &[usize]) -> usize {
        let mut scores = [0i32; MAX_IRQ];
        for &src in recently_fired {
            for dst in 0..MAX_IRQ {
                scores[dst] += self.synapses[src][dst].get_weight();
            }
        }
        scores
            .iter()
            .enumerate()
            .max_by_key(|&(_, &s)| s)
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    /// Global decay tick — run periodically to prevent weight explosion
    pub fn global_decay(&self) {
        for row in &self.synapses {
            for syn in row {
                syn.decay();
            }
        }
    }
}
