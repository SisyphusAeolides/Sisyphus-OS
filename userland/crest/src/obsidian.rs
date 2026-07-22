pub const MAXIMUM_APP_NODES: usize = 16;
pub const MAXIMUM_SDF_INSTRUCTIONS: usize = 64;
pub const MAXIMUM_SDF_STACK: usize = 16;
pub const MAXIMUM_MARCH_STEPS: u16 = 128;

const FRACTION_BITS: u32 = 16;
const ONE_RAW: i32 = 1 << FRACTION_BITS;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Fixed(i32);

impl Fixed {
    pub const ZERO: Self = Self(0);
    pub const ONE: Self = Self(ONE_RAW);

    pub const fn from_integer(value: i16) -> Self {
        Self((value as i32) << FRACTION_BITS)
    }

    pub const fn from_raw(raw: i32) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> i32 {
        self.0
    }

    pub fn from_ratio(numerator: i32, denominator: i32) -> Result<Self, ObsidianError> {
        if denominator == 0 {
            return Err(ObsidianError::Arithmetic);
        }
        let raw = (i64::from(numerator) << FRACTION_BITS) / i64::from(denominator);
        i32::try_from(raw)
            .map(Self)
            .map_err(|_| ObsidianError::Arithmetic)
    }

    pub fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }

    pub fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }

    pub fn multiply(self, other: Self) -> Self {
        let raw = (i64::from(self.0) * i64::from(other.0)) >> FRACTION_BITS;
        Self(clamp_i64(raw))
    }

    pub fn divide(self, other: Self) -> Result<Self, ObsidianError> {
        if other.0 == 0 {
            return Err(ObsidianError::Arithmetic);
        }
        let raw = (i64::from(self.0) << FRACTION_BITS) / i64::from(other.0);
        Ok(Self(clamp_i64(raw)))
    }

    pub fn abs(self) -> Self {
        Self(self.0.saturating_abs())
    }

    pub fn saturating_neg(self) -> Self {
        Self(self.0.saturating_neg())
    }

    pub fn sqrt(self) -> Result<Self, ObsidianError> {
        if self.0 < 0 {
            return Err(ObsidianError::Arithmetic);
        }
        let scaled = (self.0 as u64) << FRACTION_BITS;
        Ok(Self(integer_sqrt(scaled).min(i32::MAX as u64) as i32))
    }
}

pub fn fixed_hypot(x: Fixed, y: Fixed) -> Fixed {
    Fixed(integer_sqrt(square_sum(x, y, Fixed::ZERO)).min(i32::MAX as u64) as i32)
}

