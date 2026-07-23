use crate::capability::{
    Capability, DeviceMemoryControl, DmaControl, FaultPolicyControl, PolicyControl,
};

use super::dispatch::{
    ActivationReceipt, BackendFault, DispatchStage, FaultCode, HealthReceipt, ProbeObservation,
};
use super::fingerprint::{FirmwareFramebufferEvidence, GpuFingerprint};
use super::model::{DriverStrategy, OracleDecision};
use super::platform::{BrokerTransaction, IsolationReceipt, StrategyBroker};
use super::registry::{
    PROBE_EVIDENCE_FIRMWARE_LEASE, PROBE_EVIDENCE_HEALTH, ProbeSemantic, ProbeStep, ShimDescriptor,
};

pub trait NativeStrategyHost: Sync {
    fn begin(
        &self,
        strategy: DriverStrategy,
        fingerprint: &GpuFingerprint,
        descriptor: &ShimDescriptor,
        isolation: IsolationReceipt,
        policy: &Capability<'_, PolicyControl>,
    ) -> Result<BrokerTransaction, BackendFault>;

    fn probe(
        &self,
        strategy: DriverStrategy,
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
        strategy: DriverStrategy,
        transaction: &mut BrokerTransaction,
        fingerprint: &GpuFingerprint,
        descriptor: &ShimDescriptor,
        isolation: IsolationReceipt,
        device: &Capability<'_, DeviceMemoryControl>,
        dma: &Capability<'_, DmaControl>,
    ) -> Result<ActivationReceipt, BackendFault>;

    fn health(
        &self,
        strategy: DriverStrategy,
        transaction: &mut BrokerTransaction,
        activation: &ActivationReceipt,
        descriptor: &ShimDescriptor,
        fault_policy: &Capability<'_, FaultPolicyControl>,
    ) -> Result<HealthReceipt, BackendFault>;

    fn commit(
        &self,
        strategy: DriverStrategy,
        transaction: &mut BrokerTransaction,
        activation: &ActivationReceipt,
        health: &HealthReceipt,
        decision: &OracleDecision,
        policy: &Capability<'_, PolicyControl>,
    ) -> Result<(u64, u32), BackendFault>;

    fn rollback(
        &self,
        strategy: DriverStrategy,
        transaction: &mut BrokerTransaction,
        stage: DispatchStage,
        fault: BackendFault,
        device: &Capability<'_, DeviceMemoryControl>,
        dma: &Capability<'_, DmaControl>,
        fault_policy: &Capability<'_, FaultPolicyControl>,
    ) -> Result<(), BackendFault>;
}

pub struct NativeBrokerAdapter<'a> {
    strategy: DriverStrategy,
    host: &'a dyn NativeStrategyHost,
}

impl<'a> NativeBrokerAdapter<'a> {
    pub fn new(
        strategy: DriverStrategy,
        host: &'a dyn NativeStrategyHost,
    ) -> Result<Self, BackendFault> {
        if !strategy.native() {
            return Err(BackendFault::new(
                FaultCode::RegistryFault,
                false,
                strategy.index() as u64,
            ));
        }
        Ok(Self { strategy, host })
    }
}

impl StrategyBroker for NativeBrokerAdapter<'_> {
    fn strategy(&self) -> DriverStrategy {
        self.strategy
    }

    fn begin(
        &self,
        fingerprint: &GpuFingerprint,
        descriptor: &ShimDescriptor,
        isolation: IsolationReceipt,
        policy: &Capability<'_, PolicyControl>,
    ) -> Result<BrokerTransaction, BackendFault> {
        self.host
            .begin(self.strategy, fingerprint, descriptor, isolation, policy)
    }

    fn probe(
        &self,
        transaction: &mut BrokerTransaction,
        fingerprint: &GpuFingerprint,
        descriptor: &ShimDescriptor,
        step: ProbeStep,
        attempt: u8,
        device: &Capability<'_, DeviceMemoryControl>,
        dma: &Capability<'_, DmaControl>,
    ) -> Result<ProbeObservation, BackendFault> {
        self.host.probe(
            self.strategy,
            transaction,
            fingerprint,
            descriptor,
            step,
            attempt,
            device,
            dma,
        )
    }

    fn activate(
        &self,
        transaction: &mut BrokerTransaction,
        fingerprint: &GpuFingerprint,
        descriptor: &ShimDescriptor,
        isolation: IsolationReceipt,
        device: &Capability<'_, DeviceMemoryControl>,
        dma: &Capability<'_, DmaControl>,
    ) -> Result<ActivationReceipt, BackendFault> {
        self.host.activate(
            self.strategy,
            transaction,
            fingerprint,
            descriptor,
            isolation,
            device,
            dma,
        )
    }

    fn health(
        &self,
        transaction: &mut BrokerTransaction,
        activation: &ActivationReceipt,
        descriptor: &ShimDescriptor,
        fault_policy: &Capability<'_, FaultPolicyControl>,
    ) -> Result<HealthReceipt, BackendFault> {
        self.host.health(
            self.strategy,
            transaction,
            activation,
            descriptor,
            fault_policy,
        )
    }

    fn commit(
        &self,
        transaction: &mut BrokerTransaction,
        activation: &ActivationReceipt,
        health: &HealthReceipt,
        decision: &OracleDecision,
        policy: &Capability<'_, PolicyControl>,
    ) -> Result<(u64, u32), BackendFault> {
        self.host.commit(
            self.strategy,
            transaction,
            activation,
            health,
            decision,
            policy,
        )
    }

    fn rollback(
        &self,
        transaction: &mut BrokerTransaction,
        stage: DispatchStage,
        fault: BackendFault,
        device: &Capability<'_, DeviceMemoryControl>,
        dma: &Capability<'_, DmaControl>,
        fault_policy: &Capability<'_, FaultPolicyControl>,
    ) -> Result<(), BackendFault> {
        self.host.rollback(
            self.strategy,
            transaction,
            stage,
            fault,
            device,
            dma,
            fault_policy,
        )
    }
}

