pub mod blacklab_observer;
pub mod brokers;
pub mod dispatch;
pub mod ecam;
pub mod fingerprint;
pub mod inventory;
pub mod model;
pub mod model_weights;
pub mod platform;
pub mod registry;
pub mod shims;
pub mod telemetry;
pub mod topology;

use self::dispatch::{
    DispatchError, DispatchResolution, DriverDispatcher, DriverNetBackend, FaultCode,
    MAXIMUM_ATTEMPTS,
};
use self::fingerprint::{
    FingerprintError, FingerprintSummary, GpuFingerprint, MAXIMUM_GPU_FINGERPRINTS,
    PciConfigReader, TOPOLOGY_BOOT_DISPLAY, TOPOLOGY_INTERNAL_PANEL, TopologyEvidenceProvider,
    fingerprint_inventory,
};
use self::inventory::DisplayFunctionInventory;
use self::model::{
    CompatibilityOracle, DriverStrategy, ModelExpectation, OracleDecision, OracleError,
    OraclePolicy,
};
use self::registry::{RegistryError, ShimRegistry};
use self::telemetry::{
    DriverNetEvent, DriverNetEventKind, DriverNetObserver, decision_event, fingerprint_event,
    resolution_events, terminal_event,
};

pub const MAXIMUM_GPUS: usize = MAXIMUM_GPU_FINGERPRINTS;
pub const MAXIMUM_EVENTS_PER_RESOLUTION: usize = MAXIMUM_ATTEMPTS + 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DriverNetSecrets {
    pub fingerprint: u64,
    pub oracle: u64,
    pub dispatch: u64,
    pub telemetry: u64,
}

