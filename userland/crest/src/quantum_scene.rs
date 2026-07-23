use crate::compositor::{Rectangle, SurfaceId};

pub const SCENE_RIGHT_OBSERVE: u32 = 1 << 0;
pub const SCENE_RIGHT_MOVE: u32 = 1 << 1;
pub const SCENE_RIGHT_RESIZE: u32 = 1 << 2;
pub const SCENE_RIGHT_STYLE: u32 = 1 << 3;
pub const SCENE_RIGHT_FOCUS: u32 = 1 << 4;
pub const SCENE_RIGHT_DESTROY: u32 = 1 << 5;
pub const SCENE_RIGHT_DELEGATE: u32 = 1 << 31;
pub const SCENE_RIGHT_ALL: u32 = SCENE_RIGHT_OBSERVE
    | SCENE_RIGHT_MOVE
    | SCENE_RIGHT_RESIZE
    | SCENE_RIGHT_STYLE
    | SCENE_RIGHT_FOCUS
    | SCENE_RIGHT_DESTROY
    | SCENE_RIGHT_DELEGATE;

pub const NODE_FLAG_VISIBLE: u32 = 1 << 0;
pub const NODE_FLAG_INPUT: u32 = 1 << 1;
pub const NODE_FLAG_SECURE: u32 = 1 << 2;
pub const NODE_FLAG_MODAL: u32 = 1 << 3;
pub const NODE_FLAG_ALWAYS_ON_TOP: u32 = 1 << 4;
pub const NODE_FLAG_BACKGROUND: u32 = 1 << 5;
pub const NODE_FLAG_QUARANTINED: u32 = 1 << 6;
pub const NODE_FLAG_FROZEN: u32 = 1 << 7;
pub const NODE_FLAG_PREDICTIVE: u32 = 1 << 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum SemanticRole {
    Desktop = 1,
    Application = 2,
    Dialog = 3,
    Panel = 4,
    Notification = 5,
    Cursor = 6,
    Diagnostic = 7,
    Recovery = 8,
    Portal = 9,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct SceneToken(u64);

impl SceneToken {
    pub const INVALID: Self = Self(0);

    pub const fn raw(self) -> u64 {
        self.0
    }

    const fn new(slot: u16, generation: u16, tag: u32) -> Self {
        Self((slot as u64) | ((generation as u64) << 16) | ((tag as u64) << 32))
    }

    const fn slot(self) -> usize {
        self.0 as u16 as usize
    }

    const fn generation(self) -> u16 {
        (self.0 >> 16) as u16
    }

    const fn tag(self) -> u32 {
        (self.0 >> 32) as u32
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SceneNode {
    pub surface: SurfaceId,
    pub owner: u64,
    pub rectangle: Rectangle,
    pub z: i32,
    pub opacity_q16: u16,
    pub phase_q16: u16,
    pub role: SemanticRole,
    pub flags: u32,
    pub color_hint: [u8; 4],
    pub semantic_tag: u64,
}

impl SceneNode {
    pub fn validate(self) -> Result<(), SceneError> {
        if self.surface.0 == 0
            || self.owner == 0
            || self.rectangle.width == 0
            || self.rectangle.height == 0
            || self.rectangle.width > 16_384
            || self.rectangle.height > 16_384
            || self.opacity_q16 == 0
        {
            return Err(SceneError::InvalidNode);
        }
        Ok(())
    }

    pub const fn visible(self) -> bool {
        self.flags & NODE_FLAG_VISIBLE != 0 && self.flags & NODE_FLAG_QUARANTINED == 0
    }

    pub fn contains(self, x: i32, y: i32) -> bool {
        let Some(right) = self.rectangle.x.checked_add(self.rectangle.width as i32) else {
            return false;
        };
        let Some(bottom) = self.rectangle.y.checked_add(self.rectangle.height as i32) else {
            return false;
        };
        x >= self.rectangle.x && x < right && y >= self.rectangle.y && y < bottom
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SceneLease {
    pub token: SceneToken,
    pub rights: u32,
    pub expires_epoch: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SceneMutation {
    Move {
        token: SceneToken,
        x: i32,
        y: i32,
    },
    Resize {
        token: SceneToken,
        width: u32,
        height: u32,
    },
    Restack {
        token: SceneToken,
        z: i32,
    },
    SetOpacity {
        token: SceneToken,
        opacity_q16: u16,
    },
    SetPhase {
        token: SceneToken,
        phase_q16: u16,
    },
    SetFlags {
        token: SceneToken,
        clear: u32,
        set: u32,
    },
    SetSemanticTag {
        token: SceneToken,
        semantic_tag: u64,
    },
}

impl SceneMutation {
    const EMPTY: Self = Self::SetFlags {
        token: SceneToken::INVALID,
        clear: 0,
        set: 0,
    };

    const fn token(self) -> SceneToken {
        match self {
            Self::Move { token, .. }
            | Self::Resize { token, .. }
            | Self::Restack { token, .. }
            | Self::SetOpacity { token, .. }
            | Self::SetPhase { token, .. }
            | Self::SetFlags { token, .. }
            | Self::SetSemanticTag { token, .. } => token,
        }
    }

    const fn required_right(self) -> u32 {
        match self {
            Self::Move { .. } | Self::Restack { .. } => SCENE_RIGHT_MOVE,
            Self::Resize { .. } => SCENE_RIGHT_RESIZE,
            Self::SetOpacity { .. }
            | Self::SetPhase { .. }
            | Self::SetFlags { .. }
            | Self::SetSemanticTag { .. } => SCENE_RIGHT_STYLE,
        }
    }
}

pub struct SceneTransaction<const M: usize> {
    expected_epoch: u64,
    mutations: [SceneMutation; M],
    length: usize,
}

impl<const M: usize> SceneTransaction<M> {
    pub const fn new(expected_epoch: u64) -> Self {
        Self {
            expected_epoch,
            mutations: [SceneMutation::EMPTY; M],
            length: 0,
        }
    }

    pub fn push(&mut self, mutation: SceneMutation) -> Result<(), SceneError> {
        if mutation.token() == SceneToken::INVALID {
            return Err(SceneError::InvalidToken);
        }
        let slot = self
            .mutations
            .get_mut(self.length)
            .ok_or(SceneError::MutationCapacity)?;
        *slot = mutation;
        self.length += 1;
        Ok(())
    }

    pub fn mutations(&self) -> &[SceneMutation] {
        &self.mutations[..self.length]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DirtyRectangle {
    pub before: Rectangle,
    pub after: Rectangle,
}

impl DirtyRectangle {
    const EMPTY: Self = Self {
        before: Rectangle {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        },
        after: Rectangle {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        },
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SceneCommit<const M: usize> {
    pub epoch: u64,
    pub root: u64,
    pub mutation_count: usize,
    pub dirty: [DirtyRectangle; M],
}

impl<const M: usize> SceneCommit<M> {
    pub fn dirty(&self) -> &[DirtyRectangle] {
        &self.dirty[..self.mutation_count]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SceneError {
    ZeroCapacity,
    InvalidNode,
    DuplicateSurface,
    Capacity,
    InvalidToken,
    ForgedToken,
    StaleToken,
    LeaseExpired,
    MissingRight,
    RightsAmplification,
    InvalidMutation,
    MutationCapacity,
    EpochConflict,
    Arithmetic,
}

#[derive(Clone, Copy)]
struct SceneSlot {
    occupied: bool,
    generation: u16,
    rights: u32,
    expires_epoch: u64,
    tag: u32,
    node: SceneNode,
}

impl SceneSlot {
    const EMPTY: Self = Self {
        occupied: false,
        generation: 1,
        rights: 0,
        expires_epoch: 0,
        tag: 0,
        node: SceneNode {
            surface: SurfaceId(0),
            owner: 0,
            rectangle: Rectangle {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            },
            z: 0,
            opacity_q16: 0,
            phase_q16: 0,
            role: SemanticRole::Desktop,
            flags: 0,
            color_hint: [0; 4],
            semantic_tag: 0,
        },
    };
}

pub struct QuantumScene<const N: usize> {
    secret: u64,
    epoch: u64,
    slots: [SceneSlot; N],
    count: usize,
}

impl<const N: usize> QuantumScene<N> {
    pub fn new(secret: u64) -> Result<Self, SceneError> {
        if N == 0 || secret == 0 {
            return Err(SceneError::ZeroCapacity);
        }
        Ok(Self {
            secret,
            epoch: 1,
            slots: [SceneSlot::EMPTY; N],
            count: 0,
        })
    }

    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    pub const fn len(&self) -> usize {
        self.count
    }

    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn spawn(
        &mut self,
        node: SceneNode,
        rights: u32,
        expires_epoch: u64,
    ) -> Result<SceneLease, SceneError> {
        node.validate()?;
        if rights == 0 || expires_epoch <= self.epoch.saturating_add(1) {
            return Err(SceneError::MissingRight);
        }
        if self
            .slots
            .iter()
            .any(|slot| slot.occupied && slot.node.surface == node.surface)
        {
            return Err(SceneError::DuplicateSurface);
        }

        let index = self
            .slots
            .iter()
            .position(|slot| !slot.occupied)
            .ok_or(SceneError::Capacity)?;
        let generation = self.slots[index].generation.wrapping_add(1).max(1);
        let tag = token_tag(
            self.secret,
            index,
            generation,
            node.surface,
            node.owner,
            rights,
        );

        self.slots[index] = SceneSlot {
            occupied: true,
            generation,
            rights,
            expires_epoch,
            tag,
            node,
        };
        self.count += 1;
        self.epoch = self.epoch.wrapping_add(1).max(1);

        Ok(SceneLease {
            token: SceneToken::new(index as u16, generation, tag),
            rights,
            expires_epoch,
        })
    }

    pub fn attenuate(
        &self,
        parent: SceneLease,
        rights: u32,
        expires_epoch: u64,
    ) -> Result<SceneLease, SceneError> {
        let (_, slot) = self.validate_lease(parent, 0)?;
        if rights == 0 || rights & !parent.rights != 0 {
            return Err(SceneError::RightsAmplification);
        }
        if expires_epoch > slot.expires_epoch || expires_epoch <= self.epoch {
            return Err(SceneError::LeaseExpired);
        }

        Ok(SceneLease {
            token: parent.token,
            rights,
            expires_epoch,
        })
    }

    pub fn node(&self, lease: SceneLease) -> Result<SceneNode, SceneError> {
        let (_, slot) = self.validate_lease(lease, SCENE_RIGHT_OBSERVE)?;
        Ok(slot.node)
    }

    pub fn destroy(&mut self, lease: SceneLease) -> Result<SceneNode, SceneError> {
        let (index, _) = self.validate_lease(lease, SCENE_RIGHT_DESTROY)?;
        let node = self.slots[index].node;
        let generation = self.slots[index].generation;
        self.slots[index] = SceneSlot::EMPTY;
        self.slots[index].generation = generation;
        self.count = self.count.saturating_sub(1);
        self.epoch = self.epoch.wrapping_add(1).max(1);
        Ok(node)
    }

    pub fn commit<const M: usize>(
        &mut self,
        transaction: SceneTransaction<M>,
        leases: &[SceneLease],
    ) -> Result<SceneCommit<M>, SceneError> {
        if transaction.expected_epoch != self.epoch {
            return Err(SceneError::EpochConflict);
        }

        let mut staged = self.slots;
        let mut dirty = [DirtyRectangle::EMPTY; M];

        for (mutation_index, mutation) in transaction.mutations().iter().copied().enumerate() {
            let required = mutation.required_right();
            let lease = leases
                .iter()
                .copied()
                .find(|lease| lease.token == mutation.token())
                .ok_or(SceneError::MissingRight)?;
            self.validate_lease(lease, required)?;

            let index = validate_token_in(&staged, self.secret, mutation.token())?;
            let before = staged[index].node.rectangle;
            apply_mutation(&mut staged[index].node, mutation)?;
            staged[index].node.validate()?;
            dirty[mutation_index] = DirtyRectangle {
                before,
                after: staged[index].node.rectangle,
            };
        }

        self.slots = staged;
        self.epoch = self.epoch.wrapping_add(1).max(1);
        let root = self.root();

        Ok(SceneCommit {
            epoch: self.epoch,
            root,
            mutation_count: transaction.length,
            dirty,
        })
    }

    pub fn hit_test(&self, x: i32, y: i32) -> Option<SceneLease> {
        let mut winner: Option<(usize, i32)> = None;

        for (index, slot) in self.slots.iter().enumerate() {
            if !slot.occupied
                || !slot.node.visible()
                || slot.node.flags & NODE_FLAG_INPUT == 0
                || !slot.node.contains(x, y)
                || slot.expires_epoch <= self.epoch
            {
                continue;
            }

            let priority = if slot.node.flags & NODE_FLAG_ALWAYS_ON_TOP != 0 {
                slot.node.z.saturating_add(i32::MAX / 2)
            } else {
                slot.node.z
            };

            if winner.is_none_or(|(_, current)| priority > current) {
                winner = Some((index, priority));
            }
        }

        winner.map(|(index, _)| {
            let slot = self.slots[index];
            SceneLease {
                token: SceneToken::new(index as u16, slot.generation, slot.tag),
                rights: slot.rights,
                expires_epoch: slot.expires_epoch,
            }
        })
    }

    pub fn root(&self) -> u64 {
        let mut state = mix(self.secret, self.epoch);
        state = mix(state, self.count as u64);

        for (index, slot) in self.slots.iter().enumerate() {
            if !slot.occupied {
                continue;
            }
            state = mix(state, index as u64);
            state = mix(state, u64::from(slot.generation));
            state = mix(state, u64::from(slot.rights));
            state = mix(state, slot.expires_epoch);
            state = mix(state, slot.node.surface.0);
            state = mix(state, slot.node.owner);
            state = mix(state, slot.node.rectangle.x as u32 as u64);
            state = mix(state, slot.node.rectangle.y as u32 as u64);
            state = mix(state, u64::from(slot.node.rectangle.width));
            state = mix(state, u64::from(slot.node.rectangle.height));
            state = mix(state, slot.node.z as u32 as u64);
            state = mix(state, u64::from(slot.node.opacity_q16));
            state = mix(state, u64::from(slot.node.phase_q16));
            state = mix(state, slot.node.role as u8 as u64);
            state = mix(state, u64::from(slot.node.flags));
            state = mix(state, slot.node.semantic_tag);
            state = mix(state, u32::from_le_bytes(slot.node.color_hint) as u64);
        }

        state
    }

    pub fn copy_visible(&self, output: &mut [SceneNode]) -> usize {
        let mut count = 0;
        for slot in self
            .slots
            .iter()
            .filter(|slot| slot.occupied && slot.node.visible())
        {
            let Some(destination) = output.get_mut(count) else {
                break;
            };
            *destination = slot.node;
            count += 1;
        }

        output[..count].sort_unstable_by_key(|node| node.z);
        count
    }

    fn validate_lease(
        &self,
        lease: SceneLease,
        required_right: u32,
    ) -> Result<(usize, &SceneSlot), SceneError> {
        let index = validate_token_in(&self.slots, self.secret, lease.token)?;
        let slot = &self.slots[index];

        if lease.expires_epoch <= self.epoch || lease.expires_epoch > slot.expires_epoch {
            return Err(SceneError::LeaseExpired);
        }
        if lease.rights & !slot.rights != 0 {
            return Err(SceneError::RightsAmplification);
        }
        if lease.rights & required_right != required_right {
            return Err(SceneError::MissingRight);
        }

        Ok((index, slot))
    }
}

fn apply_mutation(node: &mut SceneNode, mutation: SceneMutation) -> Result<(), SceneError> {
    match mutation {
        SceneMutation::Move { x, y, .. } => {
            node.rectangle.x = x;
            node.rectangle.y = y;
        }
        SceneMutation::Resize { width, height, .. } => {
            if width == 0 || height == 0 {
                return Err(SceneError::InvalidMutation);
            }
            node.rectangle.width = width;
            node.rectangle.height = height;
        }
        SceneMutation::Restack { z, .. } => node.z = z,
        SceneMutation::SetOpacity { opacity_q16, .. } => {
            if opacity_q16 == 0 {
                return Err(SceneError::InvalidMutation);
            }
            node.opacity_q16 = opacity_q16;
        }
        SceneMutation::SetPhase { phase_q16, .. } => {
            node.phase_q16 = phase_q16;
        }
        SceneMutation::SetFlags { clear, set, .. } => {
            if clear & set != 0 {
                return Err(SceneError::InvalidMutation);
            }
            node.flags = (node.flags & !clear) | set;
        }
        SceneMutation::SetSemanticTag { semantic_tag, .. } => {
            node.semantic_tag = semantic_tag;
        }
    }

    Ok(())
}

fn validate_token_in<const N: usize>(
    slots: &[SceneSlot; N],
    secret: u64,
    token: SceneToken,
) -> Result<usize, SceneError> {
    if token == SceneToken::INVALID {
        return Err(SceneError::InvalidToken);
    }

    let index = token.slot();
    let slot = slots.get(index).ok_or(SceneError::InvalidToken)?;
    if !slot.occupied || slot.generation != token.generation() {
        return Err(SceneError::StaleToken);
    }

    let expected = token_tag(
        secret,
        index,
        slot.generation,
        slot.node.surface,
        slot.node.owner,
        slot.rights,
    );
    if token.tag() != expected || slot.tag != expected {
        return Err(SceneError::ForgedToken);
    }

    Ok(index)
}

fn token_tag(
    secret: u64,
    index: usize,
    generation: u16,
    surface: SurfaceId,
    owner: u64,
    rights: u32,
) -> u32 {
    let mut state = mix(secret, index as u64);
    state = mix(state, u64::from(generation));
    state = mix(state, surface.0);
    state = mix(state, owner);
    state = mix(state, u64::from(rights));
    (state ^ (state >> 32)) as u32
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

    fn node(surface: u64, z: i32) -> SceneNode {
        SceneNode {
            surface: SurfaceId(surface),
            owner: 7,
            rectangle: Rectangle {
                x: 10,
                y: 20,
                width: 100,
                height: 80,
            },
            z,
            opacity_q16: u16::MAX,
            phase_q16: 0,
            role: SemanticRole::Application,
            flags: NODE_FLAG_VISIBLE | NODE_FLAG_INPUT,
            color_hint: [10, 20, 30, 255],
            semantic_tag: 9,
        }
    }

    #[test]
    fn atomic_commit_moves_and_resizes_one_node() {
        let mut scene = QuantumScene::<8>::new(0x1234).unwrap();
        let lease = scene.spawn(node(1, 0), SCENE_RIGHT_ALL, 100).unwrap();

        let mut transaction = SceneTransaction::<4>::new(scene.epoch());
        transaction
            .push(SceneMutation::Move {
                token: lease.token,
                x: 40,
                y: 50,
            })
            .unwrap();
        transaction
            .push(SceneMutation::Resize {
                token: lease.token,
                width: 200,
                height: 160,
            })
            .unwrap();

        let commit = scene.commit(transaction, &[lease]).unwrap();
        assert_eq!(commit.mutation_count, 2);

        let updated = scene.node(lease).unwrap();
        assert_eq!(updated.rectangle.x, 40);
        assert_eq!(updated.rectangle.width, 200);
    }

    #[test]
    fn hit_test_selects_the_highest_node() {
        let mut scene = QuantumScene::<8>::new(0x5678).unwrap();
        let _low = scene.spawn(node(1, 1), SCENE_RIGHT_ALL, 100).unwrap();
        let high = scene.spawn(node(2, 9), SCENE_RIGHT_ALL, 100).unwrap();
        assert_eq!(scene.hit_test(20, 30).unwrap().token, high.token);
    }

    #[test]
    fn attenuated_lease_cannot_amplify_rights() {
        let mut scene = QuantumScene::<8>::new(0x9999).unwrap();
        let lease = scene.spawn(node(1, 0), SCENE_RIGHT_ALL, 100).unwrap();
        let observe = scene.attenuate(lease, SCENE_RIGHT_OBSERVE, 50).unwrap();
        assert_eq!(
            scene.attenuate(observe, SCENE_RIGHT_OBSERVE | SCENE_RIGHT_MOVE, 40,),
            Err(SceneError::RightsAmplification)
        );
    }
}
