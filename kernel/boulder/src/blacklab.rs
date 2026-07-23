use ::blacklab::dialect::{Bus, Personality, Registry, Transform};
use ::blacklab::echidna::{
    EchidnaError, SharedWindowRegistry, SharedWindowRequest, WindowPermissions,
};
use ::blacklab::evolution::{EvolutionChamber, EvolutionError};
use ::blacklab::graph::{
    EdgeKind, GraphError, NodeKind, PagePrediction, SemanticEdge, SemanticGraph, SemanticNode,
};
use ::blacklab::oureboros::{
    ArtifactManifest, FractalCatalog, FractalClass, FractalRecipe, FractalSeed, OureborosError,
    TargetArchitecture, measure_recipe, verify_artifact,
};
use ::blacklab::pythia::{ClassifierError, Label, NYX_ANOMALY_DETECTOR};
use ::blacklab::resonance::{
    CascadeError, CascadeInput, CascadeThresholds, RealmMode, WalkerSnapshot, WalkerState,
    plan_cascade,
};
use ::blacklab::tartarus::{
    FaultAccess, FaultDecision, FaultSnapshot, TartarusError, TartarusVoid, TombstoneRequest,
};
use ::blacklab::thermal::{
    InputQuantization, SAMPLE_THERMAL_NETWORK, ThermalError, ThermalOracle, ThermalSample,
};
use ::blacklab::timeline::{CounterScale, TimelineError, WorkloadTerm, logical_delta};

use crate::aether::{event_kind, record};
use crate::capability::{
    ArtifactSynthesisControl, Capability, FaultPolicyControl, LearningControl,
    MemorySharingControl, ProcessInstallControl, ResonanceControl, UserlandImageControl,
};
use crate::process::image::{UserImageError, prepare_user_image};
use crate::process::install::{InstallError, UserAddressSpaceBackend, install_user_image};
use crate::sync::SpinLock;

struct Runtime {
    graph: SemanticGraph,
    dialects: Registry,
    evolution: EvolutionChamber,
    shared_windows: SharedWindowRegistry,
    tombstones: TartarusVoid,
    artifacts: FractalCatalog,
    initialized: bool,
}

impl Runtime {
    const fn new() -> Self {
        Self {
            graph: SemanticGraph::new(),
            dialects: Registry::new(),
            evolution: evolution_chamber(),
            shared_windows: SharedWindowRegistry::new(),
            tombstones: TartarusVoid::new(),
            artifacts: FractalCatalog::new(),
            initialized: false,
        }
    }
}

const fn evolution_chamber() -> EvolutionChamber {
    match EvolutionChamber::new(0x1337_7331_c0de_f00d) {
        Ok(chamber) => chamber,
        Err(_) => panic!("invalid Black Lab evolution seed"),
    }
}

static RUNTIME: SpinLock<Runtime> = SpinLock::new(Runtime::new());

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Summary {
    pub logical_nanoseconds: u64,
    pub semantic_heat: u64,
    pub predictions: usize,
    pub next_epoch: u64,
    pub evolution_generation: u64,
    pub quarantined_faults: u64,
    pub materialized_bytes: usize,
    pub pid1_entry_point: u64,
    pub pid1_install_generation: u32,
    pub pid1_page_table_root: Option<u64>,
    pub pid1_owned_frames: usize,
    pub pid1_activation_validated: bool,
    pub thermal_model_actionable: bool,
}

