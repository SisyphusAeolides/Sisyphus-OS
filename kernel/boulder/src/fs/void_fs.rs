// kernel/boulder/src/fs/void_fs.rs
// #![no_std] inherited
//
// VOID_FS — Black Hole Event Horizon Filesystem
//
// A filesystem modeled as a Kerr (rotating) black hole.
// 
// Accretion Disk (Cache): Files written here orbit the singularity.
//   They heat up when accessed (X-ray emission = high cache priority).
//   Friction causes unaccessed files to slowly lose angular momentum 
//   and spiral inward toward the center.
//
// Event Horizon (Swapping/Deletion): Once a file crosses the Schwarzschild radius,
//   its plaintext is destroyed. It is irreversibly compressed into a single 
//   quantum state vector representing its semantic hash (holographic principle).
//   No actual data is stored anymore, only the mathematical shadow of the file.
//
// Hawking Radiation: The black hole slowly evaporates. Files trapped inside 
//   emit random bits of entropy back to the system (Chronovore engine). 
//   Over time, deleted files literally evaporate into pure thermal noise, 
//   completing the data lifecycle.

#![allow(dead_code)]
extern crate alloc;
use alloc::vec::Vec;
use alloc::string::String;
use alloc::collections::BTreeMap;

// ─────────────────────────────────────────────
// CONSTANTS
// ─────────────────────────────────────────────

pub const ACCRETION_DISK_RADIUS: u64 = 1024; // Outer radius
pub const EVENT_HORIZON_RADIUS:  u64 = 64;   // Schwarzschild radius
pub const FRICTION_COEFF_FP:     u32 = 0x0000_0CCD; // ≈ 0.05 inward spiral per tick
pub const XRAY_LUMINOSITY_FP:    u32 = 0x0001_0000; // outward push per access
pub const BLACK_HOLE_MASS_MAX:   u64 = 1_000_000;
pub const HAWKING_TEMP_BASE:     u64 = 100;

// ─────────────────────────────────────────────
// ACCRETION NODE (A File in Orbit)
// ─────────────────────────────────────────────

pub struct AccretionNode {
    pub inode:           u32,
    pub name:            String,
    pub radius_fp:       u64,    // 16.16 fp (distance from singularity)
    pub angular_mom_fp:  u64,    // 16.16 fp (speed)
    pub mass:            u64,    // File size in bytes
    pub temperature_fp:  u64,    // Heat from being accessed
    pub payload:         Vec<u8>,
    pub is_spaghettified: bool,  // True if crossed the event horizon
    pub semantic_hash:   u64,    // Holographic projection of data
}

impl AccretionNode {
    pub fn new(inode: u32, name: String, data: Vec<u8>) -> Self {
        let mass = data.len() as u64;
        let mut node = Self {
            inode,
            name,
            radius_fp: ACCRETION_DISK_RADIUS << 16,
            angular_mom_fp: (ACCRETION_DISK_RADIUS * 2) << 16,
            mass,
            temperature_fp: 0,
            payload: data,
            is_spaghettified: false,
            semantic_hash: 0,
        };
        node.compute_hologram();
        node
    }

    pub fn compute_hologram(&mut self) {
        let mut h = self.inode as u64;
        for &b in &self.payload {
            h = h.rotate_left(3) ^ (b as u64).wrapping_mul(0x517cc1b727220a95);
        }
        self.semantic_hash = h;
    }

    pub fn access(&mut self) {
        if self.is_spaghettified { return; }
        // Heating up increases orbital radius (pushes back from horizon)
        self.temperature_fp = self.temperature_fp.saturating_add(XRAY_LUMINOSITY_FP as u64);
        self.radius_fp = self.radius_fp.saturating_add((XRAY_LUMINOSITY_FP as u64) * 10);
        if self.radius_fp > (ACCRETION_DISK_RADIUS << 16) {
            self.radius_fp = ACCRETION_DISK_RADIUS << 16;
        }
    }

    pub fn tick_orbital_decay(&mut self) -> bool {
        if self.is_spaghettified { return true; }
        // Friction reduces radius
        let friction = (self.radius_fp * (FRICTION_COEFF_FP as u64)) >> 16;
        self.radius_fp = self.radius_fp.saturating_sub(friction);
        
        // Cool down
        self.temperature_fp = (self.temperature_fp * 99) / 100;

        // Check if crossed Event Horizon
        if self.radius_fp < (EVENT_HORIZON_RADIUS << 16) {
            self.spaghettify();
            return true;
        }
        false
    }