pub trait FirmwareFramebufferHost: Sync {
    fn now_tick(&self) -> u64;

    fn inspect(&self, evidence: FirmwareFramebufferEvidence) -> Result<(u64, u64), BackendFault>;

    fn retain(
        &self,
        object: u64,
        policy: &Capability<'_, PolicyControl>,
    ) -> Result<(u64, u32), BackendFault>;

    fn release(
        &self,
        object: u64,
        fault_policy: &Capability<'_, FaultPolicyControl>,
    ) -> Result<(), BackendFault>;
}

pub struct FirmwareFramebufferBroker<'a> {
    host: &'a dyn FirmwareFramebufferHost,
}

impl<'a> FirmwareFramebufferBroker<'a> {
    pub const fn new(host: &'a dyn FirmwareFramebufferHost) -> Self {
        Self { host }
    }
}

impl StrategyBroker for FirmwareFramebufferBroker<'_> {
    fn strategy(&self) -> DriverStrategy {
        DriverStrategy::FirmwareFramebuffer
    }

    fn begin(
        &self,
        fingerprint: &GpuFingerprint,
        _descriptor: &ShimDescriptor,
        _isolation: IsolationReceipt,
        _policy: &Capability<'_, PolicyControl>,
    ) -> Result<BrokerTransaction, BackendFault> {
        if !fingerprint.firmware_display_usable() {
            return Err(BackendFault::new(
                FaultCode::TransactionRejected,
                false,
                fingerprint.evidence_root,
            ));
        }

        let (object, root) = self.host.inspect(fingerprint.firmware_framebuffer)?;
        if object == 0 || root == 0 {
            return Err(BackendFault::new(
                FaultCode::TransactionRejected,
                false,
                object,
            ));
        }

        Ok(BrokerTransaction {
            token: object,
            state: [
                object,
                fingerprint.firmware_framebuffer.byte_length,
                u64::from(fingerprint.firmware_framebuffer.width),
                u64::from(fingerprint.firmware_framebuffer.height),
                0,
                0,
                0,
                0,
            ],
            root,
        })
    }

    fn probe(
        &self,
        transaction: &mut BrokerTransaction,
        fingerprint: &GpuFingerprint,
        _descriptor: &ShimDescriptor,
        step: ProbeStep,
        _attempt: u8,
        _device: &Capability<'_, DeviceMemoryControl>,
        _dma: &Capability<'_, DmaControl>,
    ) -> Result<ProbeObservation, BackendFault> {
        let evidence_bit = match step.semantic {
            ProbeSemantic::VerifyFirmwareLease => PROBE_EVIDENCE_FIRMWARE_LEASE,
            ProbeSemantic::EstablishHealthBaseline => PROBE_EVIDENCE_HEALTH,
            _ => {
                return Err(BackendFault::new(
                    FaultCode::ProbeFault,
                    false,
                    step.semantic as u8 as u64,
                ));
            }
        };

        let tick = self.host.now_tick();
        let root = mix(
            transaction.root,
            fingerprint.evidence_root ^ u64::from(evidence_bit) ^ tick,
        );
        transaction.root = root;

        Ok(ProbeObservation {
            semantic: step.semantic,
            evidence_bit,
            value: transaction.token,
            tick,
            root,
        })
    }

    fn activate(
        &self,
        transaction: &mut BrokerTransaction,
        fingerprint: &GpuFingerprint,
        _descriptor: &ShimDescriptor,
        _isolation: IsolationReceipt,
        _device: &Capability<'_, DeviceMemoryControl>,
        _dma: &Capability<'_, DmaControl>,
    ) -> Result<ActivationReceipt, BackendFault> {
        Ok(ActivationReceipt {
            token: transaction.token,
            strategy: DriverStrategy::FirmwareFramebuffer,
            activation_epoch: self.host.now_tick().max(1),
            transport_root: mix(transaction.root, fingerprint.evidence_root),
            framebuffer_object: transaction.token,
            backend_state: transaction.state,
        })
    }

    fn health(
        &self,
        transaction: &mut BrokerTransaction,
        activation: &ActivationReceipt,
        _descriptor: &ShimDescriptor,
        _fault_policy: &Capability<'_, FaultPolicyControl>,
    ) -> Result<HealthReceipt, BackendFault> {
        let tick = self.host.now_tick();
        Ok(HealthReceipt {
            healthy: activation.framebuffer_object != 0,
            tick,
            heartbeat: activation.activation_epoch,
            fault_mask: 0,
            root: mix(transaction.root, tick),
        })
    }

    fn commit(
        &self,
        _transaction: &mut BrokerTransaction,
        activation: &ActivationReceipt,
        _health: &HealthReceipt,
        _decision: &OracleDecision,
        policy: &Capability<'_, PolicyControl>,
    ) -> Result<(u64, u32), BackendFault> {
        let retained = self.host.retain(activation.framebuffer_object, policy)?;
        if retained.0 != activation.framebuffer_object {
            return Err(BackendFault::new(FaultCode::CommitFault, false, retained.0));
        }
        Ok(retained)
    }

    fn rollback(
        &self,
        transaction: &mut BrokerTransaction,
        _stage: DispatchStage,
        _fault: BackendFault,
        _device: &Capability<'_, DeviceMemoryControl>,
        _dma: &Capability<'_, DmaControl>,
        fault_policy: &Capability<'_, FaultPolicyControl>,
    ) -> Result<(), BackendFault> {
        self.host.release(transaction.token, fault_policy)
    }
}

