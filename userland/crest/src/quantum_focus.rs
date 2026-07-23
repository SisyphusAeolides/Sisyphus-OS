use crate::compositor::SurfaceId;
use crate::input::{InputEvent, IntegratedEvent};
use crate::quantum_scene::{
    NODE_FLAG_FROZEN, NODE_FLAG_INPUT, NODE_FLAG_QUARANTINED, NODE_FLAG_SECURE, QuantumScene,
    SCENE_RIGHT_FOCUS, SCENE_RIGHT_OBSERVE, SceneError, SceneLease, SceneToken,
};

pub const FOCUS_RIGHT_KEYBOARD: u32 = 1 << 0;
pub const FOCUS_RIGHT_POINTER: u32 = 1 << 1;
pub const FOCUS_RIGHT_TOUCH: u32 = 1 << 2;
pub const FOCUS_RIGHT_STYLUS: u32 = 1 << 3;
pub const FOCUS_RIGHT_MODAL: u32 = 1 << 4;
pub const FOCUS_RIGHT_CAPTURE: u32 = 1 << 5;
pub const FOCUS_RIGHT_ALL: u32 = FOCUS_RIGHT_KEYBOARD
    | FOCUS_RIGHT_POINTER
    | FOCUS_RIGHT_TOUCH
    | FOCUS_RIGHT_STYLUS
    | FOCUS_RIGHT_MODAL
    | FOCUS_RIGHT_CAPTURE;

const ROOT_PARENT: u16 = u16::MAX;
const MAXIMUM_MODAL_DEPTH: u8 = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct FocusToken(u64);

