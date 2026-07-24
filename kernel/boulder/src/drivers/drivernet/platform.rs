use crate::capability::{
    Capability, DeviceMemoryControl, DmaControl, FaultPolicyControl, PolicyControl,
};

use super::dispatch::{
    ActivationReceipt, BackendFault, DispatchStage, DriverLease, DriverNetBackend, FaultCode,
    HealthReceipt, ProbeObservation, ProbeTransaction,
};
use super::fingerprint::GpuFingerprint;
use super::model::{DriverStrategy, OracleDecision};
use super::registry::{MAXIMUM_SHIMS, ProbeStep, ShimDescriptor};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IsolationReceipt {
    pub token: u64,
    pub domain: u64,
    pub firmware_preserved: bool,
    pub root: u64,
}

impl IsolationReceipt {
    pub const fn valid(self) -> bool {
        self.token != 0 && self.root != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BrokerTransaction {
    pub token: u64,
    pub state: [u64; 8],
    pub root: u64,
}

impl BrokerTransaction {
    pub const fn valid(self) -> bool {
        self.token != 0 && self.root != 0
    }
}

pub trait DriverNetClock: Sync {
    fn now_tick(&self) -> u64;
}

pub trait DeviceIsolationBroker: Sync {
    fn begin(
        &self,
        fingerprint: &GpuFingerprint,
        descriptor: &ShimDescriptor,
        device: &Capability<'_, DeviceMemoryControl>,
        dma: &Capability<'_, DmaControl>,
    ) -> Result<IsolationReceipt, BackendFault>;

    fn commit(
        &self,
        receipt: IsolationReceipt,
        device: &Capability<'_, DeviceMemoryControl>,
        dma: &Capability<'_, DmaControl>,
    ) -> Result<(), BackendFault>;

    fn rollback(
        &self,
        receipt: IsolationReceipt,
        stage: DispatchStage,
        fault: BackendFault,
        device: &Capability<'_, DeviceMemoryControl>,
        dma: &Capability<'_, DmaControl>,
        fault_policy: &Capability<'_, FaultPolicyControl>,
    ) -> Result<(), BackendFault>;
}

pub trait StrategyBroker: Sync {
    fn strategy(&self) -> DriverStrategy;

    fn begin(
        &self,
        fingerprint: &GpuFingerprint,
        descriptor: &ShimDescriptor,
        isolation: IsolationReceipt,
        policy: &Capability<'_, PolicyControl>,
    ) -> Result<BrokerTransaction, BackendFault>;

    fn probe(
        &self,
        transaction: &mut BrokerTransaction,
        fingerprint: &GpuFingerprint,
        descriptor: &ShimDescriptor,
        step: ProbeStep,
        attempt: u8,
        device: &Capability<'_, DeviceMemoryControl>,
        dma: &Capability<'_, DmaControl>,
    ) -> Result<ProbeObservation, BackendFault>;

    fn activate(
        &self,
        transaction: &mut BrokerTransaction,
        fingerprint: &GpuFingerprint,
        descriptor: &ShimDescriptor,
        isolation: IsolationReceipt,
        device: &Capability<'_, DeviceMemoryControl>,
        dma: &Capability<'_, DmaControl>,
    ) -> Result<ActivationReceipt, BackendFault>;

    fn health(
        &self,
        transaction: &mut BrokerTransaction,
        activation: &ActivationReceipt,
        descriptor: &ShimDescriptor,
        fault_policy: &Capability<'_, FaultPolicyControl>,
    ) -> Result<HealthReceipt, BackendFault>;

    fn commit(
        &self,
        transaction: &mut BrokerTransaction,
        activation: &ActivationReceipt,
        health: &HealthReceipt,
        decision: &OracleDecision,
        policy: &Capability<'_, PolicyControl>,
    ) -> Result<(u64, u32), BackendFault>;

    fn rollback(
        &self,
        transaction: &mut BrokerTransaction,
        stage: DispatchStage,
        fault: BackendFault,
        device: &Capability<'_, DeviceMemoryControl>,
        dma: &Capability<'_, DmaControl>,
        fault_policy: &Capability<'_, FaultPolicyControl>,
    ) -> Result<(), BackendFault>;
}

pub struct DriverNetAuthority<'borrow, 'authority> {
    pub device: &'borrow Capability<'authority, DeviceMemoryControl>,
    pub dma: &'borrow Capability<'authority, DmaControl>,
    pub policy: &'borrow Capability<'authority, PolicyControl>,
    pub fault: &'borrow Capability<'authority, FaultPolicyControl>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlatformError {
    DuplicateBroker,
    MissingBroker,
    InvalidSecret,
}

pub struct BlackLabDriverPlatform<'borrow, 'authority> {
    clock: &'borrow dyn DriverNetClock,
    isolation: &'borrow dyn DeviceIsolationBroker,
    brokers: [&'borrow dyn StrategyBroker; MAXIMUM_SHIMS],
    authority: DriverNetAuthority<'borrow, 'authority>,
    secret: u64,
    next_transaction: u64,
    last_tick: u64,
}

impl<'borrow, 'authority> BlackLabDriverPlatform<'borrow, 'authority> {
    pub fn new(
        clock: &'borrow dyn DriverNetClock,
        isolation: &'borrow dyn DeviceIsolationBroker,
        brokers: [&'borrow dyn StrategyBroker; MAXIMUM_SHIMS],
        authority: DriverNetAuthority<'borrow, 'authority>,
        secret: u64,
    ) -> Result<Self, PlatformError> {
        if secret == 0 {
            return Err(PlatformError::InvalidSecret);
        }

        for left in 0..brokers.len() {
            for right in left + 1..brokers.len() {
                if brokers[left].strategy() == brokers[right].strategy() {
                    return Err(PlatformError::DuplicateBroker);
                }
            }
        }

        for strategy in DriverStrategy::ALL {
            if !brokers.iter().any(|broker| broker.strategy() == strategy) {
                return Err(PlatformError::MissingBroker);
            }
        }

        Ok(Self {
            clock,
            isolation,
            brokers,
            authority,
            secret,
            next_transaction: 1,
            last_tick: 0,
        })
    }

    fn broker(
        &self,
        strategy: DriverStrategy,
    ) -> Result<&'borrow dyn StrategyBroker, BackendFault> {
        self.brokers
            .iter()
            .copied()
            .find(|broker| broker.strategy() == strategy)
            .ok_or(BackendFault::new(
                FaultCode::RegistryFault,
                false,
                strategy.index() as u64,
            ))
    }

    fn decode_isolation(transaction: &ProbeTransaction) -> IsolationReceipt {
        IsolationReceipt {
            token: transaction.backend_state[0],
            domain: transaction.backend_state[1],
            firmware_preserved: transaction.firmware_preserved,
            root: transaction.backend_state[2],
        }
    }

    fn decode_broker(transaction: &ProbeTransaction) -> BrokerTransaction {
        BrokerTransaction {
            token: transaction.backend_state[3],
            state: [
                transaction.backend_state[4],
                transaction.backend_state[5],
                transaction.backend_state[6],
                transaction.backend_state[7],
                0,
                0,
                0,
                0,
            ],
            root: transaction.observation_root,
        }
    }

    fn encode_broker(transaction: &mut ProbeTransaction, broker: BrokerTransaction) {
        transaction.backend_state[3] = broker.token;
        transaction.backend_state[4..8].copy_from_slice(&broker.state[0..4]);
        transaction.observation_root = mix(transaction.observation_root, broker.root);
    }

    fn monotonic_tick(&mut self) -> Result<u64, BackendFault> {
        let now = self.clock.now_tick();
        if self.last_tick != 0 && now < self.last_tick {
            return Err(BackendFault::new(FaultCode::TimeRegression, false, now));
        }
        self.last_tick = now;
        Ok(now)
    }
}

impl DriverNetBackend for BlackLabDriverPlatform<'_, '_> {
    fn now_tick(&self) -> u64 {
        self.clock.now_tick()
    }

    fn begin(
        &mut self,
        fingerprint: &GpuFingerprint,
        descriptor: &ShimDescriptor,
        decision: &OracleDecision,
    ) -> Result<ProbeTransaction, BackendFault> {
        let now = self.monotonic_tick()?;
        let isolation = self.isolation.begin(
            fingerprint,
            descriptor,
            self.authority.device,
            self.authority.dma,
        )?;
        if !isolation.valid() {
            return Err(BackendFault::new(
                FaultCode::TransactionRejected,
                false,
                isolation.token,
            ));
        }

        let broker = self.broker(descriptor.strategy)?;
        let broker_transaction =
            match broker.begin(fingerprint, descriptor, isolation, self.authority.policy) {
                Ok(transaction) if transaction.valid() => transaction,
                Ok(transaction) => {
                    let fault =
                        BackendFault::new(FaultCode::TransactionRejected, false, transaction.token);
                    let _ = self.isolation.rollback(
                        isolation,
                        DispatchStage::BeginTransaction,
                        fault,
                        self.authority.device,
                        self.authority.dma,
                        self.authority.fault,
                    );
                    return Err(fault);
                }
                Err(fault) => {
                    let _ = self.isolation.rollback(
                        isolation,
                        DispatchStage::BeginTransaction,
                        fault,
                        self.authority.device,
                        self.authority.dma,
                        self.authority.fault,
                    );
                    return Err(fault);
                }
            };

        let sequence = self.next_transaction;
        self.next_transaction = self.next_transaction.wrapping_add(1).max(1);
        let token = mix(
            self.secret,
            sequence ^ fingerprint.evidence_root ^ decision.decision_root,
        )
        .max(1);

        Ok(ProbeTransaction {
            token,
            fingerprint_root: fingerprint.evidence_root,
            strategy: descriptor.strategy,
            started_tick: now,
            deadline_tick: now
                .saturating_add(descriptor.program.maximum_total_ticks)
                .saturating_add(descriptor.activation_budget_ticks)
                .saturating_add(descriptor.health_budget_ticks),
            evidence_mask: 0,
            observation_root: mix(decision.decision_root, broker_transaction.root),
            firmware_preserved: isolation.firmware_preserved,
            backend_state: [
                isolation.token,
                isolation.domain,
                isolation.root,
                broker_transaction.token,
                broker_transaction.state[0],
                broker_transaction.state[1],
                broker_transaction.state[2],
                broker_transaction.state[3],
            ],
        })
    }

    fn probe_step(
        &mut self,
        transaction: &mut ProbeTransaction,
        fingerprint: &GpuFingerprint,
        descriptor: &ShimDescriptor,
        step: ProbeStep,
        attempt: u8,
    ) -> Result<ProbeObservation, BackendFault> {
        let now = self.monotonic_tick()?;
        if now > transaction.deadline_tick {
            return Err(BackendFault::new(
                FaultCode::ProbeTimeout,
                false,
                transaction.deadline_tick,
            ));
        }

        let isolation = Self::decode_isolation(transaction);
        if !isolation.valid() {
            return Err(BackendFault::new(
                FaultCode::TransactionRejected,
                false,
                isolation.token,
            ));
        }

        let broker = self.broker(descriptor.strategy)?;
        let mut broker_transaction = Self::decode_broker(transaction);
        let observation = broker.probe(
            &mut broker_transaction,
            fingerprint,
            descriptor,
            step,
            attempt,
            self.authority.device,
            self.authority.dma,
        )?;
        Self::encode_broker(transaction, broker_transaction);
        Ok(observation)
    }

    fn activate(
        &mut self,
        transaction: &mut ProbeTransaction,
        fingerprint: &GpuFingerprint,
        descriptor: &ShimDescriptor,
    ) -> Result<ActivationReceipt, BackendFault> {
        let _ = self.monotonic_tick()?;
        let isolation = Self::decode_isolation(transaction);
        let broker = self.broker(descriptor.strategy)?;
        let mut broker_transaction = Self::decode_broker(transaction);
        let activation = broker.activate(
            &mut broker_transaction,
            fingerprint,
            descriptor,
            isolation,
            self.authority.device,
            self.authority.dma,
        )?;
        Self::encode_broker(transaction, broker_transaction);
        Ok(activation)
    }

    fn health_check(
        &mut self,
        transaction: &mut ProbeTransaction,
        activation: &ActivationReceipt,
        descriptor: &ShimDescriptor,
    ) -> Result<HealthReceipt, BackendFault> {
        let _ = self.monotonic_tick()?;
        let broker = self.broker(descriptor.strategy)?;
        let mut broker_transaction = Self::decode_broker(transaction);
        let health = broker.health(
            &mut broker_transaction,
            activation,
            descriptor,
            self.authority.fault,
        )?;
        Self::encode_broker(transaction, broker_transaction);
        Ok(health)
    }

    fn commit(
        &mut self,
        transaction: &mut ProbeTransaction,
        activation: &ActivationReceipt,
        health: &HealthReceipt,
        decision: &OracleDecision,
    ) -> Result<DriverLease, BackendFault> {
        let now = self.monotonic_tick()?;
        let isolation = Self::decode_isolation(transaction);
        let broker = self.broker(transaction.strategy)?;
        let mut broker_transaction = Self::decode_broker(transaction);

        let (handle, generation) = broker.commit(
            &mut broker_transaction,
            activation,
            health,
            decision,
            self.authority.policy,
        )?;
        Self::encode_broker(transaction, broker_transaction);
        if handle == 0 || generation == 0 {
            return Err(BackendFault::new(FaultCode::CommitFault, false, handle));
        }

        self.isolation
            .commit(isolation, self.authority.device, self.authority.dma)?;

        Ok(DriverLease {
            handle,
            generation,
            strategy: transaction.strategy,
            fingerprint_root: transaction.fingerprint_root,
            decision_root: decision.decision_root,
            activation_root: mix(activation.transport_root, activation.activation_epoch),
            health_root: health.root,
            framebuffer_object: activation.framebuffer_object,
            committed_tick: now.max(1),
        })
    }

    fn rollback(
        &mut self,
        transaction: &mut ProbeTransaction,
        stage: DispatchStage,
        fault: BackendFault,
    ) -> Result<(), BackendFault> {
        let isolation = Self::decode_isolation(transaction);
        let broker = self.broker(transaction.strategy)?;
        let mut broker_transaction = Self::decode_broker(transaction);

        let broker_result = broker.rollback(
            &mut broker_transaction,
            stage,
            fault,
            self.authority.device,
            self.authority.dma,
            self.authority.fault,
        );
        let isolation_result = self.isolation.rollback(
            isolation,
            stage,
            fault,
            self.authority.device,
            self.authority.dma,
            self.authority.fault,
        );
        Self::encode_broker(transaction, broker_transaction);

        match (broker_result, isolation_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), _) => Err(error),
            (_, Err(error)) => Err(error),
        }
    }
}

fn mix(mut state: u64, word: u64) -> u64 {
    state ^= word.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    state ^= state >> 30;
    state = state.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    state ^= state >> 27;
    state = state.wrapping_mul(0x94d0_49bb_1331_11eb);
    state ^ (state >> 31)
}
