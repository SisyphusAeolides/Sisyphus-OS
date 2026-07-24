// kernel/boulder/src/zx_rewrite.rs
//! ZX rewrite — spider fusion for kernel dependency graphs
//!
//! Encode a directed multi-graph as a ZX-like diagram:
//!   Z-spider: "same phase family" nodes (e.g. tasks sharing a lock domain)
//!   X-spider: "complementary" nodes (IRQ ↔ bottom-half)
//! Rules (graph-theoretic fragment):
//!   (F) same-color adjacent spiders fuse: phases add (mod M), edges merge
//!   (I) 1-ary spider with phase 0 is identity — dissolve
//!   (H) color change via Hadamard edge: Z↔X (represented as edge flag)
//!
//! Output: reduced node count → cheaper tropical_crit / fiedler input.

pub const MAX_SPIDERS: usize = 48;
pub const MAX_WIRES: usize = 96;
pub const PHASE_MOD: u16 = 360; // degrees; use 2 for GF(2) phases

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Color {
    Z = 0,
    X = 1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Spider {
    pub live: bool,
    pub color: Color,
    pub phase: u16,
    /// kernel payload: task id / irq vector
    pub tag: u32,
}

impl Spider {
    pub const EMPTY: Self = Self {
        live: false,
        color: Color::Z,
        phase: 0,
        tag: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Wire {
    pub live: bool,
    pub a: u16,
    pub b: u16,
    /// Hadamard-decorated edge (color change)
    pub hadamard: bool,
    pub weight: u16,
}

impl Wire {
    pub const EMPTY: Self = Self {
        live: false,
        a: 0,
        b: 0,
        hadamard: false,
        weight: 1,
    };
}

pub struct ZxDiagram {
    pub spiders: [Spider; MAX_SPIDERS],
    pub wires: [Wire; MAX_WIRES],
    pub n_spiders: usize,
    pub n_wires: usize,
}

impl ZxDiagram {
    pub const fn new() -> Self {
        Self {
            spiders: [Spider::EMPTY; MAX_SPIDERS],
            wires: [Wire::EMPTY; MAX_WIRES],
            n_spiders: 0,
            n_wires: 0,
        }
    }

    pub fn add_spider(&mut self, color: Color, phase: u16, tag: u32) -> Option<u16> {
        if self.n_spiders >= MAX_SPIDERS {
            return None;
        }
        let id = self.n_spiders as u16;
        self.spiders[self.n_spiders] = Spider {
            live: true,
            color,
            phase: phase % PHASE_MOD,
            tag,
        };
        self.n_spiders += 1;
        Some(id)
    }

    pub fn add_wire(&mut self, a: u16, b: u16, hadamard: bool, weight: u16) -> bool {
        if self.n_wires >= MAX_WIRES || a == b {
            return false;
        }
        self.wires[self.n_wires] = Wire {
            live: true,
            a,
            b,
            hadamard,
            weight,
        };
        self.n_wires += 1;
        true
    }

    pub fn live_spiders(&self) -> usize {
        self.spiders
            .iter()
            .take(self.n_spiders)
            .filter(|s| s.live)
            .count()
    }

    /// One fusion pass: find edge a—b same color, no H, fuse b into a.
    pub fn fuse_once(&mut self) -> bool {
        for w_idx in 0..self.n_wires {
            let w = self.wires[w_idx];
            if !w.live || w.hadamard {
                continue;
            }
            let a = w.a as usize;
            let b = w.b as usize;
            if a >= self.n_spiders || b >= self.n_spiders {
                continue;
            }
            if !self.spiders[a].live || !self.spiders[b].live {
                continue;
            }
            if self.spiders[a].color != self.spiders[b].color {
                continue;
            }
            // Fuse b → a
            self.spiders[a].phase = (self.spiders[a].phase + self.spiders[b].phase) % PHASE_MOD;
            // Keep min tag as canonical representative
            if self.spiders[b].tag < self.spiders[a].tag {
                self.spiders[a].tag = self.spiders[b].tag;
            }
            self.spiders[b].live = false;
            self.wires[w_idx].live = false;
            // Rewire edges touching b to a
            for wi in 0..self.n_wires {
                if !self.wires[wi].live {
                    continue;
                }
                if self.wires[wi].a == w.b {
                    self.wires[wi].a = w.a;
                }
                if self.wires[wi].b == w.b {
                    self.wires[wi].b = w.a;
                }
                // drop loops
                if self.wires[wi].a == self.wires[wi].b {
                    self.wires[wi].live = false;
                }
            }
            return true;
        }
        false
    }

    /// Hadamard edge between Z—X is normal; H edge between same color → color flip helper
    pub fn push_hadamards_once(&mut self) -> bool {
        for w_idx in 0..self.n_wires {
            let w = self.wires[w_idx];
            if !w.live || !w.hadamard {
                continue;
            }
            let a = w.a as usize;
            let b = w.b as usize;
            if !self.spiders[a].live || !self.spiders[b].live {
                continue;
            }
            // Color-change rule: absorb H into spider b by flipping its color
            self.spiders[b].color = match self.spiders[b].color {
                Color::Z => Color::X,
                Color::X => Color::Z,
            };
            self.wires[w_idx].hadamard = false;
            return true;
        }
        false
    }

    /// Delete phase-0 degree-1 spiders (identity)
    pub fn kill_identities(&mut self) -> bool {
        let mut deg = [0u16; MAX_SPIDERS];
        for w in self.wires.iter().take(self.n_wires) {
            if !w.live {
                continue;
            }
            deg[w.a as usize] = deg[w.a as usize].saturating_add(1);
            deg[w.b as usize] = deg[w.b as usize].saturating_add(1);
        }
        for i in 0..self.n_spiders {
            if !self.spiders[i].live {
                continue;
            }
            if self.spiders[i].phase == 0 && deg[i] <= 1 {
                self.spiders[i].live = false;
                for w in self.wires.iter_mut().take(self.n_wires) {
                    if w.a as usize == i || w.b as usize == i {
                        w.live = false;
                    }
                }
                return true;
            }
        }
        false
    }

    /// Normalize until fixpoint or bound.
    pub fn simplify(&mut self, max_steps: usize) -> usize {
        let mut steps = 0usize;
        while steps < max_steps {
            if self.fuse_once() || self.push_hadamards_once() || self.kill_identities() {
                steps += 1;
                continue;
            }
            break;
        }
        steps
    }

    /// Export residual undirected simple graph into weight matrix for Fiedler/Tropical.
    pub fn export_adjacency(
        &self,
        out: &mut [[u16; 32]; 32],
        map: &mut [u16; MAX_SPIDERS],
    ) -> usize {
        // map live spiders → dense ids
        for m in map.iter_mut() {
            *m = 0xFFFF;
        }
        let mut n = 0usize;
        for i in 0..self.n_spiders {
            if self.spiders[i].live && n < 32 {
                map[i] = n as u16;
                n += 1;
            }
        }
        for row in out.iter_mut() {
            *row = [0; 32];
        }
        for w in self.wires.iter().take(self.n_wires) {
            if !w.live {
                continue;
            }
            let a = map[w.a as usize];
            let b = map[w.b as usize];
            if a == 0xFFFF || b == 0xFFFF || a == b {
                continue;
            }
            let ai = a as usize;
            let bi = b as usize;
            out[ai][bi] = out[ai][bi].saturating_add(w.weight);
            out[bi][ai] = out[bi][ai].saturating_add(w.weight);
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuses_same_color_chain() {
        let mut d = ZxDiagram::new();
        let a = d.add_spider(Color::Z, 10, 1).unwrap();
        let b = d.add_spider(Color::Z, 20, 2).unwrap();
        let c = d.add_spider(Color::Z, 0, 3).unwrap();
        d.add_wire(a, b, false, 1);
        d.add_wire(b, c, false, 1);
        let s = d.simplify(16);
        assert!(s >= 1);
        assert!(d.live_spiders() <= 2);
    }
}
