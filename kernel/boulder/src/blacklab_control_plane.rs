use crate::argus_sentinel::{
    ArgusAssessment, ArgusError, ArgusSentinel, ArgusSeverity, TelemetrySample,
};
use crate::capability::{Capability, FaultPolicyControl};
use crate::cassandra_reactor::{
    CassandraError, CassandraInput, CassandraReactor, InterventionPlan,
};
use crate::mnemosyne_ledger::{LedgerError, LedgerEvent, LedgerEventKind, MnemosyneLedger};
use crate::oracular_mesh::{OracleEvent, OracularError, OracularMesh, TemporalVerdict};

pub const ORACLE_KIND_TELEMETRY: u16 = 0x1001;
pub const ORACLE_KIND_INTERVENTION: u16 = 0x1002;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlOutcome {
    pub assessment: ArgusAssessment,
    pub temporal: TemporalVerdict,
    pub plan: InterventionPlan,
    pub ledger_verified: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlPlaneError {
    Argus(ArgusError),
    Oracular(OracularError),
    Ledger(LedgerError),
    Cassandra(CassandraError),
}

impl From<ArgusError> for ControlPlaneError {
    fn from(error: ArgusError) -> Self {
        Self::Argus(error)
    }
}

impl From<OracularError> for ControlPlaneError {
    fn from(error: OracularError) -> Self {
        Self::Oracular(error)
    }
}

impl From<LedgerError> for ControlPlaneError {
    fn from(error: LedgerError) -> Self {
        Self::Ledger(error)
    }
}

impl From<CassandraError> for ControlPlaneError {
    fn from(error: CassandraError) -> Self {
        Self::Cassandra(error)
    }
}

pub struct BlackLabControlPlane<'a, const SENSORS: usize, const RULES: usize, const LEDGER: usize> {
    argus: &'a ArgusSentinel<SENSORS>,
    oracular: &'a OracularMesh<RULES>,
    mnemosyne: &'a MnemosyneLedger<LEDGER>,
    cassandra: &'a CassandraReactor,
}

impl<'a, const SENSORS: usize, const RULES: usize, const LEDGER: usize>
    BlackLabControlPlane<'a, SENSORS, RULES, LEDGER>
{
    pub const fn new(
        argus: &'a ArgusSentinel<SENSORS>,
        oracular: &'a OracularMesh<RULES>,
        mnemosyne: &'a MnemosyneLedger<LEDGER>,
        cassandra: &'a CassandraReactor,
    ) -> Self {
        Self {
            argus,
            oracular,
            mnemosyne,
            cassandra,
        }
    }

    pub fn ingest(
        &self,
        sample: TelemetrySample,
        policy_epoch: u64,
        authority: &Capability<'_, FaultPolicyControl>,
    ) -> Result<ControlOutcome, ControlPlaneError> {
        let assessment = self.argus.observe(sample)?;
        let oracle_event = assessment_event(assessment);
        let temporal = self.oracular.observe(oracle_event)?;

        self.mnemosyne.append(
            LedgerEvent {
                tick: sample.tick,
                subject: sample.resource,
                data0: packed_assessment(assessment),
                data1: assessment.forecast_tick.unwrap_or(0),
                kind: LedgerEventKind::Observation,
                severity: severity_code(assessment.severity),
                flags: action_flags(assessment.action) as u8,
            },
            authority,
        )?;

        for violation in temporal.as_slice() {
            self.mnemosyne.append(
                LedgerEvent {
                    tick: violation.tick,
                    subject: violation.subject,
                    data0: (u64::from(violation.rule_id) << 32)
                        | u64::from(violation.evidence_kind),
                    data1: (u64::from(violation.observed) << 8) | u64::from(violation.limit),
                    kind: LedgerEventKind::TemporalViolation,
                    severity: 4,
                    flags: violation.reason as u8,
                },
                authority,
            )?;
        }

        let ledger_verified = self.mnemosyne.verify().is_ok();
        let ledger = self.mnemosyne.seal();

        let plan = self.cassandra.synthesize(
            CassandraInput {
                now_tick: sample.tick,
                policy_epoch,
                assessment,
                temporal,
                ledger,
                ledger_verified,
            },
            authority,
        )?;

        self.mnemosyne.append(
            LedgerEvent {
                tick: sample.tick,
                subject: sample.resource,
                data0: plan.plan_root,
                data1: (u64::from(plan.votes) << 32) | plan.step_count as u64,
                kind: LedgerEventKind::PolicyDecision,
                severity: severity_code(assessment.severity),
                flags: plan.required_quorum,
            },
            authority,
        )?;

        let _ = self.oracular.observe(OracleEvent {
            tick: sample.tick,
            kind: ORACLE_KIND_INTERVENTION,
            severity: severity_code(assessment.severity),
            flags: plan.votes,
            subject: sample.resource,
            value: plan.plan_root,
        })?;

        Ok(ControlOutcome {
            assessment,
            temporal,
            plan,
            ledger_verified,
        })
    }
}

