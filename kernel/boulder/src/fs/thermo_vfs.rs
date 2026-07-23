use alloc::{collections::BTreeMap, vec::Vec};

/// Block thermal state
#[derive(Clone, Copy, PartialEq)]
pub enum ThermalState {
    Crystalline, // entropy ≈ 0, hot cache, NVMe fast lane
    Warm,        // entropy 0.3-0.7, SSD mid tier
    Gaseous,     // entropy > 0.9, cold HDD / archive
}

#[derive(Clone)]
pub struct ThermoBlock {
    pub id: u64,
    pub entropy: f64,     // Shannon entropy of block content [0,1]
    pub temperature: f64, // access frequency as "temperature" in Kelvin
    pub state: ThermalState,
    pub data: Vec<u8>,
}

impl ThermoBlock {
    pub fn new(id: u64, data: Vec<u8>) -> Self {
        let entropy = Self::shannon_entropy(&data);
        let state = if entropy < 0.3 {
            ThermalState::Crystalline
        } else if entropy < 0.7 {
            ThermalState::Warm
        } else {
            ThermalState::Gaseous
        };
        Self {
            id,
            entropy,
            temperature: 300.0,
            state,
            data,
        }
    }

    /// Shannon entropy H = -Σ p_i log₂(p_i)
    fn shannon_entropy(data: &[u8]) -> f64 {
        if data.is_empty() {
            return 0.0;
        }
        let mut freq = [0u64; 256];
        for &b in data {
            freq[b as usize] += 1;
        }
        let n = data.len() as f64;
        freq.iter()
            .filter(|&&f| f > 0)
            .map(|&f| {
                let p = f as f64 / n;
                -p * libm::log2(p)
            })
            .sum::<f64>()
            / 8.0 // normalize to [0,1]
    }

    /// Access heats the block — Boltzmann-inspired cooling
    pub fn touch(&mut self) {
        self.temperature += 50.0;
        self.recompute_state();
    }

    /// Passive cooling each GC tick
    pub fn cool(&mut self, dt: f64) {
        // Newton's law of cooling: dT/dt = -k(T - T_ambient)
        const K: f64 = 0.01;
        const T_AMB: f64 = 300.0;
        self.temperature -= K * (self.temperature - T_AMB) * dt;
        self.recompute_state();
    }

    fn recompute_state(&mut self) {
        self.state = if self.temperature > 1000.0 {
            ThermalState::Crystalline
        } else if self.temperature > 400.0 {
            ThermalState::Warm
        } else {
            ThermalState::Gaseous
        };
    }
}

/// Maxwell's Demon — GC daemon that sorts blocks by thermal state
pub struct MaxwellDemon {
    blocks: BTreeMap<u64, ThermoBlock>,
    hot_partition: Vec<u64>,  // Crystalline block IDs
    cold_partition: Vec<u64>, // Gaseous block IDs
}

impl MaxwellDemon {
    pub fn new() -> Self {
        Self {
            blocks: BTreeMap::new(),
            hot_partition: Vec::new(),
            cold_partition: Vec::new(),
        }
    }

    pub fn insert(&mut self, block: ThermoBlock) {
        let id = block.id;
        self.blocks.insert(id, block);
        self.sort_block(id);
    }

    fn sort_block(&mut self, id: u64) {
        if let Some(b) = self.blocks.get(&id) {
            match b.state {
                ThermalState::Crystalline => {
                    if !self.hot_partition.contains(&id) {
                        self.hot_partition.push(id);
                    }
                    self.cold_partition.retain(|&x| x != id);
                }
                ThermalState::Gaseous => {
                    if !self.cold_partition.contains(&id) {
                        self.cold_partition.push(id);
                    }
                    self.hot_partition.retain(|&x| x != id);
                }
                ThermalState::Warm => {
                    self.hot_partition.retain(|&x| x != id);
                    self.cold_partition.retain(|&x| x != id);
                }
            }
        }
    }

    /// GC tick — cool all blocks and re-sort
    pub fn gc_tick(&mut self, dt: f64) {
        let ids: Vec<u64> = self.blocks.keys().copied().collect();
        for id in ids {
            if let Some(b) = self.blocks.get_mut(&id) {
                b.cool(dt);
            }
            self.sort_block(id);
        }
    }
}