impl FocusToken {
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
pub enum FocusClass {
    Keyboard,
    Pointer,
    Touch,
    Stylus,
    System,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FocusGrant {
    pub token: FocusToken,
    pub surface: SurfaceId,
    pub rights: u32,
    pub generation: u32,
    pub expires_tick: u64,
    pub depth: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputDispatch {
    pub surface: SurfaceId,
    pub focus: FocusToken,
    pub class: FocusClass,
    pub observed: IntegratedEvent,
    pub predicted: Option<InputEvent>,
    pub prediction_advisory: bool,
    pub dispatch_sequence: u64,
    pub root: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FocusError {
    ZeroCapacity,
    InvalidSecret,
    Capacity,
    InvalidToken,
    ForgedToken,
    StaleToken,
    Expired,
    MissingRight,
    RightsAmplification,
    Scene(SceneError),
    SceneIneligible,
    ModalDepth,
    InvalidParent,
    NoFocus,
    TimeRegression,
}

impl From<SceneError> for FocusError {
    fn from(error: SceneError) -> Self {
        Self::Scene(error)
    }
}

#[derive(Clone, Copy)]
struct FocusRecord {
    occupied: bool,
    generation: u16,
    rights: u32,
    expires_tick: u64,
    scene_token: SceneToken,
    surface: SurfaceId,
    parent_slot: u16,
    parent_generation: u16,
    depth: u8,
    secure: bool,
    tag: u32,
    activation_sequence: u64,
}

impl FocusRecord {
    const EMPTY: Self = Self {
        occupied: false,
        generation: 1,
        rights: 0,
        expires_tick: 0,
        scene_token: SceneToken::INVALID,
        surface: SurfaceId(0),
        parent_slot: ROOT_PARENT,
        parent_generation: 0,
        depth: 0,
        secure: false,
        tag: 0,
        activation_sequence: 0,
    };
}

pub struct QuantumFocusLattice<const N: usize> {
    secret: u64,
    records: [FocusRecord; N],
    keyboard: FocusToken,
    pointer: FocusToken,
    touch: FocusToken,
    stylus: FocusToken,
    activation_sequence: u64,
    dispatch_sequence: u64,
    last_tick: u64,
}

impl<const N: usize> QuantumFocusLattice<N> {
    pub fn new(secret: u64) -> Result<Self, FocusError> {
        if N == 0 {
            return Err(FocusError::ZeroCapacity);
        }
        if secret == 0 {
            return Err(FocusError::InvalidSecret);
        }

        Ok(Self {
            secret,
            records: [FocusRecord::EMPTY; N],
            keyboard: FocusToken::INVALID,
            pointer: FocusToken::INVALID,
            touch: FocusToken::INVALID,
            stylus: FocusToken::INVALID,
            activation_sequence: 1,
            dispatch_sequence: 1,
            last_tick: 0,
        })
    }

    pub fn grant<const S: usize>(
        &mut self,
        scene: &QuantumScene<S>,
        lease: SceneLease,
        rights: u32,
        expires_tick: u64,
        now_tick: u64,
    ) -> Result<FocusGrant, FocusError> {
        if rights == 0 || expires_tick <= now_tick {
            return Err(FocusError::Expired);
        }
        if lease.rights & (SCENE_RIGHT_OBSERVE | SCENE_RIGHT_FOCUS)
            != (SCENE_RIGHT_OBSERVE | SCENE_RIGHT_FOCUS)
        {
            return Err(FocusError::MissingRight);
        }

        let node = scene.node(lease)?;
        if node.flags & NODE_FLAG_INPUT == 0
            || node.flags & (NODE_FLAG_QUARANTINED | NODE_FLAG_FROZEN) != 0
        {
            return Err(FocusError::SceneIneligible);
        }

        let index = self
            .records
            .iter()
            .position(|record| !record.occupied)
            .ok_or(FocusError::Capacity)?;
        let generation = self.records[index].generation.wrapping_add(1).max(1);
        let tag = focus_tag(
            self.secret,
            index,
            generation,
            node.surface,
            lease.token,
            rights,
        );
        let activation = self.allocate_activation();

        self.records[index] = FocusRecord {
            occupied: true,
            generation,
            rights,
            expires_tick,
            scene_token: lease.token,
            surface: node.surface,
            parent_slot: ROOT_PARENT,
            parent_generation: 0,
            depth: 0,
            secure: node.flags & NODE_FLAG_SECURE != 0,
            tag,
            activation_sequence: activation,
        };

        Ok(FocusGrant {
            token: FocusToken::new(index as u16, generation, tag),
            surface: node.surface,
            rights,
            generation: u32::from(generation),
            expires_tick,
            depth: 0,
        })
    }

    pub fn modal_child<const S: usize>(
        &mut self,
        scene: &QuantumScene<S>,
        parent: FocusGrant,
        child_lease: SceneLease,
        rights: u32,
        expires_tick: u64,
        now_tick: u64,
    ) -> Result<FocusGrant, FocusError> {
        let parent_index = self.validate_grant(parent, FOCUS_RIGHT_MODAL, now_tick)?;
        let parent_record = self.records[parent_index];

        if parent_record.depth >= MAXIMUM_MODAL_DEPTH {
            return Err(FocusError::ModalDepth);
        }
        if rights & !parent_record.rights != 0 {
            return Err(FocusError::RightsAmplification);
        }

        let mut child = self.grant(
            scene,
            child_lease,
            rights,
            expires_tick.min(parent_record.expires_tick),
            now_tick,
        )?;
        let child_index = child.token.slot();
        self.records[child_index].parent_slot = parent_index as u16;
        self.records[child_index].parent_generation = parent_record.generation;
        self.records[child_index].depth = parent_record.depth + 1;
        child.depth = parent_record.depth + 1;
        Ok(child)
    }

    pub fn activate(
        &mut self,
        grant: FocusGrant,
        class: FocusClass,
        now_tick: u64,
    ) -> Result<u64, FocusError> {
        let required = class_right(class);
        let index = self.validate_grant(grant, required, now_tick)?;
        validate_ancestry(&self.records, index)?;

        let activation = self.allocate_activation();
        self.records[index].activation_sequence = activation;

        match class {
            FocusClass::Keyboard => self.keyboard = grant.token,
            FocusClass::Pointer => self.pointer = grant.token,
            FocusClass::Touch => self.touch = grant.token,
            FocusClass::Stylus => self.stylus = grant.token,
            FocusClass::System => {
                self.keyboard = grant.token;
                self.pointer = grant.token;
                self.touch = grant.token;
                self.stylus = grant.token;
            }
        }

        self.last_tick = now_tick;
        Ok(self.root())
    }

    pub fn route<const S: usize>(
        &mut self,
        scene: &QuantumScene<S>,
        event: IntegratedEvent,
        predicted: Option<InputEvent>,
        now_tick: u64,
    ) -> Result<InputDispatch, FocusError> {
        if self.last_tick != 0 && now_tick < self.last_tick {
            return Err(FocusError::TimeRegression);
        }

        let class = classify(event.raw);
        let mut token = self.token_for(class);

        if matches!(
            event.raw,
            InputEvent::PointerAbs { .. }
                | InputEvent::PointerButton { .. }
                | InputEvent::PointerDown { .. }
                | InputEvent::PointerMove { .. }
                | InputEvent::PointerUp { .. }
                | InputEvent::TouchDown { .. }
                | InputEvent::TouchMove { .. }
                | InputEvent::TouchUp { .. }
                | InputEvent::StylusDown { .. }
                | InputEvent::StylusMove { .. }
                | InputEvent::StylusUp { .. }
        ) {
            if let Some(hit) = scene.hit_test(event.pointer_x, event.pointer_y) {
                if let Some(index) = self.records.iter().position(|record| {
                    record.occupied
                        && record.scene_token == hit.token
                        && record.expires_tick > now_tick
                }) {
                    let record = self.records[index];
                    let candidate = FocusToken::new(index as u16, record.generation, record.tag);
                    if record.rights & class_right(class) != 0 {
                        token = candidate;
                    }
                }
            }
        }

        let index = self.validate_token(token, class_right(class), now_tick)?;
        validate_ancestry(&self.records, index)?;
        let record = self.records[index];

        let dispatch_sequence = self.dispatch_sequence;
        self.dispatch_sequence = self.dispatch_sequence.wrapping_add(1).max(1);
        self.last_tick = now_tick;

        let mut dispatch = InputDispatch {
            surface: record.surface,
            focus: token,
            class,
            observed: event,
            predicted,
            prediction_advisory: predicted.is_some(),
            dispatch_sequence,
            root: 0,
        };
        dispatch.root = dispatch_root(self.secret, &dispatch);
        Ok(dispatch)
    }

    pub fn revoke(&mut self, grant: FocusGrant) -> Result<(), FocusError> {
        let index = self.validate_grant(grant, 0, 0)?;
        let token = grant.token;
        let generation = self.records[index].generation;

        for record in &mut self.records {
            if record.occupied
                && record.parent_slot == index as u16
                && record.parent_generation == generation
            {
                let child_generation = record.generation;
                *record = FocusRecord::EMPTY;
                record.generation = child_generation;
            }
        }

        self.records[index] = FocusRecord::EMPTY;
        self.records[index].generation = generation;
        clear_if_matches(&mut self.keyboard, token);
        clear_if_matches(&mut self.pointer, token);
        clear_if_matches(&mut self.touch, token);
        clear_if_matches(&mut self.stylus, token);
        Ok(())
    }

    pub fn expire(&mut self, now_tick: u64) -> usize {
        let mut expired = 0_usize;
        for index in 0..N {
            if self.records[index].occupied && self.records[index].expires_tick <= now_tick {
                let token = FocusToken::new(
                    index as u16,
                    self.records[index].generation,
                    self.records[index].tag,
                );
                let generation = self.records[index].generation;
                self.records[index] = FocusRecord::EMPTY;
                self.records[index].generation = generation;
                clear_if_matches(&mut self.keyboard, token);
                clear_if_matches(&mut self.pointer, token);
                clear_if_matches(&mut self.touch, token);
                clear_if_matches(&mut self.stylus, token);
                expired += 1;
            }
        }
        self.last_tick = self.last_tick.max(now_tick);
        expired
    }

    pub fn root(&self) -> u64 {
        let mut state = mix(self.secret, self.activation_sequence);
        state = mix(state, self.dispatch_sequence);
        state = mix(state, self.keyboard.raw());
        state = mix(state, self.pointer.raw());
        state = mix(state, self.touch.raw());
        state = mix(state, self.stylus.raw());

        for (index, record) in self.records.iter().enumerate() {
            if !record.occupied {
                continue;
            }
            state = mix(state, index as u64);
            state = mix(state, u64::from(record.generation));
            state = mix(state, u64::from(record.rights));
            state = mix(state, record.expires_tick);
            state = mix(state, record.scene_token.raw());
            state = mix(state, record.surface.0);
            state = mix(state, u64::from(record.parent_slot));
            state = mix(state, u64::from(record.parent_generation));
            state = mix(state, u64::from(record.depth));
            state = mix(state, u64::from(record.secure));
            state = mix(state, record.activation_sequence);
        }

        state
    }

    fn token_for(&self, class: FocusClass) -> FocusToken {
        match class {
            FocusClass::Keyboard => self.keyboard,
            FocusClass::Pointer => self.pointer,
            FocusClass::Touch => self.touch,
            FocusClass::Stylus => self.stylus,
            FocusClass::System => self.keyboard,
        }
    }

    fn validate_grant(
        &self,
        grant: FocusGrant,
        required: u32,
        now_tick: u64,
    ) -> Result<usize, FocusError> {
        if grant.token == FocusToken::INVALID {
            return Err(FocusError::InvalidToken);
        }

        let index = self.validate_token(grant.token, required, now_tick)?;
        let record = self.records[index];

        if grant.surface != record.surface
            || grant.rights & !record.rights != 0
            || grant.expires_tick > record.expires_tick
            || grant.generation != u32::from(record.generation)
        {
            return Err(FocusError::StaleToken);
        }

        Ok(index)
    }

    fn validate_token(
        &self,
        token: FocusToken,
        required: u32,
        now_tick: u64,
    ) -> Result<usize, FocusError> {
        if token == FocusToken::INVALID {
            return Err(FocusError::NoFocus);
        }

        let index = token.slot();
        let record = self.records.get(index).ok_or(FocusError::InvalidToken)?;

        if !record.occupied || record.generation != token.generation() {
            return Err(FocusError::StaleToken);
        }
        let expected = focus_tag(
            self.secret,
            index,
            record.generation,
            record.surface,
            record.scene_token,
            record.rights,
        );
        if expected != token.tag() || expected != record.tag {
            return Err(FocusError::ForgedToken);
        }
        if now_tick != 0 && record.expires_tick <= now_tick {
            return Err(FocusError::Expired);
        }
        if record.rights & required != required {
            return Err(FocusError::MissingRight);
        }

        Ok(index)
    }

    fn allocate_activation(&mut self) -> u64 {
        let current = self.activation_sequence;
        self.activation_sequence = self.activation_sequence.wrapping_add(1).max(1);
        current
    }
}

fn validate_ancestry<const N: usize>(
    records: &[FocusRecord; N],
    mut index: usize,
) -> Result<(), FocusError> {
    let mut depth = 0_u8;

    loop {
        let record = records[index];
        if record.parent_slot == ROOT_PARENT {
            return Ok(());
        }

        depth = depth.saturating_add(1);
        if depth > MAXIMUM_MODAL_DEPTH {
            return Err(FocusError::ModalDepth);
        }

        let parent_index = usize::from(record.parent_slot);
        let parent = records.get(parent_index).ok_or(FocusError::InvalidParent)?;

        if !parent.occupied || parent.generation != record.parent_generation {
            return Err(FocusError::InvalidParent);
        }

        index = parent_index;
    }
}

fn classify(event: InputEvent) -> FocusClass {
    match event {
        InputEvent::Key { .. } => FocusClass::Keyboard,
        InputEvent::PointerMotion { .. }
        | InputEvent::PointerButton { .. }
        | InputEvent::PointerAbs { .. }
        | InputEvent::Scroll { .. }
        | InputEvent::PointerDown { .. }
        | InputEvent::PointerMove { .. }
        | InputEvent::PointerUp { .. } => FocusClass::Pointer,
        InputEvent::TouchDown { .. }
        | InputEvent::TouchMove { .. }
        | InputEvent::TouchUp { .. } => FocusClass::Touch,
        InputEvent::StylusDown { .. }
        | InputEvent::StylusMove { .. }
        | InputEvent::StylusUp { .. } => FocusClass::Stylus,
    }
}

fn class_right(class: FocusClass) -> u32 {
    match class {
        FocusClass::Keyboard => FOCUS_RIGHT_KEYBOARD,
        FocusClass::Pointer => FOCUS_RIGHT_POINTER,
        FocusClass::Touch => FOCUS_RIGHT_TOUCH,
        FocusClass::Stylus => FOCUS_RIGHT_STYLUS,
        FocusClass::System => FOCUS_RIGHT_ALL,
    }
}

fn clear_if_matches(slot: &mut FocusToken, token: FocusToken) {
    if *slot == token {
        *slot = FocusToken::INVALID;
    }
}

fn focus_tag(
    secret: u64,
    index: usize,
    generation: u16,
    surface: SurfaceId,
    scene_token: SceneToken,
    rights: u32,
) -> u32 {
    let mut state = mix(secret, index as u64);
    state = mix(state, u64::from(generation));
    state = mix(state, surface.0);
    state = mix(state, scene_token.raw());
    state = mix(state, u64::from(rights));
    (state ^ (state >> 32)) as u32
}

fn dispatch_root(secret: u64, dispatch: &InputDispatch) -> u64 {
    let mut state = mix(secret, dispatch.surface.0);
    state = mix(state, dispatch.focus.raw());
    state = mix(state, dispatch.class as u8 as u64);
    state = mix(state, dispatch.dispatch_sequence);
    state = mix(state, u64::from(dispatch.prediction_advisory));
    state = mix(state, input_digest(dispatch.observed.raw));
    state = mix(state, dispatch.predicted.map(input_digest).unwrap_or(0));
    state
}

fn input_digest(event: InputEvent) -> u64 {
    match event {
        InputEvent::Key { code, pressed } => mix(1, u64::from(code) | (u64::from(pressed) << 32)),
        InputEvent::PointerMotion { delta_x, delta_y } => mix(2, pair_i32(delta_x, delta_y)),
        InputEvent::PointerButton { button, pressed } => {
            mix(3, u64::from(button) | (u64::from(pressed) << 32))
        }
        InputEvent::PointerAbs { x, y, buttons } => mix(mix(4, pair_i32(x, y)), u64::from(buttons)),
        InputEvent::Scroll {
            axis,
            value,
            discrete,
        } => mix(
            mix(5, u64::from(axis)),
            value as u32 as u64 | (u64::from(discrete) << 32),
        ),
        InputEvent::TouchDown { id, x, y } => mix(mix(6, u64::from(id)), pair_i32(x, y)),
        InputEvent::TouchMove { id, x, y } => mix(mix(7, u64::from(id)), pair_i32(x, y)),
        InputEvent::TouchUp { id, x, y } => mix(mix(8, u64::from(id)), pair_i32(x, y)),
        InputEvent::StylusDown { x, y, pressure } => {
            mix(mix(9, pair_i32(x, y)), u64::from(pressure))
        }
        InputEvent::StylusMove {
            x,
            y,
            pressure,
            tilt_x,
            tilt_y,
        } => {
            let mut state = mix(10, pair_i32(x, y));
            state = mix(state, u64::from(pressure));
            mix(state, pair_i16(tilt_x, tilt_y))
        }
        InputEvent::StylusUp { x, y } => mix(11, pair_i32(x, y)),
        InputEvent::PointerDown { id, x, y } => mix(mix(12, u64::from(id)), pair_i32(x, y)),
        InputEvent::PointerMove { id, x, y } => mix(mix(13, u64::from(id)), pair_i32(x, y)),
        InputEvent::PointerUp { id, x, y } => mix(mix(14, u64::from(id)), pair_i32(x, y)),
    }
}

fn pair_i32(first: i32, second: i32) -> u64 {
    u64::from(first as u32) | (u64::from(second as u32) << 32)
}

fn pair_i16(first: i16, second: i16) -> u64 {
    u64::from(first as u16) | (u64::from(second as u16) << 16)
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
    use crate::compositor::Rectangle;
    use crate::quantum_scene::{
        NODE_FLAG_INPUT, NODE_FLAG_VISIBLE, QuantumScene, SCENE_RIGHT_ALL, SceneNode, SemanticRole,
    };

    fn node(surface: u64, z: i32) -> SceneNode {
        SceneNode {
            surface: SurfaceId(surface),
            owner: 1,
            rectangle: Rectangle {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
            z,
            opacity_q16: u16::MAX,
            phase_q16: 0,
            role: SemanticRole::Application,
            flags: NODE_FLAG_VISIBLE | NODE_FLAG_INPUT,
            color_hint: [0; 4],
            semantic_tag: surface,
        }
    }

    #[test]
    fn observed_input_is_routed_while_prediction_stays_advisory() {
        let mut scene = QuantumScene::<4>::new(0x1111).unwrap();
        let lease = scene.spawn(node(1, 0), SCENE_RIGHT_ALL, 100).unwrap();
        let mut focus = QuantumFocusLattice::<4>::new(0x2222).unwrap();
        let grant = focus.grant(&scene, lease, FOCUS_RIGHT_ALL, 100, 1).unwrap();
        focus.activate(grant, FocusClass::System, 1).unwrap();

        let observed = IntegratedEvent {
            raw: InputEvent::PointerAbs {
                x: 20,
                y: 30,
                buttons: 0,
            },
            modifiers: crate::input::ModifierState::empty(),
            pointer_x: 20,
            pointer_y: 30,
        };
        let predicted = Some(InputEvent::PointerAbs {
            x: 24,
            y: 32,
            buttons: 0,
        });

        let dispatch = focus.route(&scene, observed, predicted, 2).unwrap();
        assert_eq!(dispatch.surface, SurfaceId(1));
        assert_eq!(dispatch.observed, observed);
        assert_eq!(dispatch.predicted, predicted);
        assert!(dispatch.prediction_advisory);
    }
}
