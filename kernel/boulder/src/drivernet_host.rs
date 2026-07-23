// kernel/boulder/src/drivernet_host.rs
//! Dummy integration host for Drivernet Black-Lab v2

use crate::blacklab_bootstrap::BlackLabComplex;
use crate::capability::{
    Authority, Capability, DeviceMemoryControl, DmaControl, FaultPolicyControl, PolicyControl,
};
use crate::drivers::drivernet::blacklab_observer::BlackLabDriverObserver;
use crate::drivers::drivernet::brokers::{
    FirmwareFramebufferBroker, FirmwareFramebufferHost, NativeBrokerAdapter, NativeStrategyHost,
    QuarantineBroker,
};
use crate::drivers::drivernet::dispatch::{
    ActivationReceipt, BackendFault, DispatchStage, FaultCode, HealthReceipt, ProbeObservation,
};
use crate::drivers::drivernet::fingerprint::{
    FirmwareFramebufferEvidence, GpuFingerprint, LegacyConfigurationReader,
};
use crate::drivers::drivernet::inventory::DisplayFunctionInventory;
use crate::drivers::drivernet::model::{
    DriverStrategy, ModelExpectation, OracleDecision, OraclePolicy,
};
use crate::drivers::drivernet::model_weights::{
    MODEL_CORPUS_ROOT, MODEL_ROOT, MODEL_SCHEMA_VERSION,
};
use crate::drivers::drivernet::platform::{
    BlackLabDriverPlatform, BrokerTransaction, DeviceIsolationBroker, DriverNetAuthority,
    DriverNetClock, IsolationReceipt,
};
use crate::drivers::drivernet::registry::{ProbeStep, ShimDescriptor};
use crate::drivers::drivernet::topology::BootTopologyTable;
use crate::drivers::drivernet::{DriverNet, DriverNetScratch, DriverNetSecrets, DriverNetSummary};
use crate::hw::pci::PciInventory;

pub struct BoulderDriverClock;

impl DriverNetClock for BoulderDriverClock {
    fn now_tick(&self) -> u64 {
        <crate::arch::Active as crate::arch::Architecture>::counter_sample()
    }
}

pub struct BoulderIsolationBroker;

impl DeviceIsolationBroker for BoulderIsolationBroker {
    fn begin(
        &self,
        fingerprint: &GpuFingerprint,
        descriptor: &ShimDescriptor,
        _device: &Capability<'_, DeviceMemoryControl>,
        _dma: &Capability<'_, DmaControl>,
    ) -> Result<IsolationReceipt, BackendFault> {
        if !descriptor.vendor_gate.matches(fingerprint) {
            return Err(BackendFault::new(
                FaultCode::DescriptorRejected,
                false,
                0,
            ));
        }

        Ok(IsolationReceipt {
            token: fingerprint.evidence_root,
            domain: fingerprint.iommu_group as u64,
            firmware_preserved: fingerprint.firmware_display_usable(),
            root: fingerprint.evidence_root,
        })
    }

    fn commit(
        &self,
        _receipt: IsolationReceipt,
        _device: &Capability<'_, DeviceMemoryControl>,
        _dma: &Capability<'_, DmaControl>,
    ) -> Result<(), BackendFault> {
        Ok(())
    }

    fn rollback(
        &self,
        _receipt: IsolationReceipt,
        _stage: DispatchStage,
        _fault: BackendFault,
        _device: &Capability<'_, DeviceMemoryControl>,
        _dma: &Capability<'_, DmaControl>,
        _fault_policy: &Capability<'_, FaultPolicyControl>,
    ) -> Result<(), BackendFault> {
        Ok(())
    }
}

pub struct BoulderNativeHost;

impl NativeStrategyHost for BoulderNativeHost {
    fn begin(
        &self,
        strategy: DriverStrategy,
        _fingerprint: &GpuFingerprint,
        _descriptor: &ShimDescriptor,
        isolation: IsolationReceipt,
        _policy: &Capability<'_, PolicyControl>,
    ) -> Result<BrokerTransaction, BackendFault> {
        if strategy == DriverStrategy::VirtioGpu {
            Ok(BrokerTransaction {
                token: isolation.token,
                state: [0; 8],
                root: isolation.root,
            })
        } else {
            Err(BackendFault::new(
                FaultCode::RegistryFault,
                false,
                strategy as u64,
            ))
        }
    }

    fn probe(
        &self,
        _strategy: DriverStrategy,
        _transaction: &mut BrokerTransaction,
        _fingerprint: &GpuFingerprint,
        _descriptor: &ShimDescriptor,
        _step: ProbeStep,
        _attempt: u8,
        _device: &Capability<'_, DeviceMemoryControl>,
        _dma: &Capability<'_, DmaControl>,
    ) -> Result<ProbeObservation, BackendFault> {
        Err(BackendFault::new(FaultCode::RegistryFault, false, 0))
    }

    fn activate(
        &self,
        _strategy: DriverStrategy,
        _transaction: &mut BrokerTransaction,
        _fingerprint: &GpuFingerprint,
        _descriptor: &ShimDescriptor,
        _isolation: IsolationReceipt,
        _device: &Capability<'_, DeviceMemoryControl>,
        _dma: &Capability<'_, DmaControl>,
    ) -> Result<ActivationReceipt, BackendFault> {
        Err(BackendFault::new(FaultCode::RegistryFault, false, 0))
    }

