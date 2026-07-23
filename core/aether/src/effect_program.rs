use core::convert::TryFrom;

pub const MAXIMUM_EFFECTS: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum EffectKind {
    AttachTask = 1,
    Entangle = 2,
    SetCollapseThreshold = 3,
    SetPriorityMass = 4,
    OfferKairos = 5,
    ThermalCharge = 6,
    ThermalCredit = 7,
    Rephase = 8,
}

impl TryFrom<u8> for EffectKind {
    type Error = EffectError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::AttachTask),
            2 => Ok(Self::Entangle),
            3 => Ok(Self::SetCollapseThreshold),
            4 => Ok(Self::SetPriorityMass),
            5 => Ok(Self::OfferKairos),
            6 => Ok(Self::ThermalCharge),
            7 => Ok(Self::ThermalCredit),
            8 => Ok(Self::Rephase),
            _ => Err(EffectError::UnknownEffect),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct EffectIntent {
    pub kind: u8,
    pub flags: u8,
    pub reserved: u16,

    /// Each bit refers to a prior effect that must complete first.
    pub dependencies: u32,

    pub arguments: [u64; 4],
}

impl EffectIntent {
    pub const EMPTY: Self = Self {
        kind: 0,
        flags: 0,
        reserved: 0,
        dependencies: 0,
        arguments: [0; 4],
    };

    pub const fn new(kind: EffectKind, dependencies: u32, arguments: [u64; 4]) -> Self {
        Self {
            kind: kind as u8,
            flags: 0,
            reserved: 0,
            dependencies,
            arguments,
        }
    }

    pub fn effect_kind(self) -> Result<EffectKind, EffectError> {
        EffectKind::try_from(self.kind)
    }
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct EffectProgram<const N: usize> {
    pub expected_generation: u32,
    pub length: u8,
    pub reserved: [u8; 3],

    pub expected_state_root: u64,
    pub heat_ceiling: u64,

    pub effects: [EffectIntent; N],
    pub checksum: u64,
}

impl<const N: usize> EffectProgram<N> {
    pub const fn new(
        expected_generation: u32,
        expected_state_root: u64,
        heat_ceiling: u64,
    ) -> Self {
        Self {
            expected_generation,
            length: 0,
            reserved: [0; 3],
            expected_state_root,
            heat_ceiling,
            effects: [EffectIntent::EMPTY; N],
            checksum: 0,
        }
    }

    pub fn push(&mut self, effect: EffectIntent) -> Result<(), EffectError> {
        if N > MAXIMUM_EFFECTS {
            return Err(EffectError::InvalidShape);
        }

        let index = usize::from(self.length);

        if index >= N {
            return Err(EffectError::Capacity);
        }

        self.effects[index] = effect;
        self.length = self.length.saturating_add(1);
        self.checksum = 0;

        Ok(())
    }

    pub fn seal(mut self) -> Self {
        self.checksum = self.compute_checksum();
        self
    }

    pub fn prepare(
        &self,
        observed_generation: u32,
        observed_state_root: u64,
        current_heat: u64,
    ) -> Result<PreparedEffects<N>, EffectError> {
        if N == 0 || N > MAXIMUM_EFFECTS {
            return Err(EffectError::InvalidShape);
        }

        let length = usize::from(self.length);

        if length == 0 || length > N {
            return Err(EffectError::InvalidLength);
        }

        if self.checksum != self.compute_checksum() {
            return Err(EffectError::BadChecksum);
        }

        if self.expected_generation != observed_generation {
            return Err(EffectError::GenerationConflict {
                expected: self.expected_generation,
                observed: observed_generation,
            });
        }

        if self.expected_state_root != observed_state_root {
            return Err(EffectError::StateRootConflict {
                expected: self.expected_state_root,
                observed: observed_state_root,
            });
        }

        let mut completed = 0_u32;
        let mut projected_heat = i128::from(current_heat);

        for index in 0..length {
            let effect = self.effects[index];

            let allowed_dependencies = if index == 0 { 0 } else { (1_u32 << index) - 1 };

            if effect.dependencies & !allowed_dependencies != 0 {
                return Err(EffectError::ForwardDependency(index));
            }

            if effect.dependencies & completed != effect.dependencies {
                return Err(EffectError::UnsatisfiedDependency(index));
            }

            let kind = effect.effect_kind()?;
            validate_effect(kind, effect.arguments)?;

            match kind {
                EffectKind::ThermalCharge => {
                    projected_heat = projected_heat.saturating_add(i128::from(effect.arguments[0]));
                }

                EffectKind::ThermalCredit => {
                    projected_heat = projected_heat
                        .saturating_sub(i128::from(effect.arguments[0]))
                        .max(0);
                }

                _ => {}
            }

            if projected_heat > i128::from(self.heat_ceiling) {
                return Err(EffectError::HeatCeiling {
                    projected: projected_heat.min(i128::from(u64::MAX)) as u64,
                    ceiling: self.heat_ceiling,
                });
            }

            completed |= 1_u32 << index;
        }

        Ok(PreparedEffects {
            program: *self,
            digest: self.compute_checksum(),
            projected_heat: projected_heat as u64,
        })
    }

