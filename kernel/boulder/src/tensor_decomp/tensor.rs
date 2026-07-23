//! Fixed-capacity dense tensors with explicit shape and row-major indexing.

use super::fixed::FixedError;

pub const MAX_ORDER: usize = 4;
pub const MAX_MODE_DIMENSION: usize = 8;
pub const MAX_ENTRIES: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TensorError {
    InvalidOrder,
    InvalidDimension,
    Capacity,
    Coordinate,
    ShapeMismatch,
    Arithmetic,
    Fixed(FixedError),
    ZeroSecret,
}

impl From<FixedError> for TensorError {
    fn from(error: FixedError) -> Self {
        Self::Fixed(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TensorShape {
    order: u8,
    dimensions: [u8; MAX_ORDER],
    strides: [usize; MAX_ORDER],
    length: usize,
}

impl TensorShape {
    pub fn new(order: usize, dimensions: [u8; MAX_ORDER]) -> Result<Self, TensorError> {
        if order == 0 || order > MAX_ORDER {
            return Err(TensorError::InvalidOrder);
        }

        let mut length = 1_usize;
        let mut strides = [0_usize; MAX_ORDER];

        for mode in (0..order).rev() {
            let dimension = dimensions[mode] as usize;
            if dimension == 0 || dimension > MAX_MODE_DIMENSION {
                return Err(TensorError::InvalidDimension);
            }
            strides[mode] = length;
            length = length
                .checked_mul(dimension)
                .ok_or(TensorError::Arithmetic)?;
            if length > MAX_ENTRIES {
                return Err(TensorError::Capacity);
            }
        }

        if dimensions[order..].iter().any(|dimension| *dimension != 0) {
            return Err(TensorError::InvalidDimension);
        }

        Ok(Self {
            order: order as u8,
            dimensions,
            strides,
            length,
        })
    }

    pub const fn order(self) -> usize {
        self.order as usize
    }

    pub const fn dimensions(self) -> [u8; MAX_ORDER] {
        self.dimensions
    }

    pub const fn dimension(self, mode: usize) -> usize {
        if mode < self.order as usize {
            self.dimensions[mode] as usize
        } else {
            0
        }
    }

    pub const fn length(self) -> usize {
        self.length
    }

    pub fn offset(self, coordinates: &[usize; MAX_ORDER]) -> Result<usize, TensorError> {
        let mut offset = 0_usize;

        for mode in 0..self.order() {
            if coordinates[mode] >= self.dimension(mode) {
                return Err(TensorError::Coordinate);
            }
            offset = offset
                .checked_add(
                    coordinates[mode]
                        .checked_mul(self.strides[mode])
                        .ok_or(TensorError::Arithmetic)?,
                )
                .ok_or(TensorError::Arithmetic)?;
        }

        if coordinates[self.order()..]
            .iter()
            .any(|coordinate| *coordinate != 0)
        {
            return Err(TensorError::Coordinate);
        }

        Ok(offset)
    }

    pub fn unravel(self, mut offset: usize) -> [usize; MAX_ORDER] {
        let mut coordinates = [0_usize; MAX_ORDER];

        for mode in 0..self.order() {
            coordinates[mode] = offset / self.strides[mode];
            offset %= self.strides[mode];
        }

        coordinates
    }

    pub fn same_geometry(self, other: Self) -> bool {
        self.order == other.order
            && self.dimensions == other.dimensions
            && self.length == other.length
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct DenseTensor {
    shape: TensorShape,
    values_q24: [i64; MAX_ENTRIES],
}

impl DenseTensor {
    pub fn zeros(shape: TensorShape) -> Self {
        Self {
            shape,
            values_q24: [0; MAX_ENTRIES],
        }
    }

    pub const fn shape(&self) -> TensorShape {
        self.shape
    }

    pub fn values(&self) -> &[i64] {
        &self.values_q24[..self.shape.length()]
    }

    pub fn values_mut(&mut self) -> &mut [i64] {
        &mut self.values_q24[..self.shape.length()]
    }

    pub fn clear(&mut self) {
        self.values_q24[..self.shape.length()].fill(0);
    }

    pub fn reconfigure(&mut self, shape: TensorShape) {
        self.values_q24.fill(0);
        self.shape = shape;
    }

    pub fn copy_from(&mut self, source: &DenseTensor) -> Result<(), TensorError> {
        if !self.shape.same_geometry(source.shape()) {
            return Err(TensorError::ShapeMismatch);
        }
        self.values_mut().copy_from_slice(source.values());
        Ok(())
    }

    pub fn get(&self, coordinates: &[usize; MAX_ORDER]) -> Result<i64, TensorError> {
        Ok(self.values_q24[self.shape.offset(coordinates)?])
    }

    pub fn set(
        &mut self,
        coordinates: &[usize; MAX_ORDER],
        value_q24: i64,
    ) -> Result<(), TensorError> {
        let offset = self.shape.offset(coordinates)?;
        self.values_q24[offset] = value_q24;
        Ok(())
    }

    pub fn add(
        &mut self,
        coordinates: &[usize; MAX_ORDER],
        value_q24: i64,
    ) -> Result<(), TensorError> {
        let offset = self.shape.offset(coordinates)?;
        self.values_q24[offset] = self.values_q24[offset]
            .checked_add(value_q24)
            .ok_or(TensorError::Arithmetic)?;
        Ok(())
    }

    pub fn set_linear(&mut self, offset: usize, value_q24: i64) -> Result<(), TensorError> {
        if offset >= self.shape.length() {
            return Err(TensorError::Coordinate);
        }
        self.values_q24[offset] = value_q24;
        Ok(())
    }

    pub fn get_linear(&self, offset: usize) -> Result<i64, TensorError> {
        if offset >= self.shape.length() {
            return Err(TensorError::Coordinate);
        }
        Ok(self.values_q24[offset])
    }

    pub fn frobenius_squared_q48(&self) -> Result<u128, TensorError> {
        let mut sum = 0_u128;

        for value in self.values().iter().copied() {
            let magnitude = value.unsigned_abs() as u128;
            let square = magnitude
                .checked_mul(magnitude)
                .ok_or(TensorError::Arithmetic)?;
            sum = sum.checked_add(square).ok_or(TensorError::Arithmetic)?;
        }

        Ok(sum)
    }

    pub fn root(&self, secret: u64) -> Result<u64, TensorError> {
        if secret == 0 {
            return Err(TensorError::ZeroSecret);
        }

        let mut state = mix(secret, self.shape.order as u64);
        state = mix(state, self.shape.length as u64);
        for dimension in self.shape.dimensions {
            state = mix(state, dimension as u64);
        }
        for value in self.values() {
            state = mix(state, *value as u64);
        }
        Ok(state)
    }
}

pub fn squared_error_q48(left: &DenseTensor, right: &DenseTensor) -> Result<u128, TensorError> {
    if !left.shape().same_geometry(right.shape()) {
        return Err(TensorError::ShapeMismatch);
    }

    let mut error = 0_u128;
    for index in 0..left.shape().length() {
        let difference = left.values()[index]
            .checked_sub(right.values()[index])
            .ok_or(TensorError::Arithmetic)?;
        let magnitude = difference.unsigned_abs() as u128;
        error = error
            .checked_add(
                magnitude
                    .checked_mul(magnitude)
                    .ok_or(TensorError::Arithmetic)?,
            )
            .ok_or(TensorError::Arithmetic)?;
    }
    Ok(error)
}

pub(crate) fn mix(mut state: u64, word: u64) -> u64 {
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
    fn row_major_round_trip() {
        let shape = TensorShape::new(3, [2, 3, 4, 0]).unwrap();

        for offset in 0..shape.length() {
            let coordinates = shape.unravel(offset);
            assert_eq!(shape.offset(&coordinates).unwrap(), offset);
        }
    }

    #[test]
    fn rejects_oversized_tensor() {
        assert_eq!(
            TensorShape::new(4, [8, 8, 8, 8]),
            Err(TensorError::Capacity)
        );
    }
}
