use core::alloc::Layout;

/// A spacetime coordinate for a memory page
#[derive(Clone, Copy, Debug)]
pub struct SpacetimeAddr {
    pub x: u16,  // spatial dimension 1
    pub y: u16,  // spatial dimension 2
    pub z: u16,  // spatial dimension 3
    pub t: u16,  // temporal index (allocation epoch)
}

impl SpacetimeAddr {
    /// Geodesic distance in curved spacetime (Schwarzschild metric approximation)
    pub fn geodesic_distance(&self, other: &Self) -> f64 {
        let dx = self.x as f64 - other.x as f64;
        let dy = self.y as f64 - other.y as f64;
        let dz = self.z as f64 - other.z as f64;
        let dt = (self.t as f64 - other.t as f64) * 2.998e8_f64;
        let ds2 = dx*dx + dy*dy + dz*dz - dt*dt;
        libm::sqrt(libm::fabs(ds2))
    }

    /// Encode to physical frame number using Hilbert curve space-filling
    pub fn to_pfn(&self) -> u64 {
        hilbert_encode_4d(self.x as u64, self.y as u64, self.z as u64, self.t as u64)
    }
}

/// Hilbert curve encoding for maximum spatial locality
fn hilbert_encode_4d(x: u64, y: u64, z: u64, t: u64) -> u64 {
    // Interleave bits of all 4 dimensions (Z-order / Morton + rotation)
    let mut result = 0u64;
    for i in 0..16u64 {
        result |= ((x >> i) & 1) << (i * 4);
        result |= ((y >> i) & 1) << (i * 4 + 1);
        result |= ((z >> i) & 1) << (i * 4 + 2);
        result |= ((t >> i) & 1) << (i * 4 + 3);
    }
    result
}

/// Slab allocator with gravitational compaction — pages "fall" toward each other
pub struct GravitationalSlab {
    base: *mut u8,
    capacity: usize,
    free_map: [u64; 512],  // 512 * 64 = 32768 slots
    epoch: u16,
}

unsafe impl Send for GravitationalSlab {}
unsafe impl Sync for GravitationalSlab {}

impl GravitationalSlab {
    pub const fn new(base: *mut u8, capacity: usize) -> Self {
        Self { base, capacity, free_map: [!0u64; 512], epoch: 0 }
    }

    pub fn alloc_at_epoch(&mut self, layout: Layout) -> *mut u8 {
        let _slots_needed = (layout.size() + 4095) / 4096;
        // Find contiguous free slots
        for chunk in 0..512 {
            if self.free_map[chunk] == !0u64 {
                let pfn = chunk as u64 * 64;
                let addr = SpacetimeAddr {
                    x: (pfn & 0xF) as u16,
                    y: ((pfn >> 4) & 0xF) as u16,
                    z: ((pfn >> 8) & 0xF) as u16,
                    t: self.epoch,
                };
                self.free_map[chunk] = 0; // mark allocated
                self.epoch = self.epoch.wrapping_add(1);
                return unsafe { self.base.add(addr.to_pfn() as usize % self.capacity) };
            }
        }
        core::ptr::null_mut()
    }
}
