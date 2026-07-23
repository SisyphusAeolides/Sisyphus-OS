//! Adapters from the current Boulder mathematical modules into the certified
//! control layer.

use super::hodge_implicit::{HodgeSolveError, Q32_ONE as HODGE_Q32_ONE, WeightedHodgeGraph};
use super::persistent::{FilteredComplex, PersistenceError, Simplex};
use super::primal_dual::{
    MAX_VARIABLES, OptimizationError, Q32_ONE as OPT_Q32_ONE, QuadraticProgram,
};
use super::tropical::{MAX_CLUSTER_NODES, TropicalCluster, TropicalError};

pub fn graph_from_hodge(
    nerve: &crate::hodge_cech::HodgeNerve,
) -> Result<WeightedHodgeGraph, HodgeSolveError> {
    super::hodge_implicit::from_existing_nerve(nerve)
}

pub fn populate_filtered_complex_from_hodge(
    nerve: &crate::hodge_cech::HodgeNerve,
    complex: &mut FilteredComplex,
) -> Result<(), PersistenceError> {
    complex.clear();

    if nerve.n_v > nerve.f0.len() || nerve.n_e > nerve.edges.len() || nerve.n_f > nerve.faces.len()
    {
        return Err(PersistenceError::Capacity);
    }

    for vertex in 0..nerve.n_v {
        complex.insert(Simplex::vertex(vertex as u8, 0, vertex as u64))?;
    }

    for (index, edge) in nerve.edges[..nerve.n_e.min(nerve.edges.len())]
        .iter()
        .copied()
        .enumerate()
    {
        if !edge.live {
            continue;
        }
        let simplex = Simplex::edge(
            edge.tail,
            edge.head,
            u64::from(edge.weight),
            0x1000_0000 | index as u64,
        )?;
        match complex.insert(simplex) {
            Ok(()) | Err(PersistenceError::DuplicateSimplex) => {}
            Err(error) => return Err(error),
        }
    }

    for (index, face) in nerve.faces[..nerve.n_f.min(nerve.faces.len())]
        .iter()
        .copied()
        .enumerate()
    {
        if !face.live {
            continue;
        }

        let edge_ij = nerve
            .edges
            .get(face.e_ij as usize)
            .filter(|edge| edge.live)
            .ok_or(PersistenceError::MissingFace)?;
        let edge_jk = nerve
            .edges
            .get(face.e_jk as usize)
            .filter(|edge| edge.live)
            .ok_or(PersistenceError::MissingFace)?;
        let edge_ik = nerve
            .edges
            .get(face.e_ik as usize)
            .filter(|edge| edge.live)
            .ok_or(PersistenceError::MissingFace)?;
        let edge_filtration = [edge_ij.weight, edge_jk.weight, edge_ik.weight]
            .into_iter()
            .max()
            .unwrap_or(1);

        let simplex = Simplex::triangle(
            face.v[0],
            face.v[1],
            face.v[2],
            u64::from(face.weight.max(edge_filtration)),
            0x2000_0000 | index as u64,
        )?;
        match complex.insert(simplex) {
            Ok(()) | Err(PersistenceError::DuplicateSimplex) => {}
            Err(error) => return Err(error),
        }
    }

    Ok(())
}

pub fn tropical_from_resource_quiver(
    quiver: &crate::cluster_quiver::ResourceQuiver,
    secret: u64,
) -> Result<TropicalCluster, TropicalError> {
    if quiver.n > quiver.x.len() || quiver.e_len > quiver.arrows.len() {
        return Err(TropicalError::Capacity);
    }

    let nodes = quiver.n.min(MAX_CLUSTER_NODES);
    let mut cluster = TropicalCluster::new(nodes, secret)?;

    for node in 0..nodes {
        cluster.set_coordinate(node, binary_log_q16(quiver.x[node].max(1))?)?;
    }

    for arrow in quiver.arrows[..quiver.e_len.min(quiver.arrows.len())]
        .iter()
        .copied()
    {
        if !arrow.live {
            continue;
        }
        let from = arrow.from as usize;
        let to = arrow.to as usize;
        if from < nodes && to < nodes {
            cluster.set_arrow(from, to, u16::from(arrow.mult))?;
        }
    }

    cluster.validate()?;
    Ok(cluster)
}