pub struct Initialization<Process> {
    pub summary: Summary,
    pub pid1: Process,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitializeError<ProcessError> {
    AlreadyInitialized,
    Timeline(TimelineError),
    Graph(GraphError),
    Dialect,
    Cascade(CascadeError),
    Thermal(ThermalError),
    Classifier(ClassifierError),
    Evolution(EvolutionError),
    Echidna(EchidnaError),
    Tartarus(TartarusError),
    Oureboros(OureborosError),
    UserImage(UserImageError),
    ProcessInstall(InstallError<ProcessError>),
    IncompletePlan,
}

pub struct Controls<'authority> {
    pub resonance: &'authority Capability<'authority, ResonanceControl>,
    pub learning: &'authority Capability<'authority, LearningControl>,
    pub memory_sharing: &'authority Capability<'authority, MemorySharingControl>,
    pub fault_policy: &'authority Capability<'authority, FaultPolicyControl>,
    pub artifact_synthesis: &'authority Capability<'authority, ArtifactSynthesisControl>,
    pub userland_image: &'authority Capability<'authority, UserlandImageControl>,
    pub process_install: &'authority Capability<'authority, ProcessInstallControl>,
}

#[derive(Clone, Copy)]
pub struct Pid1Source<'bytes> {
    pub bytes: &'bytes [u8],
    pub expected_sha256: [u8; 32],
    pub entry_file_offset: usize,
}