    fn health(
        &self,
        _strategy: DriverStrategy,
        _transaction: &mut BrokerTransaction,
        _activation: &ActivationReceipt,
        _descriptor: &ShimDescriptor,
        _fault_policy: &Capability<'_, FaultPolicyControl>,
    ) -> Result<HealthReceipt, BackendFault> {
        Err(BackendFault::new(FaultCode::RegistryFault, false, 0))
    }

    fn commit(
        &self,
        _strategy: DriverStrategy,
        _transaction: &mut BrokerTransaction,
        _activation: &ActivationReceipt,
        _health: &HealthReceipt,
        _decision: &OracleDecision,
        _policy: &Capability<'_, PolicyControl>,
    ) -> Result<(u64, u32), BackendFault> {
        Err(BackendFault::new(FaultCode::RegistryFault, false, 0))
    }

    fn rollback(
        &self,
        _strategy: DriverStrategy,
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

pub struct BoulderFirmwareHost;

impl FirmwareFramebufferHost for BoulderFirmwareHost {
    fn now_tick(&self) -> u64 {
        <crate::arch::Active as crate::arch::Architecture>::counter_sample()
    }

    fn inspect(&self, _evidence: FirmwareFramebufferEvidence) -> Result<(u64, u64), BackendFault> {
        Err(BackendFault::new(FaultCode::RegistryFault, false, 0))
    }

    fn retain(
        &self,
        _object: u64,
        _policy: &Capability<'_, PolicyControl>,
    ) -> Result<(u64, u32), BackendFault> {
        Err(BackendFault::new(FaultCode::RegistryFault, false, 0))
    }

    fn release(
        &self,
        _object: u64,
        _fault_policy: &Capability<'_, FaultPolicyControl>,
    ) -> Result<(), BackendFault> {
        Ok(())
    }
}

pub fn resolve_drivernet<
    const SENSORS: usize,
    const RULES: usize,
    const LEDGER: usize,
    const APERTURES: usize,
    const MAPPINGS: usize,
>(
    pci_inventory: &PciInventory,
    authority: &Authority,
    blacklab: &mut BlackLabComplex<SENSORS, RULES, LEDGER, APERTURES, MAPPINGS>,
) -> DriverNetSummary {
    let expected_model = ModelExpectation {
        schema: MODEL_SCHEMA_VERSION,
        corpus_root: MODEL_CORPUS_ROOT,
        model_root: MODEL_ROOT,
    };

    let secrets = DriverNetSecrets {
        fingerprint: 0x1111111111111111,
        oracle: 0x2222222222222222,
        dispatch: 0x3333333333333333,
        telemetry: 0x4444444444444444,
    };

    let mut drivernet =
        DriverNet::new_measured(secrets, OraclePolicy::BLACK_LAB, expected_model).unwrap();

    let mut display_inventory = DisplayFunctionInventory::<64>::new().unwrap();
    display_inventory.import_legacy(pci_inventory).unwrap();

    let configuration_reader = LegacyConfigurationReader;
    let gpu_topology = BootTopologyTable::<64>::new().unwrap();

    let device_memory = authority.grant::<DeviceMemoryControl>();
    let dma_control = authority.grant::<DmaControl>();
    let policy_control = authority.grant::<PolicyControl>();
    let fault_policy = authority.grant::<FaultPolicyControl>();

    let driver_clock = BoulderDriverClock;
    let device_isolation_broker = BoulderIsolationBroker;

    let native_strategy_host = BoulderNativeHost;
    let firmware_framebuffer_host = BoulderFirmwareHost;

    let nvidia =
        NativeBrokerAdapter::new(DriverStrategy::HermesNvidia, &native_strategy_host).unwrap();
    let amd = NativeBrokerAdapter::new(DriverStrategy::AmdDisplay, &native_strategy_host).unwrap();
    let intel =
        NativeBrokerAdapter::new(DriverStrategy::IntelDisplay, &native_strategy_host).unwrap();
    let virtio =
        NativeBrokerAdapter::new(DriverStrategy::VirtioGpu, &native_strategy_host).unwrap();
    let virtual_svga =
        NativeBrokerAdapter::new(DriverStrategy::VirtualSvga, &native_strategy_host).unwrap();

    let firmware = FirmwareFramebufferBroker::new(&firmware_framebuffer_host);
    let quarantine = QuarantineBroker::new(secrets.dispatch).unwrap();

    let brokers: [&dyn crate::drivers::drivernet::platform::StrategyBroker; 7] = [
        &nvidia,
        &amd,
        &intel,
        &virtio,
        &virtual_svga,
        &firmware,
        &quarantine,
    ];

    let mut platform = BlackLabDriverPlatform::new(
        &driver_clock,
        &device_isolation_broker,
        brokers,
        crate::drivers::drivernet::platform::DriverNetAuthority {
            device: &device_memory,
            dma: &dma_control,
            policy: &policy_control,
            fault: &fault_policy,
        },
        secrets.dispatch,
    )
    .unwrap();

    let mut observer =
        BlackLabDriverObserver::new(&blacklab.mnemosyne, &blacklab.oracular, &fault_policy);

    let mut scratch = DriverNetScratch::new();

    drivernet
        .resolve_all(
            &display_inventory,
            &configuration_reader,
            &gpu_topology,
            &mut platform,
            &mut observer,
            &mut scratch,
        )
        .unwrap()
}
