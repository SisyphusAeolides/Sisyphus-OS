use crate::argus_sentinel::{ArgusAction, ArgusAssessment, ArgusSeverity};
use crate::capability::{Capability, FaultPolicyControl};
use crate::mnemosyne_ledger::LedgerSeal;
use crate::oracular_mesh::TemporalVerdict;
use crate::sync::SpinLock;

pub const MAXIMUM_PLAN_STEPS: usize = 8;

pub const VOTE_ANOMALY: u16 = 1 << 0;
pub const VOTE_TEMPORAL: u16 = 1 << 1;
pub const VOTE_LEDGER: u16 = 1 << 2;
pub const VOTE_FORECAST: u16 = 1 << 3;
pub const VOTE_SEVERE: u16 = 1 << 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum InterventionAction {
    IncreaseSampling = 1,
    FreezeSubject = 2,
    CaptureCheckpoint = 3,
    RevokeDma = 4,
    RevokeLeases = 5,
    Quarantine = 6,
    ResetDevice = 7,
    RetireResource = 8,
    IsolateControlPlane = 9,
    RotateLedgerEpoch = 10,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct PlanStep {
    pub action: InterventionAction,
    pub required_votes: u8,
    pub flags: u16,
    pub target: u64,
    pub argument: u64,
    pub deadline_tick: u64,
}

impl PlanStep {
    const EMPTY: Self = Self {
        action: InterventionAction::IncreaseSampling,
        required_votes: 0,
        flags: 0,
        target: 0,
        argument: 0,
        deadline_tick: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CassandraPolicy {
    pub forecast_horizon_ticks: u64,
    pub action_deadline_ticks: u64,
    pub quarantine_votes: u8,
    pub destructive_votes: u8,
    pub minimum_ledger_retention: usize,
}

impl CassandraPolicy {
    pub const BLACK_LAB: Self = Self {
        forecast_horizon_ticks: 4096,
        action_deadline_ticks: 256,
        quarantine_votes: 2,
        destructive_votes: 3,
        minimum_ledger_retention: 8,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CassandraInput {
    pub now_tick: u64,
    pub policy_epoch: u64,
    pub assessment: ArgusAssessment,
    pub temporal: TemporalVerdict,
    pub ledger: LedgerSeal,
    pub ledger_verified: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InterventionPlan {
    pub plan_id: u64,
    pub policy_epoch: u64,
    pub created_tick: u64,
    pub expires_tick: u64,
    pub target: u64,
    pub risk: u16,
    pub votes: u16,
    pub required_quorum: u8,
    pub step_count: usize,
    pub steps: [PlanStep; MAXIMUM_PLAN_STEPS],
    pub evidence_root: u64,
    pub plan_root: u64,
}

impl InterventionPlan {
    pub fn steps(&self) -> &[PlanStep] {
        &self.steps[..self.step_count]
    }

    pub fn verify(&self, secret: u64) -> bool {
        self.step_count <= MAXIMUM_PLAN_STEPS && self.plan_root == plan_root(secret, self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CassandraError {
    InvalidPolicy,
    InvalidInput,
    PlanCapacity,
    SequenceExhausted,
}

struct CassandraState {
    next_plan_id: u64,
    synthesized: u64,
    containment_plans: u64,
    destructive_plans: u64,
}

impl CassandraState {
    const fn new() -> Self {
        Self {
            next_plan_id: 1,
            synthesized: 0,
            containment_plans: 0,
            destructive_plans: 0,
        }
    }
}

pub struct CassandraReactor {
    secret: u64,
    policy: CassandraPolicy,
    state: SpinLock<CassandraState>,
}

impl CassandraReactor {
    pub const fn new(secret: u64, policy: CassandraPolicy) -> Self {
        Self {
            secret,
            policy,
            state: SpinLock::new(CassandraState::new()),
        }
    }

    pub fn synthesize(
        &self,
        input: CassandraInput,
        _authority: &Capability<'_, FaultPolicyControl>,
    ) -> Result<InterventionPlan, CassandraError> {
        validate_policy(self.policy)?;
        validate_input(input)?;

        let plan_id = {
            let mut state = self.state.lock();
            let id = state.next_plan_id;
            if id == 0 || id == u64::MAX {
                return Err(CassandraError::SequenceExhausted);
            }
            state.next_plan_id = id + 1;
            state.synthesized = state.synthesized.saturating_add(1);
            id
        };

        let forecast_imminent = input.assessment.forecast_tick.is_some_and(|tick| {
            tick <= input
                .now_tick
                .saturating_add(self.policy.forecast_horizon_ticks)
        });

        let temporal_fault = input.temporal.violation_count != 0;
        let ledger_fault =
            !input.ledger_verified || input.ledger.retained < self.policy.minimum_ledger_retention;
        let severe = matches!(
            input.assessment.severity,
            ArgusSeverity::Critical | ArgusSeverity::Terminal
        );
        let anomalous = !matches!(input.assessment.severity, ArgusSeverity::Stable);

        let mut votes = 0_u16;
        if anomalous {
            votes |= VOTE_ANOMALY;
        }
        if temporal_fault {
            votes |= VOTE_TEMPORAL;
        }
        if ledger_fault {
            votes |= VOTE_LEDGER;
        }
        if forecast_imminent {
            votes |= VOTE_FORECAST;
        }
        if severe {
            votes |= VOTE_SEVERE;
        }

        let vote_count = votes.count_ones() as u8;
        let required_quorum = if matches!(input.assessment.severity, ArgusSeverity::Terminal) {
            self.policy.destructive_votes
        } else {
            self.policy.quarantine_votes
        };

        let mut plan = InterventionPlan {
            plan_id,
            policy_epoch: input.policy_epoch,
            created_tick: input.now_tick,
            expires_tick: input
                .now_tick
                .saturating_add(self.policy.action_deadline_ticks),
            target: input.assessment.resource,
            risk: input.assessment.risk,
            votes,
            required_quorum,
            step_count: 0,
            steps: [PlanStep::EMPTY; MAXIMUM_PLAN_STEPS],
            evidence_root: evidence_root(self.secret, input),
            plan_root: 0,
        };

        if anomalous {
            push_step(
                &mut plan,
                PlanStep {
                    action: InterventionAction::IncreaseSampling,
                    required_votes: 1,
                    flags: 0,
                    target: input.assessment.resource,
                    argument: u64::from(input.assessment.risk),
                    deadline_tick: input
                        .now_tick
                        .saturating_add(self.policy.action_deadline_ticks),
                },
            )?;
        }

        if temporal_fault {
            push_step(
                &mut plan,
                PlanStep {
                    action: InterventionAction::FreezeSubject,
                    required_votes: 1,
                    flags: 0,
                    target: input.assessment.resource,
                    argument: input.temporal.violation_count as u64,
                    deadline_tick: input
                        .now_tick
                        .saturating_add(self.policy.action_deadline_ticks),
                },
            )?;
            push_step(
                &mut plan,
                PlanStep {
                    action: InterventionAction::RevokeLeases,
                    required_votes: self.policy.quarantine_votes,
                    flags: 0,
                    target: input.assessment.resource,
                    argument: input.policy_epoch,
                    deadline_tick: input
                        .now_tick
                        .saturating_add(self.policy.action_deadline_ticks),
                },
            )?;
        }

        if ledger_fault {
            push_step(
                &mut plan,
                PlanStep {
                    action: InterventionAction::IsolateControlPlane,
                    required_votes: 1,
                    flags: 0,
                    target: input.assessment.resource,
                    argument: input.ledger.chain_root,
                    deadline_tick: input
                        .now_tick
                        .saturating_add(self.policy.action_deadline_ticks),
                },
            )?;
            push_step(
                &mut plan,
                PlanStep {
                    action: InterventionAction::RotateLedgerEpoch,
                    required_votes: self.policy.quarantine_votes,
                    flags: 0,
                    target: input.assessment.resource,
                    argument: input.ledger.epoch.saturating_add(1),
                    deadline_tick: input
                        .now_tick
                        .saturating_add(self.policy.action_deadline_ticks),
                },
            )?;
        }

        if vote_count >= self.policy.quarantine_votes {
            push_step(
                &mut plan,
                PlanStep {
                    action: InterventionAction::CaptureCheckpoint,
                    required_votes: self.policy.quarantine_votes,
                    flags: 0,
                    target: input.assessment.resource,
                    argument: input.ledger.chain_root,
                    deadline_tick: input
                        .now_tick
                        .saturating_add(self.policy.action_deadline_ticks),
                },
            )?;
            push_step(
                &mut plan,
                PlanStep {
                    action: InterventionAction::Quarantine,
                    required_votes: self.policy.quarantine_votes,
                    flags: 0,
                    target: input.assessment.resource,
                    argument: u64::from(votes),
                    deadline_tick: input
                        .now_tick
                        .saturating_add(self.policy.action_deadline_ticks),
                },
            )?;
        }

        match input.assessment.action {
            ArgusAction::Observe | ArgusAction::IncreaseSampling => {}
            ArgusAction::Quarantine => {
                push_step(
                    &mut plan,
                    PlanStep {
                        action: InterventionAction::Quarantine,
                        required_votes: self.policy.quarantine_votes,
                        flags: 0,
                        target: input.assessment.resource,
                        argument: u64::from(input.assessment.risk),
                        deadline_tick: input
                            .now_tick
                            .saturating_add(self.policy.action_deadline_ticks),
                    },
                )?;
            }
            ArgusAction::RevokeDma => {
                push_step(
                    &mut plan,
                    PlanStep {
                        action: InterventionAction::RevokeDma,
                        required_votes: self.policy.quarantine_votes,
                        flags: 0,
                        target: input.assessment.resource,
                        argument: input.policy_epoch,
                        deadline_tick: input
                            .now_tick
                            .saturating_add(self.policy.action_deadline_ticks),
                    },
                )?;
            }
            ArgusAction::ResetDevice => {
                push_step(
                    &mut plan,
                    PlanStep {
                        action: InterventionAction::ResetDevice,
                        required_votes: self.policy.destructive_votes,
                        flags: 0,
                        target: input.assessment.resource,
                        argument: input.policy_epoch,
                        deadline_tick: input
                            .now_tick
                            .saturating_add(self.policy.action_deadline_ticks),
                    },
                )?;
            }
            ArgusAction::RetireResource => {
                push_step(
                    &mut plan,
                    PlanStep {
                        action: InterventionAction::RetireResource,
                        required_votes: self.policy.destructive_votes,
                        flags: 0,
                        target: input.assessment.resource,
                        argument: input.policy_epoch,
                        deadline_tick: input
                            .now_tick
                            .saturating_add(self.policy.action_deadline_ticks),
                    },
                )?;
            }
        }

        plan.plan_root = plan_root(self.secret, &plan);

        let destructive = plan.steps().iter().any(|step| {
            matches!(
                step.action,
                InterventionAction::ResetDevice | InterventionAction::RetireResource
            )
        });
        let containment = plan.steps().iter().any(|step| {
            matches!(
                step.action,
                InterventionAction::Quarantine
                    | InterventionAction::RevokeDma
                    | InterventionAction::IsolateControlPlane
            )
        });

        let mut state = self.state.lock();
        if containment {
            state.containment_plans = state.containment_plans.saturating_add(1);
        }
        if destructive {
            state.destructive_plans = state.destructive_plans.saturating_add(1);
        }

        Ok(plan)
    }

    pub fn totals(&self) -> (u64, u64, u64) {
        let state = self.state.lock();
        (
            state.synthesized,
            state.containment_plans,
            state.destructive_plans,
        )
    }
}

fn push_step(plan: &mut InterventionPlan, step: PlanStep) -> Result<(), CassandraError> {
    if plan.steps[..plan.step_count]
        .iter()
        .any(|existing| existing.action == step.action && existing.target == step.target)
    {
        return Ok(());
    }

    let slot = plan
        .steps
        .get_mut(plan.step_count)
        .ok_or(CassandraError::PlanCapacity)?;
    *slot = step;
    plan.step_count += 1;
    Ok(())
}

fn validate_policy(policy: CassandraPolicy) -> Result<(), CassandraError> {
    if policy.forecast_horizon_ticks == 0
        || policy.action_deadline_ticks == 0
        || policy.quarantine_votes == 0
        || policy.destructive_votes < policy.quarantine_votes
        || policy.destructive_votes > 5
    {
        return Err(CassandraError::InvalidPolicy);
    }
    Ok(())
}

fn validate_input(input: CassandraInput) -> Result<(), CassandraError> {
    if input.policy_epoch == 0
        || input.assessment.resource == 0
        || input.assessment.tick > input.now_tick
        || input.temporal.violation_count > input.temporal.violations.len()
    {
        return Err(CassandraError::InvalidInput);
    }
    Ok(())
}

fn evidence_root(secret: u64, input: CassandraInput) -> u64 {
    let mut state = mix(secret, input.now_tick);
    state = mix(state, input.policy_epoch);
    state = mix(state, input.assessment.resource);
    state = mix(state, u64::from(input.assessment.risk));
    state = mix(state, input.assessment.tick);
    state = mix(state, input.temporal.violation_count as u64);
    for violation in input.temporal.as_slice() {
        state = mix(state, u64::from(violation.rule_id));
        state = mix(state, violation.tick);
        state = mix(state, violation.subject);
        state = mix(state, u64::from(violation.evidence_kind));
    }
    state = mix(state, input.ledger.epoch);
    state = mix(state, input.ledger.chain_root);
    state = mix(state, input.ledger.retained as u64);
    mix(state, if input.ledger_verified { 1 } else { 0 })
}

fn plan_root(secret: u64, plan: &InterventionPlan) -> u64 {
    let mut state = mix(secret, plan.plan_id);
    state = mix(state, plan.policy_epoch);
    state = mix(state, plan.created_tick);
    state = mix(state, plan.expires_tick);
    state = mix(state, plan.target);
    state = mix(state, u64::from(plan.risk));
    state = mix(state, u64::from(plan.votes));
    state = mix(state, u64::from(plan.required_quorum));
    state = mix(state, plan.evidence_root);
    state = mix(state, plan.step_count as u64);

    for step in plan.steps() {
        state = mix(state, step.action as u8 as u64);
        state = mix(state, u64::from(step.required_votes));
        state = mix(state, u64::from(step.flags));
        state = mix(state, step.target);
        state = mix(state, step.argument);
        state = mix(state, step.deadline_tick);
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
    use crate::argus_sentinel::{ArgusAction, ArgusAssessment, ArgusSeverity};
    use crate::capability::Authority;
    use crate::mnemosyne_ledger::LedgerSeal;
    use crate::oracular_mesh::TemporalVerdict;

    fn assessment() -> ArgusAssessment {
        ArgusAssessment {
            resource: 7,
            tick: 100,
            severity: ArgusSeverity::Critical,
            action: ArgusAction::RevokeDma,
            risk: 760,
            anomaly_q16: 2 << 16,
            trend_q16: 1 << 16,
            thermal_margin_q16: 2 << 16,
            cusum_q16: 4 << 16,
            forecast_tick: Some(120),
            sample_count: 32,
        }
    }

    #[test]
    fn synthesizes_a_sealed_containment_plan() {
        let authority = unsafe { Authority::assume_root() };
        let fault = authority.grant::<FaultPolicyControl>();
        let reactor = CassandraReactor::new(0x1234, CassandraPolicy::BLACK_LAB);

        let plan = reactor
            .synthesize(
                CassandraInput {
                    now_tick: 100,
                    policy_epoch: 9,
                    assessment: assessment(),
                    temporal: TemporalVerdict {
                        violations: [crate::oracular_mesh::TemporalViolation {
                            rule_id: 1,
                            reason: crate::oracular_mesh::TemporalReason::ForbiddenEvent,
                            tick: 100,
                            subject: 7,
                            evidence_kind: 3,
                            observed: 1,
                            limit: 0,
                        };
                            crate::oracular_mesh::MAXIMUM_VIOLATIONS_PER_EVENT],
                        violation_count: 1,
                        armed_rules: 0,
                        satisfied_rules: 0,
                    },
                    ledger: LedgerSeal {
                        epoch: 3,
                        retained: 32,
                        overwritten: 0,
                        first_sequence: 1,
                        last_sequence: 32,
                        anchor_root: 1,
                        chain_root: 2,
                    },
                    ledger_verified: true,
                },
                &fault,
            )
            .unwrap();

        assert!(plan.verify(0x1234));
        assert!(
            plan.steps()
                .iter()
                .any(|step| step.action == InterventionAction::RevokeDma)
        );
        assert!(
            plan.steps()
                .iter()
                .any(|step| step.action == InterventionAction::Quarantine)
        );
    }
}
