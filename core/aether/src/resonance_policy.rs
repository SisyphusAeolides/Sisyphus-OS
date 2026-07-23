pub const POLICY_REPHASE: u32 = 1 << 0;
pub const POLICY_QUARANTINE: u32 = 1 << 1;
pub const POLICY_THERMAL_CLAMP: u32 = 1 << 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct ResonancePolicy {
    pub collapse_threshold: u64,
    pub heat_ceiling: u64,
    pub quarantine_ticks: u64,

    pub priority_mass: u16,
    pub target_phase: u16,

    pub maximum_pairs: u16,
    pub reserved: u16,

    pub flags: u32,
}

impl ResonancePolicy {
    pub const DEFAULT: Self = Self {
        collapse_threshold: 64,
        heat_ceiling: 850_000,
        quarantine_ticks: 4096,
        priority_mass: 0x8000,
        target_phase: 0,
        maximum_pairs: 256,
        reserved: 0,
        flags: 0,
    };

    pub fn validate(self) -> Result<Self, PolicyError> {
        if !(1..=(1_u64 << 48)).contains(&self.collapse_threshold) {
            return Err(PolicyError::CollapseThreshold);
        }

        if self.heat_ceiling == 0 || self.heat_ceiling > 100_000_000 {
            return Err(PolicyError::HeatCeiling);
        }

        if self.quarantine_ticks > 10_000_000 {
            return Err(PolicyError::QuarantineDuration);
        }

        if self.target_phase >= 1024 {
            return Err(PolicyError::TargetPhase);
        }

        if self.maximum_pairs == 0 || self.maximum_pairs > 4096 {
            return Err(PolicyError::MaximumPairs);
        }

        if self.flags & !(POLICY_REPHASE | POLICY_QUARANTINE | POLICY_THERMAL_CLAMP) != 0 {
            return Err(PolicyError::Flags);
        }

        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PolicyError {
    CollapseThreshold,
    HeatCeiling,
    QuarantineDuration,
    TargetPhase,
    MaximumPairs,
    Flags,
}
