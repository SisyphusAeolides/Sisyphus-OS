//! Bounded min-plus service calculus with online conformal calibration.
//!
//! The controller combines three pieces of evidence:
//!
//! * a deterministic rate-latency service curve;
//! * a bounded arrival envelope;
//! * a one-sided conformal residual guard learned from completed work.
//!
//! Admission is allowed only when the complete risk-adjusted delay bound fits
//! inside the caller's deadline. All state is fixed-capacity and allocation
//! free, so the same implementation can be used by the kernel and userland.

pub const Q16_ONE: u64 = 1 << 16;
pub const DEFAULT_CONFORMAL_NUMERATOR: u8 = 15;
pub const DEFAULT_CONFORMAL_DENOMINATOR: u8 = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServiceCurve {
    /// Smallest number of completions guaranteed in one service window.
    pub minimum_completions_per_window: u16,
    /// Largest number of new jobs admitted in one service window.
    pub maximum_admissions_per_window: u16,
    /// Service-window duration in platform ticks.
    pub window_ticks: u64,
    /// Fixed device, scheduler, or transport latency in platform ticks.
    pub latency_ticks: u64,
    /// Maximum live backlog admitted by policy.
    pub maximum_backlog: u16,
}

impl ServiceCurve {
    pub const fn valid(self) -> bool {
        self.minimum_completions_per_window != 0
            && self.maximum_admissions_per_window != 0
            && self.window_ticks != 0
            && self.maximum_backlog != 0
            && self.minimum_completions_per_window <= self.maximum_backlog
            && self.maximum_admissions_per_window <= self.maximum_backlog
    }

    /// Deterministic min-plus horizontal-deviation bound for one new job.
    pub fn deterministic_delay(self, backlog_before: usize) -> Option<u64> {
        if !self.valid() || backlog_before >= usize::from(self.maximum_backlog) {
            return None;
        }

        let demand = u64::try_from(backlog_before).ok()?.checked_add(1)?;
        let service = u64::from(self.minimum_completions_per_window);
        let windows = demand.checked_add(service - 1)?.checked_div(service)?;
        self.latency_ticks
            .checked_add(windows.checked_mul(self.window_ticks)?)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CalibrationFault {
    ZeroCapacity,
    InvalidCoverage,
    TimeRegression,
    ArithmeticOverflow,
    CorruptCertificate,
}

/// Fixed-capacity one-sided split-conformal residual calibrator.
///
/// Only positive underprediction residuals are retained. This makes the
/// returned order statistic a direct safety guard rather than a symmetric
/// variance estimate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResidualCalibrator<const N: usize> {
    residuals: [u64; N],
    length: usize,
    cursor: usize,
    samples: u64,
    coverage_numerator: u8,
    coverage_denominator: u8,
    additive_slack: u64,
    maximum_guard: u64,
    secret: u64,
    root: u64,
}

impl<const N: usize> ResidualCalibrator<N> {
    pub fn new(
        coverage_numerator: u8,
        coverage_denominator: u8,
        additive_slack: u64,
        maximum_guard: u64,
        secret: u64,
    ) -> Result<Self, CalibrationFault> {
        if N == 0 {
            return Err(CalibrationFault::ZeroCapacity);
        }
        if coverage_numerator == 0
            || coverage_denominator == 0
            || coverage_numerator >= coverage_denominator
            || maximum_guard == 0
            || secret == 0
        {
            return Err(CalibrationFault::InvalidCoverage);
        }

        let mut calibrator = Self {
            residuals: [0; N],
            length: 0,
            cursor: 0,
            samples: 0,
            coverage_numerator,
            coverage_denominator,
            additive_slack,
            maximum_guard,
            secret,
            root: 0,
        };
        calibrator.seal();
        Ok(calibrator)
    }

    pub fn kernel_default(
        additive_slack: u64,
        maximum_guard: u64,
        secret: u64,
    ) -> Result<Self, CalibrationFault> {
        Self::new(
            DEFAULT_CONFORMAL_NUMERATOR,
            DEFAULT_CONFORMAL_DENOMINATOR,
            additive_slack,
            maximum_guard,
            secret,
        )
    }

    pub const fn samples(&self) -> u64 {
        self.samples
    }