pub fn initialize<Backend: UserAddressSpaceBackend>(
    controls: Controls<'_>,
    install_backend: &mut Backend,
    pid1_source: Pid1Source<'_>,
) -> Result<Initialization<Backend::Process>, InitializeError<Backend::Error>> {
    let mut runtime = RUNTIME.lock();
    if runtime.initialized {
        return Err(InitializeError::AlreadyInitialized);
    }

    let logical_nanoseconds = logical_delta(
        1_000,
        CounterScale::new(1, 2),
        &[WorkloadTerm {
            weighted_work: 400,
            capacity: 4,
            active: true,
        }],
    )
    .map_err(InitializeError::Timeline)?;

    let arena = runtime
        .graph
        .add_node(SemanticNode {
            kind: NodeKind::Arena,
            semantic_class: 1,
            object_handle: 1,
            length: 4096,
            entropy: 7,
            heat: 6_000,
            epoch: 1,
        })
        .map_err(InitializeError::Graph)?;
    let cache = runtime
        .graph
        .add_node(SemanticNode {
            kind: NodeKind::Cache,
            semantic_class: 1,
            object_handle: 2,
            length: 4096,
            entropy: 9,
            heat: 5_000,
            epoch: 1,
        })
        .map_err(InitializeError::Graph)?;
    runtime
        .graph
        .add_edge(SemanticEdge {
            from: arena,
            to: cache,
            kind: EdgeKind::Predicts,
            weight: 88,
        })
        .map_err(InitializeError::Graph)?;
    let semantic_heat = runtime.graph.semantic_heat(1);

    let pci = runtime
        .dialects
        .add_personality(Personality {
            bus: Bus::Pci,
            class: 1,
            vendor_id: 0xffff,
            device_id: 0xffff,
            register_stride: 4,
            irq_style: 1,
            dma_style: 1,
        })
        .map_err(|_| InitializeError::Dialect)?;
    let platform = runtime
        .dialects
        .add_personality(Personality {
            bus: Bus::Platform,
            class: 1,
            vendor_id: 0xffff,
            device_id: 0xffff,
            register_stride: 4,
            irq_style: 1,
            dma_style: 1,
        })
        .map_err(|_| InitializeError::Dialect)?;
    runtime
        .dialects
        .add_transform(Transform {
            from: pci,
            to: platform,
            operation_class: 1,
            latency_cost: 8,
            semantic_loss: 1,
        })
        .map_err(|_| InitializeError::Dialect)?;

    let thermal_oracle = ThermalOracle::new(
        &SAMPLE_THERMAL_NETWORK,
        InputQuantization {
            runnable_threads_per_unit: 1,
            semantic_heat_per_unit: 100,
            flux_per_unit: 100,
            millicelsius_per_unit: 1_000,
        },
        1_000,
        0,
        None,
    )
    .map_err(InitializeError::Thermal)?;
    let thermal_sample = ThermalSample {
        runnable_threads: 3,
        semantic_heat,
        flux_rate: 6_000,
        current_temperature_millicelsius: 70_000,
    };
    let thermal_forecast = thermal_oracle.forecast(thermal_sample);
    if thermal_forecast.validated
        || thermal_oracle.advise(thermal_sample, 85_000) != Err(ThermalError::UnvalidatedModel)
    {
        return Err(InitializeError::IncompletePlan);
    }

    runtime
        .evolution
        .initialize()
        .map_err(InitializeError::Evolution)?;
    let evolution_inputs = [3, 110, 60, 70];
    for epoch in 1..=4 {
        runtime
            .evolution
            .predict_population(epoch, 1, &evolution_inputs)
            .map_err(InitializeError::Evolution)?;
        let observation = runtime
            .evolution
            .observe(epoch + 1, 70)
            .map_err(InitializeError::Evolution)?;
        if observation.capsules_scored != 1 {
            return Err(InitializeError::IncompletePlan);
        }
    }
    let evolution_generation = runtime
        .evolution
        .evolve()
        .map_err(InitializeError::Evolution)?
        .generation;
    let _advisory_candidate = runtime
        .evolution
        .apex_prediction(&evolution_inputs)
        .map_err(InitializeError::Evolution)?;

    let shared_window = runtime
        .shared_windows
        .grant(SharedWindowRequest {
            host_address_space: 10,
            peer_address_space: 11,
            memory_object: 12,
            host_page: 100,
            peer_page: 200,
            page_count: 2,
            permissions: WindowPermissions::read_write(),
            granted_epoch: 1,
            expires_epoch: 3,
        })
        .map_err(InitializeError::Echidna)?;
    let window_info = runtime
        .shared_windows
        .resolve(&shared_window, 1)
        .map_err(InitializeError::Echidna)?;
    if !window_info
        .request
        .permissions
        .contains(WindowPermissions::READ)
    {
        return Err(InitializeError::IncompletePlan);
    }
    runtime
        .shared_windows
        .revoke(&shared_window)
        .map_err(InitializeError::Echidna)?;
    if runtime.shared_windows.resolve(&shared_window, 2) != Err(EchidnaError::InvalidHandle) {
        return Err(InitializeError::IncompletePlan);
    }

    let tombstone = runtime
        .tombstones
        .retire(TombstoneRequest {
            memory_object: 12,
            first_page: 40,
            page_count: 2,
            retired_epoch: 1,
        })
        .map_err(InitializeError::Tartarus)?;
    let fault_decision = runtime.tombstones.inspect_fault(FaultSnapshot {
        address_space_handle: 10,
        memory_object: 12,
        page_number: 41,
        walker_id: 1,
        realm: 1,
        access: FaultAccess::Write,
        epoch: 2,
        semantic_heat: semantic_heat as u32,
    });
    if fault_decision
        != (FaultDecision::Quarantine {
            walker_id: 1,
            memory_object: 12,
            learning_queued: true,
        })
    {
        return Err(InitializeError::IncompletePlan);
    }
    let deferred_sample = runtime
        .tombstones
        .take_learning_sample()
        .ok_or(InitializeError::IncompletePlan)?;
    NYX_ANOMALY_DETECTOR
        .learn(&deferred_sample.features, Label::Suspicious)
        .map_err(InitializeError::Classifier)?;
    runtime
        .tombstones
        .reclaim(&tombstone, 2)
        .map_err(InitializeError::Tartarus)?;
    let quarantined_faults = runtime.tombstones.quarantined_faults();

    let recipe = FractalRecipe {
        algorithm_version: 1,
        base_entropy: 0x1234_5678_9abc_def0,
        structural_mutator: 0x0fed_cba9_8765_4321,
    };
    let expected_sha256 = measure_recipe(recipe, 64).map_err(InitializeError::Oureboros)?;
    runtime
        .artifacts
        .plant_seed(FractalSeed {
            inode_id: 1,
            class: FractalClass::Configuration,
            architecture: TargetArchitecture::Independent,
            recipe,
            unfolded_size_bytes: 64,
            entry_offset: 0,
            expected_sha256,
        })
        .map_err(InitializeError::Oureboros)?;
    let mut artifact = [0_u8; 64];
    let artifact = runtime
        .artifacts
        .materialize(1, &mut artifact)
        .map_err(InitializeError::Oureboros)?;
    let measurement = artifact.measurement();
    if measurement.sha256 != expected_sha256
        || measurement.class != FractalClass::Configuration
        || runtime.artifacts.len() != 1
    {
        return Err(InitializeError::IncompletePlan);
    }
    let materialized_bytes = measurement.bytes_written;

    let pid1_artifact = verify_artifact(
        ArtifactManifest {
            inode_id: 2,
            class: FractalClass::Executable,
            architecture: TargetArchitecture::X86_64,
            entry_offset: pid1_source.entry_file_offset,
            expected_sha256: pid1_source.expected_sha256,
        },
        pid1_source.bytes,
    )
    .map_err(InitializeError::Oureboros)?;
    let pid1_image = prepare_user_image(pid1_artifact, controls.userland_image)
        .map_err(InitializeError::UserImage)?;
    if pid1_image.measurement().sha256 != pid1_source.expected_sha256
        || runtime.artifacts.len() != 1
    {
        return Err(InitializeError::IncompletePlan);
    }
    let installed_pid1 = install_user_image(pid1_image, install_backend, controls.process_install)
        .map_err(InitializeError::ProcessInstall)?;
    let pid1_entry_point = installed_pid1.entry_point;
    let process_info = match install_backend.process_info(&installed_pid1.process) {
        Some(info) => info,
        None => {
            install_backend
                .release_process(&installed_pid1.process)
                .map_err(|error| InitializeError::ProcessInstall(InstallError::Backend(error)))?;
            return Err(InitializeError::IncompletePlan);
        }
    };
    let pid1_install_generation = match install_backend.process_generation(&installed_pid1.process)
    {
        Some(generation) => generation,
        None => {
            install_backend
                .release_process(&installed_pid1.process)
                .map_err(|error| InitializeError::ProcessInstall(InstallError::Backend(error)))?;
            return Err(InitializeError::IncompletePlan);
        }
    };
    let pid1_page_table_root = process_info.address_space_root;
    let pid1_owned_frames = process_info.owned_frames;
    if process_info.entry_point != pid1_entry_point
        || process_info.segment_count != installed_pid1.segment_count
    {
        install_backend
            .release_process(&installed_pid1.process)
            .map_err(|error| InitializeError::ProcessInstall(InstallError::Backend(error)))?;
        return Err(InitializeError::IncompletePlan);
    }
    // SAFETY: Black Lab initialization runs as a serialized bootstrap phase.
    // The backend must restore the kernel root before this call returns. The
    // committed process handle is transferred in `Initialization` on success.
    unsafe {
        install_backend.validate_activation(&installed_pid1.process, controls.process_install)
    }
    .map_err(|error| InitializeError::ProcessInstall(InstallError::Backend(error)))?;
    let pid1_activation_validated = true;
    let completion = (|| -> Result<Summary, InitializeError<Backend::Error>> {
        let fault_features = [1, 0, 0, 0, 0, 0, 0, 0];
        NYX_ANOMALY_DETECTOR
            .learn(&fault_features, Label::Benign)
            .map_err(InitializeError::Classifier)?;
        if NYX_ANOMALY_DETECTOR
            .classify(&fault_features)
            .map_err(InitializeError::Classifier)?
            .label
            != Label::Benign
        {
            return Err(InitializeError::IncompletePlan);
        }

        let walkers = [WalkerSnapshot {
            walker_id: 1,
            state: WalkerState::Active,
            realm_mode: RealmMode::Eclipse,
            address_space_handle: 3,
            current_page: 10,
            capability_fingerprint: 0xa5,
            source_node: arena.as_u16(),
        }];
        let plan = plan_cascade(
            CascadeInput {
                counter_sample: 1_000,
                global_flux: 6_000,
                semantic_heat,
                logic_weight: -1,
                epoch: 1,
                source_personality: pci,
                walkers: &walkers,
            },
            CascadeThresholds {
                flux: 5_000,
                semantic_heat: 10_000,
                maximum_lane_skew: 500,
                prediction_confidence_percent: 85,
            },
        )
        .map_err(InitializeError::Cascade)?;
        let prediction = plan
            .predictions()
            .first()
            .ok_or(InitializeError::IncompletePlan)?;
        runtime
            .graph
            .add_prediction(PagePrediction {
                source_node: arena,
                address_space_handle: prediction.address_space_handle,
                page_number: prediction.page_number,
                replay_epoch: prediction.replay_epoch,
                semantic_hash: prediction.semantic_hash,
                confidence_percent: prediction.confidence_percent,
            })
            .map_err(InitializeError::Graph)?;
        let morph = plan.morph.ok_or(InitializeError::IncompletePlan)?;
        if plan.barrier.is_none()
            || plan.anomaly.is_none()
            || runtime
                .dialects
                .best_transform(morph.source, morph.desired_bus)
                .is_none()
        {
            return Err(InitializeError::IncompletePlan);
        }

        runtime.initialized = true;
        record(event_kind::RESONANCE_PLAN, semantic_heat, plan.next_epoch);
        Ok(Summary {
            logical_nanoseconds,
            semantic_heat,
            predictions: runtime.graph.predictions().len(),
            next_epoch: plan.next_epoch,
            evolution_generation,
            quarantined_faults,
            materialized_bytes,
            pid1_entry_point,
            pid1_install_generation,
            pid1_page_table_root,
            pid1_owned_frames,
            pid1_activation_validated,
            thermal_model_actionable: thermal_forecast.validated,
        })
    })();

    match completion {
        Ok(summary) => Ok(Initialization {
            summary,
            pid1: installed_pid1.process,
        }),
        Err(error) => {
            install_backend
                .release_process(&installed_pid1.process)
                .map_err(|cleanup| {
                    InitializeError::ProcessInstall(InstallError::Cleanup(cleanup))
                })?;
            Err(error)
        }
    }
}