pub fn quadratic_program_from_actuation(
    actuation: &crate::manifold_orchestrator::Actuation,
) -> Result<QuadraticProgram, OptimizationError> {
    let variables = (actuation.n_ceilings as usize).min(MAX_VARIABLES);
    if variables == 0 {
        return Err(OptimizationError::InvalidDimension);
    }

    let mut program = QuadraticProgram::EMPTY;
    program.variables = variables;
    program.constraints = 1;

    let mut total_ceiling = 0_i64;
    for variable in 0..variables {
        let ceiling_q32 = q16_unsigned_to_q32(actuation.ceilings[variable]);
        let migration_q32 = if variable < actuation.n_migrate as usize {
            q16_signed_to_q32(actuation.migrate[variable])
        } else {
            0
        };

        let target = ceiling_q32
            .checked_sub(migration_q32)
            .ok_or(OptimizationError::Arithmetic)?
            .max(0);

        program.diagonal_q32[variable] = OPT_Q32_ONE;
        program.linear_q32[variable] = -target;
        program.lower_q32[variable] = 0;
        program.upper_q32[variable] = ceiling_q32
            .checked_mul(2)
            .ok_or(OptimizationError::Arithmetic)?
            .max(OPT_Q32_ONE / 64);
        program.matrix_q32[0][variable] = OPT_Q32_ONE;
        total_ceiling = total_ceiling
            .checked_add(ceiling_q32)
            .ok_or(OptimizationError::Arithmetic)?;
    }

    program.bound_q32[0] = total_ceiling;
    program.validate()?;
    Ok(program)
}

pub fn hodge_state_from_actuation(
    actuation: &crate::manifold_orchestrator::Actuation,
) -> [i64; super::hodge_implicit::MAX_VERTICES] {
    let mut state = [0_i64; super::hodge_implicit::MAX_VERTICES];
    let length = (actuation.n_ceilings as usize)
        .min(state.len())
        .min(actuation.ceilings.len());

    for index in 0..length {
        state[index] = q16_unsigned_to_q32(actuation.ceilings[index]);
    }

    state
}

pub fn pressure_from_actuation(
    actuation: &crate::manifold_orchestrator::Actuation,
) -> [u64; MAX_CLUSTER_NODES] {
    let mut pressure = [0_u64; MAX_CLUSTER_NODES];
    let length = (actuation.n_migrate as usize).min(MAX_CLUSTER_NODES);

    for index in 0..length {
        pressure[index] = q16_signed_to_q32(actuation.migrate[index]).unsigned_abs();
    }

    pressure
}

fn q16_unsigned_to_q32(value: u32) -> i64 {
    i64::from(value) << 16
}

fn q16_signed_to_q32(value: i32) -> i64 {
    i64::from(value) << 16
}

fn binary_log_q16(value: u32) -> Result<i64, TropicalError> {
    if value == 0 {
        return Err(TropicalError::Arithmetic);
    }

    let integer = 31 - value.leading_zeros();
    let mut normalized = (u128::from(value) << 32) >> integer;
    let mut fraction = 0_u32;

    for bit in 0..16 {
        normalized = normalized
            .checked_mul(normalized)
            .ok_or(TropicalError::Arithmetic)?
            >> 32;
        if normalized >= (2_u128 << 32) {
            normalized >>= 1;
            fraction |= 1_u32 << (15 - bit);
        }
    }

    Ok(((i64::from(integer) - 16) << 16)
        .checked_add(i64::from(fraction))
        .ok_or(TropicalError::Arithmetic)?)
}

pub const fn default_hodge_tau_q32() -> i64 {
    HODGE_Q32_ONE / 8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_log_tracks_exact_powers_of_two() {
        assert_eq!(binary_log_q16(crate::cluster_quiver::FP_ONE).unwrap(), 0);
        assert_eq!(
            binary_log_q16(crate::cluster_quiver::FP_ONE.saturating_mul(2),).unwrap(),
            1 << 16
        );
        assert_eq!(
            binary_log_q16(crate::cluster_quiver::FP_ONE.saturating_mul(8),).unwrap(),
            3 << 16
        );
    }
}
