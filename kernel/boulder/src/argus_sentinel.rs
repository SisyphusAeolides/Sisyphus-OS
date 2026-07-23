use crate::capability::{Capability, LearningControl};
use crate::sync::SpinLock;

pub const Q16_ONE: i64 = 1 << 16;
const SIGNAL_ALPHA_SHIFT: u32 = 3;
const TREND_ALPHA_SHIFT: u32 = 2;
const RATE_ALPHA_SHIFT: u32 = 3;
const MAXIMUM_NORMALIZED_Q16: u64 = 16 << 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TelemetrySample {
    pub tick: u64,
    pub resource: u64,
    pub signal_q16: i32,
    pub temperature_q16: i32,
    pub pressure_q16: i32,
    pub corrections: u32,
    pub faults: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArgusPolicy {
    pub warm_temperature_q16: i32,
    pub critical_temperature_q16: i32,
    pub signal_limit_q16: i32,
    pub pressure_limit_q16: i32,
    pub correction_watch_q16: u32,
    pub fault_watch_q16: u32,
    pub cusum_limit_q16: u32,
    pub watch_risk: u16,
    pub degraded_risk: u16,
    pub critical_risk: u16,
    pub terminal_risk: u16,
}

impl ArgusPolicy {
    pub const BLACK_LAB: Self = Self {
        warm_temperature_q16: 75 << 16,
        critical_temperature_q16: 95 << 16,
        signal_limit_q16: 8 << 16,
        pressure_limit_q16: 8 << 16,
        correction_watch_q16: 4 << 16,
        fault_watch_q16: 1 << 15,
        cusum_limit_q16: 12 << 16,
        watch_risk: 240,
        degraded_risk: 440,
        critical_risk: 680,
        terminal_risk: 880,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArgusSeverity {
    Stable,
    Watch,
    Degraded,
    Critical,
    Terminal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArgusAction {
    Observe,
    IncreaseSampling,
    Quarantine,
    RevokeDma,
    ResetDevice,
    RetireResource,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArgusAssessment {
    pub resource: u64,
    pub tick: u64,
    pub severity: ArgusSeverity,
    pub action: ArgusAction,
    pub risk: u16,
    pub anomaly_q16: u32,
    pub trend_q16: i32,
    pub thermal_margin_q16: i32,
    pub cusum_q16: u32,
    pub forecast_tick: Option<u64>,
    pub sample_count: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SensorSnapshot {
    pub resource: u64,
    pub sample_count: u32,
    pub last_tick: u64,
    pub mean_q16: i32,
    pub deviation_q16: u32,
    pub trend_q16: i32,
    pub temperature_mean_q16: i32,
    pub correction_ema_q16: u32,
    pub fault_ema_q16: u32,
    pub cusum_q16: u32,
    pub last_risk: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArgusError {
    ZeroCapacity,
    Capacity,
    UnknownResource,
    TimeRegression,
    InvalidPolicy,
}

#[derive(Clone, Copy)]
struct SensorState {
    occupied: bool,
    resource: u64,
    sample_count: u32,
    last_tick: u64,
    last_signal_q16: i64,
    mean_q16: i64,
    deviation_q16: u64,
    trend_q16: i64,
    temperature_mean_q16: i64,
    correction_ema_q16: u64,
    fault_ema_q16: u64,
    cusum_up_q16: i64,
    cusum_down_q16: i64,
    last_risk: u16,
}

impl SensorState {
    const EMPTY: Self = Self {
        occupied: false,
        resource: 0,
        sample_count: 0,
        last_tick: 0,
        last_signal_q16: 0,
        mean_q16: 0,
        deviation_q16: 0,
        trend_q16: 0,
        temperature_mean_q16: 0,
        correction_ema_q16: 0,
        fault_ema_q16: 0,
        cusum_up_q16: 0,
        cusum_down_q16: 0,
        last_risk: 0,
    };

    fn initialize(sample: TelemetrySample) -> Self {
        Self {
            occupied: true,
            resource: sample.resource,
            sample_count: 1,
            last_tick: sample.tick,
            last_signal_q16: i64::from(sample.signal_q16),
            mean_q16: i64::from(sample.signal_q16),
            deviation_q16: 1 << 8,
            trend_q16: 0,
            temperature_mean_q16: i64::from(sample.temperature_q16),
            correction_ema_q16: u64::from(sample.corrections) << 16,
            fault_ema_q16: u64::from(sample.faults) << 16,
            cusum_up_q16: 0,
            cusum_down_q16: 0,
            last_risk: 0,
        }
    }

    fn snapshot(self) -> SensorSnapshot {
        SensorSnapshot {
            resource: self.resource,
            sample_count: self.sample_count,
            last_tick: self.last_tick,
            mean_q16: clamp_i64_to_i32(self.mean_q16),
            deviation_q16: clamp_u64_to_u32(self.deviation_q16),
            trend_q16: clamp_i64_to_i32(self.trend_q16),
            temperature_mean_q16: clamp_i64_to_i32(self.temperature_mean_q16),
            correction_ema_q16: clamp_u64_to_u32(self.correction_ema_q16),
            fault_ema_q16: clamp_u64_to_u32(self.fault_ema_q16),
            cusum_q16: clamp_u64_to_u32(
                abs_i64(self.cusum_up_q16).max(abs_i64(self.cusum_down_q16)),
            ),
            last_risk: self.last_risk,
        }
    }
}

struct ArgusState<const N: usize> {
    sensors: [SensorState; N],
    policy: ArgusPolicy,
    observations: u64,
}

impl<const N: usize> ArgusState<N> {
    const fn new(policy: ArgusPolicy) -> Self {
        Self {
            sensors: [SensorState::EMPTY; N],
            policy,
            observations: 0,
        }
    }
}

pub struct ArgusSentinel<const N: usize> {
    state: SpinLock<ArgusState<N>>,
}

impl<const N: usize> ArgusSentinel<N> {
    pub const fn new(policy: ArgusPolicy) -> Self {
        Self {
            state: SpinLock::new(ArgusState::new(policy)),
        }
    }

    pub fn observe(&self, sample: TelemetrySample) -> Result<ArgusAssessment, ArgusError> {
        if N == 0 {
            return Err(ArgusError::ZeroCapacity);
        }

        let mut state = self.state.lock();
        validate_policy(state.policy)?;
        let policy = state.policy;

        let index = if let Some(index) = state
            .sensors
            .iter()
            .position(|sensor| sensor.occupied && sensor.resource == sample.resource)
        {
            index
        } else {
            let index = state
                .sensors
                .iter()
                .position(|sensor| !sensor.occupied)
                .ok_or(ArgusError::Capacity)?;
            state.sensors[index] = SensorState::initialize(sample);
            state.observations = state.observations.saturating_add(1);
            let assessment = assess(&state.sensors[index], sample, policy, 1);
            state.sensors[index].last_risk = assessment.risk;
            return Ok(assessment);
        };

        let sensor = &mut state.sensors[index];
        if sample.tick < sensor.last_tick {
            return Err(ArgusError::TimeRegression);
        }

        let interval = sample.tick.saturating_sub(sensor.last_tick).max(1);
        let signal = i64::from(sample.signal_q16);
        let temperature = i64::from(sample.temperature_q16);

        let residual = signal.saturating_sub(sensor.mean_q16);
        let absolute_residual = abs_i64(residual);

        sensor.mean_q16 = ewma_i64(sensor.mean_q16, signal, SIGNAL_ALPHA_SHIFT);
        sensor.deviation_q16 = ewma_u64(
            sensor.deviation_q16.max(1 << 8),
            absolute_residual.max(1 << 8),
            SIGNAL_ALPHA_SHIFT,
        );

        let instantaneous_trend = signal.saturating_sub(sensor.last_signal_q16);
        sensor.trend_q16 = ewma_i64(sensor.trend_q16, instantaneous_trend, TREND_ALPHA_SHIFT);
        sensor.temperature_mean_q16 =
            ewma_i64(sensor.temperature_mean_q16, temperature, SIGNAL_ALPHA_SHIFT);
        sensor.correction_ema_q16 = ewma_u64(
            sensor.correction_ema_q16,
            u64::from(sample.corrections) << 16,
            RATE_ALPHA_SHIFT,
        );
        sensor.fault_ema_q16 = ewma_u64(
            sensor.fault_ema_q16,
            u64::from(sample.faults) << 16,
            RATE_ALPHA_SHIFT,
        );

        let drift = (sensor.deviation_q16 >> 4).max(1 << 6) as i64;
        sensor.cusum_up_q16 = sensor
            .cusum_up_q16
            .saturating_add(residual)
            .saturating_sub(drift)
            .max(0);
        sensor.cusum_down_q16 = sensor
            .cusum_down_q16
            .saturating_sub(residual)
            .saturating_sub(drift)
            .max(0);

        sensor.sample_count = sensor.sample_count.saturating_add(1);
        sensor.last_tick = sample.tick;
        sensor.last_signal_q16 = signal;

        let assessment = assess(sensor, sample, policy, interval);
        sensor.last_risk = assessment.risk;
        state.observations = state.observations.saturating_add(1);
        Ok(assessment)
    }

    pub fn snapshot(&self, resource: u64) -> Result<SensorSnapshot, ArgusError> {
        let state = self.state.lock();
        state
            .sensors
            .iter()
            .find(|sensor| sensor.occupied && sensor.resource == resource)
            .copied()
            .map(SensorState::snapshot)
            .ok_or(ArgusError::UnknownResource)
    }

    pub fn retune(
        &self,
        policy: ArgusPolicy,
        _authority: &Capability<'_, LearningControl>,
    ) -> Result<(), ArgusError> {
        validate_policy(policy)?;
        self.state.lock().policy = policy;
        Ok(())
    }

    pub fn forget(
        &self,
        resource: u64,
        _authority: &Capability<'_, LearningControl>,
    ) -> Result<(), ArgusError> {
        let mut state = self.state.lock();
        let sensor = state
            .sensors
            .iter_mut()
            .find(|sensor| sensor.occupied && sensor.resource == resource)
            .ok_or(ArgusError::UnknownResource)?;
        *sensor = SensorState::EMPTY;
        Ok(())
    }

    pub fn observations(&self) -> u64 {
        self.state.lock().observations
    }
}

fn assess(
    sensor: &SensorState,
    sample: TelemetrySample,
    policy: ArgusPolicy,
    interval: u64,
) -> ArgusAssessment {
    let signal = i64::from(sample.signal_q16);
    let residual = abs_i64(signal.saturating_sub(sensor.mean_q16));
    let anomaly_q16 = normalized_q16(residual, sensor.deviation_q16.max(1 << 8));

    let trend_q16 = abs_i64(sensor.trend_q16);
    let trend_normalized = normalized_q16(trend_q16, sensor.deviation_q16.max(1 << 8));

    let correction_normalized = normalized_q16(
        sensor.correction_ema_q16,
        u64::from(policy.correction_watch_q16).max(1),
    );
    let fault_normalized = normalized_q16(
        sensor.fault_ema_q16,
        u64::from(policy.fault_watch_q16).max(1),
    );

    let temperature = i64::from(sample.temperature_q16);
    let thermal_excess = temperature
        .saturating_sub(i64::from(policy.warm_temperature_q16))
        .max(0) as u64;
    let thermal_span = i64::from(policy.critical_temperature_q16)
        .saturating_sub(i64::from(policy.warm_temperature_q16))
        .max(1) as u64;
    let thermal_normalized = normalized_q16(thermal_excess, thermal_span);

    let pressure_excess = abs_i64(i64::from(sample.pressure_q16));
    let pressure_normalized = normalized_q16(
        pressure_excess,
        abs_i64(i64::from(policy.pressure_limit_q16)).max(1),
    );

    let cusum = abs_i64(sensor.cusum_up_q16).max(abs_i64(sensor.cusum_down_q16));
    let cusum_normalized = normalized_q16(cusum, u64::from(policy.cusum_limit_q16).max(1));

    let risk = weighted_risk([
        (anomaly_q16, 260_u16),
        (trend_normalized, 120),
        (correction_normalized, 120),
        (fault_normalized, 220),
        (thermal_normalized, 140),
        (pressure_normalized, 60),
        (cusum_normalized, 80),
    ]);

    let severity = if risk >= policy.terminal_risk {
        ArgusSeverity::Terminal
    } else if risk >= policy.critical_risk {
        ArgusSeverity::Critical
    } else if risk >= policy.degraded_risk {
        ArgusSeverity::Degraded
    } else if risk >= policy.watch_risk {
        ArgusSeverity::Watch
    } else {
        ArgusSeverity::Stable
    };

    let action = match severity {
        ArgusSeverity::Stable => ArgusAction::Observe,
        ArgusSeverity::Watch => ArgusAction::IncreaseSampling,
        ArgusSeverity::Degraded => ArgusAction::Quarantine,
        ArgusSeverity::Critical => {
            if sensor.fault_ema_q16 >= u64::from(policy.fault_watch_q16) {
                ArgusAction::RevokeDma
            } else {
                ArgusAction::ResetDevice
            }
        }
        ArgusSeverity::Terminal => ArgusAction::RetireResource,
    };

    ArgusAssessment {
        resource: sample.resource,
        tick: sample.tick,
        severity,
        action,
        risk,
        anomaly_q16: clamp_u64_to_u32(anomaly_q16),
        trend_q16: clamp_i64_to_i32(sensor.trend_q16),
        thermal_margin_q16: clamp_i64_to_i32(
            i64::from(policy.critical_temperature_q16).saturating_sub(temperature),
        ),
        cusum_q16: clamp_u64_to_u32(cusum),
        forecast_tick: forecast_failure(sensor, sample, policy, interval),
        sample_count: sensor.sample_count,
    }
}

fn forecast_failure(
    sensor: &SensorState,
    sample: TelemetrySample,
    policy: ArgusPolicy,
    interval: u64,
) -> Option<u64> {
    let mut nearest: Option<u64> = None;

    if sensor.trend_q16 > 0 {
        let signal_headroom =
            i64::from(policy.signal_limit_q16).saturating_sub(i64::from(sample.signal_q16));
        if signal_headroom <= 0 {
            nearest = Some(sample.tick);
        } else {
            let samples = (signal_headroom as u64).saturating_add(sensor.trend_q16 as u64 - 1)
                / sensor.trend_q16 as u64;
            nearest = Some(sample.tick.saturating_add(samples.saturating_mul(interval)));
        }
    }

    let thermal_delta = sensor
        .temperature_mean_q16
        .saturating_sub(i64::from(sample.temperature_q16));
    let observed_thermal_trend = thermal_delta.saturating_neg();

    if observed_thermal_trend > 0 {
        let thermal_headroom = i64::from(policy.critical_temperature_q16)
            .saturating_sub(i64::from(sample.temperature_q16));
        let thermal_tick = if thermal_headroom <= 0 {
            sample.tick
        } else {
            let samples = (thermal_headroom as u64)
                .saturating_add(observed_thermal_trend as u64 - 1)
                / observed_thermal_trend as u64;
            sample.tick.saturating_add(samples.saturating_mul(interval))
        };

        nearest = Some(match nearest {
            Some(current) => current.min(thermal_tick),
            None => thermal_tick,
        });
    }

    nearest
}

fn validate_policy(policy: ArgusPolicy) -> Result<(), ArgusError> {
    if policy.warm_temperature_q16 >= policy.critical_temperature_q16
        || policy.signal_limit_q16 <= 0
        || policy.pressure_limit_q16 <= 0
        || policy.correction_watch_q16 == 0
        || policy.fault_watch_q16 == 0
        || policy.cusum_limit_q16 == 0
        || !(policy.watch_risk < policy.degraded_risk
            && policy.degraded_risk < policy.critical_risk
            && policy.critical_risk < policy.terminal_risk
            && policy.terminal_risk <= 1000)
    {
        return Err(ArgusError::InvalidPolicy);
    }
    Ok(())
}

fn ewma_i64(current: i64, target: i64, shift: u32) -> i64 {
    current.saturating_add(target.saturating_sub(current) >> shift)
}

fn ewma_u64(current: u64, target: u64, shift: u32) -> u64 {
    if target >= current {
        current.saturating_add((target - current) >> shift)
    } else {
        current.saturating_sub((current - target) >> shift)
    }
}

fn normalized_q16(value: u64, reference: u64) -> u64 {
    if reference == 0 {
        return MAXIMUM_NORMALIZED_Q16;
    }
    value
        .saturating_mul(Q16_ONE as u64)
        .checked_div(reference)
        .unwrap_or(MAXIMUM_NORMALIZED_Q16)
        .min(MAXIMUM_NORMALIZED_Q16)
}

fn weighted_risk<const N: usize>(components: [(u64, u16); N]) -> u16 {
    let mut score = 0_u64;
    for (normalized, weight) in components {
        let bounded = normalized.min(Q16_ONE as u64);
        score = score.saturating_add(
            bounded
                .saturating_mul(u64::from(weight))
                .checked_div(Q16_ONE as u64)
                .unwrap_or(u64::from(weight)),
        );
    }
    score.min(1000) as u16
}

fn abs_i64(value: i64) -> u64 {
    value.unsigned_abs()
}

fn clamp_i64_to_i32(value: i64) -> i32 {
    value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

fn clamp_u64_to_u32(value: u64) -> u32 {
    value.min(u64::from(u32::MAX)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Authority;

    #[test]
    fn stable_signal_remains_bounded() {
        let sentinel = ArgusSentinel::<4>::new(ArgusPolicy::BLACK_LAB);

        let mut last = None;
        for tick in 1..32 {
            last = Some(
                sentinel
                    .observe(TelemetrySample {
                        tick,
                        resource: 7,
                        signal_q16: 1 << 16,
                        temperature_q16: 45 << 16,
                        pressure_q16: 1 << 16,
                        corrections: 0,
                        faults: 0,
                    })
                    .unwrap(),
            );
        }

        let assessment = last.unwrap();
        assert!(assessment.risk < ArgusPolicy::BLACK_LAB.watch_risk);
        assert_eq!(assessment.severity, ArgusSeverity::Stable);
    }

    #[test]
    fn faults_and_heat_drive_containment() {
        let sentinel = ArgusSentinel::<4>::new(ArgusPolicy::BLACK_LAB);

        for tick in 1..8 {
            sentinel
                .observe(TelemetrySample {
                    tick,
                    resource: 9,
                    signal_q16: (tick as i32) << 16,
                    temperature_q16: (88 + tick as i32) << 16,
                    pressure_q16: 7 << 16,
                    corrections: 12,
                    faults: 3,
                })
                .unwrap();
        }

        let assessment = sentinel
            .observe(TelemetrySample {
                tick: 8,
                resource: 9,
                signal_q16: 12 << 16,
                temperature_q16: 98 << 16,
                pressure_q16: 8 << 16,
                corrections: 16,
                faults: 4,
            })
            .unwrap();

        assert!(assessment.risk >= ArgusPolicy::BLACK_LAB.degraded_risk);
        assert!(matches!(
            assessment.action,
            ArgusAction::Quarantine
                | ArgusAction::RevokeDma
                | ArgusAction::ResetDevice
                | ArgusAction::RetireResource
        ));
    }

    #[test]
    fn policy_changes_require_scoped_learning_authority() {
        let authority = unsafe { Authority::assume_root() };
        let learning = authority.grant::<LearningControl>();
        let sentinel = ArgusSentinel::<2>::new(ArgusPolicy::BLACK_LAB);
        assert!(sentinel.retune(ArgusPolicy::BLACK_LAB, &learning).is_ok());
    }
}