fn clamp_i64(value: i64) -> i32 {
    value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Vector3 {
    pub x: Fixed,
    pub y: Fixed,
    pub z: Fixed,
}

impl Vector3 {
    pub const ZERO: Self = Self {
        x: Fixed::ZERO,
        y: Fixed::ZERO,
        z: Fixed::ZERO,
    };

    fn add_scaled(self, direction: Self, distance: Fixed) -> Self {
        Self {
            x: self.x.saturating_add(direction.x.multiply(distance)),
            y: self.y.saturating_add(direction.y.multiply(distance)),
            z: self.z.saturating_add(direction.z.multiply(distance)),
        }
    }

    fn normalized(self) -> Result<Self, ObsidianError> {
        let squared = square_sum(self.x, self.y, self.z);
        let magnitude = Fixed(integer_sqrt(squared).min(i32::MAX as u64) as i32);
        if magnitude == Fixed::ZERO {
            return Err(ObsidianError::InvalidRay);
        }
        Ok(Self {
            x: self.x.divide(magnitude)?,
            y: self.y.divide(magnitude)?,
            z: self.z.divide(magnitude)?,
        })
    }
}

fn square_sum(x: Fixed, y: Fixed, z: Fixed) -> u64 {
    let sum = i128::from(x.0) * i128::from(x.0)
        + i128::from(y.0) * i128::from(y.0)
        + i128::from(z.0) * i128::from(z.0);
    sum.clamp(0, i128::from(u64::MAX)) as u64
}

fn integer_sqrt(value: u64) -> u64 {
    if value < 2 {
        return value;
    }
    let mut estimate = 1_u64 << ((64 - value.leading_zeros() as u64).div_ceil(2));
    loop {
        let next = (estimate + value / estimate) / 2;
        if next >= estimate {
            return estimate;
        }
        estimate = next;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SdfInstruction {
    Empty,
    Sphere { center: Vector3, radius: Fixed },
    PlaneY { height: Fixed },
    Union,
    Intersection,
    Subtract,
}

#[derive(Clone, Copy)]
pub struct SdfProgram {
    instructions: [SdfInstruction; MAXIMUM_SDF_INSTRUCTIONS],
    length: u8,
}

impl SdfProgram {
    pub fn new(instructions: &[SdfInstruction]) -> Result<Self, ObsidianError> {
        if instructions.is_empty() || instructions.len() > MAXIMUM_SDF_INSTRUCTIONS {
            return Err(ObsidianError::InvalidProgram);
        }
        let mut depth = 0_usize;
        for instruction in instructions {
            match instruction {
                SdfInstruction::Sphere { radius, .. } if *radius > Fixed::ZERO => depth += 1,
                SdfInstruction::PlaneY { .. } => depth += 1,
                SdfInstruction::Union | SdfInstruction::Intersection | SdfInstruction::Subtract
                    if depth >= 2 =>
                {
                    depth -= 1;
                }
                _ => return Err(ObsidianError::InvalidProgram),
            }
            if depth > MAXIMUM_SDF_STACK {
                return Err(ObsidianError::InvalidProgram);
            }
        }
        if depth != 1 {
            return Err(ObsidianError::InvalidProgram);
        }
        let mut program = Self {
            instructions: [SdfInstruction::Empty; MAXIMUM_SDF_INSTRUCTIONS],
            length: instructions.len() as u8,
        };
        program.instructions[..instructions.len()].copy_from_slice(instructions);
        Ok(program)
    }

    fn distance(&self, point: Vector3) -> Result<Fixed, ObsidianError> {
        let mut stack = [Fixed::ZERO; MAXIMUM_SDF_STACK];
        let mut depth = 0_usize;
        for instruction in &self.instructions[..usize::from(self.length)] {
            match *instruction {
                SdfInstruction::Sphere { center, radius } => {
                    let delta = Vector3 {
                        x: point.x.saturating_sub(center.x),
                        y: point.y.saturating_sub(center.y),
                        z: point.z.saturating_sub(center.z),
                    };
                    let magnitude =
                        Fixed(integer_sqrt(square_sum(delta.x, delta.y, delta.z)) as i32);
                    stack[depth] = magnitude.saturating_sub(radius);
                    depth += 1;
                }
                SdfInstruction::PlaneY { height } => {
                    stack[depth] = point.y.saturating_sub(height);
                    depth += 1;
                }
                SdfInstruction::Union => combine(&mut stack, &mut depth, Fixed::min)?,
                SdfInstruction::Intersection => combine(&mut stack, &mut depth, Fixed::max)?,
                SdfInstruction::Subtract => {
                    combine(&mut stack, &mut depth, |left, right| {
                        left.max(Fixed(right.0.saturating_neg()))
                    })?;
                }
                SdfInstruction::Empty => return Err(ObsidianError::InvalidProgram),
            }
        }
        stack.first().copied().ok_or(ObsidianError::InvalidProgram)
    }
}

fn combine(
    stack: &mut [Fixed; MAXIMUM_SDF_STACK],
    depth: &mut usize,
    operation: impl FnOnce(Fixed, Fixed) -> Fixed,
) -> Result<(), ObsidianError> {
    if *depth < 2 {
        return Err(ObsidianError::InvalidProgram);
    }
    let right = stack[*depth - 1];
    let left = stack[*depth - 2];
    *depth -= 1;
    stack[*depth - 1] = operation(left, right);
    Ok(())
}

#[derive(Clone, Copy)]
pub struct SemanticAppNode {
    pub app_id: u32,
    pub heat_signature: u32,
    pub center_x: Fixed,
    pub center_y: Fixed,
    pub color: [u8; 4],
    program: SdfProgram,
}

impl SemanticAppNode {
    pub const fn new(
        app_id: u32,
        heat_signature: u32,
        center_x: Fixed,
        center_y: Fixed,
        color: [u8; 4],
        program: SdfProgram,
    ) -> Self {
        Self {
            app_id,
            heat_signature,
            center_x,
            center_y,
            color,
            program,
        }
    }
}

pub struct ObsidianShell {
    active_nodes: [Option<SemanticAppNode>; MAXIMUM_APP_NODES],
    node_count: usize,
}

impl ObsidianShell {
    pub const fn new() -> Self {
        Self {
            active_nodes: [None; MAXIMUM_APP_NODES],
            node_count: 0,
        }
    }

    pub fn assimilate_app(&mut self, node: SemanticAppNode) -> Result<(), ObsidianError> {
        if node.app_id == 0
            || node.heat_signature == 0
            || self.active_nodes[..self.node_count]
                .iter()
                .flatten()
                .any(|existing| existing.app_id == node.app_id)
        {
            return Err(ObsidianError::InvalidNode);
        }
        let slot = self
            .active_nodes
            .get_mut(self.node_count)
            .ok_or(ObsidianError::CapacityExceeded)?;
        *slot = Some(node);
        self.node_count += 1;
        Ok(())
    }

    pub fn calculate_warp_manifold(&self, x: Fixed, y: Fixed) -> Option<u32> {
        self.dominant_node(x, y).map(|node| node.app_id)
    }

    pub fn evaluate_pixel(
        &self,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    ) -> Result<[u8; 4], ObsidianError> {
        if x >= width || y >= height || width == 0 || height == 0 {
            return Err(ObsidianError::InvalidViewport);
        }
        let nx = Fixed::from_ratio((x as i32).saturating_mul(2), width as i32)?
            .saturating_sub(Fixed::ONE);
        let ny = Fixed::from_ratio((y as i32).saturating_mul(2), height as i32)?
            .saturating_sub(Fixed::ONE);
        let Some(node) = self.dominant_node(nx, ny) else {
            return Ok([10, 10, 12, 255]);
        };
        let ray = Ray {
            origin: Vector3 {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                z: Fixed::from_integer(-4),
            },
            direction: Vector3 {
                x: nx,
                y: ny,
                z: Fixed::ONE,
            }
            .normalized()?,
        };
        match march(&node.program, ray)? {
            MarchResult::Hit { .. } => Ok(node.color),
            MarchResult::Miss { .. } => Ok([10, 10, 12, 255]),
        }
    }

    fn dominant_node(&self, x: Fixed, y: Fixed) -> Option<&SemanticAppNode> {
        let mut selected = None;
        let mut maximum_score = 0_u64;
        for node in self.active_nodes[..self.node_count].iter().flatten() {
            let dx = i64::from(x.saturating_sub(node.center_x).0);
            let dy = i64::from(y.saturating_sub(node.center_y).0);
            let distance_squared = (dx * dx + dy * dy).max(1) as u64;
            let score = (u64::from(node.heat_signature) << 32) / distance_squared;
            if selected.is_none() || score > maximum_score {
                selected = Some(node);
                maximum_score = score;
            }
        }
        selected
    }
}

impl Default for ObsidianShell {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy)]
struct Ray {
    origin: Vector3,
    direction: Vector3,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MarchResult {
    Hit { distance: Fixed, steps: u16 },
    Miss { distance: Fixed, steps: u16 },
}

fn march(program: &SdfProgram, ray: Ray) -> Result<MarchResult, ObsidianError> {
    let epsilon = Fixed::from_ratio(1, 256)?;
    let minimum_step = Fixed::from_ratio(1, 1024)?;
    let maximum_distance = Fixed::from_integer(32);
    let mut distance = Fixed::ZERO;
    for step in 0..MAXIMUM_MARCH_STEPS {
        let point = ray.origin.add_scaled(ray.direction, distance);
        let sample = program.distance(point)?;
        if sample.abs() <= epsilon {
            return Ok(MarchResult::Hit {
                distance,
                steps: step + 1,
            });
        }
        distance = distance.saturating_add(sample.max(minimum_step));
        if distance > maximum_distance {
            return Ok(MarchResult::Miss {
                distance,
                steps: step + 1,
            });
        }
    }
    Ok(MarchResult::Miss {
        distance,
        steps: MAXIMUM_MARCH_STEPS,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObsidianError {
    Arithmetic,
    InvalidProgram,
    InvalidNode,
    CapacityExceeded,
    InvalidViewport,
    InvalidRay,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sphere() -> SdfProgram {
        SdfProgram::new(&[SdfInstruction::Sphere {
            center: Vector3::ZERO,
            radius: Fixed::ONE,
        }])
        .unwrap()
    }

    #[test]
    fn validates_and_marches_a_bounded_sphere() {
        let result = march(
            &sphere(),
            Ray {
                origin: Vector3 {
                    x: Fixed::ZERO,
                    y: Fixed::ZERO,
                    z: Fixed::from_integer(-4),
                },
                direction: Vector3 {
                    x: Fixed::ZERO,
                    y: Fixed::ZERO,
                    z: Fixed::ONE,
                },
            },
        )
        .unwrap();
        assert!(matches!(result, MarchResult::Hit { .. }));
    }

    #[test]
    fn semantic_gravity_selects_before_ray_marching() {
        let mut shell = ObsidianShell::new();
        shell
            .assimilate_app(SemanticAppNode {
                app_id: 7,
                heat_signature: 100,
                center_x: Fixed::ZERO,
                center_y: Fixed::ZERO,
                color: [255, 0, 0, 255],
                program: sphere(),
            })
            .unwrap();
        assert_eq!(
            shell.calculate_warp_manifold(Fixed::ZERO, Fixed::ZERO),
            Some(7)
        );
        assert_eq!(
            shell.evaluate_pixel(960, 540, 1920, 1080).unwrap(),
            [255, 0, 0, 255]
        );
    }
}
