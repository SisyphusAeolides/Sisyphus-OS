//! Finite cellular sheaves over a hardware cover.
//!
//! Every restriction is an explicit GF(2) linear map.  Local capability
//! sections glue exactly when their cellular coboundary vanishes.

pub const MAX_OPENS: usize = 32;
pub const MAX_OVERLAPS: usize = 64;
pub const MAX_SECTIONS: usize = 96;
pub const MAX_STALK_BITS: usize = 64;
pub const MAX_GLOBAL_BITS: usize = 64;
const NONE: u16 = u16::MAX;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SheafError {
    Capacity,
    UnknownOpen,
    DuplicateOpen,
    DuplicateOverlap,
    InvalidDimension,
    InvalidLinearMap,
    GlobalDimension,
    StaleEpoch,
    MissingSection,
    ZeroSecret,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenId(pub u16);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OverlapId(pub u16);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BinaryLinearMap {
    pub input_dimension: u8,
    pub output_dimension: u8,
    pub rows: [u64; MAX_STALK_BITS],
}

impl BinaryLinearMap {
    pub const ZERO: Self = Self {
        input_dimension: 0,
        output_dimension: 0,
        rows: [0; MAX_STALK_BITS],
    };

    pub fn identity(dimension: u8) -> Result<Self, SheafError> {
        if dimension as usize > MAX_STALK_BITS {
            return Err(SheafError::InvalidDimension);
        }

        let mut map = Self {
            input_dimension: dimension,
            output_dimension: dimension,
            rows: [0; MAX_STALK_BITS],
        };
        for bit in 0..dimension as usize {
            map.rows[bit] = 1_u64 << bit;
        }
        Ok(map)
    }

    pub fn projection(input_dimension: u8, selected_bits: &[u8]) -> Result<Self, SheafError> {
        if input_dimension as usize > MAX_STALK_BITS || selected_bits.len() > MAX_STALK_BITS {
            return Err(SheafError::InvalidDimension);
        }

        let mut map = Self {
            input_dimension,
            output_dimension: selected_bits.len() as u8,
            rows: [0; MAX_STALK_BITS],
        };

        for (output, input) in selected_bits.iter().copied().enumerate() {
            if input >= input_dimension {
                return Err(SheafError::InvalidLinearMap);
            }
            map.rows[output] = 1_u64 << input;
        }

        Ok(map)
    }

    pub fn validate(self) -> Result<(), SheafError> {
        if self.input_dimension as usize > MAX_STALK_BITS
            || self.output_dimension as usize > MAX_STALK_BITS
        {
            return Err(SheafError::InvalidDimension);
        }

        let input_mask = dimension_mask(self.input_dimension);
        for row in self.rows[..self.output_dimension as usize].iter().copied() {
            if row & !input_mask != 0 {
                return Err(SheafError::InvalidLinearMap);
            }
        }

        if self.rows[self.output_dimension as usize..]
            .iter()
            .any(|row| *row != 0)
        {
            return Err(SheafError::InvalidLinearMap);
        }

        Ok(())
    }

    pub fn apply(self, input: u64) -> Result<u64, SheafError> {
        self.validate()?;
        let input = input & dimension_mask(self.input_dimension);
        let mut output = 0_u64;

        for row in 0..self.output_dimension as usize {
            let parity = (self.rows[row] & input).count_ones() & 1;
            output |= u64::from(parity) << row;
        }

        Ok(output)
    }

    pub fn rank(self) -> Result<u8, SheafError> {
        self.validate()?;
        let mut rows = self.rows;
        Ok(binary_rank(
            &mut rows,
            self.output_dimension as usize,
            self.input_dimension as usize,
        ) as u8)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SheafOpen {
    pub id: OpenId,
    pub geometry_key: u64,
    pub stalk_dimension: u8,
    pub flags: u32,
}

impl SheafOpen {
    pub const EMPTY: Self = Self {
        id: OpenId(NONE),
        geometry_key: 0,
        stalk_dimension: 0,
        flags: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SheafOverlap {
    pub id: OverlapId,
    pub left: OpenId,
    pub right: OpenId,
    pub stalk_dimension: u8,
    pub restrict_left: BinaryLinearMap,
    pub restrict_right: BinaryLinearMap,
    pub geometry_key: u64,
}

impl SheafOverlap {
    pub const EMPTY: Self = Self {
        id: OverlapId(NONE),
        left: OpenId(NONE),
        right: OpenId(NONE),
        stalk_dimension: 0,
        restrict_left: BinaryLinearMap::ZERO,
        restrict_right: BinaryLinearMap::ZERO,
        geometry_key: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalSection {
    pub owner: u64,
    pub open: OpenId,
    pub value: u64,
    pub epoch: u64,
}

impl LocalSection {
    pub const EMPTY: Self = Self {
        owner: 0,
        open: OpenId(NONE),
        value: 0,
        epoch: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GlueCertificate {
    pub owner: u64,
    pub epoch: u64,
    pub checked_overlaps: u16,
    pub incomplete_overlaps: u16,
    pub nonzero_obstructions: u16,
    pub obstruction_weight: u32,
    pub first_failure: OverlapId,
    pub obstruction_root: u64,
    pub global_section_dimension: u8,
    pub root: u64,
}

impl GlueCertificate {
    pub const EMPTY: Self = Self {
        owner: 0,
        epoch: 0,
        checked_overlaps: 0,
        incomplete_overlaps: 0,
        nonzero_obstructions: 0,
        obstruction_weight: 0,
        first_failure: OverlapId(NONE),
        obstruction_root: 0,
        global_section_dimension: 0,
        root: 0,
    };

    pub const fn glued(self) -> bool {
        self.incomplete_overlaps == 0 && self.nonzero_obstructions == 0
    }

    pub fn verify(&self, secret: u64) -> bool {
        self.root == certificate_root(secret, self)
    }
}

pub struct CellularCapabilitySheaf {
    opens: [SheafOpen; MAX_OPENS],
    open_count: usize,
    overlaps: [SheafOverlap; MAX_OVERLAPS],
    overlap_count: usize,
    sections: [LocalSection; MAX_SECTIONS],
    section_count: usize,
    epoch: u64,
    secret: u64,
}

impl CellularCapabilitySheaf {
    pub fn new(secret: u64) -> Result<Self, SheafError> {
        if secret == 0 {
            return Err(SheafError::ZeroSecret);
        }

        Ok(Self {
            opens: [SheafOpen::EMPTY; MAX_OPENS],
            open_count: 0,
            overlaps: [SheafOverlap::EMPTY; MAX_OVERLAPS],
            overlap_count: 0,
            sections: [LocalSection::EMPTY; MAX_SECTIONS],
            section_count: 0,
            epoch: 1,
            secret,
        })
    }

    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn opens(&self) -> &[SheafOpen] {
        &self.opens[..self.open_count]
    }

    pub fn overlaps(&self) -> &[SheafOverlap] {
        &self.overlaps[..self.overlap_count]
    }

    pub fn add_open(
        &mut self,
        geometry_key: u64,
        stalk_dimension: u8,
        flags: u32,
    ) -> Result<OpenId, SheafError> {
        if stalk_dimension == 0 || stalk_dimension as usize > MAX_STALK_BITS {
            return Err(SheafError::InvalidDimension);
        }
        if self
            .opens()
            .iter()
            .any(|open| open.geometry_key == geometry_key)
        {
            return Err(SheafError::DuplicateOpen);
        }

        let id = OpenId(self.open_count as u16);
        let destination = self
            .opens
            .get_mut(self.open_count)
            .ok_or(SheafError::Capacity)?;
        *destination = SheafOpen {
            id,
            geometry_key,
            stalk_dimension,
            flags,
        };
        self.open_count += 1;
        self.epoch = self.epoch.wrapping_add(1).max(1);
        Ok(id)
    }

    pub fn add_overlap(
        &mut self,
        left: OpenId,
        right: OpenId,
        geometry_key: u64,
        restrict_left: BinaryLinearMap,
        restrict_right: BinaryLinearMap,
    ) -> Result<OverlapId, SheafError> {
        if left == right {
            return Err(SheafError::DuplicateOverlap);
        }

        let left_open = self.open(left)?;
        let right_open = self.open(right)?;
        restrict_left.validate()?;
        restrict_right.validate()?;

        if restrict_left.input_dimension != left_open.stalk_dimension
            || restrict_right.input_dimension != right_open.stalk_dimension
            || restrict_left.output_dimension != restrict_right.output_dimension
            || restrict_left.output_dimension == 0
        {
            return Err(SheafError::InvalidLinearMap);
        }

        let (canonical_left, canonical_right) = if left.0 < right.0 {
            (left, right)
        } else {
            (right, left)
        };

        if self
            .overlaps()
            .iter()
            .any(|overlap| overlap.left == canonical_left && overlap.right == canonical_right)
        {
            return Err(SheafError::DuplicateOverlap);
        }

        let (map_left, map_right) = if left == canonical_left {
            (restrict_left, restrict_right)
        } else {
            (restrict_right, restrict_left)
        };

        let id = OverlapId(self.overlap_count as u16);
        let destination = self
            .overlaps
            .get_mut(self.overlap_count)
            .ok_or(SheafError::Capacity)?;
        *destination = SheafOverlap {
            id,
            left: canonical_left,
            right: canonical_right,
            stalk_dimension: map_left.output_dimension,
            restrict_left: map_left,
            restrict_right: map_right,
            geometry_key,
        };
        self.overlap_count += 1;
        self.epoch = self.epoch.wrapping_add(1).max(1);
        Ok(id)
    }

    pub fn install(&mut self, owner: u64, open: OpenId, value: u64) -> Result<(), SheafError> {
        let open_record = self.open(open)?;
        let value = value & dimension_mask(open_record.stalk_dimension);

        if let Some(section) = self.sections[..self.section_count]
            .iter_mut()
            .find(|section| section.owner == owner && section.open == open)
        {
            section.value = value;
            section.epoch = self.epoch;
            return Ok(());
        }

        let destination = self
            .sections
            .get_mut(self.section_count)
            .ok_or(SheafError::Capacity)?;
        *destination = LocalSection {
            owner,
            open,
            value,
            epoch: self.epoch,
        };
        self.section_count += 1;
        Ok(())
    }

    pub fn revoke_owner(&mut self, owner: u64) {
        for section in &mut self.sections[..self.section_count] {
            if section.owner == owner {
                *section = LocalSection::EMPTY;
            }
        }
        self.compact_sections();
        self.epoch = self.epoch.wrapping_add(1).max(1);
    }

    pub fn certify(&self, owner: u64) -> Result<GlueCertificate, SheafError> {
        let global_dimension = self.global_section_dimension()?;
        let mut certificate = GlueCertificate {
            owner,
            epoch: self.epoch,
            global_section_dimension: global_dimension,
            ..GlueCertificate::EMPTY
        };
        let mut obstruction_root = mix(self.secret, owner ^ self.epoch);

        for overlap in self.overlaps() {
            let left = self.section(owner, overlap.left);
            let right = self.section(owner, overlap.right);

            let (Some(left), Some(right)) = (left, right) else {
                certificate.incomplete_overlaps = certificate.incomplete_overlaps.saturating_add(1);
                if certificate.first_failure == OverlapId(NONE) {
                    certificate.first_failure = overlap.id;
                }
                obstruction_root = mix(obstruction_root, overlap.geometry_key);
                continue;
            };

            if left.epoch != self.epoch || right.epoch != self.epoch {
                return Err(SheafError::StaleEpoch);
            }

            let left_image = overlap.restrict_left.apply(left.value)?;
            let right_image = overlap.restrict_right.apply(right.value)?;
            let obstruction = left_image ^ right_image;

            certificate.checked_overlaps = certificate.checked_overlaps.saturating_add(1);
            certificate.obstruction_weight = certificate
                .obstruction_weight
                .saturating_add(obstruction.count_ones());
            obstruction_root = mix(obstruction_root, overlap.geometry_key ^ obstruction);

            if obstruction != 0 {
                certificate.nonzero_obstructions =
                    certificate.nonzero_obstructions.saturating_add(1);
                if certificate.first_failure == OverlapId(NONE) {
                    certificate.first_failure = overlap.id;
                }
            }
        }

        certificate.obstruction_root = obstruction_root;
        certificate.root = certificate_root(self.secret, &certificate);
        Ok(certificate)
    }

    pub fn allow(
        &self,
        owner: u64,
        open: OpenId,
        required: u64,
        certificate: &GlueCertificate,
    ) -> Result<(), SheafError> {
        if !certificate.verify(self.secret)
            || certificate.owner != owner
            || certificate.epoch != self.epoch
            || !certificate.glued()
        {
            return Err(SheafError::MissingSection);
        }

        let open_record = self.open(open)?;
        let section = self
            .section(owner, open)
            .ok_or(SheafError::MissingSection)?;
        let required = required & dimension_mask(open_record.stalk_dimension);

        if section.epoch != self.epoch || section.value & required != required {
            return Err(SheafError::MissingSection);
        }

        Ok(())
    }

    pub fn global_section_dimension(&self) -> Result<u8, SheafError> {
        let mut offsets = [0_u8; MAX_OPENS];
        let mut total_dimension = 0_usize;

        for open in self.opens() {
            offsets[open.id.0 as usize] = total_dimension as u8;
            total_dimension += open.stalk_dimension as usize;
            if total_dimension > MAX_GLOBAL_BITS {
                return Err(SheafError::GlobalDimension);
            }
        }

        let mut equations = [0_u64; MAX_STALK_BITS];
        let mut equation_count = 0_usize;

        for overlap in self.overlaps() {
            let left_offset = offsets[overlap.left.0 as usize] as usize;
            let right_offset = offsets[overlap.right.0 as usize] as usize;

            for row in 0..overlap.stalk_dimension as usize {
                let left_row = overlap.restrict_left.rows[row] << left_offset;
                let right_row = overlap.restrict_right.rows[row] << right_offset;
                let equation = left_row ^ right_row;

                if equation != 0 {
                    let destination = equations
                        .get_mut(equation_count)
                        .ok_or(SheafError::Capacity)?;
                    *destination = equation;
                    equation_count += 1;
                }
            }
        }

        let rank = binary_rank(&mut equations, equation_count, total_dimension);
        Ok((total_dimension - rank) as u8)
    }

    fn open(&self, id: OpenId) -> Result<SheafOpen, SheafError> {
        self.opens()
            .get(id.0 as usize)
            .copied()
            .filter(|open| open.id == id)
            .ok_or(SheafError::UnknownOpen)
    }

    fn section(&self, owner: u64, open: OpenId) -> Option<LocalSection> {
        self.sections[..self.section_count]
            .iter()
            .copied()
            .find(|section| section.owner == owner && section.open == open)
    }

    fn compact_sections(&mut self) {
        let mut write = 0_usize;
        for read in 0..self.section_count {
            if self.sections[read].open != OpenId(NONE) {
                self.sections[write] = self.sections[read];
                write += 1;
            }
        }
        for index in write..self.section_count {
            self.sections[index] = LocalSection::EMPTY;
        }
        self.section_count = write;
    }
}

fn binary_rank(rows: &mut [u64], row_count: usize, column_count: usize) -> usize {
    let mut rank = 0_usize;

    for column in 0..column_count {
        let pivot = (rank..row_count).find(|row| rows[*row] & (1_u64 << column) != 0);
        let Some(pivot) = pivot else {
            continue;
        };

        rows.swap(rank, pivot);
        for row in 0..row_count {
            if row != rank && rows[row] & (1_u64 << column) != 0 {
                rows[row] ^= rows[rank];
            }
        }
        rank += 1;

        if rank == row_count {
            break;
        }
    }

    rank
}

const fn dimension_mask(dimension: u8) -> u64 {
    if dimension >= 64 {
        u64::MAX
    } else if dimension == 0 {
        0
    } else {
        (1_u64 << dimension) - 1
    }
}

fn certificate_root(secret: u64, certificate: &GlueCertificate) -> u64 {
    let mut state = mix(secret, certificate.owner);
    state = mix(state, certificate.epoch);
    state = mix(
        state,
        certificate.checked_overlaps as u64
            | ((certificate.incomplete_overlaps as u64) << 16)
            | ((certificate.nonzero_obstructions as u64) << 32),
    );
    state = mix(state, certificate.obstruction_weight as u64);
    state = mix(state, certificate.first_failure.0 as u64);
    state = mix(state, certificate.obstruction_root);
    mix(state, certificate.global_section_dimension as u64)
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
    fn incompatible_restrictions_create_a_real_obstruction() {
        let mut sheaf = CellularCapabilitySheaf::new(7).unwrap();
        let left = sheaf.add_open(1, 4, 0).unwrap();
        let right = sheaf.add_open(2, 4, 0).unwrap();
        let projection = BinaryLinearMap::projection(4, &[0, 1]).unwrap();
        sheaf
            .add_overlap(left, right, 3, projection, projection)
            .unwrap();

        sheaf.install(11, left, 0b0001).unwrap();
        sheaf.install(11, right, 0b0010).unwrap();

        let certificate = sheaf.certify(11).unwrap();
        assert_eq!(certificate.nonzero_obstructions, 1);
        assert!(!certificate.glued());
    }

    #[test]
    fn equal_restrictions_glue_and_authorize() {
        let mut sheaf = CellularCapabilitySheaf::new(9).unwrap();
        let left = sheaf.add_open(1, 4, 0).unwrap();
        let right = sheaf.add_open(2, 4, 0).unwrap();
        let projection = BinaryLinearMap::projection(4, &[0, 1]).unwrap();
        sheaf
            .add_overlap(left, right, 3, projection, projection)
            .unwrap();

        sheaf.install(12, left, 0b1101).unwrap();
        sheaf.install(12, right, 0b1001).unwrap();

        let certificate = sheaf.certify(12).unwrap();
        assert!(certificate.glued());
        sheaf.allow(12, left, 0b0100, &certificate).unwrap();
    }
}