    fn spaghettify(&mut self) {
        self.is_spaghettified = true;
        // Tidal forces rip the file apart. Payload is completely dropped.
        // All that remains is the 2D holographic semantic hash on the horizon.
        let mut empty = Vec::new();
        core::mem::swap(&mut self.payload, &mut empty);
        self.radius_fp = EVENT_HORIZON_RADIUS << 16;
        self.mass = 0; // Mass added to central black hole
    }
}

// ─────────────────────────────────────────────
// SINGULARITY (The Black Hole)
// ─────────────────────────────────────────────

pub struct Singularity {
    pub mass:             u64,
    pub spin:             u64, // Kerr metric angular momentum
    pub hawking_entropy:  u64, // Extracted thermal noise
}

impl Singularity {
    pub const fn new() -> Self {
        Self { mass: 1000, spin: 0, hawking_entropy: 0 }
    }

    pub fn consume(&mut self, file_mass: u64, file_hash: u64) {
        self.mass = self.mass.saturating_add(file_mass).min(BLACK_HOLE_MASS_MAX);
        self.spin ^= file_hash; // Information paradox preservation
    }

    pub fn hawking_radiate(&mut self, tick: u64) -> Option<u64> {
        // Temperature is inversely proportional to mass
        if self.mass < 10 { return None; }
        let temp = HAWKING_TEMP_BASE * 1000 / self.mass;
        
        // Radiate pseudo-random bits based on spin and temp
        if temp > 0 && tick % temp == 0 {
            self.mass -= 1;
            let radiation = self.spin.wrapping_mul(0x9e3779b97f4a7c15).rotate_right(13);
            self.spin ^= radiation;
            self.hawking_entropy += 1;
            return Some(radiation);
        }
        None
    }
}

// ─────────────────────────────────────────────
// VOID FS Core
// ─────────────────────────────────────────────

pub struct VoidFs {
    pub nodes:        BTreeMap<u32, AccretionNode>,
    pub black_hole:   Singularity,
    pub tick_clock:   u64,
    pub total_evap:   u64,
}

impl VoidFs {
    pub fn new() -> Self {
        Self {
            nodes: BTreeMap::new(),
            black_hole: Singularity::new(),
            tick_clock: 0,
            total_evap: 0,
        }
    }

    pub fn write_file(&mut self, inode: u32, name: String, data: Vec<u8>) {
        let node = AccretionNode::new(inode, name, data);
        self.nodes.insert(inode, node);
    }

    pub fn read_file(&mut self, inode: u32) -> Option<&[u8]> {
        if let Some(node) = self.nodes.get_mut(&inode) {
            if node.is_spaghettified {
                // File has fallen in. Only the holographic hash remains.
                return None;
            }
            node.access();
            return Some(&node.payload);
        }
        None
    }

    pub fn tick(&mut self) -> Option<u64> {
        self.tick_clock += 1;
        let mut consumed = Vec::new();

        // Decay orbits
        for (inode, node) in &mut self.nodes {
            if !node.is_spaghettified && node.tick_orbital_decay() {
                consumed.push((*inode, node.mass, node.semantic_hash));
            }
        }

        // Feed singularity
        for (_, mass, hash) in consumed {
            self.black_hole.consume(mass, hash);
        }

        // Hawking Radiation (return entropy to system)
        if let Some(entropy) = self.black_hole.hawking_radiate(self.tick_clock) {
            self.total_evap += 1;
            return Some(entropy);
        }
        None
    }

    pub fn stats(&self) -> VoidStats {
        VoidStats {
            files_in_orbit: self.nodes.iter().filter(|(_, n)| !n.is_spaghettified).count() as u32,
            files_destroyed: self.nodes.iter().filter(|(_, n)| n.is_spaghettified).count() as u32,
            black_hole_mass: self.black_hole.mass,
            hawking_radiation_events: self.total_evap,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct VoidStats {
    pub files_in_orbit: u32,
    pub files_destroyed: u32,
    pub black_hole_mass: u64,
    pub hawking_radiation_events: u64,
}
