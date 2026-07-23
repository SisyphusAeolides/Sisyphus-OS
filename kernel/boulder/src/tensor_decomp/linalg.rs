//! Fixed-capacity linear algebra for CP-ALS and Tucker-HOSVD.

use super::fixed;
use super::tensor::TensorError;

pub const MAX_MATRIX_DIMENSION: usize = 8;
pub const MAX_MATRIX_ENTRIES: usize = MAX_MATRIX_DIMENSION * MAX_MATRIX_DIMENSION;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SmallMatrix {
    rows: u8,
    columns: u8,
    values_q24: [i64; MAX_MATRIX_ENTRIES],
}

impl SmallMatrix {
    pub const ZERO: Self = Self {
        rows: 0,
        columns: 0,
        values_q24: [0; MAX_MATRIX_ENTRIES],
    };

    pub fn zeros(rows: usize, columns: usize) -> Result<Self, TensorError> {
        if rows == 0
            || columns == 0
            || rows > MAX_MATRIX_DIMENSION
            || columns > MAX_MATRIX_DIMENSION
        {
            return Err(TensorError::InvalidDimension);
        }

        Ok(Self {
            rows: rows as u8,
            columns: columns as u8,
            values_q24: [0; MAX_MATRIX_ENTRIES],
        })
    }

    pub fn identity(dimension: usize) -> Result<Self, TensorError> {
        let mut matrix = Self::zeros(dimension, dimension)?;
        for index in 0..dimension {
            matrix.set(index, index, fixed::ONE)?;
        }
        Ok(matrix)
    }

    pub const fn rows(&self) -> usize {
        self.rows as usize
    }

    pub const fn columns(&self) -> usize {
        self.columns as usize
    }

    pub fn get(&self, row: usize, column: usize) -> Result<i64, TensorError> {
        if row >= self.rows() || column >= self.columns() {
            return Err(TensorError::Coordinate);
        }
        Ok(self.values_q24[row * MAX_MATRIX_DIMENSION + column])
    }

    pub fn set(&mut self, row: usize, column: usize, value_q24: i64) -> Result<(), TensorError> {
        if row >= self.rows() || column >= self.columns() {
            return Err(TensorError::Coordinate);
        }
        self.values_q24[row * MAX_MATRIX_DIMENSION + column] = value_q24;
        Ok(())
    }

    pub fn add(&mut self, row: usize, column: usize, value_q24: i64) -> Result<(), TensorError> {
        let current = self.get(row, column)?;
        self.set(
            row,
            column,
            current
                .checked_add(value_q24)
                .ok_or(TensorError::Arithmetic)?,
        )
    }

    pub fn column(&self, column: usize) -> Result<[i64; MAX_MATRIX_DIMENSION], TensorError> {
        if column >= self.columns() {
            return Err(TensorError::Coordinate);
        }
        let mut output = [0_i64; MAX_MATRIX_DIMENSION];
        for row in 0..self.rows() {
            output[row] = self.get(row, column)?;
        }
        Ok(output)
    }

    pub fn set_column(
        &mut self,
        column: usize,
        vector: &[i64; MAX_MATRIX_DIMENSION],
    ) -> Result<(), TensorError> {
        if column >= self.columns() {
            return Err(TensorError::Coordinate);
        }
        for row in 0..self.rows() {
            self.set(row, column, vector[row])?;
        }
        Ok(())
    }

    pub fn root(&self, secret: u64) -> Result<u64, TensorError> {
        if secret == 0 {
            return Err(TensorError::ZeroSecret);
        }
        let mut state = super::tensor::mix(secret, self.rows as u64 | ((self.columns as u64) << 8));
        for row in 0..self.rows() {
            for column in 0..self.columns() {
                state = super::tensor::mix(state, self.get(row, column)? as u64);
            }
        }
        Ok(state)
    }
}

pub fn dot(
    left: &[i64; MAX_MATRIX_DIMENSION],
    right: &[i64; MAX_MATRIX_DIMENSION],
    length: usize,
) -> Result<i64, TensorError> {
    if length > MAX_MATRIX_DIMENSION {
        return Err(TensorError::InvalidDimension);
    }

    let mut sum = 0_i64;
    for index in 0..length {
        sum = sum
            .checked_add(fixed::mul(left[index], right[index])?)
            .ok_or(TensorError::Arithmetic)?;
    }
    Ok(sum)
}

