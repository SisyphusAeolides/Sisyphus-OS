use alloc::vec::Vec;

const BUS_NODES: usize = 64; // max devices on discovery bus

/// Reaction-diffusion field on the hardware bus
pub struct ReactionDiffusionBus {
    activator: [f64; BUS_NODES], // device signal strength
    inhibitor: [f64; BUS_NODES], // bus noise / interference
    // Turing parameters (Du, Dv, f, k)
    da: f64, // activator diffusion rate
    di: f64, // inhibitor diffusion rate
    feed: f64,
    kill: f64,
}

impl ReactionDiffusionBus {
    /// Classic Gray-Scott parameters: du=0.16, dv=0.08, f=0.035, k=0.065
    pub fn new_gray_scott() -> Self {
        Self {
            activator: [0.5; BUS_NODES],
            inhibitor: [0.25; BUS_NODES],
            da: 0.16,
            di: 0.08,
            feed: 0.035,
            kill: 0.065,
        }
    }

    /// Inject a device probe pulse at a bus address
    pub fn probe(&mut self, addr: usize, strength: f64) {
        if addr < BUS_NODES {
            let mut val = self.activator[addr] + strength;
            if val > 1.0 {
                val = 1.0;
            }
            self.activator[addr] = val;
        }
    }

    /// Reaction-diffusion step (Gray-Scott model)
    /// du/dt = Da∇²u - uv² + f(1-u)
    /// dv/dt = Di∇²v + uv² - (f+k)v
    pub fn step(&mut self, dt: f64) {
        let mut new_a = self.activator;
        let mut new_i = self.inhibitor;

        for i in 0..BUS_NODES {
            let left = if i == 0 { BUS_NODES - 1 } else { i - 1 };
            let right = if i == BUS_NODES - 1 { 0 } else { i + 1 };

            // Discrete Laplacian (1D)
            let lap_a = self.activator[left] - 2.0 * self.activator[i] + self.activator[right];
            let lap_i = self.inhibitor[left] - 2.0 * self.inhibitor[i] + self.inhibitor[right];

            let u = self.activator[i];
            let v = self.inhibitor[i];
            let uv2 = u * v * v;

            new_a[i] += dt * (self.da * lap_a - uv2 + self.feed * (1.0 - u));
            new_i[i] += dt * (self.di * lap_i + uv2 - (self.feed + self.kill) * v);

            if new_a[i] < 0.0 {
                new_a[i] = 0.0;
            } else if new_a[i] > 1.0 {
                new_a[i] = 1.0;
            }

            if new_i[i] < 0.0 {
                new_i[i] = 0.0;
            } else if new_i[i] > 1.0 {
                new_i[i] = 1.0;
            }
        }
        self.activator = new_a;
        self.inhibitor = new_i;
    }

    /// Detect Turing pattern peaks — device presence indicated by local activator maxima
    pub fn detect_devices(&self) -> Vec<usize> {
        let threshold = 0.7;
        let mut devices = Vec::new();
        for i in 0..BUS_NODES {
            let left = if i == 0 { BUS_NODES - 1 } else { i - 1 };
            let right = if i == BUS_NODES - 1 { 0 } else { i + 1 };
            // Local maximum above threshold = device detected
            if self.activator[i] > threshold
                && self.activator[i] > self.activator[left]
                && self.activator[i] > self.activator[right]
            {
                devices.push(i);
            }
        }
        devices
    }

    /// Measure pattern wavelength — identifies device type by spatial frequency
    pub fn dominant_wavelength(&self) -> f64 {
        let mut crossings = 0usize;
        let mean = self.activator.iter().sum::<f64>() / BUS_NODES as f64;
        for i in 0..BUS_NODES {
            let j = (i + 1) % BUS_NODES;
            if (self.activator[i] - mean) * (self.activator[j] - mean) < 0.0 {
                crossings += 1;
            }
        }
        if crossings == 0 {
            return 0.0;
        }
        (2.0 * BUS_NODES as f64) / crossings as f64
    }
}
