//! Persistent homology through dimension two over GF(2).
//!
//! The implementation performs the standard left-to-right boundary-matrix
//! reduction.  Every simplex is ordered by filtration, dimension, and vertex
//! tuple.  Finite and essential bars are returned with sealed representative
//! roots.

pub const MAX_SIMPLICES: usize = 160;
pub const MAX_BARS: usize = 160;
pub const MAX_VERTICES_PER_SIMPLEX: usize = 3;
const COLUMN_LIMBS: usize = 3;
const NONE: u16 = u16::MAX;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistenceError {
    Capacity,
    InvalidDimension,
    InvalidVertices,
    DuplicateSimplex,
    MissingFace,
    FiltrationViolation,
    ZeroSecret,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Simplex {
    pub dimension: u8,
    pub vertices: [u8; MAX_VERTICES_PER_SIMPLEX],
    pub filtration: u64,
    pub tag: u64,
}

impl Simplex {
    pub const EMPTY: Self = Self {
        dimension: 0,
        vertices: [0; MAX_VERTICES_PER_SIMPLEX],
        filtration: 0,
        tag: 0,
    };

    pub fn vertex(vertex: u8, filtration: u64, tag: u64) -> Self {
        Self {
            dimension: 0,
            vertices: [vertex, 0, 0],
            filtration,
            tag,
        }
    }

    pub fn edge(
        first: u8,
        second: u8,
        filtration: u64,
        tag: u64,
    ) -> Result<Self, PersistenceError> {
        if first == second {
            return Err(PersistenceError::InvalidVertices);
        }
        let (first, second) = if first < second {
            (first, second)
        } else {
            (second, first)
        };
        Ok(Self {
            dimension: 1,
            vertices: [first, second, 0],
            filtration,
            tag,
        })
    }

    pub fn triangle(
        first: u8,
        second: u8,
        third: u8,
        filtration: u64,
        tag: u64,
    ) -> Result<Self, PersistenceError> {
        let mut vertices = [first, second, third];
        vertices.sort_unstable();
        if vertices[0] == vertices[1] || vertices[1] == vertices[2] {
            return Err(PersistenceError::InvalidVertices);
        }
        Ok(Self {
            dimension: 2,
            vertices,
            filtration,
            tag,
        })
    }

    pub const fn vertex_count(self) -> usize {
        self.dimension as usize + 1
    }

    fn canonical(self) -> bool {
        match self.dimension {
            0 => true,
            1 => self.vertices[0] < self.vertices[1],
            2 => self.vertices[0] < self.vertices[1] && self.vertices[1] < self.vertices[2],
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Column {
    limbs: [u64; COLUMN_LIMBS],
}

impl Column {
    const ZERO: Self = Self {
        limbs: [0; COLUMN_LIMBS],
    };

    fn singleton(index: usize) -> Self {
        let mut column = Self::ZERO;
        column.set(index);
        column
    }

    fn set(&mut self, index: usize) {
        self.limbs[index / 64] |= 1_u64 << (index % 64);
    }

    fn xor_assign(&mut self, other: Self) {
        for limb in 0..COLUMN_LIMBS {
            self.limbs[limb] ^= other.limbs[limb];
        }
    }

    fn is_zero(self) -> bool {
        self.limbs.iter().all(|limb| *limb == 0)
    }

    fn low(self) -> Option<usize> {
        for limb_index in (0..COLUMN_LIMBS).rev() {
            let limb = self.limbs[limb_index];
            if limb != 0 {
                let bit = 63 - limb.leading_zeros() as usize;
                return Some(limb_index * 64 + bit);
            }
        }
        None
    }

    fn root(self, secret: u64) -> u64 {
        let mut state = secret;
        for limb in self.limbs {
            state = mix(state, limb);
        }
        state
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PersistenceBar {
    pub dimension: u8,
    pub birth: u64,
    pub death: u64,
    pub birth_simplex: u16,
    pub death_simplex: u16,
    pub representative_root: u64,
}

impl PersistenceBar {
    pub const ESSENTIAL_DEATH: u64 = u64::MAX;

    pub const EMPTY: Self = Self {
        dimension: 0,
        birth: 0,
        death: 0,
        birth_simplex: NONE,
        death_simplex: NONE,
        representative_root: 0,
    };

    pub const fn essential(self) -> bool {
        self.death == Self::ESSENTIAL_DEATH
    }

    pub const fn persistence(self) -> u64 {
        if self.essential() {
            u64::MAX
        } else {
            self.death.saturating_sub(self.birth)
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct PersistenceReport {
    bars: [PersistenceBar; MAX_BARS],
    pub bar_count: usize,
    pub simplex_count: usize,
    pub finite_count: [u16; 3],
    pub essential_count: [u16; 3],
    pub maximum_persistence: [u64; 3],
    pub complex_root: u64,
    pub barcode_root: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PersistenceDigest {
    pub bar_count: u16,
    pub simplex_count: u16,
    pub finite_count: [u16; 3],
    pub essential_count: [u16; 3],
    pub maximum_persistence: [u64; 3],
    pub complex_root: u64,
    pub barcode_root: u64,
}

impl PersistenceDigest {
    pub const EMPTY: Self = Self {
        bar_count: 0,
        simplex_count: 0,
        finite_count: [0; 3],
        essential_count: [0; 3],
        maximum_persistence: [0; 3],
        complex_root: 0,
        barcode_root: 0,
    };
}

impl PersistenceReport {
    pub const EMPTY: Self = Self {
        bars: [PersistenceBar::EMPTY; MAX_BARS],
        bar_count: 0,
        simplex_count: 0,
        finite_count: [0; 3],
        essential_count: [0; 3],
        maximum_persistence: [0; 3],
        complex_root: 0,
        barcode_root: 0,
    };

    pub fn bars(&self) -> &[PersistenceBar] {
        &self.bars[..self.bar_count]
    }

    pub fn digest(&self) -> PersistenceDigest {
        PersistenceDigest {
            bar_count: self.bar_count.min(u16::MAX as usize) as u16,
            simplex_count: self.simplex_count.min(u16::MAX as usize) as u16,
            finite_count: self.finite_count,
            essential_count: self.essential_count,
            maximum_persistence: self.maximum_persistence,
            complex_root: self.complex_root,
            barcode_root: self.barcode_root,
        }
    }

    pub fn betti_at(&self, dimension: u8, filtration: u64) -> u16 {
        self.bars()
            .iter()
            .filter(|bar| {
                bar.dimension == dimension
                    && bar.birth <= filtration
                    && (bar.essential() || filtration < bar.death)
            })
            .count()
            .min(u16::MAX as usize) as u16
    }

    pub fn verify(&self, secret: u64) -> bool {
        self.bar_count <= MAX_BARS
            && self.simplex_count <= MAX_SIMPLICES
            && self.barcode_root == barcode_root(secret, self)
    }
}

pub struct FilteredComplex {
    simplices: [Simplex; MAX_SIMPLICES],
    length: usize,
}

pub struct PersistenceWorkspace {
    ordered: [Simplex; MAX_SIMPLICES],
    reduced: [Column; MAX_SIMPLICES],
    witnesses: [Column; MAX_SIMPLICES],
    pivot_owner: [u16; MAX_SIMPLICES],
    births: [bool; MAX_SIMPLICES],
    paired_births: [bool; MAX_SIMPLICES],
}

impl PersistenceWorkspace {
    pub const fn new() -> Self {
        Self {
            ordered: [Simplex::EMPTY; MAX_SIMPLICES],
            reduced: [Column::ZERO; MAX_SIMPLICES],
            witnesses: [Column::ZERO; MAX_SIMPLICES],
            pivot_owner: [NONE; MAX_SIMPLICES],
            births: [false; MAX_SIMPLICES],
            paired_births: [false; MAX_SIMPLICES],
        }
    }

    fn clear(&mut self) {
        self.ordered = [Simplex::EMPTY; MAX_SIMPLICES];
        self.reduced = [Column::ZERO; MAX_SIMPLICES];
        self.witnesses = [Column::ZERO; MAX_SIMPLICES];
        self.pivot_owner = [NONE; MAX_SIMPLICES];
        self.births = [false; MAX_SIMPLICES];
        self.paired_births = [false; MAX_SIMPLICES];
    }
}

impl Default for PersistenceWorkspace {
    fn default() -> Self {
        Self::new()
    }
}

impl FilteredComplex {
    pub const fn new() -> Self {
        Self {
            simplices: [Simplex::EMPTY; MAX_SIMPLICES],
            length: 0,
        }
    }

    pub fn insert(&mut self, simplex: Simplex) -> Result<(), PersistenceError> {
        if simplex.dimension > 2 {
            return Err(PersistenceError::InvalidDimension);
        }
        if !simplex.canonical() {
            return Err(PersistenceError::InvalidVertices);
        }
        if self.simplices[..self.length]
            .iter()
            .any(|existing| same_simplex(*existing, simplex))
        {
            return Err(PersistenceError::DuplicateSimplex);
        }

        let destination = self
            .simplices
            .get_mut(self.length)
            .ok_or(PersistenceError::Capacity)?;
        *destination = simplex;
        self.length += 1;
        Ok(())
    }

    pub fn simplices(&self) -> &[Simplex] {
        &self.simplices[..self.length]
    }

    pub fn clear(&mut self) {
        self.simplices = [Simplex::EMPTY; MAX_SIMPLICES];
        self.length = 0;
    }

    pub const fn len(&self) -> usize {
        self.length
    }

    pub fn reduce_into(
        &self,
        secret: u64,
        workspace: &mut PersistenceWorkspace,
        report: &mut PersistenceReport,
    ) -> Result<(), PersistenceError> {
        if secret == 0 {
            return Err(PersistenceError::ZeroSecret);
        }

        workspace.clear();
        workspace.ordered[..self.length].copy_from_slice(&self.simplices[..self.length]);
        workspace.ordered[..self.length].sort_unstable_by_key(simplex_order_key);

        validate_filtration(&workspace.ordered[..self.length])?;

        *report = PersistenceReport {
            simplex_count: self.length,
            complex_root: complex_root(secret, &workspace.ordered[..self.length]),
            ..PersistenceReport::EMPTY
        };

        for column_index in 0..self.length {
            let mut column = boundary_column(&workspace.ordered[..self.length], column_index)?;
            let mut witness = Column::singleton(column_index);

            loop {
                let Some(low) = column.low() else {
                    workspace.births[column_index] = true;
                    workspace.reduced[column_index] = Column::ZERO;
                    workspace.witnesses[column_index] = witness;
                    break;
                };

                let owner = workspace.pivot_owner[low];
                if owner == NONE {
                    workspace.pivot_owner[low] = column_index as u16;
                    workspace.reduced[column_index] = column;
                    workspace.witnesses[column_index] = witness;
                    workspace.paired_births[low] = true;

                    let bar = PersistenceBar {
                        dimension: workspace.ordered[low].dimension,
                        birth: workspace.ordered[low].filtration,
                        death: workspace.ordered[column_index].filtration,
                        birth_simplex: low as u16,
                        death_simplex: column_index as u16,
                        representative_root: witness.root(mix(secret, workspace.ordered[low].tag)),
                    };
                    push_bar(report, bar)?;
                    break;
                }

                let owner = owner as usize;
                column.xor_assign(workspace.reduced[owner]);
                witness.xor_assign(workspace.witnesses[owner]);
            }
        }

        for index in 0..self.length {
            if workspace.births[index] && !workspace.paired_births[index] {
                let bar = PersistenceBar {
                    dimension: workspace.ordered[index].dimension,
                    birth: workspace.ordered[index].filtration,
                    death: PersistenceBar::ESSENTIAL_DEATH,
                    birth_simplex: index as u16,
                    death_simplex: NONE,
                    representative_root: workspace.witnesses[index]
                        .root(mix(secret, workspace.ordered[index].tag)),
                };
                push_bar(report, bar)?;
            }
        }

        report.bars[..report.bar_count].sort_unstable_by_key(|bar| {
            (
                bar.dimension,
                bar.birth,
                bar.death,
                bar.birth_simplex,
                bar.death_simplex,
            )
        });
        report.barcode_root = barcode_root(secret, report);
        Ok(())
    }
}

impl Default for FilteredComplex {
    fn default() -> Self {
        Self::new()
    }
}

fn push_bar(report: &mut PersistenceReport, bar: PersistenceBar) -> Result<(), PersistenceError> {
    let destination = report
        .bars
        .get_mut(report.bar_count)
        .ok_or(PersistenceError::Capacity)?;
    *destination = bar;
    report.bar_count += 1;

    let dimension = bar.dimension as usize;
    if dimension < 3 {
        if bar.essential() {
            report.essential_count[dimension] = report.essential_count[dimension].saturating_add(1);
        } else {
            report.finite_count[dimension] = report.finite_count[dimension].saturating_add(1);
            report.maximum_persistence[dimension] =
                report.maximum_persistence[dimension].max(bar.persistence());
        }
    }

    Ok(())
}

fn simplex_order_key(simplex: &Simplex) -> (u64, u8, [u8; 3], u64) {
    (
        simplex.filtration,
        simplex.dimension,
        simplex.vertices,
        simplex.tag,
    )
}

fn same_simplex(left: Simplex, right: Simplex) -> bool {
    left.dimension == right.dimension && left.vertices == right.vertices
}

fn find_simplex(
    simplices: &[Simplex],
    dimension: u8,
    vertices: [u8; 3],
    before: usize,
) -> Option<usize> {
    simplices[..before]
        .iter()
        .position(|simplex| simplex.dimension == dimension && simplex.vertices == vertices)
}

fn boundary_column(simplices: &[Simplex], index: usize) -> Result<Column, PersistenceError> {
    let simplex = simplices[index];
    let mut boundary = Column::ZERO;

    match simplex.dimension {
        0 => {}
        1 => {
            for vertex in [simplex.vertices[0], simplex.vertices[1]] {
                let face = [vertex, 0, 0];
                let face_index =
                    find_simplex(simplices, 0, face, index).ok_or(PersistenceError::MissingFace)?;
                boundary.set(face_index);
            }
        }
        2 => {
            let faces = [
                [simplex.vertices[0], simplex.vertices[1], 0],
                [simplex.vertices[0], simplex.vertices[2], 0],
                [simplex.vertices[1], simplex.vertices[2], 0],
            ];
            for face in faces {
                let face_index =
                    find_simplex(simplices, 1, face, index).ok_or(PersistenceError::MissingFace)?;
                boundary.set(face_index);
            }
        }
        _ => return Err(PersistenceError::InvalidDimension),
    }

    Ok(boundary)
}

fn validate_filtration(simplices: &[Simplex]) -> Result<(), PersistenceError> {
    for (index, simplex) in simplices.iter().copied().enumerate() {
        if simplex.dimension == 0 {
            continue;
        }

        let boundary = boundary_column(simplices, index)?;
        for face_index in 0..index {
            let limb = boundary.limbs[face_index / 64];
            if limb & (1_u64 << (face_index % 64)) != 0
                && simplices[face_index].filtration > simplex.filtration
            {
                return Err(PersistenceError::FiltrationViolation);
            }
        }
    }

    Ok(())
}

fn complex_root(secret: u64, simplices: &[Simplex]) -> u64 {
    let mut state = mix(secret, simplices.len() as u64);
    for simplex in simplices {
        state = mix(state, simplex.dimension as u64);
        state = mix(state, simplex.filtration);
        state = mix(
            state,
            simplex.vertices[0] as u64
                | ((simplex.vertices[1] as u64) << 8)
                | ((simplex.vertices[2] as u64) << 16),
        );
        state = mix(state, simplex.tag);
    }
    state
}

fn barcode_root(secret: u64, report: &PersistenceReport) -> u64 {
    let mut state = mix(secret, report.complex_root);
    state = mix(state, report.simplex_count as u64);
    state = mix(state, report.bar_count as u64);

    for bar in report.bars() {
        state = mix(state, bar.dimension as u64);
        state = mix(state, bar.birth);
        state = mix(state, bar.death);
        state = mix(
            state,
            bar.birth_simplex as u64 | ((bar.death_simplex as u64) << 16),
        );
        state = mix(state, bar.representative_root);
    }

    state
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
    fn triangle_kills_one_dimensional_cycle() {
        let mut complex = FilteredComplex::new();
        for vertex in 0..3 {
            complex
                .insert(Simplex::vertex(vertex, 0, vertex as u64))
                .unwrap();
        }
        complex.insert(Simplex::edge(0, 1, 1, 10).unwrap()).unwrap();
        complex.insert(Simplex::edge(1, 2, 1, 11).unwrap()).unwrap();
        complex.insert(Simplex::edge(0, 2, 2, 12).unwrap()).unwrap();
        complex
            .insert(Simplex::triangle(0, 1, 2, 5, 13).unwrap())
            .unwrap();

        let mut workspace = PersistenceWorkspace::new();
        let mut report = PersistenceReport::EMPTY;
        complex.reduce_into(7, &mut workspace, &mut report).unwrap();
        assert_eq!(report.betti_at(0, 10), 1);
        assert_eq!(report.betti_at(1, 3), 1);
        assert_eq!(report.betti_at(1, 5), 0);
        assert!(report.verify(7));
    }

    #[test]
    fn disconnected_vertices_are_essential_h0_classes() {
        let mut complex = FilteredComplex::new();
        complex.insert(Simplex::vertex(0, 0, 1)).unwrap();
        complex.insert(Simplex::vertex(1, 0, 2)).unwrap();

        let mut workspace = PersistenceWorkspace::new();
        let mut report = PersistenceReport::EMPTY;
        complex.reduce_into(9, &mut workspace, &mut report).unwrap();
        assert_eq!(report.essential_count[0], 2);
        assert_eq!(report.betti_at(0, 100), 2);
    }
}
