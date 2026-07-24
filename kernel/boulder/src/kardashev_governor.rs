// kernel/boulder/src/kardashev_governor.rs
//! KARDASHEV GOVERNOR — Civilization energy ladder for Sisyphus
//!
//! Type 0: bare survival — throttle everything non-essential
//! Type I: planetary — full-machine optimum (all sockets, iGPU+dGPU)
//! Type II: stellar — treat accelerator mesh as a Dyson instrument
//!
//! Inputs (read-only observations):
//!   - thermogenesis::ThermalPage (zone, throttle_hint, budgets)
//!   - drivernet resolved GPU strategies
//!   - chronovore crystal activity (quiet windows = harvestable slack)
//!
//! Outputs (policy actuators):
//!   - Noether ceiling adjustments (DMA/IRQ/thermal charges)
//!   - scheduler priority mass bias
//!   - drivernet strategy demotion under Type 0
//!   - ghost chronicle civilization events


/// 16.16 fixed point
pub type Fp = u32;
pub const FP_ONE: Fp = 0x1_0000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u8)]
pub enum KardashevType {
    /// Brownout / thermal necrosis avoidance
    Type0Survival = 0,
    /// Single-machine optimum
    Type1Planetary = 1,
    /// Multi-device stellar mesh (GPU+CPU+NPU as one engine)
    Type2Stellar = 2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CivilizationSensors {
    /// 0..=3 style zone from ThermalPage.temperature_zone
    pub thermal_zone: u8,
    pub throttle_hint: u8,
    /// cpu_used / cpu_budget in 16.16
    pub cpu_util_fp: Fp,
    /// Fraction of DMA Noether charge used
    pub dma_util_fp: Fp,
    /// GPU compute claimed (hermes armed / mesa / none)
    pub gpu_armed: bool,
    pub gpu_is_hermes: bool,
    /// Chronovore time crystal locked
    pub crystal_active: bool,
    /// Live driver count
    pub live_drivers: u16,
    /// Phononic IRQ temperature max across cores
    pub irq_temp_fp: Fp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CivilizationActuators {
    pub level: KardashevType,
    /// Scale Noether DMA ceiling (16.16 multiplier)
    pub dma_ceiling_scale_fp: Fp,
    pub irq_ceiling_scale_fp: Fp,
    pub thermal_ceiling_scale_fp: Fp,
    /// Scheduler priority mass bias for interactive vs batch
    pub interactive_bias_fp: Fp,
    pub batch_bias_fp: Fp,
    /// If true, drivernet should demote HermesNative → DrmKmsOnly
    pub demote_gpu: bool,
    /// If true, allow driver mitosis
    pub allow_mitosis: bool,
    /// If true, allow ER=EPR wormhole page pairs
    pub allow_entangled_memory: bool,
    /// Target TSC quiet-window harvest rate
    pub harvest_slack: bool,
}

impl CivilizationActuators {
    pub const fn type1_default() -> Self {
        Self {
            level: KardashevType::Type1Planetary,
            dma_ceiling_scale_fp: FP_ONE,
            irq_ceiling_scale_fp: FP_ONE,
            thermal_ceiling_scale_fp: FP_ONE,
            interactive_bias_fp: FP_ONE,
            batch_bias_fp: FP_ONE,
            demote_gpu: false,
            allow_mitosis: true,
            allow_entangled_memory: true,
            harvest_slack: true,
        }
    }
}

pub struct KardashevGovernor {
    level: KardashevType,
    /// Hysteresis counters to avoid flapping
    down_streak: u8,
    up_streak: u8,
    epoch: u64,
    last: CivilizationActuators,
}

impl KardashevGovernor {
    pub const fn new() -> Self {
        Self {
            level: KardashevType::Type1Planetary,
            down_streak: 0,
            up_streak: 0,
            epoch: 0,
            last: CivilizationActuators::type1_default(),
        }
    }

    pub fn level(&self) -> KardashevType {
        self.level
    }

    pub fn last_actuators(&self) -> CivilizationActuators {
        self.last
    }

    /// Observe sensors, possibly transition civilization type, emit actuators.
    pub fn tick(&mut self, s: CivilizationSensors) -> CivilizationActuators {
        self.epoch = self.epoch.wrapping_add(1);
        let desired = self.classify(&s);
        self.transition(desired);
        let act = self.synthesize(self.level, &s);
        self.last = act;
        act
    }

    fn classify(&self, s: &CivilizationSensors) -> KardashevType {
        // Hard survival trips
        if s.thermal_zone >= 3
            || s.throttle_hint >= 2
            || s.cpu_util_fp > FP_ONE.saturating_mul(95) / 100
        {
            return KardashevType::Type0Survival;
        }
        if s.irq_temp_fp > FP_ONE.saturating_mul(8) {
            return KardashevType::Type0Survival;
        }
        // Stellar: crystal lock + hermes GPU + healthy thermal + headroom
        if s.crystal_active
            && s.gpu_is_hermes
            && s.gpu_armed
            && s.thermal_zone == 0
            && s.cpu_util_fp < FP_ONE.saturating_mul(70) / 100
            && s.dma_util_fp < FP_ONE.saturating_mul(70) / 100
        {
            return KardashevType::Type2Stellar;
        }
        KardashevType::Type1Planetary
    }

    fn transition(&mut self, desired: KardashevType) {
        // Hysteresis: need 3 consecutive ticks to move
        if desired < self.level {
            self.down_streak = self.down_streak.saturating_add(1);
            self.up_streak = 0;
            if self.down_streak >= 2 {
                self.level = desired;
                self.down_streak = 0;
            }
        } else if desired > self.level {
            self.up_streak = self.up_streak.saturating_add(1);
            self.down_streak = 0;
            if self.up_streak >= 3 {
                self.level = desired;
                self.up_streak = 0;
            }
        } else {
            self.up_streak = 0;
            self.down_streak = 0;
        }
    }

    fn synthesize(&self, level: KardashevType, s: &CivilizationSensors) -> CivilizationActuators {
        match level {
            KardashevType::Type0Survival => CivilizationActuators {
                level,
                dma_ceiling_scale_fp: FP_ONE / 4,
                irq_ceiling_scale_fp: FP_ONE / 2,
                thermal_ceiling_scale_fp: FP_ONE / 3,
                interactive_bias_fp: FP_ONE + FP_ONE / 2,
                batch_bias_fp: FP_ONE / 4,
                demote_gpu: true,
                allow_mitosis: false,
                allow_entangled_memory: false,
                harvest_slack: false,
            },
            KardashevType::Type1Planetary => {
                let mut a = CivilizationActuators::type1_default();
                // Slight batch boost if crystal says quiet windows exist
                if s.crystal_active {
                    a.batch_bias_fp = FP_ONE + FP_ONE / 8;
                    a.harvest_slack = true;
                }
                a
            }
            KardashevType::Type2Stellar => CivilizationActuators {
                level,
                dma_ceiling_scale_fp: FP_ONE + FP_ONE / 2,
                irq_ceiling_scale_fp: FP_ONE + FP_ONE / 4,
                thermal_ceiling_scale_fp: FP_ONE + FP_ONE / 4,
                interactive_bias_fp: FP_ONE,
                batch_bias_fp: FP_ONE + FP_ONE / 2,
                demote_gpu: false,
                allow_mitosis: true,
                allow_entangled_memory: true,
                harvest_slack: true,
            },
        }
    }
}

/// Map ThermalPage atomics → sensors fragment (call from governor tick site).
pub fn cpu_util_from_thermal(budget_ticks: u64, used_ticks: u64) -> Fp {
    if budget_ticks == 0 {
        return 0;
    }
    let u = used_ticks.min(budget_ticks);
    ((u as u64 * FP_ONE as u64) / budget_ticks) as Fp
}