pub fn norm(vector: &[i64; MAX_MATRIX_DIMENSION], length: usize) -> Result<i64, TensorError> {
    let squared = dot(vector, vector, length)?;
    fixed::sqrt(squared).map_err(Into::into)
}

pub fn normalize(
    vector: &mut [i64; MAX_MATRIX_DIMENSION],
    length: usize,
    floor_q24: i64,
) -> Result<i64, TensorError> {
    let magnitude = norm(vector, length)?;
    if magnitude <= floor_q24 {
        return Err(TensorError::Arithmetic);
    }

    for value in &mut vector[..length] {
        *value = fixed::div(*value, magnitude)?;
    }
    Ok(magnitude)
}

pub fn gram(matrix: SmallMatrix) -> Result<SmallMatrix, TensorError> {
    let mut output = SmallMatrix::zeros(matrix.columns(), matrix.columns())?;

    for left in 0..matrix.columns() {
        for right in left..matrix.columns() {
            let mut value = 0_i64;
            for row in 0..matrix.rows() {
                value = value
                    .checked_add(fixed::mul(matrix.get(row, left)?, matrix.get(row, right)?)?)
                    .ok_or(TensorError::Arithmetic)?;
            }
            output.set(left, right, value)?;
            output.set(right, left, value)?;
        }
    }

    Ok(output)
}

pub fn hadamard_assign(target: &mut SmallMatrix, other: SmallMatrix) -> Result<(), TensorError> {
    if target.rows() != other.rows() || target.columns() != other.columns() {
        return Err(TensorError::ShapeMismatch);
    }

    for row in 0..target.rows() {
        for column in 0..target.columns() {
            target.set(
                row,
                column,
                fixed::mul(target.get(row, column)?, other.get(row, column)?)?,
            )?;
        }
    }
    Ok(())
}

pub fn cholesky(matrix: SmallMatrix, diagonal_floor_q24: i64) -> Result<SmallMatrix, TensorError> {
    if matrix.rows() != matrix.columns() {
        return Err(TensorError::ShapeMismatch);
    }
    let dimension = matrix.rows();
    let mut lower = SmallMatrix::zeros(dimension, dimension)?;

    for row in 0..dimension {
        for column in 0..=row {
            let mut value = matrix.get(row, column)?;

            for inner in 0..column {
                value = value
                    .checked_sub(fixed::mul(
                        lower.get(row, inner)?,
                        lower.get(column, inner)?,
                    )?)
                    .ok_or(TensorError::Arithmetic)?;
            }

            if row == column {
                if value < diagonal_floor_q24 {
                    value = diagonal_floor_q24;
                }
                lower.set(row, column, fixed::sqrt(value)?)?;
            } else {
                lower.set(row, column, fixed::div(value, lower.get(column, column)?)?)?;
            }
        }
    }

    Ok(lower)
}

pub fn solve_spd(
    matrix: SmallMatrix,
    right_hand_side: &[i64; MAX_MATRIX_DIMENSION],
    dimension: usize,
    diagonal_floor_q24: i64,
) -> Result<[i64; MAX_MATRIX_DIMENSION], TensorError> {
    if matrix.rows() != dimension
        || matrix.columns() != dimension
        || dimension == 0
        || dimension > MAX_MATRIX_DIMENSION
    {
        return Err(TensorError::ShapeMismatch);
    }

    let lower = cholesky(matrix, diagonal_floor_q24)?;
    let mut intermediate = [0_i64; MAX_MATRIX_DIMENSION];
    let mut solution = [0_i64; MAX_MATRIX_DIMENSION];

    for row in 0..dimension {
        let mut value = right_hand_side[row];
        for column in 0..row {
            value = value
                .checked_sub(fixed::mul(lower.get(row, column)?, intermediate[column])?)
                .ok_or(TensorError::Arithmetic)?;
        }
        intermediate[row] = fixed::div(value, lower.get(row, row)?)?;
    }

    for row in (0..dimension).rev() {
        let mut value = intermediate[row];
        for column in row + 1..dimension {
            value = value
                .checked_sub(fixed::mul(lower.get(column, row)?, solution[column])?)
                .ok_or(TensorError::Arithmetic)?;
        }
        solution[row] = fixed::div(value, lower.get(row, row)?)?;
    }

    Ok(solution)
}

