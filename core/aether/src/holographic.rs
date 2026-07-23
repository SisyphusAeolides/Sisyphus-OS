pub const MAXIMUM_PROOF_DEPTH: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HolographicError {
    InvalidShape,
    LeafOutOfRange,
    TreeTooDeep,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HolographicProof {
    pub siblings: [u64; MAXIMUM_PROOF_DEPTH],
    pub depth: u8,
}

impl HolographicProof {
    pub const EMPTY: Self = Self {
        siblings: [0; MAXIMUM_PROOF_DEPTH],
        depth: 0,
    };
}

pub struct HolographicTree<const LEAVES: usize, const NODES: usize> {
    nodes: [u64; NODES],
    epoch: u64,
}

impl<const LEAVES: usize, const NODES: usize> HolographicTree<LEAVES, NODES> {
    pub const fn new() -> Self {
        Self {
            nodes: [0; NODES],
            epoch: 0,
        }
    }

    pub const fn shape_is_valid() -> bool {
        LEAVES != 0
            && LEAVES.is_power_of_two()
            && NODES == LEAVES * 2
            && LEAVES <= (1 << MAXIMUM_PROOF_DEPTH)
    }

    pub fn clear(&mut self) -> Result<(), HolographicError> {
        self.require_shape()?;
        self.nodes.fill(0);
        self.epoch = self.epoch.wrapping_add(1).max(1);
        Ok(())
    }

    /// Writes a leaf without rebuilding the internal tree.
    ///
    /// Use this for complete snapshots, then call rebuild().
    pub fn write_leaf(&mut self, index: usize, value: u64) -> Result<(), HolographicError> {
        self.require_shape()?;

        if index >= LEAVES {
            return Err(HolographicError::LeafOutOfRange);
        }

        self.nodes[LEAVES + index] = leaf_digest(index, value);

        Ok(())
    }

    pub fn rebuild(&mut self) -> Result<u64, HolographicError> {
        self.require_shape()?;

        for node in (1..LEAVES).rev() {
            self.nodes[node] = branch_digest(self.nodes[node * 2], self.nodes[node * 2 + 1]);
        }

        self.epoch = self.epoch.wrapping_add(1).max(1);

        Ok(self.nodes[1])
    }

    pub fn update(&mut self, index: usize, value: u64) -> Result<u64, HolographicError> {
        self.write_leaf(index, value)?;

        let mut node = LEAVES + index;

        while node > 1 {
            node /= 2;

            self.nodes[node] = branch_digest(self.nodes[node * 2], self.nodes[node * 2 + 1]);
        }

        self.epoch = self.epoch.wrapping_add(1).max(1);

        Ok(self.nodes[1])
    }

    pub fn proof(&self, index: usize) -> Result<HolographicProof, HolographicError> {
        self.require_shape()?;

        if index >= LEAVES {
            return Err(HolographicError::LeafOutOfRange);
        }

        let mut proof = HolographicProof::EMPTY;
        let mut node = LEAVES + index;
        let mut depth = 0_usize;

        while node > 1 {
            if depth == MAXIMUM_PROOF_DEPTH {
                return Err(HolographicError::TreeTooDeep);
            }

            proof.siblings[depth] = if node & 1 == 0 {
                self.nodes[node + 1]
            } else {
                self.nodes[node - 1]
            };

            node /= 2;
            depth += 1;
        }

        proof.depth = depth as u8;
        Ok(proof)
    }

    pub fn verify(
        index: usize,
        value: u64,
        proof: &HolographicProof,
        expected_root: u64,
    ) -> Result<bool, HolographicError> {
        if !Self::shape_is_valid() {
            return Err(HolographicError::InvalidShape);
        }

        if index >= LEAVES {
            return Err(HolographicError::LeafOutOfRange);
        }

        let depth = usize::from(proof.depth);

        if depth > MAXIMUM_PROOF_DEPTH {
            return Err(HolographicError::TreeTooDeep);
        }

        let mut node = LEAVES + index;
        let mut digest = leaf_digest(index, value);

        for sibling in &proof.siblings[..depth] {
            digest = if node & 1 == 0 {
                branch_digest(digest, *sibling)
            } else {
                branch_digest(*sibling, digest)
            };

            node /= 2;
        }

        Ok(digest == expected_root)
    }

    pub const fn root(&self) -> u64 {
        if NODES > 1 { self.nodes[1] } else { 0 }
    }

    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    fn require_shape(&self) -> Result<(), HolographicError> {
        if Self::shape_is_valid() {
            Ok(())
        } else {
            Err(HolographicError::InvalidShape)
        }
    }
}

impl<const LEAVES: usize, const NODES: usize> Default for HolographicTree<LEAVES, NODES> {
    fn default() -> Self {
        Self::new()
    }
}

fn leaf_digest(index: usize, value: u64) -> u64 {
    mix(0x4c45_4146_5f53_5441 ^ index as u64, value)
}

fn branch_digest(left: u64, right: u64) -> u64 {
    mix(left ^ 0x4252_414e_4348_5f31, right.rotate_left(17))
}

fn mix(mut state: u64, value: u64) -> u64 {
    state ^= value.wrapping_add(0x517c_c1b7_2722_0a95);
    state = state.rotate_left(31);
    state = state.wrapping_mul(0x9e37_79b1_85eb_ca87);
    state ^ (state >> 28)
}
