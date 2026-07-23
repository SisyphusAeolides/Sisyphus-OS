//! Unconditionally stable implicit diffusion on a weighted 0-cochain.
//!
//! The step solves
//!
//! ```text
//! (M + tau * L) x_{n+1} = M x_n
//! ```
//!
//! with preconditioned conjugate gradients.  M is a positive diagonal mass
//! matrix and L is the weighted graph Laplacian.  A certificate records the
//! residual, mass defect, and Dirichlet-energy descent.

pub const MAX_VERTICES: usize = 32;
pub const MAX_EDGES: usize = 64;
pub const Q32_ONE: i64 = 1_i64 << 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HodgeSolveError {
    Capacity,
    InvalidVertex,
    InvalidWeight,
    InvalidStep,
    Arithmetic,
    Singular,
    ZeroSecret,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WeightedEdge {
    pub tail: u8,
    pub head: u8,
    pub conductance_q32: i64,
}

impl WeightedEdge {
    pub const EMPTY: Self = Self {
        tail: 0,
        head: 0,
        conductance_q32: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HodgeStepCertificate {
    pub vertices: u8,
    pub edges: u8,
    pub iterations: u16,
    pub converged: bool,
    pub tau_q32: i64,
    pub initial_energy_q32: u64,
    pub final_energy_q32: u64,
    pub residual_norm_q32: u64,
    pub mass_defect_q32: u64,
    pub state_root: u64,
    pub root: u64,
}

impl HodgeStepCertificate {
    pub const EMPTY: Self = Self {
        vertices: 0,
        edges: 0,
        iterations: 0,
        converged: false,
        tau_q32: 0,
        initial_energy_q32: 0,
        final_energy_q32: 0,
        residual_norm_q32: 0,
        mass_defect_q32: 0,
        state_root: 0,
        root: 0,
    };

    pub const fn energy_descended(self) -> bool {
        self.final_energy_q32 <= self.initial_energy_q32
    }

    pub fn verify(&self, secret: u64, residual_limit_q32: u64, mass_limit_q32: u64) -> bool {
        self.converged
            && self.energy_descended()
            && self.residual_norm_q32 <= residual_limit_q32
            && self.mass_defect_q32 <= mass_limit_q32
            && self.root == certificate_root(secret, self)
    }
}

pub struct WeightedHodgeGraph {
    vertex_count: usize,
    edge_count: usize,
    masses_q32: [i64; MAX_VERTICES],
    edges: [WeightedEdge; MAX_EDGES],
}

impl WeightedHodgeGraph {
    pub fn new(vertex_count: usize) -> Result<Self, HodgeSolveError> {
        if vertex_count == 0 || vertex_count > MAX_VERTICES {
            return Err(HodgeSolveError::Capacity);
        }

        let mut masses_q32 = [0_i64; MAX_VERTICES];
        for mass in &mut masses_q32[..vertex_count] {
            *mass = Q32_ONE;
        }

        Ok(Self {
            vertex_count,
            edge_count: 0,
            masses_q32,
            edges: [WeightedEdge::EMPTY; MAX_EDGES],
        })
    }

    pub const fn vertex_count(&self) -> usize {
        self.vertex_count
    }

    pub const fn edge_count(&self) -> usize {
        self.edge_count
    }

    pub fn set_mass(&mut self, vertex: usize, mass_q32: i64) -> Result<(), HodgeSolveError> {
        if vertex >= self.vertex_count {
            return Err(HodgeSolveError::InvalidVertex);
        }
        if mass_q32 <= 0 {
            return Err(HodgeSolveError::InvalidWeight);
        }
        self.masses_q32[vertex] = mass_q32;
        Ok(())
    }

    pub fn add_edge(
        &mut self,
        tail: usize,
        head: usize,
        conductance_q32: i64,
    ) -> Result<(), HodgeSolveError> {
        if tail >= self.vertex_count || head >= self.vertex_count || tail == head {
            return Err(HodgeSolveError::InvalidVertex);
        }
        if conductance_q32 <= 0 {
            return Err(HodgeSolveError::InvalidWeight);
        }

        let destination = self
            .edges
            .get_mut(self.edge_count)
            .ok_or(HodgeSolveError::Capacity)?;
        *destination = WeightedEdge {
            tail: tail as u8,
            head: head as u8,
            conductance_q32,
        };
        self.edge_count += 1;
        Ok(())
    }

    pub fn solve_implicit(
        &self,
        initial: &[i64; MAX_VERTICES],
        tau_q32: i64,
        maximum_iterations: u16,
        tolerance_q32: u64,
        secret: u64,
    ) -> Result<([i64; MAX_VERTICES], HodgeStepCertificate), HodgeSolveError> {
        if tau_q32 <= 0 || maximum_iterations == 0 {
            return Err(HodgeSolveError::InvalidStep);
        }
        if secret == 0 {
            return Err(HodgeSolveError::ZeroSecret);
        }

        let mut solution = *initial;
        let rhs = self.mass_apply(initial)?;
        let mut applied = [0_i64; MAX_VERTICES];
        self.system_apply(&solution, tau_q32, &mut applied)?;

        let mut residual = [0_i64; MAX_VERTICES];
        for index in 0..self.vertex_count {
            residual[index] = rhs[index]
                .checked_sub(applied[index])
                .ok_or(HodgeSolveError::Arithmetic)?;
        }

        let diagonal = self.system_diagonal(tau_q32)?;
        let mut preconditioned = [0_i64; MAX_VERTICES];
        self.precondition(&residual, &diagonal, &mut preconditioned)?;
        let mut direction = preconditioned;
        let mut rz = dot_q32(&residual, &preconditioned, self.vertex_count)?;
        if rz < 0 {
            return Err(HodgeSolveError::Singular);
        }

        let tolerance_squared = (tolerance_q32 as u128)
            .checked_mul(tolerance_q32 as u128)
            .ok_or(HodgeSolveError::Arithmetic)?
            >> 32;

        let mut iterations = 0_u16;
        let mut converged = norm_squared_q32(&residual, self.vertex_count)? <= tolerance_squared;

        while !converged && iterations < maximum_iterations {
            let mut image = [0_i64; MAX_VERTICES];
            self.system_apply(&direction, tau_q32, &mut image)?;

            let denominator = dot_q32(&direction, &image, self.vertex_count)?;
            if denominator <= 0 || rz <= 0 {
                return Err(HodgeSolveError::Singular);
            }

            let alpha_q32 = ratio_q32(rz, denominator)?;
            axpy(&mut solution, alpha_q32, &direction, self.vertex_count)?;
            axpy(&mut residual, -alpha_q32, &image, self.vertex_count)?;
            iterations = iterations.saturating_add(1);

            let residual_squared = norm_squared_q32(&residual, self.vertex_count)?;
            if residual_squared <= tolerance_squared {
                converged = true;
                break;
            }

            self.precondition(&residual, &diagonal, &mut preconditioned)?;
            let next_rz = dot_q32(&residual, &preconditioned, self.vertex_count)?;
            if next_rz < 0 {
                return Err(HodgeSolveError::Singular);
            }

            let beta_q32 = ratio_q32(next_rz, rz)?;
            for index in 0..self.vertex_count {
                direction[index] = preconditioned[index]
                    .checked_add(mul_q32(beta_q32, direction[index])?)
                    .ok_or(HodgeSolveError::Arithmetic)?;
            }
            rz = next_rz;
        }

        let initial_energy = self.dirichlet_energy(initial)?;
        let final_energy = self.dirichlet_energy(&solution)?;
        let residual_squared = norm_squared_q32(&residual, self.vertex_count)?;
        let residual_norm = integer_sqrt(
            residual_squared
                .checked_shl(32)
                .ok_or(HodgeSolveError::Arithmetic)?,
        );
        let mass_before = self.weighted_mass(initial)?;
        let mass_after = self.weighted_mass(&solution)?;
        let mass_defect = mass_before
            .checked_sub(mass_after)
            .ok_or(HodgeSolveError::Arithmetic)?
            .unsigned_abs();
        let state_root = state_root(secret, &solution, self.vertex_count);

        let mut certificate = HodgeStepCertificate {
            vertices: self.vertex_count as u8,
            edges: self.edge_count as u8,
            iterations,
            converged,
            tau_q32,
            initial_energy_q32: initial_energy,
            final_energy_q32: final_energy,
            residual_norm_q32: residual_norm.min(u64::MAX as u128) as u64,
            mass_defect_q32: mass_defect.min(u64::MAX as u128) as u64,
            state_root,
            root: 0,
        };
        certificate.root = certificate_root(secret, &certificate);

        Ok((solution, certificate))
    }

    pub fn dirichlet_energy(&self, state: &[i64; MAX_VERTICES]) -> Result<u64, HodgeSolveError> {
        let mut energy = 0_u128;

        for edge in self.edges[..self.edge_count].iter().copied() {
            let difference = state[edge.tail as usize]
                .checked_sub(state[edge.head as usize])
                .ok_or(HodgeSolveError::Arithmetic)?;
            let square = (difference as i128)
                .checked_mul(difference as i128)
                .ok_or(HodgeSolveError::Arithmetic)?;
            let weighted = square
                .checked_mul(edge.conductance_q32 as i128)
                .ok_or(HodgeSolveError::Arithmetic)?
                >> 64;

            if weighted < 0 {
                return Err(HodgeSolveError::Arithmetic);
            }
            energy = energy
                .checked_add(weighted as u128)
                .ok_or(HodgeSolveError::Arithmetic)?;
        }

        Ok((energy >> 1).min(u64::MAX as u128) as u64)
    }

    fn mass_apply(
        &self,
        input: &[i64; MAX_VERTICES],
    ) -> Result<[i64; MAX_VERTICES], HodgeSolveError> {
        let mut output = [0_i64; MAX_VERTICES];
        for index in 0..self.vertex_count {
            output[index] = mul_q32(self.masses_q32[index], input[index])?;
        }
        Ok(output)
    }

    fn system_apply(
        &self,
        input: &[i64; MAX_VERTICES],
        tau_q32: i64,
        output: &mut [i64; MAX_VERTICES],
    ) -> Result<(), HodgeSolveError> {
        *output = self.mass_apply(input)?;

        for edge in self.edges[..self.edge_count].iter().copied() {
            let tail = edge.tail as usize;
            let head = edge.head as usize;
            let gradient = input[tail]
                .checked_sub(input[head])
                .ok_or(HodgeSolveError::Arithmetic)?;
            let flux = mul_q32(edge.conductance_q32, gradient)?;
            let scaled_flux = mul_q32(tau_q32, flux)?;

            output[tail] = output[tail]
                .checked_add(scaled_flux)
                .ok_or(HodgeSolveError::Arithmetic)?;
            output[head] = output[head]
                .checked_sub(scaled_flux)
                .ok_or(HodgeSolveError::Arithmetic)?;
        }

        Ok(())
    }

    fn system_diagonal(&self, tau_q32: i64) -> Result<[i64; MAX_VERTICES], HodgeSolveError> {
        let mut diagonal = self.masses_q32;

        for edge in self.edges[..self.edge_count].iter().copied() {
            let contribution = mul_q32(tau_q32, edge.conductance_q32)?;
            for vertex in [edge.tail as usize, edge.head as usize] {
                diagonal[vertex] = diagonal[vertex]
                    .checked_add(contribution)
                    .ok_or(HodgeSolveError::Arithmetic)?;
            }
        }

        if diagonal[..self.vertex_count]
            .iter()
            .any(|entry| *entry <= 0)
        {
            return Err(HodgeSolveError::Singular);
        }

        Ok(diagonal)
    }

    fn precondition(
        &self,
        residual: &[i64; MAX_VERTICES],
        diagonal: &[i64; MAX_VERTICES],
        output: &mut [i64; MAX_VERTICES],
    ) -> Result<(), HodgeSolveError> {
        *output = [0; MAX_VERTICES];
        for index in 0..self.vertex_count {
            output[index] = div_q32(residual[index], diagonal[index])?;
        }
        Ok(())
    }

    fn weighted_mass(&self, state: &[i64; MAX_VERTICES]) -> Result<i128, HodgeSolveError> {
        let mut mass = 0_i128;
        for index in 0..self.vertex_count {
            mass = mass
                .checked_add(
                    (self.masses_q32[index] as i128)
                        .checked_mul(state[index] as i128)
                        .ok_or(HodgeSolveError::Arithmetic)?
                        >> 32,
                )
                .ok_or(HodgeSolveError::Arithmetic)?;
        }
        Ok(mass)
    }
}

pub fn from_existing_nerve(
    nerve: &crate::hodge_cech::HodgeNerve,
) -> Result<WeightedHodgeGraph, HodgeSolveError> {
    let mut graph = WeightedHodgeGraph::new(nerve.n_v)?;
    for edge in nerve.edges[..nerve.n_e.min(nerve.edges.len())]
        .iter()
        .copied()
    {
        if edge.live {
            graph.add_edge(
                edge.tail as usize,
                edge.head as usize,
                i64::from(edge.weight) * Q32_ONE,
            )?;
        }
    }
    Ok(graph)
}

fn mul_q32(left: i64, right: i64) -> Result<i64, HodgeSolveError> {
    let value = (left as i128)
        .checked_mul(right as i128)
        .ok_or(HodgeSolveError::Arithmetic)?
        >> 32;
    i64::try_from(value).map_err(|_| HodgeSolveError::Arithmetic)
}

fn div_q32(numerator: i64, denominator: i64) -> Result<i64, HodgeSolveError> {
    if denominator == 0 {
        return Err(HodgeSolveError::Singular);
    }
    let value = (numerator as i128)
        .checked_shl(32)
        .ok_or(HodgeSolveError::Arithmetic)?
        / denominator as i128;
    i64::try_from(value).map_err(|_| HodgeSolveError::Arithmetic)
}

fn ratio_q32(numerator: i128, denominator: i128) -> Result<i64, HodgeSolveError> {
    if denominator == 0 {
        return Err(HodgeSolveError::Singular);
    }
    let value = numerator
        .checked_shl(32)
        .ok_or(HodgeSolveError::Arithmetic)?
        / denominator;
    i64::try_from(value).map_err(|_| HodgeSolveError::Arithmetic)
}

fn dot_q32(
    left: &[i64; MAX_VERTICES],
    right: &[i64; MAX_VERTICES],
    length: usize,
) -> Result<i128, HodgeSolveError> {
    let mut sum = 0_i128;
    for index in 0..length {
        sum = sum
            .checked_add(
                (left[index] as i128)
                    .checked_mul(right[index] as i128)
                    .ok_or(HodgeSolveError::Arithmetic)?
                    >> 32,
            )
            .ok_or(HodgeSolveError::Arithmetic)?;
    }
    Ok(sum)
}

fn norm_squared_q32(vector: &[i64; MAX_VERTICES], length: usize) -> Result<u128, HodgeSolveError> {
    let value = dot_q32(vector, vector, length)?;
    if value < 0 {
        return Err(HodgeSolveError::Arithmetic);
    }
    Ok(value as u128)
}

fn axpy(
    target: &mut [i64; MAX_VERTICES],
    alpha_q32: i64,
    vector: &[i64; MAX_VERTICES],
    length: usize,
) -> Result<(), HodgeSolveError> {
    for index in 0..length {
        target[index] = target[index]
            .checked_add(mul_q32(alpha_q32, vector[index])?)
            .ok_or(HodgeSolveError::Arithmetic)?;
    }
    Ok(())
}

fn integer_sqrt(value: u128) -> u128 {
    if value < 2 {
        return value;
    }

    let significant_bits = 128_u32 - value.leading_zeros();
    let mut current = 1_u128 << significant_bits.div_ceil(2);
    loop {
        let next = (current + value / current) / 2;
        if next >= current {
            return current;
        }
        current = next;
    }
}

fn state_root(secret: u64, state: &[i64; MAX_VERTICES], length: usize) -> u64 {
    let mut root = mix(secret, length as u64);
    for value in &state[..length] {
        root = mix(root, *value as u64);
    }
    root
}

fn certificate_root(secret: u64, certificate: &HodgeStepCertificate) -> u64 {
    let mut state = mix(
        secret,
        certificate.vertices as u64
            | ((certificate.edges as u64) << 8)
            | ((certificate.iterations as u64) << 16),
    );
    state = mix(state, u64::from(certificate.converged));
    state = mix(state, certificate.tau_q32 as u64);
    state = mix(state, certificate.initial_energy_q32);
    state = mix(state, certificate.final_energy_q32);
    state = mix(state, certificate.residual_norm_q32);
    state = mix(state, certificate.mass_defect_q32);
    mix(state, certificate.state_root)
}

fn mix(mut state: u64, word: u64) -> u64 {
    state ^= word.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    state ^= state >> 30;
    state = state.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    state ^= state >> 27;
    state = state.wrapping_mul(0x94d0_49bb_1331_11eb);
    state ^ (state >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn implicit_heat_descends_energy() {
        let mut graph = WeightedHodgeGraph::new(3).unwrap();
        graph.add_edge(0, 1, Q32_ONE).unwrap();
        graph.add_edge(1, 2, Q32_ONE).unwrap();

        let mut state = [0_i64; MAX_VERTICES];
        state[0] = 10 * Q32_ONE;

        let (_next, certificate) = graph
            .solve_implicit(&state, Q32_ONE / 4, 32, 1 << 12, 7)
            .unwrap();

        assert!(certificate.converged);
        assert!(certificate.energy_descended());
        assert!(certificate.verify(7, 1 << 20, 1 << 20));
    }
}