    fn compute_checksum(&self) -> u64 {
        let mut digest = mix(0x4546_4645_4354_5331, u64::from(self.expected_generation));

        digest = mix(digest, u64::from(self.length));
        digest = mix(digest, self.expected_state_root);
        digest = mix(digest, self.heat_ceiling);

        let length = usize::from(self.length).min(N);

        for effect in &self.effects[..length] {
            digest = mix(digest, u64::from(effect.kind));
            digest = mix(digest, u64::from(effect.flags));
            digest = mix(digest, u64::from(effect.dependencies));

            for argument in effect.arguments {
                digest = mix(digest, argument);
            }
        }

        digest
    }
}

#[derive(Clone, Copy)]
pub struct PreparedEffects<const N: usize> {
    program: EffectProgram<N>,
    digest: u64,
    projected_heat: u64,
}

impl<const N: usize> PreparedEffects<N> {
    pub fn effects(&self) -> &[EffectIntent] {
        &self.program.effects[..usize::from(self.program.length)]
    }

    pub const fn digest(&self) -> u64 {
        self.digest
    }

    pub const fn projected_heat(&self) -> u64 {
        self.projected_heat
    }

    pub const fn expected_generation(&self) -> u32 {
        self.program.expected_generation
    }

    pub const fn expected_state_root(&self) -> u64 {
        self.program.expected_state_root
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectError {
    InvalidShape,
    InvalidLength,
    Capacity,
    BadChecksum,
    UnknownEffect,

    ForwardDependency(usize),
    UnsatisfiedDependency(usize),

    InvalidTask,
    InvalidArgument,

    GenerationConflict { expected: u32, observed: u32 },

    StateRootConflict { expected: u64, observed: u64 },

    HeatCeiling { projected: u64, ceiling: u64 },
}

fn validate_effect(kind: EffectKind, arguments: [u64; 4]) -> Result<(), EffectError> {
    match kind {
        EffectKind::AttachTask => {
            validate_task(arguments[0])?;
        }

        EffectKind::Entangle => {
            validate_task(arguments[0])?;
            validate_task(arguments[1])?;

            if arguments[0] == arguments[1] {
                return Err(EffectError::InvalidTask);
            }

            let phase = arguments[2] as u16;

            if phase >= 1024 {
                return Err(EffectError::InvalidArgument);
            }

            let amplitude = arguments[3];

            if amplitude == 0 {
                return Err(EffectError::InvalidArgument);
            }
        }

        EffectKind::SetCollapseThreshold => {
            if !(1..=(1_u64 << 48)).contains(&arguments[0]) {
                return Err(EffectError::InvalidArgument);
            }
        }

        EffectKind::SetPriorityMass => {
            if arguments[0] > u16::MAX as u64 {
                return Err(EffectError::InvalidArgument);
            }
        }

        EffectKind::OfferKairos => {}

        EffectKind::ThermalCharge | EffectKind::ThermalCredit => {
            if arguments[0] == 0 {
                return Err(EffectError::InvalidArgument);
            }
        }

        EffectKind::Rephase => {
            if arguments[0] >= 1024 {
                return Err(EffectError::InvalidArgument);
            }
        }
    }

    Ok(())
}

fn validate_task(raw: u64) -> Result<(), EffectError> {
    let slot = raw as u16;
    let generation = (raw >> 16) as u16;

    if slot == u16::MAX || generation == 0 {
        Err(EffectError::InvalidTask)
    } else {
        Ok(())
    }
}

fn mix(mut state: u64, value: u64) -> u64 {
    state ^= value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    state = state.rotate_left(27);
    state = state.wrapping_mul(0x94d0_49bb_1331_11eb);
    state ^ (state >> 31)
}
