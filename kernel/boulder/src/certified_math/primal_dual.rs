//! Bounded primal-dual resource allocation.
//!
//! The solver handles a diagonal convex quadratic objective with linear
//! inequalities and box constraints:
//!
//!   minimize  1/2 x^T Q x + c^T x
//!   subject to A x <= b, lower <= x <= upper.
//!
//! A projected Arrow-Hurwicz iteration produces a candidate.  The candidate
//! is usable only when the returned KKT certificate satisfies independently
//! configured feasibility, stationarity, and complementarity bounds.

pub const MAX_VARIABLES: usize = 16;
pub const MAX_CONSTRAINTS: usize = 16;
pub const Q32_ONE: i64 = 1_i64 << 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OptimizationError {
    InvalidDimension,
    InvalidBounds,
    NonConvex,
    InvalidStep,
    Arithmetic,
    ZeroSecret,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuadraticProgram {
    pub variables: usize,
    pub constraints: usize,
    pub diagonal_q32: [i64; MAX_VARIABLES],
    pub linear_q32: [i64; MAX_VARIABLES],
    pub lower_q32: [i64; MAX_VARIABLES],
    pub upper_q32: [i64; MAX_VARIABLES],
    pub matrix_q32: [[i64; MAX_VARIABLES]; MAX_CONSTRAINTS],
    pub bound_q32: [i64; MAX_CONSTRAINTS],
}

impl QuadraticProgram {
    pub const EMPTY: Self = Self {
        variables: 0,
        constraints: 0,
        diagonal_q32: [0; MAX_VARIABLES],
        linear_q32: [0; MAX_VARIABLES],
        lower_q32: [0; MAX_VARIABLES],
        upper_q32: [0; MAX_VARIABLES],
        matrix_q32: [[0; MAX_VARIABLES]; MAX_CONSTRAINTS],
        bound_q32: [0; MAX_CONSTRAINTS],
    };

    pub fn validate(&self) -> Result<(), OptimizationError> {
        if self.variables == 0
            || self.variables > MAX_VARIABLES
            || self.constraints > MAX_CONSTRAINTS
        {
            return Err(OptimizationError::InvalidDimension);
        }

        for variable in 0..self.variables {
            if self.diagonal_q32[variable] <= 0 {
                return Err(OptimizationError::NonConvex);
            }
            if self.lower_q32[variable] > self.upper_q32[variable] {
                return Err(OptimizationError::InvalidBounds);
            }
        }

        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OptimizationCertificate {
    pub variables: u8,
    pub constraints: u8,
    pub iterations: u16,
    pub objective_q32: i64,
    pub maximum_primal_violation_q32: u64,
    pub maximum_dual_violation_q32: u64,
    pub stationarity_residual_q32: u64,
    pub complementarity_residual_q32: u64,
    pub primal_root: u64,
    pub dual_root: u64,
    pub root: u64,
}

impl OptimizationCertificate {
    pub const EMPTY: Self = Self {
        variables: 0,
        constraints: 0,
        iterations: 0,
        objective_q32: 0,
        maximum_primal_violation_q32: 0,
        maximum_dual_violation_q32: 0,
        stationarity_residual_q32: 0,
        complementarity_residual_q32: 0,
        primal_root: 0,
        dual_root: 0,
        root: 0,
    };

    pub fn verify(
        &self,
        secret: u64,
        primal_limit_q32: u64,
        dual_limit_q32: u64,
        stationarity_limit_q32: u64,
        complementarity_limit_q32: u64,
    ) -> bool {
        self.maximum_primal_violation_q32 <= primal_limit_q32
            && self.maximum_dual_violation_q32 <= dual_limit_q32
            && self.stationarity_residual_q32 <= stationarity_limit_q32
            && self.complementarity_residual_q32 <= complementarity_limit_q32
            && self.root == certificate_root(secret, self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OptimizationResult {
    pub primal_q32: [i64; MAX_VARIABLES],
    pub dual_q32: [i64; MAX_CONSTRAINTS],
    pub certificate: OptimizationCertificate,
}

pub struct PrimalDualSolver {
    primal_step_q32: i64,
    dual_step_q32: i64,
    maximum_iterations: u16,
    secret: u64,
}

impl PrimalDualSolver {
    pub fn new(
        primal_step_q32: i64,
        dual_step_q32: i64,
        maximum_iterations: u16,
        secret: u64,
    ) -> Result<Self, OptimizationError> {
        if primal_step_q32 <= 0 || dual_step_q32 <= 0 || maximum_iterations == 0 {
            return Err(OptimizationError::InvalidStep);
        }
        if secret == 0 {
            return Err(OptimizationError::ZeroSecret);
        }

        Ok(Self {
            primal_step_q32,
            dual_step_q32,
            maximum_iterations,
            secret,
        })
    }

    pub fn solve(
        &self,
        program: &QuadraticProgram,
        initial_q32: &[i64; MAX_VARIABLES],
    ) -> Result<OptimizationResult, OptimizationError> {
        program.validate()?;

        let mut primal = [0_i64; MAX_VARIABLES];
        for variable in 0..program.variables {
            primal[variable] = initial_q32[variable]
                .clamp(program.lower_q32[variable], program.upper_q32[variable]);
        }
        let mut dual = [0_i64; MAX_CONSTRAINTS];

        for _ in 0..self.maximum_iterations {
            let gradient = lagrangian_gradient(program, &primal, &dual)?;

            for variable in 0..program.variables {
                let step = mul_q32(self.primal_step_q32, gradient[variable])?;
                primal[variable] = primal[variable]
                    .checked_sub(step)
                    .ok_or(OptimizationError::Arithmetic)?
                    .clamp(program.lower_q32[variable], program.upper_q32[variable]);
            }

            let residuals = constraint_residuals(program, &primal)?;
            for constraint in 0..program.constraints {
                let step = mul_q32(self.dual_step_q32, residuals[constraint])?;
                dual[constraint] = dual[constraint]
                    .checked_add(step)
                    .ok_or(OptimizationError::Arithmetic)?
                    .max(0);
            }
        }

        let certificate = certify(
            self.secret,
            program,
            &primal,
            &dual,
            self.maximum_iterations,
        )?;

        Ok(OptimizationResult {
            primal_q32: primal,
            dual_q32: dual,
            certificate,
        })
    }
}

fn lagrangian_gradient(
    program: &QuadraticProgram,
    primal: &[i64; MAX_VARIABLES],
    dual: &[i64; MAX_CONSTRAINTS],
) -> Result<[i64; MAX_VARIABLES], OptimizationError> {
    let mut gradient = [0_i64; MAX_VARIABLES];

    for variable in 0..program.variables {
        gradient[variable] = mul_q32(program.diagonal_q32[variable], primal[variable])?
            .checked_add(program.linear_q32[variable])
            .ok_or(OptimizationError::Arithmetic)?;
    }

    for constraint in 0..program.constraints {
        for variable in 0..program.variables {
            let contribution = mul_q32(program.matrix_q32[constraint][variable], dual[constraint])?;
            gradient[variable] = gradient[variable]
                .checked_add(contribution)
                .ok_or(OptimizationError::Arithmetic)?;
        }
    }

    Ok(gradient)
}

fn constraint_residuals(
    program: &QuadraticProgram,
    primal: &[i64; MAX_VARIABLES],
) -> Result<[i64; MAX_CONSTRAINTS], OptimizationError> {
    let mut residuals = [0_i64; MAX_CONSTRAINTS];

    for constraint in 0..program.constraints {
        let mut value = 0_i64;
        for variable in 0..program.variables {
            value = value
                .checked_add(mul_q32(
                    program.matrix_q32[constraint][variable],
                    primal[variable],
                )?)
                .ok_or(OptimizationError::Arithmetic)?;
        }
        residuals[constraint] = value
            .checked_sub(program.bound_q32[constraint])
            .ok_or(OptimizationError::Arithmetic)?;
    }

    Ok(residuals)
}

fn certify(
    secret: u64,
    program: &QuadraticProgram,
    primal: &[i64; MAX_VARIABLES],
    dual: &[i64; MAX_CONSTRAINTS],
    iterations: u16,
) -> Result<OptimizationCertificate, OptimizationError> {
    let residuals = constraint_residuals(program, primal)?;
    let gradient = lagrangian_gradient(program, primal, dual)?;

    let mut maximum_primal_violation = 0_u64;
    let mut maximum_dual_violation = 0_u64;
    let mut complementarity = 0_u64;
    let mut stationarity = 0_u64;

    for constraint in 0..program.constraints {
        maximum_primal_violation =
            maximum_primal_violation.max(residuals[constraint].max(0) as u64);
        maximum_dual_violation = maximum_dual_violation.max((-dual[constraint]).max(0) as u64);

        let product = mul_q32(dual[constraint], residuals[constraint])?.unsigned_abs();
        complementarity = complementarity.max(product);
    }

    for variable in 0..program.variables {
        let projected = primal[variable]
            .checked_sub(gradient[variable])
            .ok_or(OptimizationError::Arithmetic)?
            .clamp(program.lower_q32[variable], program.upper_q32[variable]);
        stationarity = stationarity.max(primal[variable].abs_diff(projected));
    }

    let objective = objective(program, primal)?;
    let primal_root = vector_root(secret, primal, program.variables);
    let dual_root = vector_root(mix(secret, primal_root), dual, program.constraints);

    let mut certificate = OptimizationCertificate {
        variables: program.variables as u8,
        constraints: program.constraints as u8,
        iterations,
        objective_q32: objective,
        maximum_primal_violation_q32: maximum_primal_violation,
        maximum_dual_violation_q32: maximum_dual_violation,
        stationarity_residual_q32: stationarity,
        complementarity_residual_q32: complementarity,
        primal_root,
        dual_root,
        root: 0,
    };
    certificate.root = certificate_root(secret, &certificate);
    Ok(certificate)
}

fn objective(
    program: &QuadraticProgram,
    primal: &[i64; MAX_VARIABLES],
) -> Result<i64, OptimizationError> {
    let mut value = 0_i64;

    for variable in 0..program.variables {
        let square = mul_q32(primal[variable], primal[variable])?;
        let quadratic = mul_q32(program.diagonal_q32[variable], square)? / 2;
        let linear = mul_q32(program.linear_q32[variable], primal[variable])?;

        value = value
            .checked_add(quadratic)
            .and_then(|current| current.checked_add(linear))
            .ok_or(OptimizationError::Arithmetic)?;
    }

    Ok(value)
}

fn mul_q32(left: i64, right: i64) -> Result<i64, OptimizationError> {
    let product = (left as i128)
        .checked_mul(right as i128)
        .ok_or(OptimizationError::Arithmetic)?
        >> 32;
    i64::try_from(product).map_err(|_| OptimizationError::Arithmetic)
}

fn vector_root<const N: usize>(secret: u64, vector: &[i64; N], length: usize) -> u64 {
    let mut state = mix(secret, length as u64);
    for value in &vector[..length] {
        state = mix(state, *value as u64);
    }
    state
}

fn certificate_root(secret: u64, certificate: &OptimizationCertificate) -> u64 {
    let mut state = mix(
        secret,
        certificate.variables as u64
            | ((certificate.constraints as u64) << 8)
            | ((certificate.iterations as u64) << 16),
    );
    state = mix(state, certificate.objective_q32 as u64);
    state = mix(state, certificate.maximum_primal_violation_q32);
    state = mix(state, certificate.maximum_dual_violation_q32);
    state = mix(state, certificate.stationarity_residual_q32);
    state = mix(state, certificate.complementarity_residual_q32);
    state = mix(state, certificate.primal_root);
    mix(state, certificate.dual_root)
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
    fn solves_a_bounded_capacity_allocation() {
        let mut program = QuadraticProgram::EMPTY;
        program.variables = 2;
        program.constraints = 1;
        program.diagonal_q32[0] = Q32_ONE;
        program.diagonal_q32[1] = Q32_ONE;
        program.linear_q32[0] = -3 * Q32_ONE;
        program.linear_q32[1] = -2 * Q32_ONE;
        program.lower_q32[0] = 0;
        program.lower_q32[1] = 0;
        program.upper_q32[0] = 4 * Q32_ONE;
        program.upper_q32[1] = 4 * Q32_ONE;
        program.matrix_q32[0][0] = Q32_ONE;
        program.matrix_q32[0][1] = Q32_ONE;
        program.bound_q32[0] = 4 * Q32_ONE;

        let solver = PrimalDualSolver::new(Q32_ONE / 16, Q32_ONE / 16, 256, 7).unwrap();
        let result = solver.solve(&program, &[0; MAX_VARIABLES]).unwrap();

        assert!(result.certificate.maximum_primal_violation_q32 < Q32_ONE as u64 / 8);
        assert!(result.certificate.root != 0);
    }
}
