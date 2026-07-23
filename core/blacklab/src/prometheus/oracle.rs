use alloc::{collections::BTreeMap, vec::Vec};

/// 2-state Kalman filter per service: state = [cpu_usage, mem_usage]
/// Models process health as a linear dynamical system
pub struct ServiceKalman {
    pub x: [f64; 2],      // state estimate [cpu%, mem_mb]
    pub p: [[f64; 2]; 2], // error covariance matrix
    // System model: x_k+1 = F*x_k + noise
    pub f: [[f64; 2]; 2],   // state transition (near-identity for slow drift)
    pub q: [[f64; 2]; 2],   // process noise covariance
    pub r: [[f64; 2]; 2],   // measurement noise covariance
    pub anomaly_score: f64, // Mahalanobis distance from healthy trajectory
    pub predicted_ttf: f64, // time-to-failure in seconds (negative = already dead)
}

impl ServiceKalman {
    pub fn new(initial_cpu: f64, initial_mem: f64) -> Self {
        Self {
            x: [initial_cpu, initial_mem],
            p: [[1.0, 0.0], [0.0, 1.0]],
            // Slight drift model: assume 0.1% state growth per tick
            f: [[1.001, 0.0], [0.0, 1.001]],
            q: [[0.01, 0.0], [0.0, 0.01]], // small process noise
            r: [[0.5, 0.0], [0.0, 0.5]],   // measurement noise
            anomaly_score: 0.0,
            predicted_ttf: core::f64::INFINITY,
        }
    }

    /// Predict step: project state forward one tick
    pub fn predict(&mut self) {
        // x_pred = F * x
        let x0 = self.f[0][0] * self.x[0] + self.f[0][1] * self.x[1];
        let x1 = self.f[1][0] * self.x[0] + self.f[1][1] * self.x[1];
        self.x = [x0, x1];

        // P_pred = F*P*F^T + Q (2x2 manual multiply)
        let fp00 = self.f[0][0] * self.p[0][0] + self.f[0][1] * self.p[1][0];
        let fp01 = self.f[0][0] * self.p[0][1] + self.f[0][1] * self.p[1][1];
        let fp10 = self.f[1][0] * self.p[0][0] + self.f[1][1] * self.p[1][0];
        let fp11 = self.f[1][0] * self.p[0][1] + self.f[1][1] * self.p[1][1];

        self.p = [
            [
                fp00 * self.f[0][0] + fp01 * self.f[0][1] + self.q[0][0],
                fp00 * self.f[1][0] + fp01 * self.f[1][1] + self.q[0][1],
            ],
            [
                fp10 * self.f[0][0] + fp11 * self.f[0][1] + self.q[1][0],
                fp10 * self.f[1][0] + fp11 * self.f[1][1] + self.q[1][1],
            ],
        ];
    }

