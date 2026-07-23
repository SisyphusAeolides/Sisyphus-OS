use alloc::vec::Vec;

/// A window as a patch on a Riemannian 2-manifold
#[derive(Clone)]
pub struct ManifoldWindow {
    pub id: u32,
    pub position: [f64; 2],              // geodesic coordinates (u, v)
    pub velocity: [f64; 2],              // tangent vector
    pub curvature: f64,                  // Gaussian curvature K at this point
    pub metric: [[f64; 2]; 2],           // local metric tensor g_ij
    pub christoffel: [[[f64; 2]; 2]; 2], // Γ^k_ij connection coefficients
}

impl ManifoldWindow {
    pub fn new(id: u32, u: f64, v: f64) -> Self {
        Self {
            id,
            position: [u, v],
            velocity: [0.0; 2],
            curvature: 0.0,
            // Euclidean metric to start — can warp to hyperbolic/spherical
            metric: [[1.0, 0.0], [0.0, 1.0]],
            christoffel: [[[0.0; 2]; 2]; 2],
        }
    }

    /// Apply geodesic spring force toward another window
    /// F = -k * (d - rest_length) * geodesic_unit_vector
    pub fn attract_to(&mut self, other: &ManifoldWindow, k: f64, rest: f64) {
        let du = other.position[0] - self.position[0];
        let dv = other.position[1] - self.position[1];

        // Geodesic distance on current metric
        let g = &self.metric;
        let dist = libm::sqrt(g[0][0] * du * du + 2.0 * g[0][1] * du * dv + g[1][1] * dv * dv);

        if dist < 1e-9 {
            return;
        }
        let force = k * (dist - rest);
        let fu = force * du / dist;
        let fv = force * dv / dist;

        // Parallel transport the force vector along the geodesic
        // dV^k/dt = -Γ^k_ij V^i dx^j (geodesic equation)
        let gam = &self.christoffel;
        let acc_u = fu
            - gam[0][0][0] * fu * du
            - gam[0][0][1] * fu * dv
            - gam[0][1][0] * fv * du
            - gam[0][1][1] * fv * dv;
        let acc_v = fv
            - gam[1][0][0] * fu * du
            - gam[1][0][1] * fu * dv
            - gam[1][1][0] * fv * du
            - gam[1][1][1] * fv * dv;

        self.velocity[0] += acc_u * 0.016; // 60fps dt
        self.velocity[1] += acc_v * 0.016;
    }

    /// Step the window position along its geodesic
    pub fn step(&mut self) {
        let gam = &self.christoffel;
        let v = self.velocity;
        // Geodesic deviation: d²x^k/dt² = -Γ^k_ij (dx^i/dt)(dx^j/dt)
        let delta_u = -(gam[0][0][0] * v[0] * v[0]
            + 2.0 * gam[0][0][1] * v[0] * v[1]
            + gam[0][1][1] * v[1] * v[1])
            * 0.016;
        let delta_v = -(gam[1][0][0] * v[0] * v[0]
            + 2.0 * gam[1][0][1] * v[0] * v[1]
            + gam[1][1][1] * v[1] * v[1])
            * 0.016;

        self.velocity[0] += delta_u;
        self.velocity[1] += delta_v;
        self.position[0] += self.velocity[0] * 0.016;
        self.position[1] += self.velocity[1] * 0.016;
    }

    /// Warp local metric to hyperbolic (saddle) space — windows spread outward
    pub fn set_hyperbolic(&mut self, radius: f64) {
        // Poincaré disk metric: g = 4r²/(1-r²)² * I
        let p0 = self.position[0];
        let p1 = self.position[1];
        let r2 = (p0 * p0 + p1 * p1) / (radius * radius);
        let factor = 4.0 * (radius * radius) / ((1.0 - r2) * (1.0 - r2));
        self.metric = [[factor, 0.0], [0.0, factor]];
        self.curvature = -1.0 / (radius * radius); // constant negative curvature
    }

    /// Warp to spherical — windows cluster on a virtual globe
    pub fn set_spherical(&mut self, radius: f64) {
        let theta = self.position[0] / radius;
        let sin_t = libm::sin(theta);
        self.metric = [
            [radius * radius, 0.0],
            [0.0, radius * radius * sin_t * sin_t],
        ];
        self.curvature = 1.0 / (radius * radius); // constant positive curvature
    }
}

/// The manifold compositor scene graph
pub struct ManifoldScene {
    pub windows: Vec<ManifoldWindow>,
    pub global_curvature: f64, // +1 spherical, 0 flat, -1 hyperbolic
}

impl ManifoldScene {
    pub fn new(curvature: f64) -> Self {
        Self {
            windows: Vec::new(),
            global_curvature: curvature,
        }
    }

    pub fn tick(&mut self) {
        // Apply mutual geodesic attraction between related windows
        let n = self.windows.len();
        for i in 0..n {
            for j in (i + 1)..n {
                let other = self.windows[j].clone();
                self.windows[i].attract_to(&other, 0.5, 200.0);
            }
        }
        for w in &mut self.windows {
            w.step();
        }
    }
}