fn assessment_event(assessment: ArgusAssessment) -> OracleEvent {
    OracleEvent {
        tick: assessment.tick,
        kind: ORACLE_KIND_TELEMETRY,
        severity: severity_code(assessment.severity),
        flags: action_flags(assessment.action) as u16,
        subject: assessment.resource,
        value: packed_assessment(assessment),
    }
}

fn packed_assessment(assessment: ArgusAssessment) -> u64 {
    (u64::from(assessment.risk) << 48)
        | (u64::from(assessment.anomaly_q16) << 16)
        | u64::from(action_flags(assessment.action))
}

fn severity_code(severity: ArgusSeverity) -> u8 {
    match severity {
        ArgusSeverity::Stable => 0,
        ArgusSeverity::Watch => 1,
        ArgusSeverity::Degraded => 2,
        ArgusSeverity::Critical => 3,
        ArgusSeverity::Terminal => 4,
    }
}

fn action_flags(action: crate::argus_sentinel::ArgusAction) -> u16 {
    match action {
        crate::argus_sentinel::ArgusAction::Observe => 1 << 0,
        crate::argus_sentinel::ArgusAction::IncreaseSampling => 1 << 1,
        crate::argus_sentinel::ArgusAction::Quarantine => 1 << 2,
        crate::argus_sentinel::ArgusAction::RevokeDma => 1 << 3,
        crate::argus_sentinel::ArgusAction::ResetDevice => 1 << 4,
        crate::argus_sentinel::ArgusAction::RetireResource => 1 << 5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::argus_sentinel::{ArgusPolicy, ArgusSentinel};
    use crate::capability::Authority;
    use crate::cassandra_reactor::{CassandraPolicy, CassandraReactor};
    use crate::mnemosyne_ledger::MnemosyneLedger;
    use crate::oracular_mesh::OracularMesh;

    #[test]
    fn telemetry_becomes_a_sealed_plan_and_ledger_record() {
        let authority = unsafe { Authority::assume_root() };
        let fault = authority.grant::<FaultPolicyControl>();

        let argus = ArgusSentinel::<8>::new(ArgusPolicy::BLACK_LAB);
        let oracular = OracularMesh::<8>::new();
        let ledger = MnemosyneLedger::<32>::new(0x1111, 1);
        let cassandra_policy = CassandraPolicy {
            minimum_ledger_retention: 0,
            ..CassandraPolicy::BLACK_LAB
        };
        let cassandra = CassandraReactor::new(0x2222, cassandra_policy);
        let plane = BlackLabControlPlane::new(&argus, &oracular, &ledger, &cassandra);

        let outcome = plane
            .ingest(
                TelemetrySample {
                    tick: 1,
                    resource: 7,
                    signal_q16: 1 << 16,
                    temperature_q16: 40 << 16,
                    pressure_q16: 1 << 16,
                    corrections: 0,
                    faults: 0,
                },
                1,
                &fault,
            )
            .unwrap();

        assert!(outcome.plan.verify(0x2222));
        assert!(ledger.seal().retained >= 2);
    }
}