    pub const fn retained(&self) -> usize {
        self.length
    }

    pub const fn root(&self) -> u64 {
        self.root
    }

    pub fn push(&mut self, residual: u64) -> Result<u64, CalibrationFault> {
        let bounded = residual.min(self.maximum_guard);
        self.residuals[self.cursor] = bounded;
        self.cursor = (self.cursor + 1) % N;
        self.length = self.length.saturating_add(1).min(N);
        self.samples = self.samples.saturating_add(1);
        self.seal();
        self.guard()
    }

    pub fn guard(&self) -> Result<u64, CalibrationFault> {
        if self.length == 0 {
            return Ok(self.additive_slack.min(self.maximum_guard));
        }

        let mut sorted = [0_u64; N];
        sorted[..self.length].copy_from_slice(&self.residuals[..self.length]);
        sorted[..self.length].sort_unstable();

        let numerator = self
            .length
            .checked_add(1)
            .and_then(|value| {
                value.checked_mul(usize::from(self.coverage_numerator))
            })
            .ok_or(CalibrationFault::ArithmeticOverflow)?;
        let denominator = usize::from(self.coverage_denominator);
        let rank = numerator
            .checked_add(denominator - 1)
            .ok_or(CalibrationFault::ArithmeticOverflow)?
            / denominator;
        let index = rank.saturating_sub(1).min(self.length - 1);

        sorted[index]
            .checked_add(self.additive_slack)
            .ok_or(CalibrationFault::ArithmeticOverflow)
            .map(|value| value.min(self.maximum_guard))
    }

    pub fn verify_root(&self) -> bool {
        self.root == calibrator_root(self)
    }

