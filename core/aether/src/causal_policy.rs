use crate::resonance_policy::{POLICY_REPHASE, PolicyError, ResonancePolicy};

pub const MAXIMUM_POLICY_OPERATIONS: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum PolicyMutationKind {
    SetCollapseThreshold = 0,
    SetHeatCeiling = 1,
    SetQuarantineTicks = 2,
    SetPriorityMass = 3,
    SetTargetPhase = 4,
    SetMaximumPairs = 5,
    SetFlags = 6,
    AddFlags = 7,
    RemoveFlags = 8,
}

impl PolicyMutationKind {
    fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::SetCollapseThreshold),
            1 => Some(Self::SetHeatCeiling),
            2 => Some(Self::SetQuarantineTicks),
            3 => Some(Self::SetPriorityMass),
            4 => Some(Self::SetTargetPhase),
            5 => Some(Self::SetMaximumPairs),
            6 => Some(Self::SetFlags),
            7 => Some(Self::AddFlags),
            8 => Some(Self::RemoveFlags),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct CausalPolicyOperation {
    pub kind: u8,
    pub reserved: u8,
    pub dependencies: u16,
    pub reserved_two: u32,
    pub value: u64,
}

impl CausalPolicyOperation {
    pub const EMPTY: Self = Self {
        kind: 0,
        reserved: 0,
        dependencies: 0,
        reserved_two: 0,
        value: 0,
    };

    pub const fn new(kind: PolicyMutationKind, dependencies: u16, value: u64) -> Self {
        Self {
            kind: kind as u8,
            reserved: 0,
            dependencies,
            reserved_two: 0,
            value,
        }
    }
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct CausalPolicyBatch {
    pub expected_epoch: u64,
    pub operations: [CausalPolicyOperation; MAXIMUM_POLICY_OPERATIONS],
    pub length: u8,
    pub reserved: [u8; 7],
    pub checksum: u64,
}

impl CausalPolicyBatch {
    pub const fn empty(expected_epoch: u64) -> Self {
        Self {
            expected_epoch,
            operations: [CausalPolicyOperation::EMPTY; MAXIMUM_POLICY_OPERATIONS],
            length: 0,
            reserved: [0; 7],
            checksum: 0,
        }
    }

    pub fn seal(mut self) -> Self {
        self.checksum = self.compute_checksum();
        self
    }

    pub fn compile(
        &self,
        current_epoch: u64,
        base: ResonancePolicy,
    ) -> Result<PolicyTransition, CausalPolicyError> {
        if self.expected_epoch != current_epoch {
            return Err(CausalPolicyError::StaleEpoch {
                expected: self.expected_epoch,
                observed: current_epoch,
            });
        }

        if self.checksum != self.compute_checksum() {
            return Err(CausalPolicyError::BadChecksum);
        }

        let length = usize::from(self.length);

        if length == 0 || length > MAXIMUM_POLICY_OPERATIONS {
            return Err(CausalPolicyError::InvalidLength);
        }

        let mut policy = base;
        let mut completed = 0_u16;

        for index in 0..length {
            let operation = self.operations[index];

            let allowed_dependencies = if index == 0 { 0 } else { (1_u16 << index) - 1 };

            if operation.dependencies & !allowed_dependencies != 0 {
                return Err(CausalPolicyError::ForwardDependency(index));
            }

            if operation.dependencies & completed != operation.dependencies {
                return Err(CausalPolicyError::UnsatisfiedDependency(index));
            }

            let kind = PolicyMutationKind::from_raw(operation.kind)
                .ok_or(CausalPolicyError::UnknownMutation(index))?;

            apply_operation(&mut policy, kind, operation.value)
                .map_err(CausalPolicyError::Policy)?;

            completed |= 1_u16 << index;
        }

        let policy = policy.validate().map_err(CausalPolicyError::Policy)?;

        Ok(PolicyTransition {
            expected_epoch: current_epoch,
            next_epoch: current_epoch.wrapping_add(1).max(1),
            policy,
            digest: transition_digest(current_epoch, completed, policy),
        })
    }

    fn compute_checksum(&self) -> u64 {
        let mut digest = mix(0x243f_6a88_85a3_08d3, self.expected_epoch);

        digest = mix(digest, u64::from(self.length));

        for operation in &self.operations[..usize::from(self.length).min(MAXIMUM_POLICY_OPERATIONS)]
        {
            digest = mix(digest, u64::from(operation.kind));
            digest = mix(digest, u64::from(operation.dependencies));
            digest = mix(digest, operation.value);
        }

        digest
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PolicyTransition {
    pub expected_epoch: u64,
    pub next_epoch: u64,
    pub policy: ResonancePolicy,
    pub digest: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CausalPolicyError {
    InvalidLength,
    BadChecksum,
    StaleEpoch { expected: u64, observed: u64 },
    ForwardDependency(usize),
    UnsatisfiedDependency(usize),
    UnknownMutation(usize),
    ValueOutOfRange,
    Policy(PolicyError),
}

fn apply_operation(
    policy: &mut ResonancePolicy,
    kind: PolicyMutationKind,
    value: u64,
) -> Result<(), PolicyError> {
    match kind {
        PolicyMutationKind::SetCollapseThreshold => {
            policy.collapse_threshold = value;
        }

        PolicyMutationKind::SetHeatCeiling => {
            policy.heat_ceiling = value;
        }

        PolicyMutationKind::SetQuarantineTicks => {
            policy.quarantine_ticks = value;
        }

        PolicyMutationKind::SetPriorityMass => {
            policy.priority_mass = u16::try_from(value).map_err(|_| PolicyError::TargetPhase)?;
        }

        PolicyMutationKind::SetTargetPhase => {
            policy.target_phase = u16::try_from(value).map_err(|_| PolicyError::TargetPhase)?;

            policy.flags |= POLICY_REPHASE;
        }

        PolicyMutationKind::SetMaximumPairs => {
            policy.maximum_pairs = u16::try_from(value).map_err(|_| PolicyError::MaximumPairs)?;
        }

        PolicyMutationKind::SetFlags => {
            policy.flags = u32::try_from(value).map_err(|_| PolicyError::Flags)?;
        }

        PolicyMutationKind::AddFlags => {
            policy.flags |= u32::try_from(value).map_err(|_| PolicyError::Flags)?;
        }

        PolicyMutationKind::RemoveFlags => {
            policy.flags &= !u32::try_from(value).map_err(|_| PolicyError::Flags)?;
        }
    }

    Ok(())
}

fn transition_digest(epoch: u64, completed: u16, policy: ResonancePolicy) -> u64 {
    let mut digest = mix(epoch, u64::from(completed));
    digest = mix(digest, policy.collapse_threshold);
    digest = mix(digest, policy.heat_ceiling);
    digest = mix(digest, policy.quarantine_ticks);

    digest = mix(
        digest,
        u64::from(policy.priority_mass)
            | (u64::from(policy.target_phase) << 16)
            | (u64::from(policy.maximum_pairs) << 32),
    );

    mix(digest, u64::from(policy.flags))
}

fn mix(mut state: u64, value: u64) -> u64 {
    state ^= value.wrapping_add(0x517c_c1b7_2722_0a95);
    state = state.rotate_left(31);
    state = state.wrapping_mul(0x9e37_79b1_85eb_ca87);
    state ^ (state >> 28)
}
