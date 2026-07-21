use core::sync::atomic::{AtomicU8, Ordering};

use sisyphus_driver_abi::{STATUS_INVALID_ARGUMENT, Status};

const DEFAULT_CHARGE_START: u8 = 75;
const DEFAULT_CHARGE_STOP: u8 = 90;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ThermalProfile {
    Performance = 0,
    Balanced = 1,
    Quiet = 2,
}

pub trait PlatformPowerBackend: Sync {
    fn set_battery_thresholds(&self, start_percent: u8, stop_percent: u8) -> Status;
    fn set_thermal_profile(&self, profile: ThermalProfile) -> Status;
}

pub struct AdvancedPowerController {
    charge_start_threshold: AtomicU8,
    charge_stop_threshold: AtomicU8,
    thermal_profile: AtomicU8,
}

impl AdvancedPowerController {
    pub const fn new() -> Self {
        Self {
            charge_start_threshold: AtomicU8::new(DEFAULT_CHARGE_START),
            charge_stop_threshold: AtomicU8::new(DEFAULT_CHARGE_STOP),
            thermal_profile: AtomicU8::new(ThermalProfile::Balanced as u8),
        }
    }

    pub fn battery_thresholds(&self) -> (u8, u8) {
        (
            self.charge_start_threshold.load(Ordering::Acquire),
            self.charge_stop_threshold.load(Ordering::Acquire),
        )
    }

    pub fn set_battery_thresholds(&self, start_percent: u8, stop_percent: u8) -> Status {
        if start_percent > 100 || stop_percent > 100 || start_percent >= stop_percent {
            return STATUS_INVALID_ARGUMENT;
        }
        self.charge_start_threshold
            .store(start_percent, Ordering::Release);
        self.charge_stop_threshold
            .store(stop_percent, Ordering::Release);
        sisyphus_driver_abi::STATUS_OK
    }

    pub fn thermal_profile(&self) -> ThermalProfile {
        match self.thermal_profile.load(Ordering::Acquire) {
            0 => ThermalProfile::Performance,
            2 => ThermalProfile::Quiet,
            _ => ThermalProfile::Balanced,
        }
    }

    pub fn set_thermal_profile(&self, profile: ThermalProfile) {
        self.thermal_profile.store(profile as u8, Ordering::Release);
    }

    pub fn apply(&self, backend: &dyn PlatformPowerBackend) -> Status {
        let (start, stop) = self.battery_thresholds();
        let status = backend.set_battery_thresholds(start, stop);
        if status != sisyphus_driver_abi::STATUS_OK {
            return status;
        }
        backend.set_thermal_profile(self.thermal_profile())
    }
}

impl Default for AdvancedPowerController {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    struct TestBackend {
        thresholds: AtomicU32,
        profile: AtomicU8,
    }

    impl PlatformPowerBackend for TestBackend {
        fn set_battery_thresholds(&self, start_percent: u8, stop_percent: u8) -> Status {
            self.thresholds.store(
                u32::from(start_percent) | (u32::from(stop_percent) << 8),
                Ordering::Release,
            );
            sisyphus_driver_abi::STATUS_OK
        }

        fn set_thermal_profile(&self, profile: ThermalProfile) -> Status {
            self.profile.store(profile as u8, Ordering::Release);
            sisyphus_driver_abi::STATUS_OK
        }
    }

    #[test]
    fn validates_policy_before_applying_a_backend() {
        let controller = AdvancedPowerController::new();
        assert_eq!(
            controller.set_battery_thresholds(90, 75),
            STATUS_INVALID_ARGUMENT
        );
        assert_eq!(controller.set_battery_thresholds(70, 85), 0);
        controller.set_thermal_profile(ThermalProfile::Quiet);

        let backend = TestBackend {
            thresholds: AtomicU32::new(0),
            profile: AtomicU8::new(0),
        };
        assert_eq!(controller.apply(&backend), 0);
        assert_eq!(backend.thresholds.load(Ordering::Acquire), 70 | (85 << 8));
        assert_eq!(backend.profile.load(Ordering::Acquire), 2);
    }
}