    fn seal(&mut self) {
        self.root = calibrator_root(self);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionFault {
    InvalidCurve,
    BacklogSaturated,
    ArrivalEnvelopeExceeded,
    DeadlineUnsafe,
    TimeRegression,
    ArithmeticOverflow,
    StaleReservation,
    CorruptObservation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdmissionCertificate {
    pub reservation_sequence: u64,
    pub admitted_tick: u64,
    pub window_start: u64,
    pub backlog_before: u16,
    pub admitted_before: u16,
    pub deterministic_delay_ticks: u64,
    pub uncertainty_guard_ticks: u64,
    pub drift_penalty_ticks: u64,
    pub delay_bound_ticks: u64,
    pub deadline_slack_ticks: u64,
    pub curve_root: u64,
    pub calibration_root: u64,
    pub certificate_root: u64,
}

impl AdmissionCertificate {
    pub const EMPTY: Self = Self {
        reservation_sequence: 0,
        admitted_tick: 0,
        window_start: 0,
        backlog_before: 0,
        admitted_before: 0,
        deterministic_delay_ticks: 0,
        uncertainty_guard_ticks: 0,
        drift_penalty_ticks: 0,
        delay_bound_ticks: 0,
        deadline_slack_ticks: 0,
        curve_root: 0,
        calibration_root: 0,
        certificate_root: 0,
    };

    pub const fn valid(self) -> bool {
        self.reservation_sequence != 0
            && self.delay_bound_ticks != 0
            && self.curve_root != 0
            && self.calibration_root != 0
            && self.certificate_root != 0
    }
}

/// Fixed-capacity service controller.
///
/// `N` is the number of completed-job residuals retained by the conformal
/// calibrator. The virtual backlog is a Lyapunov queue used to penalize
/// sustained under-service without changing the deterministic hard bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServiceController<const N: usize> {
    curve: ServiceCurve,
    secret: u64,
    window_start: u64,
    last_tick: u64,
    admitted_in_window: u16,
    reservation_sequence: u64,
    accepted: u64,
    rejected: u64,
    observed: u64,
    virtual_backlog_q16: u64,
    curve_root: u64,
    calibrator: ResidualCalibrator<N>,
}

impl<const N: usize> ServiceController<N> {
    pub fn new(
        curve: ServiceCurve,
        now_tick: u64,
        secret: u64,
    ) -> Result<Self, AdmissionFault> {
        if !curve.valid() || secret == 0 || N == 0 {
            return Err(AdmissionFault::InvalidCurve);
        }

        let curve_root = service_curve_root(secret, curve);
        if curve_root == 0 {
            return Err(AdmissionFault::InvalidCurve);
        }

        let maximum_guard = curve
            .window_ticks
            .checked_mul(u64::from(curve.maximum_backlog))
            .ok_or(AdmissionFault::ArithmeticOverflow)?;
        let calibrator = ResidualCalibrator::kernel_default(
            1,
            maximum_guard.max(1),
            mix(secret, 0x4341_4c49_4252_4154),
        )
        .map_err(|_| AdmissionFault::InvalidCurve)?;

        Ok(Self {
            curve,
            secret,
            window_start: now_tick,
            last_tick: now_tick,
            admitted_in_window: 0,
            reservation_sequence: 0,
            accepted: 0,
            rejected: 0,
            observed: 0,
            virtual_backlog_q16: 0,
            curve_root,
            calibrator,
        })
    }

    pub const fn curve(&self) -> ServiceCurve {
        self.curve
    }

    pub const fn curve_root(&self) -> u64 {
        self.curve_root
    }

    pub const fn accepted(&self) -> u64 {
        self.accepted
    }

    pub const fn rejected(&self) -> u64 {
        self.rejected
    }

    pub const fn observed(&self) -> u64 {
        self.observed
    }

    pub const fn virtual_backlog_q16(&self) -> u64 {
        self.virtual_backlog_q16
    }

    pub fn uncertainty_guard_ticks(&self) -> u64 {
        self.calibrator.guard().unwrap_or(self.curve.window_ticks)
    }

    pub fn admit(
        &mut self,
        now_tick: u64,
        deadline_tick: u64,
        backlog_before: usize,
    ) -> Result<AdmissionCertificate, AdmissionFault> {
        self.advance(now_tick)?;

        if backlog_before >= usize::from(self.curve.maximum_backlog) {
            self.reject();
            return Err(AdmissionFault::BacklogSaturated);
        }
        if self.admitted_in_window >= self.curve.maximum_admissions_per_window {
            self.reject();
            return Err(AdmissionFault::ArrivalEnvelopeExceeded);
        }

        let deterministic = self
            .curve
            .deterministic_delay(backlog_before)
            .ok_or(AdmissionFault::BacklogSaturated)?;
        let uncertainty = self
            .calibrator
            .guard()
            .map_err(|_| AdmissionFault::ArithmeticOverflow)?;
        let drift_penalty = self.drift_penalty_ticks();
        let delay = deterministic
            .checked_add(uncertainty)
            .and_then(|value| value.checked_add(drift_penalty))
            .ok_or(AdmissionFault::ArithmeticOverflow)?;
        let required_deadline = now_tick
            .checked_add(delay)
            .ok_or(AdmissionFault::ArithmeticOverflow)?;
        if deadline_tick < required_deadline {
            self.reject();
            return Err(AdmissionFault::DeadlineUnsafe);
        }

        let admitted_before = self.admitted_in_window;
        self.admitted_in_window = self
            .admitted_in_window
            .checked_add(1)
            .ok_or(AdmissionFault::ArithmeticOverflow)?;
        self.reservation_sequence =
            self.reservation_sequence.wrapping_add(1).max(1);
        self.accepted = self.accepted.saturating_add(1);
        self.virtual_backlog_q16 = self
            .virtual_backlog_q16
            .saturating_add(Q16_ONE);

        let backlog_before = u16::try_from(backlog_before)
            .map_err(|_| AdmissionFault::BacklogSaturated)?;
        let mut certificate = AdmissionCertificate {
            reservation_sequence: self.reservation_sequence,
            admitted_tick: now_tick,
            window_start: self.window_start,
            backlog_before,
            admitted_before,
            deterministic_delay_ticks: deterministic,
            uncertainty_guard_ticks: uncertainty,
            drift_penalty_ticks: drift_penalty,
            delay_bound_ticks: delay,
            deadline_slack_ticks: deadline_tick - required_deadline,
            curve_root: self.curve_root,
            calibration_root: self.calibrator.root(),
            certificate_root: 0,
        };
        certificate.certificate_root =
            admission_certificate_root(self.secret, certificate);
        Ok(certificate)
    }

    pub fn rollback(
        &mut self,
        certificate: AdmissionCertificate,
    ) -> Result<(), AdmissionFault> {
        if !self.verify_certificate(certificate)
            || certificate.window_start != self.window_start
            || certificate.reservation_sequence != self.reservation_sequence
            || self.admitted_in_window
                != certificate.admitted_before.saturating_add(1)
        {
            return Err(AdmissionFault::StaleReservation);
        }

        self.admitted_in_window = certificate.admitted_before;
        self.accepted = self.accepted.saturating_sub(1);
        self.virtual_backlog_q16 =
            self.virtual_backlog_q16.saturating_sub(Q16_ONE);
        Ok(())
    }

    /// Records the completion of an admitted job.
    ///
    /// The calibrator receives only positive underprediction residuals. A job
    /// finishing earlier than its deterministic-plus-drift estimate contributes
    /// zero residual and still drains one unit of Lyapunov backlog.
    pub fn observe_completion(
        &mut self,
        certificate: AdmissionCertificate,
        completion_tick: u64,
    ) -> Result<u64, AdmissionFault> {
        if !self.verify_certificate(certificate)
            || completion_tick < certificate.admitted_tick
        {
            return Err(AdmissionFault::CorruptObservation);
        }

        self.advance(completion_tick)?;
        let observed_delay = completion_tick - certificate.admitted_tick;
        let predicted_without_uncertainty = certificate
            .deterministic_delay_ticks
            .checked_add(certificate.drift_penalty_ticks)
            .ok_or(AdmissionFault::ArithmeticOverflow)?;
        let residual = observed_delay.saturating_sub(predicted_without_uncertainty);
        let guard = self
            .calibrator
            .push(residual)
            .map_err(|_| AdmissionFault::CorruptObservation)?;

        self.virtual_backlog_q16 =
            self.virtual_backlog_q16.saturating_sub(Q16_ONE);
        if observed_delay > certificate.delay_bound_ticks {
            let excess = observed_delay - certificate.delay_bound_ticks;
            let pressure = excess
                .checked_mul(Q16_ONE)
                .and_then(|value| value.checked_div(self.curve.window_ticks))
                .unwrap_or(u64::MAX);
            self.virtual_backlog_q16 =
                self.virtual_backlog_q16.saturating_add(pressure);
        }
        self.observed = self.observed.saturating_add(1);
        Ok(guard)
    }

    pub fn verify_certificate(&self, certificate: AdmissionCertificate) -> bool {
        certificate.valid()
            && certificate.curve_root == self.curve_root
            && certificate.certificate_root
                == admission_certificate_root(self.secret, certificate)
    }

    fn advance(&mut self, now_tick: u64) -> Result<(), AdmissionFault> {
        if now_tick < self.last_tick || now_tick < self.window_start {
            self.reject();
            return Err(AdmissionFault::TimeRegression);
        }

        let elapsed = now_tick - self.window_start;
        if elapsed >= self.curve.window_ticks {
            let windows = elapsed / self.curve.window_ticks;
            self.window_start = self
                .window_start
                .checked_add(
                    windows
                        .checked_mul(self.curve.window_ticks)
                        .ok_or(AdmissionFault::ArithmeticOverflow)?,
                )
                .ok_or(AdmissionFault::ArithmeticOverflow)?;
            self.admitted_in_window = 0;

            let service = windows
                .checked_mul(u64::from(
                    self.curve.minimum_completions_per_window,
                ))
                .and_then(|value| value.checked_mul(Q16_ONE))
                .ok_or(AdmissionFault::ArithmeticOverflow)?;
            self.virtual_backlog_q16 =
                self.virtual_backlog_q16.saturating_sub(service);
        }
        self.last_tick = now_tick;
        Ok(())
    }

    fn drift_penalty_ticks(&self) -> u64 {
        let jobs = self.virtual_backlog_q16 >> 16;
        if jobs == 0 {
            return 0;
        }
        let service = u64::from(self.curve.minimum_completions_per_window);
        let windows = jobs.saturating_add(service - 1) / service;
        windows.saturating_mul(self.curve.window_ticks)
    }

    fn reject(&mut self) {
        self.rejected = self.rejected.saturating_add(1);
    }
}

pub fn service_curve_root(secret: u64, curve: ServiceCurve) -> u64 {
    let mut state = mix(secret, u64::from(curve.minimum_completions_per_window));
    state = mix(state, u64::from(curve.maximum_admissions_per_window));
    state = mix(state, curve.window_ticks);
    state = mix(state, curve.latency_ticks);
    mix(state, u64::from(curve.maximum_backlog))
}

pub fn admission_certificate_root(
    secret: u64,
    certificate: AdmissionCertificate,
) -> u64 {
    let mut state = mix(secret, certificate.reservation_sequence);
    state = mix(state, certificate.admitted_tick);
    state = mix(state, certificate.window_start);
    state = mix(
        state,
        u64::from(certificate.backlog_before)
            | (u64::from(certificate.admitted_before) << 16),
    );
    state = mix(state, certificate.deterministic_delay_ticks);
    state = mix(state, certificate.uncertainty_guard_ticks);
    state = mix(state, certificate.drift_penalty_ticks);
    state = mix(state, certificate.delay_bound_ticks);
    state = mix(state, certificate.deadline_slack_ticks);
    state = mix(state, certificate.curve_root);
    mix(state, certificate.calibration_root)
}

fn calibrator_root<const N: usize>(calibrator: &ResidualCalibrator<N>) -> u64 {
    let mut state = mix(calibrator.secret, calibrator.samples);
    state = mix(
        state,
        u64::try_from(calibrator.length).unwrap_or(u64::MAX),
    );
    state = mix(
        state,
        u64::try_from(calibrator.cursor).unwrap_or(u64::MAX),
    );
    state = mix(
        state,
        u64::from(calibrator.coverage_numerator)
            | (u64::from(calibrator.coverage_denominator) << 8),
    );
    state = mix(state, calibrator.additive_slack);
    state = mix(state, calibrator.maximum_guard);
    for residual in &calibrator.residuals[..calibrator.length] {
        state = mix(state, *residual);
    }
    state
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

    fn curve() -> ServiceCurve {
        ServiceCurve {
            minimum_completions_per_window: 2,
            maximum_admissions_per_window: 4,
            window_ticks: 100,
            latency_ticks: 20,
            maximum_backlog: 8,
        }
    }

    #[test]
    fn deterministic_delay_is_min_plus_horizontal_deviation() {
        assert_eq!(curve().deterministic_delay(0), Some(120));
        assert_eq!(curve().deterministic_delay(1), Some(120));
        assert_eq!(curve().deterministic_delay(2), Some(220));
    }

    #[test]
    fn conformal_guard_tracks_positive_underprediction() {
        let mut calibrator =
            ResidualCalibrator::<8>::kernel_default(1, 1_000, 7).unwrap();
        for residual in [1, 2, 3, 4, 5, 6, 7, 100] {
            calibrator.push(residual).unwrap();
        }
        assert!(calibrator.guard().unwrap() >= 100);
        assert!(calibrator.verify_root());
    }

    #[test]
    fn unsafe_deadline_is_rejected() {
        let mut controller = ServiceController::<8>::new(curve(), 1_000, 7).unwrap();
        assert_eq!(
            controller.admit(1_000, 1_120, 0),
            Err(AdmissionFault::DeadlineUnsafe),
        );
    }

    #[test]
    fn failed_submission_rolls_back_latest_reservation() {
        let mut controller = ServiceController::<8>::new(curve(), 1_000, 7).unwrap();
        let certificate = controller.admit(1_000, 1_500, 0).unwrap();
        controller.rollback(certificate).unwrap();
        assert_eq!(controller.accepted(), 0);
        assert_eq!(controller.virtual_backlog_q16(), 0);
    }

    #[test]
    fn completion_updates_uncertainty_and_drains_virtual_backlog() {
        let mut controller = ServiceController::<8>::new(curve(), 1_000, 7).unwrap();
        let certificate = controller.admit(1_000, 2_000, 0).unwrap();
        let before = controller.uncertainty_guard_ticks();
        let after = controller
            .observe_completion(certificate, 1_400)
            .unwrap();
        assert!(after >= before);
        assert_eq!(controller.observed(), 1);
    }
}
