use crate::capability::{Capability, PolicyControl};
use crate::sync::SpinLock;

pub const MAXIMUM_RATE_SAMPLES: usize = 8;
pub const MAXIMUM_VIOLATIONS_PER_EVENT: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OracleEvent {
    pub tick: u64,
    pub kind: u16,
    pub severity: u8,
    pub flags: u16,
    pub subject: u64,
    pub value: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Predicate {
    pub kind: u16,
    pub minimum_severity: u8,
    pub required_flags: u16,
    pub subject_mask: u64,
    pub subject_value: u64,
    pub value_mask: u64,
    pub value_value: u64,
}

impl Predicate {
    pub const ANY: Self = Self {
        kind: 0,
        minimum_severity: 0,
        required_flags: 0,
        subject_mask: 0,
        subject_value: 0,
        value_mask: 0,
        value_value: 0,
    };

    pub const fn kind(kind: u16) -> Self {
        Self { kind, ..Self::ANY }
    }

    pub const fn matches(self, event: OracleEvent) -> bool {
        (self.kind == 0 || self.kind == event.kind)
            && event.severity >= self.minimum_severity
            && event.flags & self.required_flags == self.required_flags
            && (event.subject & self.subject_mask) == (self.subject_value & self.subject_mask)
            && (event.value & self.value_mask) == (self.value_value & self.value_mask)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TemporalMode {
    LeadsTo,
    Inhibits,
    RateLimit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TemporalRule {
    pub id: u32,
    pub mode: TemporalMode,
    pub trigger: Predicate,
    pub target: Predicate,
    pub horizon_ticks: u64,
    pub maximum_hits: u8,
    pub correlate_subject: bool,
}

impl TemporalRule {
    pub const fn leads_to(
        id: u32,
        trigger: Predicate,
        target: Predicate,
        horizon_ticks: u64,
        correlate_subject: bool,
    ) -> Self {
        Self {
            id,
            mode: TemporalMode::LeadsTo,
            trigger,
            target,
            horizon_ticks,
            maximum_hits: 0,
            correlate_subject,
        }
    }

    pub const fn inhibits(
        id: u32,
        trigger: Predicate,
        forbidden: Predicate,
        horizon_ticks: u64,
        correlate_subject: bool,
    ) -> Self {
        Self {
            id,
            mode: TemporalMode::Inhibits,
            trigger,
            target: forbidden,
            horizon_ticks,
            maximum_hits: 0,
            correlate_subject,
        }
    }

    pub const fn rate_limit(
        id: u32,
        target: Predicate,
        horizon_ticks: u64,
        maximum_hits: u8,
    ) -> Self {
        Self {
            id,
            mode: TemporalMode::RateLimit,
            trigger: Predicate::ANY,
            target,
            horizon_ticks,
            maximum_hits,
            correlate_subject: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum TemporalReason {
    ConsequentMissing,
    ForbiddenEvent,
    RateExceeded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TemporalViolation {
    pub rule_id: u32,
    pub reason: TemporalReason,
    pub tick: u64,
    pub subject: u64,
    pub evidence_kind: u16,
    pub observed: u8,
    pub limit: u8,
}

impl TemporalViolation {
    const EMPTY: Self = Self {
        rule_id: 0,
        reason: TemporalReason::ConsequentMissing,
        tick: 0,
        subject: 0,
        evidence_kind: 0,
        observed: 0,
        limit: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TemporalVerdict {
    pub violations: [TemporalViolation; MAXIMUM_VIOLATIONS_PER_EVENT],
    pub violation_count: usize,
    pub armed_rules: usize,
    pub satisfied_rules: usize,
}

impl TemporalVerdict {
    const fn new() -> Self {
        Self {
            violations: [TemporalViolation::EMPTY; MAXIMUM_VIOLATIONS_PER_EVENT],
            violation_count: 0,
            armed_rules: 0,
            satisfied_rules: 0,
        }
    }

    fn push(&mut self, violation: TemporalViolation) {
        if let Some(slot) = self.violations.get_mut(self.violation_count) {
            *slot = violation;
            self.violation_count += 1;
        }
    }

    pub fn as_slice(&self) -> &[TemporalViolation] {
        &self.violations[..self.violation_count]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuleStatistics {
    pub id: u32,
    pub armed: bool,
    pub satisfactions: u64,
    pub violations: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OracularError {
    ZeroCapacity,
    RuleCapacity,
    DuplicateRule,
    InvalidRule,
    RuleNotFound,
    TimeRegression,
}

#[derive(Clone, Copy)]
struct RuleRuntime {
    occupied: bool,
    rule: TemporalRule,
    armed: bool,
    deadline: u64,
    subject: u64,
    hit_ticks: [u64; MAXIMUM_RATE_SAMPLES],
    hit_count: usize,
    hit_cursor: usize,
    satisfactions: u64,
    violations: u64,
}

impl RuleRuntime {
    const EMPTY: Self = Self {
        occupied: false,
        rule: TemporalRule {
            id: 0,
            mode: TemporalMode::LeadsTo,
            trigger: Predicate::ANY,
            target: Predicate::ANY,
            horizon_ticks: 0,
            maximum_hits: 0,
            correlate_subject: false,
        },
        armed: false,
        deadline: 0,
        subject: 0,
        hit_ticks: [0; MAXIMUM_RATE_SAMPLES],
        hit_count: 0,
        hit_cursor: 0,
        satisfactions: 0,
        violations: 0,
    };

    fn subject_matches(&self, event: OracleEvent) -> bool {
        !self.rule.correlate_subject || self.subject == event.subject
    }

    fn remember_hit(&mut self, tick: u64) {
        self.hit_ticks[self.hit_cursor] = tick;
        self.hit_cursor = (self.hit_cursor + 1) % MAXIMUM_RATE_SAMPLES;
        self.hit_count = self.hit_count.saturating_add(1).min(MAXIMUM_RATE_SAMPLES);
    }

    fn recent_hits(&self, now: u64, horizon: u64) -> u8 {
        self.hit_ticks
            .iter()
            .take(self.hit_count)
            .filter(|&&tick| now.saturating_sub(tick) <= horizon)
            .count() as u8
    }
}

struct OracularState<const N: usize> {
    rules: [RuleRuntime; N],
    last_tick: u64,
    observed: u64,
}

impl<const N: usize> OracularState<N> {
    const fn new() -> Self {
        Self {
            rules: [RuleRuntime::EMPTY; N],
            last_tick: 0,
            observed: 0,
        }
    }
}

pub struct OracularMesh<const N: usize> {
    state: SpinLock<OracularState<N>>,
}

impl<const N: usize> OracularMesh<N> {
    pub const fn new() -> Self {
        Self {
            state: SpinLock::new(OracularState::new()),
        }
    }

    pub fn install(
        &self,
        rule: TemporalRule,
        _authority: &Capability<'_, PolicyControl>,
    ) -> Result<(), OracularError> {
        if N == 0 {
            return Err(OracularError::ZeroCapacity);
        }
        validate_rule(rule)?;

        let mut state = self.state.lock();
        if state
            .rules
            .iter()
            .any(|runtime| runtime.occupied && runtime.rule.id == rule.id)
        {
            return Err(OracularError::DuplicateRule);
        }

        let slot = state
            .rules
            .iter_mut()
            .find(|runtime| !runtime.occupied)
            .ok_or(OracularError::RuleCapacity)?;
        *slot = RuleRuntime {
            occupied: true,
            rule,
            ..RuleRuntime::EMPTY
        };
        Ok(())
    }

    pub fn remove(
        &self,
        rule_id: u32,
        _authority: &Capability<'_, PolicyControl>,
    ) -> Result<(), OracularError> {
        let mut state = self.state.lock();
        let slot = state
            .rules
            .iter_mut()
            .find(|runtime| runtime.occupied && runtime.rule.id == rule_id)
            .ok_or(OracularError::RuleNotFound)?;
        *slot = RuleRuntime::EMPTY;
        Ok(())
    }

    pub fn observe(&self, event: OracleEvent) -> Result<TemporalVerdict, OracularError> {
        let mut state = self.state.lock();
        if state.observed != 0 && event.tick < state.last_tick {
            return Err(OracularError::TimeRegression);
        }

        state.last_tick = event.tick;
        state.observed = state.observed.saturating_add(1);
        let mut verdict = TemporalVerdict::new();

        for runtime in state.rules.iter_mut().filter(|runtime| runtime.occupied) {
            match runtime.rule.mode {
                TemporalMode::LeadsTo => observe_leads_to(runtime, event, &mut verdict),
                TemporalMode::Inhibits => observe_inhibits(runtime, event, &mut verdict),
                TemporalMode::RateLimit => observe_rate(runtime, event, &mut verdict),
            }
        }

        verdict.armed_rules = state
            .rules
            .iter()
            .filter(|runtime| runtime.occupied && runtime.armed)
            .count();

        Ok(verdict)
    }

    pub fn statistics(&self, rule_id: u32) -> Result<RuleStatistics, OracularError> {
        let state = self.state.lock();
        let runtime = state
            .rules
            .iter()
            .find(|runtime| runtime.occupied && runtime.rule.id == rule_id)
            .ok_or(OracularError::RuleNotFound)?;

        Ok(RuleStatistics {
            id: runtime.rule.id,
            armed: runtime.armed,
            satisfactions: runtime.satisfactions,
            violations: runtime.violations,
        })
    }

    pub fn observed_events(&self) -> u64 {
        self.state.lock().observed
    }
}

impl<const N: usize> Default for OracularMesh<N> {
    fn default() -> Self {
        Self::new()
    }
}

fn observe_leads_to(runtime: &mut RuleRuntime, event: OracleEvent, verdict: &mut TemporalVerdict) {
    if runtime.armed && event.tick > runtime.deadline {
        runtime.violations = runtime.violations.saturating_add(1);
        verdict.push(TemporalViolation {
            rule_id: runtime.rule.id,
            reason: TemporalReason::ConsequentMissing,
            tick: event.tick,
            subject: runtime.subject,
            evidence_kind: event.kind,
            observed: 0,
            limit: 1,
        });
        runtime.armed = false;
    }

    if runtime.armed
        && runtime.rule.target.matches(event)
        && runtime.subject_matches(event)
        && event.tick <= runtime.deadline
    {
        runtime.armed = false;
        runtime.satisfactions = runtime.satisfactions.saturating_add(1);
        verdict.satisfied_rules = verdict.satisfied_rules.saturating_add(1);
    }

    if !runtime.armed && runtime.rule.trigger.matches(event) {
        runtime.armed = true;
        runtime.subject = event.subject;
        runtime.deadline = event.tick.saturating_add(runtime.rule.horizon_ticks);
    }
}

fn observe_inhibits(runtime: &mut RuleRuntime, event: OracleEvent, verdict: &mut TemporalVerdict) {
    if runtime.armed && event.tick > runtime.deadline {
        runtime.armed = false;
        runtime.satisfactions = runtime.satisfactions.saturating_add(1);
        verdict.satisfied_rules = verdict.satisfied_rules.saturating_add(1);
    }

    if runtime.armed
        && runtime.rule.target.matches(event)
        && runtime.subject_matches(event)
        && event.tick <= runtime.deadline
    {
        runtime.violations = runtime.violations.saturating_add(1);
        verdict.push(TemporalViolation {
            rule_id: runtime.rule.id,
            reason: TemporalReason::ForbiddenEvent,
            tick: event.tick,
            subject: event.subject,
            evidence_kind: event.kind,
            observed: 1,
            limit: 0,
        });
    }

    if runtime.rule.trigger.matches(event) {
        runtime.armed = true;
        runtime.subject = event.subject;
        runtime.deadline = event.tick.saturating_add(runtime.rule.horizon_ticks);
    }
}

fn observe_rate(runtime: &mut RuleRuntime, event: OracleEvent, verdict: &mut TemporalVerdict) {
    if !runtime.rule.target.matches(event) {
        return;
    }

    runtime.remember_hit(event.tick);
    let hits = runtime.recent_hits(event.tick, runtime.rule.horizon_ticks);

    if hits > runtime.rule.maximum_hits {
        runtime.violations = runtime.violations.saturating_add(1);
        verdict.push(TemporalViolation {
            rule_id: runtime.rule.id,
            reason: TemporalReason::RateExceeded,
            tick: event.tick,
            subject: event.subject,
            evidence_kind: event.kind,
            observed: hits,
            limit: runtime.rule.maximum_hits,
        });
    } else {
        runtime.satisfactions = runtime.satisfactions.saturating_add(1);
        verdict.satisfied_rules = verdict.satisfied_rules.saturating_add(1);
    }
}

fn validate_rule(rule: TemporalRule) -> Result<(), OracularError> {
    if rule.id == 0 || rule.horizon_ticks == 0 {
        return Err(OracularError::InvalidRule);
    }

    if matches!(rule.mode, TemporalMode::RateLimit)
        && (rule.maximum_hits == 0 || rule.maximum_hits as usize >= MAXIMUM_RATE_SAMPLES)
    {
        return Err(OracularError::InvalidRule);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Authority;

    #[test]
    fn detects_a_missing_consequent() {
        let authority = unsafe { Authority::assume_root() };
        let policy = authority.grant::<PolicyControl>();
        let mesh = OracularMesh::<4>::new();

        mesh.install(
            TemporalRule::leads_to(1, Predicate::kind(10), Predicate::kind(11), 5, true),
            &policy,
        )
        .unwrap();

        mesh.observe(OracleEvent {
            tick: 10,
            kind: 10,
            severity: 0,
            flags: 0,
            subject: 7,
            value: 0,
        })
        .unwrap();

        let verdict = mesh
            .observe(OracleEvent {
                tick: 16,
                kind: 99,
                severity: 0,
                flags: 0,
                subject: 7,
                value: 0,
            })
            .unwrap();

        assert_eq!(verdict.violation_count, 1);
        assert_eq!(
            verdict.violations[0].reason,
            TemporalReason::ConsequentMissing
        );
    }

    #[test]
    fn correlates_subjects_for_obligations() {
        let authority = unsafe { Authority::assume_root() };
        let policy = authority.grant::<PolicyControl>();
        let mesh = OracularMesh::<4>::new();

        mesh.install(
            TemporalRule::leads_to(1, Predicate::kind(10), Predicate::kind(11), 5, true),
            &policy,
        )
        .unwrap();

        mesh.observe(OracleEvent {
            tick: 10,
            kind: 10,
            severity: 0,
            flags: 0,
            subject: 7,
            value: 0,
        })
        .unwrap();

        let unrelated = mesh
            .observe(OracleEvent {
                tick: 11,
                kind: 11,
                severity: 0,
                flags: 0,
                subject: 8,
                value: 0,
            })
            .unwrap();
        assert_eq!(unrelated.satisfied_rules, 0);

        let related = mesh
            .observe(OracleEvent {
                tick: 12,
                kind: 11,
                severity: 0,
                flags: 0,
                subject: 7,
                value: 0,
            })
            .unwrap();
        assert_eq!(related.satisfied_rules, 1);
    }

    #[test]
    fn rate_limit_detects_bursts() {
        let authority = unsafe { Authority::assume_root() };
        let policy = authority.grant::<PolicyControl>();
        let mesh = OracularMesh::<2>::new();

        mesh.install(
            TemporalRule::rate_limit(9, Predicate::kind(3), 10, 2),
            &policy,
        )
        .unwrap();

        for tick in [1, 2] {
            let verdict = mesh
                .observe(OracleEvent {
                    tick,
                    kind: 3,
                    severity: 0,
                    flags: 0,
                    subject: 1,
                    value: 0,
                })
                .unwrap();
            assert_eq!(verdict.violation_count, 0);
        }

        let verdict = mesh
            .observe(OracleEvent {
                tick: 3,
                kind: 3,
                severity: 0,
                flags: 0,
                subject: 1,
                value: 0,
            })
            .unwrap();
        assert_eq!(verdict.violation_count, 1);
        assert_eq!(verdict.violations[0].reason, TemporalReason::RateExceeded);
    }
}
