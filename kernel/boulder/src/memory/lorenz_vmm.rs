use alloc::collections::BTreeMap;

/// Lorenz attractor state (σ=10, ρ=28, β=8/3 — classic chaotic parameters)
#[derive(Clone, Copy)]
pub struct LorenzState {
    pub x: f64, // access frequency axis
    pub y: f64, // recency axis
    pub z: f64, // spatial locality axis
}

impl LorenzState {
    pub const SIGMA: f64 = 10.0;
    pub const RHO: f64 = 28.0;
    pub const BETA: f64 = 2.667;

    pub fn new(freq: f64, recency: f64, locality: f64) -> Self {
        Self { x: freq, y: recency, z: locality }
    }

    /// Runge-Kutta 4th order step on Lorenz equations
    /// dx/dt = σ(y - x)
    /// dy/dt = x(ρ - z) - y
    /// dz/dt = xy - βz
    pub fn step(&mut self, dt: f64) {
        let lorenz = |s: LorenzState| -> (f64, f64, f64) {
            (
                Self::SIGMA * (s.y - s.x),
                s.x * (Self::RHO - s.z) - s.y,
                s.x * s.y - Self::BETA * s.z,
            )
        };

        let (k1x, k1y, k1z) = lorenz(*self);
        let s2 = LorenzState { x: self.x + k1x*dt/2.0, y: self.y + k1y*dt/2.0, z: self.z + k1z*dt/2.0 };
        let (k2x, k2y, k2z) = lorenz(s2);
        let s3 = LorenzState { x: self.x + k2x*dt/2.0, y: self.y + k2y*dt/2.0, z: self.z + k2z*dt/2.0 };
        let (k3x, k3y, k3z) = lorenz(s3);
        let s4 = LorenzState { x: self.x + k3x*dt, y: self.y + k3y*dt, z: self.z + k3z*dt };
        let (k4x, k4y, k4z) = lorenz(s4);

        self.x += dt/6.0 * (k1x + 2.0*k2x + 2.0*k3x + k4x);
        self.y += dt/6.0 * (k1y + 2.0*k2y + 2.0*k3y + k4y);
        self.z += dt/6.0 * (k1z + 2.0*k2z + 2.0*k3z + k4z);
    }

    /// Distance from the Lorenz attractor wings' center points
    /// Wing centers approximately at (±sqrt(β(ρ-1)), ±sqrt(β(ρ-1)), ρ-1)
    pub fn attractor_distance(&self) -> f64 {
        let center_val = libm::sqrt(Self::BETA * (Self::RHO - 1.0));
        let x1 = self.x - center_val;
        let y1 = self.y - center_val;
        let z1 = self.z - (Self::RHO - 1.0);
        let d1 = libm::sqrt(x1*x1 + y1*y1 + z1*z1);
        
        let x2 = self.x + center_val;
        let y2 = self.y + center_val;
        let z2 = self.z - (Self::RHO - 1.0);
        let d2 = libm::sqrt(x2*x2 + y2*y2 + z2*z2);
        
        if d1 < d2 { d1 } else { d2 }
    }
}

pub struct LorenzPageEntry {
    pub pfn: u64,
    pub state: LorenzState,
    pub pinned: bool,
}

pub struct LorenzVMM {
    pages: BTreeMap<u64, LorenzPageEntry>, // vaddr → entry
    capacity: usize,
    dt: f64,
}

impl LorenzVMM {
    pub fn new(capacity: usize) -> Self {
        Self { pages: BTreeMap::new(), capacity, dt: 0.01 }
    }

    pub fn access(&mut self, vaddr: u64) {
        if let Some(entry) = self.pages.get_mut(&vaddr) {
            // Access increases frequency (x) and resets recency (y)
            let new_x = entry.state.x + 5.0;
            entry.state.x = if new_x < 50.0 { new_x } else { 50.0 };
            entry.state.y = 28.0;
            entry.state.step(self.dt);
        }
    }

    /// Age all pages — trajectory evolves on attractor
    pub fn age_tick(&mut self) {
        for entry in self.pages.values_mut() {
            if !entry.pinned {
                let new_x = entry.state.x - 0.1;
                entry.state.x = if new_x > 0.0 { new_x } else { 0.0 }; // frequency decays
                entry.state.step(self.dt);
            }
        }
    }

    /// Evict the page furthest from the attractor basin
    pub fn evict_one(&mut self) -> Option<u64> {
        if self.pages.len() < self.capacity { return None; }
        let victim = self.pages.iter()
            .filter(|(_, e)| !e.pinned)
            .max_by(|(_, a), (_, b)| {
                a.state.attractor_distance()
                    .partial_cmp(&b.state.attractor_distance())
                    .unwrap()
            })
            .map(|(&vaddr, _)| vaddr)?;
        self.pages.remove(&victim);
        Some(victim)
    }
}
