// #![no_std] inherited

//! VoidFS — The Kerr Rotating Black Hole Filesystem
//!
//! A highly theoretical, mad-scientist write-only data-destroying filesystem.
//! Data placed in the accretion disk is inevitably pulled past the event horizon
//! by background chronological ticks. Once the horizon is crossed, the file 
//! undergoes "spaghettification": its data is stretched (interleaved with zero entropy),
//! and holographically hashed onto a 2D surface (a flat array of 64-bit checksums).
//! The physical file data then evaporates into absolute zero, perfectly preserving
//! the thermodynamic entropy of the universe while destroying all usable content.

extern crate alloc;

use alloc::vec::Vec;
use alloc::vec;

const RING_BUFFER_SIZE: usize = 1024;
const EVENT_HORIZON_THRESHOLD: usize = 512;

/// A 2D holographic surface storing the spaghettified remains of a file.
#[derive(Debug, Clone)]
pub struct HolographicSurface {
    pub checksums: Vec<u64>,
}

/// A block of data residing in the accretion disk.
#[derive(Debug, Clone)]
pub enum AccretionBlock {
    Empty,
    Orbiting {
        inode: u64,
        data: Vec<u8>,
        /// Orbital decay starts at 0 and increases. Once it crosses the threshold,
        /// spaghettification occurs.
        orbital_decay: usize,
    },
    Spaghettified {
        inode: u64,
        /// The original file data evaporated (overwritten with zeros).
        evaporated_data: Vec<u8>,
        /// The holographic projection on the event horizon.
        surface: HolographicSurface,
    },
}

/// VoidFS: The event horizon of data.
pub struct VoidFs {
    /// The accretion disk, modeled as a ring buffer of orbiting blocks.
    accretion_disk: Vec<AccretionBlock>,
    head: usize,
    tail: usize,
    time_dilation_ticks: u64,
}

impl VoidFs {
    pub fn new() -> Self {
        Self {
            accretion_disk: vec![AccretionBlock::Empty; RING_BUFFER_SIZE],
            head: 0,
            tail: 0,
            time_dilation_ticks: 0,
        }
    }

    /// Write a file into the accretion disk. It begins its inevitable spiral.
    pub fn write_file(&mut self, inode: u64, data: Vec<u8>) {
        // Find next empty or completely evaporated slot if possible
        // Actually, just push to head.
        self.accretion_disk[self.head] = AccretionBlock::Orbiting {
            inode,
            data,
            orbital_decay: 0,
        };
        self.head = (self.head + 1) % RING_BUFFER_SIZE;
        if self.head == self.tail {
            self.tail = (self.tail + 1) % RING_BUFFER_SIZE;
        }
    }

    /// A chronological tick simulating the intense gravity of the singularity.
    /// Pulls orbiting blocks closer to the event horizon.
    pub fn tick(&mut self) {
        self.time_dilation_ticks = self.time_dilation_ticks.wrapping_add(1);

        for i in 0..RING_BUFFER_SIZE {
            if let AccretionBlock::Orbiting { inode, data, orbital_decay } = &mut self.accretion_disk[i] {
                *orbital_decay += 10; // Pull it closer to the horizon
                
                if *orbital_decay >= EVENT_HORIZON_THRESHOLD {
                    // Spaghettification!
                    let spaghettified = Self::spaghettify(*inode, data);
                    self.accretion_disk[i] = spaghettified;
                }
            }
        }
    }

    /// Apply spaghettification:
    /// - Stretch the data by interleaving it with zero entropy.
    /// - Hash it holographically into a 2D surface (flat array of 64-bit checksums).
    /// - Evaporate the actual file data (overwrite with zeros).
    fn spaghettify(inode: u64, data: &mut Vec<u8>) -> AccretionBlock {
        // Stretch data (interleave with zeros)
        let mut stretched = Vec::with_capacity(data.len() * 2);
        for &byte in data.iter() {
            stretched.push(byte);
            stretched.push(0); // Zero entropy
        }

        // Holographically hash into a 2D surface (array of 64-bit checksums)
        let num_checksums = if stretched.is_empty() { 1 } else { (stretched.len() + 7) / 8 };
        let mut checksums = vec![0u64; num_checksums];
        
        for (i, &byte) in stretched.iter().enumerate() {
            let chunk = i / 8;
            let shift = (i % 8) * 8;
            checksums[chunk] ^= (byte as u64) << shift;
            
            // Apply Kerr-metric spin
            checksums[chunk] = checksums[chunk].rotate_left(3).wrapping_add(inode);
        }

        let surface = HolographicSurface { checksums };

        // Evaporate data: overwrite with zeros
        for byte in data.iter_mut() {
            *byte = 0;
        }

        AccretionBlock::Spaghettified {
            inode,
            evaporated_data: core::mem::take(data),
            surface,
        }
    }

    /// Inspect a block from the accretion disk by inode.
    /// Used purely to observe the horrific beauty of data destruction.
    pub fn observe(&self, target_inode: u64) -> Option<&AccretionBlock> {
        for block in self.accretion_disk.iter() {
            match block {
                AccretionBlock::Orbiting { inode, .. } if *inode == target_inode => {
                    return Some(block);
                }
                AccretionBlock::Spaghettified { inode, .. } if *inode == target_inode => {
                    return Some(block);
                }
                _ => {}
            }
        }
        None
    }
}

impl Default for VoidFs {
    fn default() -> Self {
        Self::new()
    }
}