impl DriverNetSecrets {
    pub const fn valid(self) -> bool {
        let values = [self.fingerprint, self.oracle, self.dispatch, self.telemetry];

        let mut left = 0_usize;
        while left < values.len() {
            if values[left] == 0 {
                return false;
            }

            let mut right = left + 1;
            while right < values.len() {
                if values[left] == values[right] {
                    return false;
                }
                right += 1;
            }
            left += 1;
        }

        true
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GpuResolutionStatus {
    Committed,
    Quarantined,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GpuResolution {
    pub fingerprint: GpuFingerprint,
    pub strategy: DriverStrategy,
    pub status: GpuResolutionStatus,
    pub confidence_q16: u16,
    pub framebuffer_object: u64,
    pub driver_handle: u64,
    pub driver_generation: u32,
    pub decision_root: u64,
    pub resolution_root: u64,
    pub fault: FaultCode,
}

impl GpuResolution {
    pub const EMPTY: Self = Self {
        fingerprint: GpuFingerprint::EMPTY,
        strategy: DriverStrategy::Quarantine,
        status: GpuResolutionStatus::Failed,
        confidence_q16: 0,
        framebuffer_object: 0,
        driver_handle: 0,
        driver_generation: 0,
        decision_root: 0,
        resolution_root: 0,
        fault: FaultCode::None,
    };

    pub const fn display_available(self) -> bool {
        matches!(
            self.status,
            GpuResolutionStatus::Committed | GpuResolutionStatus::Quarantined
        ) && self.framebuffer_object != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DriverNetSummary {
    pub resolutions: [GpuResolution; MAXIMUM_GPUS],
    pub length: usize,
    pub primary_index: Option<usize>,
    pub fingerprint_summary: FingerprintSummary,
    pub native_count: usize,
    pub firmware_count: usize,
    pub quarantined_count: usize,
    pub failed_count: usize,
    pub display_available: bool,
    pub summary_root: u64,
}

impl DriverNetSummary {
    pub const EMPTY: Self = Self {
        resolutions: [GpuResolution::EMPTY; MAXIMUM_GPUS],
        length: 0,
        primary_index: None,
        fingerprint_summary: FingerprintSummary {
            length: 0,
            display_functions: 0,
            inventory_overflowed: false,
            configuration_faults: 0,
            synthetic_firmware_entry: false,
        },
        native_count: 0,
        firmware_count: 0,
        quarantined_count: 0,
        failed_count: 0,
        display_available: false,
        summary_root: 0,
    };

    pub fn resolutions(&self) -> &[GpuResolution] {
        &self.resolutions[..self.length]
    }

    pub fn primary(&self) -> Option<&GpuResolution> {
        self.primary_index
            .and_then(|index| self.resolutions.get(index))
    }

    pub fn verify(&self, secret: u64) -> bool {
        self.length <= MAXIMUM_GPUS
            && self.primary_index.is_none_or(|index| index < self.length)
            && self.summary_root == summary_root(secret, self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DriverNetError {
    InvalidSecrets,
    Fingerprint(FingerprintError),
    Oracle(OracleError),
    Registry(RegistryError),
    Dispatch,
}

impl From<FingerprintError> for DriverNetError {
    fn from(error: FingerprintError) -> Self {
        Self::Fingerprint(error)
    }
}

impl From<OracleError> for DriverNetError {
    fn from(error: OracleError) -> Self {
        Self::Oracle(error)
    }
}

impl From<RegistryError> for DriverNetError {
    fn from(error: RegistryError) -> Self {
        Self::Registry(error)
    }
}

pub struct DriverNetScratch {
    pub fingerprints: [GpuFingerprint; MAXIMUM_GPUS],
}

impl DriverNetScratch {
    pub const fn new() -> Self {
        Self {
            fingerprints: [GpuFingerprint::EMPTY; MAXIMUM_GPUS],
        }
    }
}

impl Default for DriverNetScratch {
    fn default() -> Self {
        Self::new()
    }
}

pub struct DriverNet {
    secrets: DriverNetSecrets,
    oracle: CompatibilityOracle,
    registry: ShimRegistry,
    dispatcher: DriverDispatcher,
}

impl DriverNet {
    pub fn new(secrets: DriverNetSecrets, policy: OraclePolicy) -> Result<Self, DriverNetError> {
        Self::new_measured(secrets, policy, ModelExpectation::COMPILED)
    }

    pub fn new_measured(
        secrets: DriverNetSecrets,
        policy: OraclePolicy,
        model: ModelExpectation,
    ) -> Result<Self, DriverNetError> {
        if !secrets.valid() {
            return Err(DriverNetError::InvalidSecrets);
        }

        Ok(Self {
            secrets,
            oracle: CompatibilityOracle::new_with_expectation(secrets.oracle, policy, model)?,
            registry: ShimRegistry::black_lab()?,
            dispatcher: DriverDispatcher::new(secrets.dispatch, secrets.oracle)
                .map_err(|_| DriverNetError::Dispatch)?,
        })
    }

    pub fn resolve_all<const N: usize>(
        &mut self,
        inventory: &DisplayFunctionInventory<N>,
        config: &dyn PciConfigReader,
        topology: &dyn TopologyEvidenceProvider,
        backend: &mut dyn DriverNetBackend,
        observer: &mut dyn DriverNetObserver,
        scratch: &mut DriverNetScratch,
    ) -> Result<DriverNetSummary, DriverNetError> {
        let fingerprint_summary = fingerprint_inventory(
            inventory,
            config,
            topology,
            self.secrets.fingerprint,
            &mut scratch.fingerprints,
        )?;

        let mut summary = DriverNetSummary {
            fingerprint_summary,
            ..DriverNetSummary::EMPTY
        };

        if fingerprint_summary.inventory_overflowed {
            let mut event = DriverNetEvent {
                tick: backend.now_tick(),
                kind: DriverNetEventKind::InventoryOverflow,
                severity: 4,
                strategy: DriverStrategy::Quarantine,
                address: 0,
                fingerprint_root: 0,
                decision_root: 0,
                data0: inventory.functions().len() as u64,
                data1: fingerprint_summary.display_functions as u64,
                root: 0,
            };
            event.seal(self.secrets.telemetry);
            observer.observe(event);
        }

        if fingerprint_summary.configuration_faults != 0 {
            let mut event = DriverNetEvent {
                tick: backend.now_tick(),
                kind: DriverNetEventKind::ConfigurationIncomplete,
                severity: 3,
                strategy: DriverStrategy::Quarantine,
                address: 0,
                fingerprint_root: 0,
                decision_root: 0,
                data0: u64::from(fingerprint_summary.configuration_faults),
                data1: fingerprint_summary.display_functions as u64,
                root: 0,
            };
            event.seal(self.secrets.telemetry);
            observer.observe(event);
        }

        for index in 0..fingerprint_summary.length {
            let fingerprint = scratch.fingerprints[index];
            observer.observe(fingerprint_event(
                self.secrets.telemetry,
                backend.now_tick(),
                &fingerprint,
            ));

            let decision = self.oracle.classify(&fingerprint)?;
            observer.observe(decision_event(
                self.secrets.telemetry,
                backend.now_tick(),
                &fingerprint,
                &decision,
            ));

            let resolution =
                self.dispatcher
                    .resolve(&fingerprint, &decision, &self.registry, backend);

            let compact = match resolution {
                Ok(resolution) => {
                    self.observe_resolution(&fingerprint, &resolution, observer);
                    compact_resolution(fingerprint, &decision, &resolution)
                }
                Err(error) => {
                    let fault = dispatch_fault(&error);
                    observer.observe(terminal_event(
                        self.secrets.telemetry,
                        backend.now_tick(),
                        DriverNetEventKind::Quarantine,
                        DriverStrategy::Quarantine,
                        &fingerprint,
                        decision.decision_root,
                        fault,
                        0,
                    ));

                    GpuResolution {
                        fingerprint,
                        strategy: DriverStrategy::Quarantine,
                        status: GpuResolutionStatus::Failed,
                        confidence_q16: decision.confidence_q16,
                        framebuffer_object: 0,
                        driver_handle: 0,
                        driver_generation: 0,
                        decision_root: decision.decision_root,
                        resolution_root: 0,
                        fault,
                    }
                }
            };

            summary.resolutions[summary.length] = compact;
            summary.length += 1;
            update_counts(&mut summary, compact);
        }

        summary.primary_index = select_primary(summary.resolutions());
        summary.display_available = summary
            .primary()
            .is_some_and(|resolution| resolution.display_available());

        if let Some(primary) = summary.primary() {
            let mut event = DriverNetEvent {
                tick: backend.now_tick(),
                kind: DriverNetEventKind::PrimarySelected,
                severity: u8::from(!primary.display_available()),
                strategy: primary.strategy,
                address: self::telemetry::packed_address(&primary.fingerprint),
                fingerprint_root: primary.fingerprint.evidence_root,
                decision_root: primary.decision_root,
                data0: primary.framebuffer_object,
                data1: primary.resolution_root,
                root: 0,
            };
            event.seal(self.secrets.telemetry);
            observer.observe(event);
        } else {
            let fingerprint = scratch
                .fingerprints
                .first()
                .copied()
                .unwrap_or(GpuFingerprint::EMPTY);
            observer.observe(terminal_event(
                self.secrets.telemetry,
                backend.now_tick(),
                DriverNetEventKind::NoDisplay,
                DriverStrategy::Quarantine,
                &fingerprint,
                0,
                FaultCode::NoSafeStrategy,
                0,
            ));
        }

        summary.summary_root = summary_root(self.secrets.dispatch, &summary);
        Ok(summary)
    }

    pub const fn oracle(&self) -> &CompatibilityOracle {
        &self.oracle
    }

    pub fn registry(&self) -> &ShimRegistry {
        &self.registry
    }

    pub const fn totals(&self) -> (u64, u64) {
        self.dispatcher.totals()
    }

    fn observe_resolution(
        &self,
        fingerprint: &GpuFingerprint,
        resolution: &DispatchResolution,
        observer: &mut dyn DriverNetObserver,
    ) {
        let mut events = [DriverNetEvent::EMPTY; MAXIMUM_EVENTS_PER_RESOLUTION];
        let length =
            resolution_events(self.secrets.telemetry, fingerprint, resolution, &mut events);
        for event in events[..length].iter().copied() {
            observer.observe(event);
        }
    }
}

fn compact_resolution(
    fingerprint: GpuFingerprint,
    decision: &OracleDecision,
    resolution: &DispatchResolution,
) -> GpuResolution {
    let status = if resolution.active_strategy == DriverStrategy::Quarantine {
        GpuResolutionStatus::Quarantined
    } else {
        GpuResolutionStatus::Committed
    };

    GpuResolution {
        fingerprint,
        strategy: resolution.active_strategy,
        status,
        confidence_q16: decision.confidence_q16,
        framebuffer_object: resolution.lease.framebuffer_object,
        driver_handle: resolution.lease.handle,
        driver_generation: resolution.lease.generation,
        decision_root: decision.decision_root,
        resolution_root: resolution.resolution_root,
        fault: FaultCode::None,
    }
}

fn update_counts(summary: &mut DriverNetSummary, resolution: GpuResolution) {
    match resolution.status {
        GpuResolutionStatus::Committed => {
            if resolution.strategy == DriverStrategy::FirmwareFramebuffer {
                summary.firmware_count = summary.firmware_count.saturating_add(1);
            } else {
                summary.native_count = summary.native_count.saturating_add(1);
            }
        }
        GpuResolutionStatus::Quarantined => {
            summary.quarantined_count = summary.quarantined_count.saturating_add(1);
        }
        GpuResolutionStatus::Failed => {
            summary.failed_count = summary.failed_count.saturating_add(1);
        }
    }
}

fn select_primary(resolutions: &[GpuResolution]) -> Option<usize> {
    let mut selected = None;
    let mut selected_score = i64::MIN;

    for (index, resolution) in resolutions.iter().enumerate() {
        if !resolution.display_available() {
            continue;
        }

        let mut score = 0_i64;
        if resolution.fingerprint.topology_flags & TOPOLOGY_BOOT_DISPLAY != 0 {
            score += 1_000_000;
        }
        if resolution.fingerprint.topology_flags & TOPOLOGY_INTERNAL_PANEL != 0 {
            score += 500_000;
        }
        score += i64::from(resolution.confidence_q16);
        score += strategy_primary_weight(resolution.strategy);

        if score > selected_score {
            selected = Some(index);
            selected_score = score;
        }
    }

    selected
}

fn strategy_primary_weight(strategy: DriverStrategy) -> i64 {
    match strategy {
        DriverStrategy::HermesNvidia
        | DriverStrategy::AmdDisplay
        | DriverStrategy::IntelDisplay => 100_000,
        DriverStrategy::VirtioGpu | DriverStrategy::VirtualSvga => 80_000,
        DriverStrategy::FirmwareFramebuffer => 40_000,
        DriverStrategy::Quarantine => 0,
    }
}

fn dispatch_fault(error: &DispatchError) -> FaultCode {
    match error {
        DispatchError::CorruptDecision => FaultCode::DecisionCorrupt,
        DispatchError::FingerprintMismatch => FaultCode::FingerprintMismatch,
        DispatchError::Registry(_) => FaultCode::RegistryFault,
        DispatchError::TimeRegression => FaultCode::TimeRegression,
        DispatchError::CleanupIncomplete { .. } => FaultCode::RollbackFault,
        DispatchError::NoSafeStrategy { .. } => FaultCode::NoSafeStrategy,
    }
}

fn summary_root(secret: u64, summary: &DriverNetSummary) -> u64 {
    let mut state = mix(secret, summary.length as u64);
    state = mix(state, summary.primary_index.unwrap_or(usize::MAX) as u64);
    state = mix(state, summary.fingerprint_summary.display_functions as u64);
    state = mix(
        state,
        summary.fingerprint_summary.inventory_overflowed as u64,
    );
    state = mix(
        state,
        u64::from(summary.fingerprint_summary.configuration_faults),
    );
    state = mix(state, summary.native_count as u64);
    state = mix(state, summary.firmware_count as u64);
    state = mix(state, summary.quarantined_count as u64);
    state = mix(state, summary.failed_count as u64);
    state = mix(state, summary.display_available as u64);

    for resolution in summary.resolutions() {
        state = mix(state, resolution.fingerprint.evidence_root);
        state = mix(state, resolution.strategy.index() as u64);
        state = mix(state, resolution.status as u8 as u64);
        state = mix(state, u64::from(resolution.confidence_q16));
        state = mix(state, resolution.framebuffer_object);
        state = mix(state, resolution.driver_handle);
        state = mix(state, u64::from(resolution.driver_generation));
        state = mix(state, resolution.decision_root);
        state = mix(state, resolution.resolution_root);
        state = mix(state, resolution.fault as u16 as u64);
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
