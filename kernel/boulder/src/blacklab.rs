use ::blacklab::dialect::{Bus, Personality, Registry, Transform};
use ::blacklab::echidna::{
    EchidnaError, SharedWindowRegistry, SharedWindowRequest, WindowPermissions,
};
use ::blacklab::evolution::{EvolutionChamber, EvolutionError};
use ::blacklab::graph::{
    EdgeKind, GraphError, NodeKind, PagePrediction, SemanticEdge, SemanticGraph, SemanticNode,
};
use ::blacklab::oureboros::{
    FractalCatalog, FractalClass, FractalRecipe, FractalSeed, MINIMAL_X86_64_ELF_BYTES,
    OureborosError, TargetArchitecture, measure_recipe,
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
use crate::process::install::{
    DryRunAddressSpace, DryRunError, InstallError, ProcessImageInfo, install_user_image,
};
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
    pub thermal_model_actionable: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitializeError {
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
    ProcessInstall(InstallError<DryRunError>),
    IncompletePlan,
}

pub fn initialize(
    _authority: &Capability<'_, ResonanceControl>,
    _learning: &Capability<'_, LearningControl>,
    _memory_sharing: &Capability<'_, MemorySharingControl>,
    _fault_policy: &Capability<'_, FaultPolicyControl>,
    _artifact_synthesis: &Capability<'_, ArtifactSynthesisControl>,
    userland_image: &Capability<'_, UserlandImageControl>,
    process_install: &Capability<'_, ProcessInstallControl>,
) -> Result<Summary, InitializeError> {
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

    let pid1_recipe = FractalRecipe {
        algorithm_version: 2,
        base_entropy: 0x9999_8888_7777_6666,
        structural_mutator: 0xaaaa_bbbb_cccc_dddd,
    };
    let pid1_digest = measure_recipe(pid1_recipe, MINIMAL_X86_64_ELF_BYTES)
        .map_err(InitializeError::Oureboros)?;
    runtime
        .artifacts
        .plant_seed(FractalSeed {
            inode_id: 2,
            class: FractalClass::Executable,
            architecture: TargetArchitecture::X86_64,
            recipe: pid1_recipe,
            unfolded_size_bytes: MINIMAL_X86_64_ELF_BYTES as u32,
            entry_offset: 128,
            expected_sha256: pid1_digest,
        })
        .map_err(InitializeError::Oureboros)?;
    let mut pid1_bytes = [0_u8; MINIMAL_X86_64_ELF_BYTES];
    let pid1_artifact = runtime
        .artifacts
        .materialize(2, &mut pid1_bytes)
        .map_err(InitializeError::Oureboros)?;
    let pid1_image =
        prepare_user_image(pid1_artifact, userland_image).map_err(InitializeError::UserImage)?;
    if pid1_image.measurement().sha256 != pid1_digest || runtime.artifacts.len() != 2 {
        return Err(InitializeError::IncompletePlan);
    }
    let mut install_backend = DryRunAddressSpace::<256>::new();
    let installed_pid1 = install_user_image(pid1_image, &mut install_backend, process_install)
        .map_err(InitializeError::ProcessInstall)?;
    let pid1_entry_point = installed_pid1.entry_point;
    let pid1_install_generation = installed_pid1.process.generation();
    if install_backend.resolve_process(&installed_pid1.process)
        != Some(ProcessImageInfo {
            entry_point: pid1_entry_point,
            segment_count: 1,
        })
    {
        return Err(InitializeError::IncompletePlan);
    }
    install_backend
        .release_process(&installed_pid1.process)
        .map_err(|error| InitializeError::ProcessInstall(InstallError::Backend(error)))?;
    if install_backend
        .resolve_process(&installed_pid1.process)
        .is_some()
    {
        return Err(InitializeError::IncompletePlan);
    }

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
        thermal_model_actionable: thermal_forecast.validated,
    })
}