pub struct QuarantineBroker {
    secret: u64,
}

impl QuarantineBroker {
    pub fn new(secret: u64) -> Result<Self, BackendFault> {
        if secret == 0 {
            return Err(BackendFault::new(FaultCode::RegistryFault, false, 0));
        }
        Ok(Self { secret })
    }
}

impl StrategyBroker for QuarantineBroker {
    fn strategy(&self) -> DriverStrategy {
        DriverStrategy::Quarantine
    }

    fn begin(
        &self,
        fingerprint: &GpuFingerprint,
        _descriptor: &ShimDescriptor,
        _isolation: IsolationReceipt,
        _policy: &Capability<'_, PolicyControl>,
    ) -> Result<BrokerTransaction, BackendFault> {
        let token = mix(self.secret, fingerprint.evidence_root).max(1);
        Ok(BrokerTransaction {
            token,
            state: [fingerprint.evidence_root, 0, 0, 0, 0, 0, 0, 0],
            root: mix(token, 1),
        })
    }

    fn probe(
        &self,
        transaction: &mut BrokerTransaction,
        _fingerprint: &GpuFingerprint,
        _descriptor: &ShimDescriptor,
        step: ProbeStep,
        _attempt: u8,
        _device: &Capability<'_, DeviceMemoryControl>,
        _dma: &Capability<'_, DmaControl>,
    ) -> Result<ProbeObservation, BackendFault> {
        if step.semantic != ProbeSemantic::EstablishHealthBaseline {
            return Err(BackendFault::new(
                FaultCode::ProbeFault,
                false,
                step.semantic as u8 as u64,
            ));
        }

        let root = mix(transaction.root, PROBE_EVIDENCE_HEALTH as u64);
        transaction.root = root;
        Ok(ProbeObservation {
            semantic: step.semantic,
            evidence_bit: PROBE_EVIDENCE_HEALTH,
            value: transaction.token,
            tick: 1,
            root,
        })
    }

    fn activate(
        &self,
        transaction: &mut BrokerTransaction,
        _fingerprint: &GpuFingerprint,
        _descriptor: &ShimDescriptor,
        _isolation: IsolationReceipt,
        _device: &Capability<'_, DeviceMemoryControl>,
        _dma: &Capability<'_, DmaControl>,
    ) -> Result<ActivationReceipt, BackendFault> {
        Ok(ActivationReceipt {
            token: transaction.token,
            strategy: DriverStrategy::Quarantine,
            activation_epoch: 1,
            transport_root: mix(transaction.root, 2),
            framebuffer_object: 0,
            backend_state: transaction.state,
        })
    }

    fn health(
        &self,
        transaction: &mut BrokerTransaction,
        _activation: &ActivationReceipt,
        _descriptor: &ShimDescriptor,
        _fault_policy: &Capability<'_, FaultPolicyControl>,
    ) -> Result<HealthReceipt, BackendFault> {
        Ok(HealthReceipt {
            healthy: true,
            tick: 1,
            heartbeat: 1,
            fault_mask: 0,
            root: mix(transaction.root, 3),
        })
    }

    fn commit(
        &self,
        transaction: &mut BrokerTransaction,
        _activation: &ActivationReceipt,
        _health: &HealthReceipt,
        _decision: &OracleDecision,
        _policy: &Capability<'_, PolicyControl>,
    ) -> Result<(u64, u32), BackendFault> {
        Ok((mix(transaction.token, 4).max(1), 1))
    }

    fn rollback(
        &self,
        _transaction: &mut BrokerTransaction,
        _stage: DispatchStage,
        _fault: BackendFault,
        _device: &Capability<'_, DeviceMemoryControl>,
        _dma: &Capability<'_, DmaControl>,
        _fault_policy: &Capability<'_, FaultPolicyControl>,
    ) -> Result<(), BackendFault> {
        Ok(())
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
