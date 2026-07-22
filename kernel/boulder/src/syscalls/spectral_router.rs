use alloc::vec::Vec;

const MAX_SYSCALLS: usize = 512;

pub struct SyscallGraph {
    // Adjacency as co-occurrence weights — syscalls called together get high weight
    adjacency: [[f32; MAX_SYSCALLS]; MAX_SYSCALLS],
    call_counts: [u64; MAX_SYSCALLS],
    last_syscall: Option<usize>,
}

impl SyscallGraph {
    pub fn new() -> Self {
        Self {
            adjacency: [[0.0; MAX_SYSCALLS]; MAX_SYSCALLS],
            call_counts: [0u64; MAX_SYSCALLS],
            last_syscall: None,
        }
    }

    /// Record a syscall — builds co-occurrence graph online
    pub fn record(&mut self, syscall_id: usize) {
        if syscall_id >= MAX_SYSCALLS { return; }
        self.call_counts[syscall_id] += 1;
        if let Some(prev) = self.last_syscall {
            // Reinforce edge between consecutive syscalls
            self.adjacency[prev][syscall_id] += 1.0;
            self.adjacency[syscall_id][prev] += 1.0;
        }
        self.last_syscall = Some(syscall_id);
    }

    /// Compute degree vector D_ii = Σ_j A_ij
    fn degree(&self) -> [f32; MAX_SYSCALLS] {
        let mut d = [0.0f32; MAX_SYSCALLS];
        for i in 0..MAX_SYSCALLS {
            d[i] = self.adjacency[i].iter().sum();
        }
        d
    }

    /// Power iteration for Fiedler vector approximation
    /// (second eigenvector of normalized Laplacian L = D - A)
    /// Returns partition: true = fast path, false = slow path
    pub fn spectral_partition(&self, iters: usize) -> [bool; MAX_SYSCALLS] {
        let degree = self.degree();
        // Initialize random-ish vector (use call counts as proxy)
        let mut v: [f32; MAX_SYSCALLS] = [0.0; MAX_SYSCALLS];
        for i in 0..MAX_SYSCALLS {
            v[i] = libm::logf(self.call_counts[i] as f32 + 1.0);
        }

        // Orthogonalize against constant vector (remove Fiedler_0)
        let mean = v.iter().sum::<f32>() / MAX_SYSCALLS as f32;
        for x in &mut v { *x -= mean; }

        // Power iteration on L_normalized
        for _ in 0..iters {
            let mut new_v = [0.0f32; MAX_SYSCALLS];
            for i in 0..MAX_SYSCALLS {
                let di = if degree[i] < 1.0 { 1.0 } else { degree[i] };
                for j in 0..MAX_SYSCALLS {
                    // L_sym = I - D^{-1/2} A D^{-1/2}
                    let dj = if degree[j] < 1.0 { 1.0 } else { degree[j] };
                    let l_ij = if i == j { 1.0 }
                               else { -self.adjacency[i][j] / libm::sqrtf(di * dj) };
                    new_v[i] += l_ij * v[j];
                }
            }
            // Re-orthogonalize and normalize
            let m = new_v.iter().sum::<f32>() / MAX_SYSCALLS as f32;
            for x in &mut new_v { *x -= m; }
            let mut norm = libm::sqrtf(new_v.iter().map(|x| x*x).sum::<f32>());
            if norm < 1e-9 { norm = 1e-9; }
            for x in &mut new_v { *x /= norm; }
            v = new_v;
        }

        // Partition by sign of Fiedler vector
        let mut partition = [false; MAX_SYSCALLS];
        for i in 0..MAX_SYSCALLS {
            partition[i] = v[i] > 0.0; // positive = fast path
        }
        partition
    }

    /// Hot syscalls: partition = true AND high call count
    pub fn fast_path_syscalls(&self) -> Vec<usize> {
        let partition = self.spectral_partition(20);
        (0..MAX_SYSCALLS)
            .filter(|&i| partition[i] && self.call_counts[i] > 100)
            .collect()
    }
}