    /// Update step: fuse measurement with prediction
    pub fn update(&mut self, measured_cpu: f64, measured_mem: f64) {
        let z = [measured_cpu, measured_mem];
        // Innovation y = z - H*x (H = I for direct observation)
        let y = [z[0] - self.x[0], z[1] - self.x[1]];

        // S = P + R (innovation covariance)
        let s = [
            [self.p[0][0] + self.r[0][0], self.p[0][1] + self.r[0][1]],
            [self.p[1][0] + self.r[1][0], self.p[1][1] + self.r[1][1]],
        ];

        // K = P * S^{-1} (Kalman gain, 2x2 inverse)
        let det_val = s[0][0] * s[1][1] - s[0][1] * s[1][0];
        let abs_det = if det_val < 0.0 { -det_val } else { det_val };
        let det = if abs_det < 1e-12 { 1e-12 } else { det_val };

        let s_inv = [
            [s[1][1] / det, -s[0][1] / det],
            [-s[1][0] / det, s[0][0] / det],
        ];

        let k = [
            [
                self.p[0][0] * s_inv[0][0] + self.p[0][1] * s_inv[1][0],
                self.p[0][0] * s_inv[0][1] + self.p[0][1] * s_inv[1][1],
            ],
            [
                self.p[1][0] * s_inv[0][0] + self.p[1][1] * s_inv[1][0],
                self.p[1][0] * s_inv[0][1] + self.p[1][1] * s_inv[1][1],
            ],
        ];

        // x = x + K*y
        self.x[0] += k[0][0] * y[0] + k[0][1] * y[1];
        self.x[1] += k[1][0] * y[0] + k[1][1] * y[1];

        // P = (I - K)*P
        self.p = [
            [
                (1.0 - k[0][0]) * self.p[0][0] - k[0][1] * self.p[1][0],
                (1.0 - k[0][0]) * self.p[0][1] - k[0][1] * self.p[1][1],
            ],
            [
                -k[1][0] * self.p[0][0] + (1.0 - k[1][1]) * self.p[1][0],
                -k[1][0] * self.p[0][1] + (1.0 - k[1][1]) * self.p[1][1],
            ],
        ];

        // Mahalanobis anomaly score: d² = y^T * S^{-1} * y
        self.anomaly_score = y[0] * (s_inv[0][0] * y[0] + s_inv[0][1] * y[1])
            + y[1] * (s_inv[1][0] * y[0] + s_inv[1][1] * y[1]);

        // Project time-to-failure: how many ticks until x exceeds 95% cpu or 90% mem?
        let ch = 95.0 - self.x[0];
        let cpu_headroom = if ch > 0.0 { ch } else { 0.0 };

        let mh = 90.0 - self.x[1];
        let mem_headroom = if mh > 0.0 { mh } else { 0.0 };

        let drift_cpu = (self.f[0][0] - 1.0) * self.x[0]; // drift per tick
        let drift_mem = (self.f[1][1] - 1.0) * self.x[1];

        let ttf_cpu = if drift_cpu > 1e-9 {
            cpu_headroom / drift_cpu
        } else {
            core::f64::INFINITY
        };
        let ttf_mem = if drift_mem > 1e-9 {
            mem_headroom / drift_mem
        } else {
            core::f64::INFINITY
        };

        self.predicted_ttf = if ttf_cpu < ttf_mem { ttf_cpu } else { ttf_mem };
    }

    /// Is this service heading for a crash?
    pub fn is_precritical(&self) -> bool {
        self.anomaly_score > 9.21 || // chi-squared p<0.01 threshold for 2DOF
        self.predicted_ttf < 30.0 // less than 30 ticks to projected failure
    }
}

/// The Oracle — PID 1's precognitive supervision engine
pub struct OracleSupervisor {
    filters: BTreeMap<u32, ServiceKalman>, // pid → Kalman state
    pre_spawned: BTreeMap<u32, u32>,       // original_pid → standby_replica_pid
}

impl OracleSupervisor {
    pub fn new() -> Self {
        Self {
            filters: BTreeMap::new(),
            pre_spawned: BTreeMap::new(),
        }
    }

    pub fn register(&mut self, pid: u32, cpu: f64, mem: f64) {
        self.filters.insert(pid, ServiceKalman::new(cpu, mem));
    }

    pub fn observe(&mut self, pid: u32, cpu: f64, mem: f64) {
        if let Some(kf) = self.filters.get_mut(&pid) {
            kf.predict();
            kf.update(cpu, mem);
        }
    }

    /// Returns PIDs that need pre-emptive hot-swap NOW
    pub fn precritical_services(&self) -> Vec<(u32, f64)> {
        self.filters
            .iter()
            .filter(|(_, kf)| kf.is_precritical())
            .map(|(&pid, kf)| (pid, kf.predicted_ttf))
            .collect()
    }

    /// Called when PID 1 spawns a standby replica before original fails
    pub fn register_hot_standby(&mut self, original: u32, replica: u32) {
        self.pre_spawned.insert(original, replica);
    }

    /// When original dies — promote standby instantly, zero downtime
    pub fn promote_standby(&mut self, dead_pid: u32) -> Option<u32> {
        self.filters.remove(&dead_pid);
        self.pre_spawned.remove(&dead_pid)
    }
}