// ─── RESONANCE FIELD ────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LearningGradient {
    pub phase_delta: i16,
    pub gain_q16: u16,
    pub confidence_q16: u16,
}

impl LearningGradient {
    pub const ZERO: Self = Self {
        phase_delta: 0,
        gain_q16: 0,
        confidence_q16: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LatticeCell {
    re_q31: i64,
    im_q31: i64,
    observations: u32,
    pub gradient: LearningGradient,
    epoch: u32,
}

impl LatticeCell {
    pub const ZERO: Self = Self {
        re_q31: 0,
        im_q31: 0,
        observations: 0,
        gradient: LearningGradient::ZERO,
        epoch: 0,
    };

    fn accumulate(&mut self, re_q31: i64, im_q31: i64, epoch: u32) {
        self.re_q31 = self.re_q31.saturating_add(re_q31);
        self.im_q31 = self.im_q31.saturating_add(im_q31);
        self.observations = self.observations.saturating_add(1);
        self.epoch = epoch;
    }

    fn energy(&self) -> u64 {
        let re = self.re_q31 as i128;
        let im = self.im_q31 as i128;
        let energy = re * re + im * im;
        energy.min(u64::MAX as i128) as u64
    }

    fn score(&self) -> u64 {
        let gain = 65_536_u128 + u128::from(self.gradient.gain_q16);
        ((self.energy() as u128 * gain) >> 16).min(u64::MAX as u128) as u64
    }
}

pub struct ResonanceField<const BINS: usize> {
    cells: [LatticeCell; BINS],
    epoch: u32,
    total_samples: u64,
}

impl<const BINS: usize> ResonanceField<BINS> {
    pub const fn new() -> Self {
        Self {
            cells: [LatticeCell::ZERO; BINS],
            epoch: 0,
            total_samples: 0,
        }
    }

    /// `phase_bin` spans the full u16 phase circle.
    /// `weight_q16` is 1.0 at 65535.
    pub fn accumulate(&mut self, phase_bin: u16, re_q31: i32, im_q31: i32, weight_q16: u16) {
        if BINS == 0 {
            return;
        }

        self.epoch = self.epoch.wrapping_add(1).max(1);
        self.total_samples = self.total_samples.saturating_add(1);

        let center = phase_to_index::<BINS>(phase_bin);
        let left = center.checked_sub(1).unwrap_or(BINS - 1);
        let right = (center + 1) % BINS;

        let re = (i64::from(re_q31) * i64::from(weight_q16)) >> 16;
        let im = (i64::from(im_q31) * i64::from(weight_q16)) >> 16;

        self.cells[center].accumulate(re, im, self.epoch);

        // Small neighboring contribution prevents hard bin-edge discontinuities.
        if left != center {
            self.cells[left].accumulate(re >> 3, im >> 3, self.epoch);
        }
        if right != center && right != left {
            self.cells[right].accumulate(re >> 3, im >> 3, self.epoch);
        }
    }

    pub fn eigenphase_bin(&self) -> Option<u16> {
        if BINS == 0 || self.total_samples == 0 {
            return None;
        }

        let index = self
            .cells
            .iter()
            .enumerate()
            .max_by_key(|(_, cell)| cell.score())
            .map(|(index, _)| index)?;

        Some(index_to_phase::<BINS>(index))
    }

    pub fn learn(&mut self, target_phase: u16, rate_q16: u16) {
        let Some(current_phase) = self.eigenphase_bin() else {
            return;
        };

        let current_index = phase_to_index::<BINS>(current_phase);
        let delta = wrapped_phase_delta(current_phase, target_phase);
        let cell = &mut self.cells[current_index];

        let scaled_delta = ((i64::from(delta) * i64::from(rate_q16)) >> 16)
            .clamp(i16::MIN as i64, i16::MAX as i64) as i16;

        cell.gradient.phase_delta = cell.gradient.phase_delta.saturating_add(scaled_delta);
        cell.gradient.gain_q16 = cell.gradient.gain_q16.saturating_add(rate_q16 >> 3);
        cell.gradient.confidence_q16 = cell.gradient.confidence_q16.saturating_add(rate_q16 >> 2);
    }

    pub fn decay(&mut self, shift: u32) {
        let shift = shift.min(31);
        for cell in &mut self.cells {
            cell.re_q31 >>= shift;
            cell.im_q31 >>= shift;
            cell.gradient.gain_q16 >>= 1;
            cell.gradient.confidence_q16 >>= 1;
        }
    }

    pub const fn total_samples(&self) -> u64 {
        self.total_samples
    }
}

impl<const BINS: usize> Default for ResonanceField<BINS> {
    fn default() -> Self {
        Self::new()
    }
}

fn phase_to_index<const BINS: usize>(phase: u16) -> usize {
    (usize::from(phase) * BINS) >> 16
}

fn index_to_phase<const BINS: usize>(index: usize) -> u16 {
    (((index as u64 * 65_536) + (BINS as u64 / 2)) / BINS as u64) as u16
}

fn wrapped_phase_delta(from: u16, to: u16) -> i32 {
    let raw = i32::from(to) - i32::from(from);
    if raw > 32_767 {
        raw - 65_536
    } else if raw < -32_768 {
        raw + 65_536
    } else {
        raw
    }
}