pub fn matrix_vector(
    matrix: SmallMatrix,
    vector: &[i64; MAX_MATRIX_DIMENSION],
) -> Result<[i64; MAX_MATRIX_DIMENSION], TensorError> {
    let mut output = [0_i64; MAX_MATRIX_DIMENSION];

    for row in 0..matrix.rows() {
        for column in 0..matrix.columns() {
            output[row] = output[row]
                .checked_add(fixed::mul(matrix.get(row, column)?, vector[column])?)
                .ok_or(TensorError::Arithmetic)?;
        }
    }

    Ok(output)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EigenspaceCertificate {
    pub dimension: u8,
    pub rank: u8,
    pub iterations: u16,
    pub orthogonality_defect_q24: u64,
    pub maximum_relative_residual_q24: u64,
    pub eigenvalue_floor_q24: i64,
    pub basis_root: u64,
}

impl EigenspaceCertificate {
    pub const EMPTY: Self = Self {
        dimension: 0,
        rank: 0,
        iterations: 0,
        orthogonality_defect_q24: 0,
        maximum_relative_residual_q24: 0,
        eigenvalue_floor_q24: 0,
        basis_root: 0,
    };
}

pub fn dominant_eigenspace(
    covariance: SmallMatrix,
    rank: usize,
    iterations: u16,
    tolerance_q24: i64,
    secret: u64,
) -> Result<
    (
        SmallMatrix,
        [i64; MAX_MATRIX_DIMENSION],
        EigenspaceCertificate,
    ),
    TensorError,
> {
    if covariance.rows() != covariance.columns()
        || rank == 0
        || rank > covariance.rows()
        || iterations == 0
        || secret == 0
    {
        return Err(TensorError::InvalidDimension);
    }

    let dimension = covariance.rows();
    let mut basis = SmallMatrix::zeros(dimension, rank)?;

    for column in 0..rank {
        let mut vector = [0_i64; MAX_MATRIX_DIMENSION];
        for row in 0..dimension {
            let word = super::tensor::mix(secret ^ ((column as u64) << 32), row as u64);
            let signed = ((word >> 32) as i32) as i64;
            vector[row] = (signed >> 8).clamp(-fixed::ONE, fixed::ONE);
        }
        orthogonalize_against(&basis, column, &mut vector, dimension)?;
        if normalize(&mut vector, dimension, tolerance_q24).is_err() {
            vector.fill(0);
            vector[column % dimension] = fixed::ONE;
            orthogonalize_against(&basis, column, &mut vector, dimension)?;
            normalize(&mut vector, dimension, 1)?;
        }
        basis.set_column(column, &vector)?;
    }

    let mut completed = 0_u16;
    for iteration in 0..iterations {
        let previous = basis;
        let mut next = SmallMatrix::zeros(dimension, rank)?;

        for column in 0..rank {
            let vector = previous.column(column)?;
            let mut image = matrix_vector(covariance, &vector)?;
            orthogonalize_against(&next, column, &mut image, dimension)?;

            if normalize(&mut image, dimension, tolerance_q24).is_err() {
                image = vector;
            }
            next.set_column(column, &image)?;
        }

        basis = next;
        completed = iteration.saturating_add(1);

        let defect = subspace_change(previous, basis)?;
        if defect <= tolerance_q24.unsigned_abs() {
            break;
        }
    }

    let mut eigenvalues = [0_i64; MAX_MATRIX_DIMENSION];
    let mut maximum_residual = 0_u64;

    for column in 0..rank {
        let vector = basis.column(column)?;
        let image = matrix_vector(covariance, &vector)?;
        let eigenvalue = dot(&vector, &image, dimension)?;
        eigenvalues[column] = eigenvalue;

        let mut residual = image;
        for row in 0..dimension {
            residual[row] = residual[row]
                .checked_sub(fixed::mul(eigenvalue, vector[row])?)
                .ok_or(TensorError::Arithmetic)?;
        }
        let residual_norm = norm(&residual, dimension)?;
        let denominator = eigenvalue
            .checked_abs()
            .ok_or(TensorError::Arithmetic)?
            .max(tolerance_q24.max(1));
        let relative_residual = fixed::div(residual_norm, denominator)?;
        maximum_residual = maximum_residual.max(relative_residual.unsigned_abs());
    }

    sort_eigenspace_descending(&mut basis, &mut eigenvalues, rank)?;

    let orthogonality_defect = orthogonality_defect(basis, rank)?;
    let certificate = EigenspaceCertificate {
        dimension: dimension as u8,
        rank: rank as u8,
        iterations: completed,
        orthogonality_defect_q24: orthogonality_defect,
        maximum_relative_residual_q24: maximum_residual,
        eigenvalue_floor_q24: eigenvalues[rank - 1],
        basis_root: basis.root(secret)?,
    };

    Ok((basis, eigenvalues, certificate))
}

fn orthogonalize_against(
    basis: &SmallMatrix,
    columns: usize,
    vector: &mut [i64; MAX_MATRIX_DIMENSION],
    length: usize,
) -> Result<(), TensorError> {
    for column in 0..columns {
        let existing = basis.column(column)?;
        let coefficient = dot(&existing, vector, length)?;
        for row in 0..length {
            vector[row] = vector[row]
                .checked_sub(fixed::mul(coefficient, existing[row])?)
                .ok_or(TensorError::Arithmetic)?;
        }
    }
    Ok(())
}

fn subspace_change(previous: SmallMatrix, next: SmallMatrix) -> Result<u64, TensorError> {
    let mut maximum = 0_u64;

    for column in 0..previous.columns() {
        let left = previous.column(column)?;
        let right = next.column(column)?;
        let overlap = dot(&left, &right, previous.rows())?;
        let defect = fixed::ONE
            .checked_sub(overlap.abs())
            .ok_or(TensorError::Arithmetic)?
            .unsigned_abs();
        maximum = maximum.max(defect);
    }

    Ok(maximum)
}

fn orthogonality_defect(basis: SmallMatrix, rank: usize) -> Result<u64, TensorError> {
    let gram_matrix = gram(basis)?;
    let mut maximum = 0_u64;

    for row in 0..rank {
        for column in 0..rank {
            let expected = if row == column { fixed::ONE } else { 0 };
            maximum = maximum.max(gram_matrix.get(row, column)?.abs_diff(expected));
        }
    }

    Ok(maximum)
}

fn sort_eigenspace_descending(
    basis: &mut SmallMatrix,
    eigenvalues: &mut [i64; MAX_MATRIX_DIMENSION],
    rank: usize,
) -> Result<(), TensorError> {
    for left in 0..rank {
        let mut best = left;
        for right in left + 1..rank {
            if eigenvalues[right] > eigenvalues[best] {
                best = right;
            }
        }

        if best != left {
            eigenvalues.swap(left, best);
            let left_column = basis.column(left)?;
            let best_column = basis.column(best)?;
            basis.set_column(left, &best_column)?;
            basis.set_column(best, &left_column)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cholesky_solves_a_positive_system() {
        let mut matrix = SmallMatrix::zeros(2, 2).unwrap();
        matrix.set(0, 0, 4 * fixed::ONE).unwrap();
        matrix.set(0, 1, fixed::ONE).unwrap();
        matrix.set(1, 0, fixed::ONE).unwrap();
        matrix.set(1, 1, 3 * fixed::ONE).unwrap();

        let rhs = [fixed::ONE, 2 * fixed::ONE, 0, 0, 0, 0, 0, 0];
        let solution = solve_spd(matrix, &rhs, 2, fixed::ONE / 4096).unwrap();

        let image = matrix_vector(matrix, &solution).unwrap();
        assert!(image[0].abs_diff(rhs[0]) < (fixed::ONE / 1024) as u64);
        assert!(image[1].abs_diff(rhs[1]) < (fixed::ONE / 1024) as u64);
    }

    #[test]
    fn dominant_eigenspace_finds_diagonal_axes() {
        let mut covariance = SmallMatrix::zeros(3, 3).unwrap();
        covariance.set(0, 0, 9 * fixed::ONE).unwrap();
        covariance.set(1, 1, 4 * fixed::ONE).unwrap();
        covariance.set(2, 2, fixed::ONE).unwrap();

        let (_basis, eigenvalues, certificate) =
            dominant_eigenspace(covariance, 2, 64, fixed::ONE / 4096, 7).unwrap();

        assert!(eigenvalues[0] > eigenvalues[1]);
        assert!(certificate.orthogonality_defect_q24 < fixed::ONE as u64 / 1024);
    }
}
