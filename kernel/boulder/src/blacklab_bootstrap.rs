use crate::argus_sentinel::{ArgusPolicy, ArgusSentinel};
use crate::blacklab_control_plane::{
    BlackLabControlPlane, ORACLE_KIND_INTERVENTION, ORACLE_KIND_TELEMETRY,
};
use crate::capability::{Capability, PolicyControl};
use crate::cassandra_reactor::{CassandraPolicy, CassandraReactor};
use crate::charybdis_dma_firewall::CharybdisDmaFirewall;
use crate::mnemosyne_ledger::MnemosyneLedger;
use crate::oracular_mesh::{OracularError, OracularMesh, Predicate, TemporalRule};

pub const RULE_CRITICAL_TELEMETRY_REQUIRES_PLAN: u32 = 0x424c_0001;
pub const RULE_INTERVENTION_RATE_LIMIT: u32 = 0x424c_0002;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlackLabSeeds {
    pub ledger_secret: u64,
    pub plan_secret: u64,
    pub dma_secret: u64,
    pub ledger_epoch: u64,
}

impl BlackLabSeeds {
    pub const fn valid(self) -> bool {
        self.ledger_secret != 0
            && self.plan_secret != 0
            && self.dma_secret != 0
            && self.ledger_epoch != 0
            && self.ledger_secret != self.plan_secret
            && self.plan_secret != self.dma_secret
            && self.ledger_secret != self.dma_secret
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlackLabBootstrapError {
    InvalidSeeds,
    Rule(OracularError),
}

impl From<OracularError> for BlackLabBootstrapError {
    fn from(error: OracularError) -> Self {
        Self::Rule(error)
    }
}

pub struct BlackLabComplex<
    const SENSORS: usize,
    const RULES: usize,
    const LEDGER: usize,
    const APERTURES: usize,
    const MAPPINGS: usize,
> {
    pub argus: ArgusSentinel<SENSORS>,
    pub oracular: OracularMesh<RULES>,
    pub mnemosyne: MnemosyneLedger<LEDGER>,
    pub cassandra: CassandraReactor,
    pub charybdis: CharybdisDmaFirewall<APERTURES, MAPPINGS>,
}

impl<
    const SENSORS: usize,
    const RULES: usize,
    const LEDGER: usize,
    const APERTURES: usize,
    const MAPPINGS: usize,
> BlackLabComplex<SENSORS, RULES, LEDGER, APERTURES, MAPPINGS>
{
    pub fn new(seeds: BlackLabSeeds) -> Result<Self, BlackLabBootstrapError> {
        if !seeds.valid() {
            return Err(BlackLabBootstrapError::InvalidSeeds);
        }

        Ok(Self {
            argus: ArgusSentinel::new(ArgusPolicy::BLACK_LAB),
            oracular: OracularMesh::new(),
            mnemosyne: MnemosyneLedger::new(seeds.ledger_secret, seeds.ledger_epoch),
            cassandra: CassandraReactor::new(seeds.plan_secret, CassandraPolicy::BLACK_LAB),
            charybdis: CharybdisDmaFirewall::new(seeds.dma_secret),
        })
    }

    pub fn install_default_rules(
        &self,
        authority: &Capability<'_, PolicyControl>,
    ) -> Result<(), BlackLabBootstrapError> {
        let critical_telemetry = Predicate {
            kind: ORACLE_KIND_TELEMETRY,
            minimum_severity: 3,
            required_flags: 0,
            subject_mask: 0,
            subject_value: 0,
            value_mask: 0,
            value_value: 0,
        };

        self.oracular.install(
            TemporalRule::leads_to(
                RULE_CRITICAL_TELEMETRY_REQUIRES_PLAN,
                critical_telemetry,
                Predicate::kind(ORACLE_KIND_INTERVENTION),
                1,
                true,
            ),
            authority,
        )?;

        self.oracular.install(
            TemporalRule::rate_limit(
                RULE_INTERVENTION_RATE_LIMIT,
                Predicate::kind(ORACLE_KIND_INTERVENTION),
                1024,
                4,
            ),
            authority,
        )?;

        Ok(())
    }

    pub const fn control_plane(&self) -> BlackLabControlPlane<'_, SENSORS, RULES, LEDGER> {
        BlackLabControlPlane::new(
            &self.argus,
            &self.oracular,
            &self.mnemosyne,
            &self.cassandra,
        )
    }
}

pub type KernelBlackLabComplex = BlackLabComplex<256, 64, 2048, 64, 512>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Authority;

    #[test]
    fn materializes_the_complete_bounded_complex() {
        let authority = unsafe { Authority::assume_root() };
        let policy = authority.grant::<PolicyControl>();

        let complex = BlackLabComplex::<8, 8, 32, 4, 16>::new(BlackLabSeeds {
            ledger_secret: 1,
            plan_secret: 2,
            dma_secret: 3,
            ledger_epoch: 1,
        })
        .unwrap();

        complex.install_default_rules(&policy).unwrap();
        assert_eq!(complex.oracular.observed_events(), 0);
    }
}
