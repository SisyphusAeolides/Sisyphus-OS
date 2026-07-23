// kernel/boulder/src/manifold_orchestrator.rs
//! Manifold Orchestrator — supreme governor of the black-lab math pipeline
//!
//! Owns and sequences:
//!   ResourceQuiver  (cluster mutations / ceiling tokens)
//!   HodgeNerve      (Δ₀, Δ₁ with 2-simplices / load diffusion)
//!   CyclotomicFairQ (n=64 NTT fair-queue)
//!
//! Does NOT replace BlackLabControlPlane (Argus/Cassandra) or AxiomManifold.
//! Those remain peers; this module publishes Actuation for them to consume.
//!
//! Boot:  boot_after_drivernet(pci, drivernet, serial)
//! Tick:  tick(now_tsc) -> Option<Actuation>

#![allow(dead_code)]

use crate::cluster_quiver::{
    ClusterFault, FP_ONE as Q_ONE, Fp as QFp, MAX_N, NodeKind, ResourceQuiver,
};
use crate::cyclotomic_ntt::CyclotomicFairQ;
use crate::drivers::drivernet::DriverNetSummary;
use crate::drivers::drivernet::model::DriverStrategy;
use crate::ghost_chronicle::{GhostChronicle, ghost_kind as orch_kind};
use crate::hodge_cech::{FP_ONE as H_ONE, Fp as HFp, HodgeFault, HodgeNerve, MAX_F, MAX_V};
use crate::hw::pci::{PciDevice, PciInventory};
use crate::serial::SerialPort;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const MAX_SEED_DEV: usize = 10;
const GHOST_CAP: usize = 64;

use crate::manifold_topo::{
    TropicalReport, cech_h1_on_hodge, fiedler_on_hodge, tropical_critical, zx_simplify_quiver,
};

// ---------------------------------------------------------------------------
// Public actuation surface
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Actuation {
    /// Fair-queue class selected this tick (0..classes)
    pub fair_class: u16,
    /// Last cluster vertex mutated (0xFFFF = none)
    pub mutated_node: u16,
    /// Hodge non-harmonic energy after heat step
    pub energy0: u64,
    /// Ceiling scales snapshot (cluster x[i]), 16.16
    pub ceilings: [QFp; MAX_N],
    pub n_ceilings: u8,
    /// Migration hint: per-vertex delta from discrete gradient (16.16 signed)
    pub migrate: [HFp; MAX_V],
    pub n_migrate: u8,
    pub epoch: u64,
    /// Fiedler bipartition: bit i set ⇒ vertex on positive side of cut.
    pub fiedler_mask: u64,
    pub fiedler_value_fp: i32, // algebraic connectivity λ₂ (16.16)

    /// Čech H¹: non-zero ⇒ cover fails to glue (obstruction present).
    pub cech_h1_dim: u16,
    pub cech_obstructed: bool,

    /// Tropical critical chain (vertex indices, 0xFF = pad).
    pub tropical_chain: [u8; 8],
    pub tropical_chain_len: u8,
    pub tropical_length_fp: i32, // max-plus path weight

    /// ZX rewrite stats
    pub zx_edges_before: u16,
    pub zx_edges_after: u16,
}

impl Actuation {
    pub const EMPTY: Self = Self {
        fair_class: 0,
        mutated_node: 0xFFFF,
        energy0: 0,
        ceilings: [0; MAX_N],
        n_ceilings: 0,
        migrate: [0; MAX_V],
        n_migrate: 0,
        epoch: 0,
        fiedler_mask: 0,
        fiedler_value_fp: 0,
        cech_h1_dim: 0,
        cech_obstructed: false,
        tropical_chain: [0xFF; 8],
        tropical_chain_len: 0,
        tropical_length_fp: 0,
        zx_edges_before: 0,
        zx_edges_after: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OrchFault {
    NotReady,
    Seed(ClusterFault),
    Hodge(HodgeFault),
    Cluster(ClusterFault),
    AlreadyBooted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SeedReport {
    pub devices_kept: u8,
    pub strategy_nodes: u8,
    pub arrows: u16,
    pub faces: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum OrchPhase {
    Cold = 0,
    Seeded = 1,
    Proven = 2,
    Live = 3,
    Degraded = 4,
}

// ---------------------------------------------------------------------------
// Governor state
// ---------------------------------------------------------------------------

pub struct ManifoldOrchestrator {
    phase: OrchPhase,
    quiver: ResourceQuiver,
    hodge: HodgeNerve,
    fairq: CyclotomicFairQ,
    ghost: GhostChronicle<GHOST_CAP>,
    last: Actuation,
    seed: SeedReport,
    boot_energy: u64,
    epoch: u64,
    /// heat τ (16.16)
    tau_fp: HFp,
    /// mutate if congestion > threshold
    mut_threshold: QFp,
    complex_ok: bool,
    fiedler_mask: u64,
    fiedler_value_fp: i32,
    cech_h1_dim: u16,
    cech_obstructed: bool,
    zx_edges_before: u16,
    zx_edges_after: u16,
}

impl ManifoldOrchestrator {
    pub const fn new() -> Self {
        Self {
            phase: OrchPhase::Cold,
            quiver: ResourceQuiver::new(0),
            hodge: HodgeNerve::new(0),
            fairq: CyclotomicFairQ::empty(),
            ghost: GhostChronicle::new(0x44_A1_F01D),
            last: Actuation::EMPTY,
            seed: SeedReport {
                devices_kept: 0,
                strategy_nodes: 0,
                arrows: 0,
                faces: 0,
            },
            boot_energy: 0,
            epoch: 0,
            tau_fp: H_ONE / 8,
            mut_threshold: Q_ONE / 4,
            complex_ok: false,
            fiedler_mask: 0,
            fiedler_value_fp: 0,
            cech_h1_dim: 0,
            cech_obstructed: false,
            zx_edges_before: 0,
            zx_edges_after: 0,
        }
    }

    pub fn phase(&self) -> OrchPhase {
        self.phase
    }

    pub fn last_actuation(&self) -> Actuation {
        self.last
    }

    pub fn seed_report(&self) -> SeedReport {
        self.seed
    }

    pub fn complex_identity_ok(&self) -> bool {
        self.complex_ok
    }

    pub fn quiver(&self) -> &ResourceQuiver {
        &self.quiver
    }

    pub fn hodge(&self) -> &HodgeNerve {
        &self.hodge
    }

    pub fn fairq(&self) -> &CyclotomicFairQ {
        &self.fairq
    }

    // ------- boot pipeline -------

    pub fn boot(
        &mut self,
        inv: &PciInventory,
        drive: &DriverNetSummary,
        serial: &mut SerialPort,
    ) -> Result<Actuation, OrchFault> {
        if self.phase != OrchPhase::Cold {
            return Err(OrchFault::AlreadyBooted);
        }

        // 1. Seed quiver from PCI + drivernet
        let rep = seed_quiver(inv, drive, &mut self.quiver).map_err(OrchFault::Seed)?;
        self.seed = rep;
        self.phase = OrchPhase::Seeded;
        self.record(
            0,
            orch_kind::SEED_REPORT,
            rep.devices_kept as u64,
            ((rep.strategy_nodes as u64) << 16) | rep.arrows as u64,
        );
        let _ = writeln!(
            serial,
            "Manifold: seed devs={} strat={} arrows={}",
            rep.devices_kept, rep.strategy_nodes, rep.arrows
        );

        // 1b. ZX simplify quiver BEFORE first cluster mutation
        let zx = zx_simplify_quiver(&mut self.quiver);
        self.record(
            0,
            orch_kind::ZX_REWRITE,
            zx.edges_before as u64,
            zx.edges_after as u64,
        );
        let _ = writeln!(
            serial,
            "Manifold: ZX rewrite E {} -> {} fused={} cancel={}",
            zx.edges_before, zx.edges_after, zx.fused_nodes, zx.canceled_cycles
        );
        self.zx_edges_before = zx.edges_before;
        self.zx_edges_after = zx.edges_after;

        // 2. Hodge from (simplified) quiver
        let faces = hodge_from_quiver(&self.quiver, &mut self.hodge);
        self.seed.faces = faces;
        self.boot_energy = self.hodge.nonharmonic_energy0();
        let _ = writeln!(
            serial,
            "Manifold: Hodge V={} E={} F={} energy0={}",
            self.hodge.n_v, self.hodge.n_e, faces, self.boot_energy
        );

        // 2b. Fiedler on Hodge adjacency
        let fied = fiedler_on_hodge(&self.hodge);
        self.record(
            0,
            orch_kind::FIEDLER_CUT,
            fied.mask,
            fied.lambda2_fp as u32 as u64,
        );
        let _ = writeln!(
            serial,
            "Manifold: Fiedler λ₂={} mask={:#x} +{}/-{}",
            fied.lambda2_fp, fied.mask, fied.n_pos, fied.n_neg
        );
        self.fiedler_mask = fied.mask;
        self.fiedler_value_fp = fied.lambda2_fp;

        // 2c. Čech H¹ on same nerve
        let h1 = cech_h1_on_hodge(&self.hodge);
        self.record(
            0,
            orch_kind::CECH_H1,
            h1.betti1 as u64,
            h1.components as u64,
        );
        let _ = writeln!(
            serial,
            "Manifold: Čech H¹ β₁={} components={} obstructed={}",
            h1.betti1, h1.components, h1.obstructed
        );
        self.cech_h1_dim = h1.betti1;
        self.cech_obstructed = h1.obstructed;

        // 3. Prove δ₁ ∘ δ₀ = 0 on gradient
        self.complex_ok = prove_complex_identity(&mut self.hodge);
        self.record(
            0,
            orch_kind::COMPLEX_ID,
            self.complex_ok as u64,
            self.boot_energy,
        );
        let _ = writeln!(
            serial,
            "Manifold: δ₁δ₀=0 {}",
            if self.complex_ok { "ok" } else { "FAIL" }
        );
        if !self.complex_ok {
            self.phase = OrchPhase::Degraded;
        } else {
            self.phase = OrchPhase::Proven;
        }

        // 4. Heat bring-up
        self.hodge.heat_flow0(self.tau_fp, 16);
        let e1 = self.hodge.nonharmonic_energy0();
        self.record(0, orch_kind::HODGE_HEAT, self.boot_energy, e1);
        let _ = writeln!(
            serial,
            "Manifold: heat energy0 {} -> {}",
            self.boot_energy, e1
        );

        // 5. Cluster mutation
        let mut_node = match self.quiver.mutate_hottest(self.mut_threshold) {
            Ok(Some(k)) => {
                self.record(0, orch_kind::CLUSTER_MUT, k as u64, self.quiver.x[k] as u64);
                let _ = writeln!(
                    serial,
                    "Manifold: cluster μ_{} x'={:#x} arrows={}",
                    k,
                    self.quiver.x[k],
                    self.quiver.live_arrows()
                );
                k as u16
            }
            Ok(None) => {
                let _ = writeln!(serial, "Manifold: cluster cool (no μ)");
                0xFFFFu16
            }
            Err(e) => {
                let _ = writeln!(serial, "Manifold: cluster err {e:?}");
                self.phase = OrchPhase::Degraded;
                0xFFFF
            }
        };

        // 6. NTT64 fair-queue
        self.fairq = CyclotomicFairQ::new(16);
        for i in 0..self.quiver.n.min(16) {
            let amt = (self.quiver.x[i] / Q_ONE).max(1);
            self.fairq.charge(i, amt);
        }
        let pick = self.fairq.quantum(1) as u16;
        self.record(0, orch_kind::NTT_PICK, pick as u64, self.fairq.picks);
        let _ = writeln!(
            serial,
            "Manifold: NTT64 pick={} total_picks={}",
            pick, self.fairq.picks
        );

        // 6b. Tropical critical residual chain
        let trop = tropical_critical(&self.quiver);
        self.record(
            0,
            orch_kind::TROPICAL_CRIT,
            trop.length_fp as u32 as u64,
            trop.len as u64,
        );
        let _ = writeln!(
            serial,
            "Manifold: tropical crit len={} weight={}",
            trop.len, trop.length_fp
        );

        // 7. Publish actuation
        let act = self.publish(pick, mut_node, &trop);
        self.record(0, orch_kind::MANIFOLD_BOOT, self.phase as u64, act.energy0);
        if self.phase != OrchPhase::Degraded {
            self.phase = OrchPhase::Live;
        }
        let _ = writeln!(
            serial,
            "Manifold: orchestrator LIVE phase={:?} epoch={}",
            self.phase, self.epoch
        );
        Ok(act)
    }

    // ------- steady-state tick -------

    pub fn tick(&mut self, now_tsc: u64) -> Result<Actuation, OrchFault> {
        if self.phase != OrchPhase::Live && self.phase != OrchPhase::Degraded {
            return Err(OrchFault::NotReady);
        }
        self.epoch = self.epoch.wrapping_add(1);

        // Sync loads from cluster mass → Hodge 0-cochain
        for i in 0..self.quiver.n.min(self.hodge.n_v) {
            let _ = self.hodge.set_load(i, self.quiver.x[i] as HFp);
        }

        // Heat
        self.hodge.heat_step0(self.tau_fp / 2);
        let e = self.hodge.nonharmonic_energy0();
        if self.epoch & 15 == 0 {
            self.record(now_tsc, orch_kind::HODGE_HEAT, e, self.epoch);
        }

        // Mild congestion drive from |migration| magnitude
        let mut mig = [0; MAX_V];
        self.hodge.migration_delta(&mut mig);
        for i in 0..self.quiver.n.min(MAX_V) {
            let c = mig[i].unsigned_abs().min(Q_ONE.saturating_mul(8));
            let _ = self.quiver.set_congestion(i, c);
        }

        if self.epoch & 7 == 0 {
            let zx = zx_simplify_quiver(&mut self.quiver);
            self.zx_edges_before = zx.edges_before;
            self.zx_edges_after = zx.edges_after;
        }

        let mut_node = match self.quiver.mutate_hottest(self.mut_threshold) {
            Ok(Some(k)) => {
                self.record(
                    now_tsc,
                    orch_kind::CLUSTER_MUT,
                    k as u64,
                    self.quiver.x[k] as u64,
                );
                k as u16
            }
            Ok(None) => 0xFFFF,
            Err(_) => {
                self.phase = OrchPhase::Degraded;
                0xFFFF
            }
        };

        let pick = self.fairq.quantum(1) as u16;
        self.record(now_tsc, orch_kind::NTT_PICK, pick as u64, self.fairq.picks);

        let trop = tropical_critical(&self.quiver);

        if self.epoch & 31 == 0 {
            let fied = fiedler_on_hodge(&self.hodge);
            let h1 = cech_h1_on_hodge(&self.hodge);
            self.fiedler_mask = fied.mask;
            self.fiedler_value_fp = fied.lambda2_fp;
            self.cech_h1_dim = h1.betti1;
            self.cech_obstructed = h1.obstructed;
        }

        Ok(self.publish(pick, mut_node, &trop))
    }

    /// External subsystems charge fair-queue deficit (syscall class, IRQ storm, …).
    pub fn charge_class(&mut self, class: usize, amount: u32) {
        if self.phase == OrchPhase::Live || self.phase == OrchPhase::Degraded {
            self.fairq.charge(class, amount);
        }
    }

    fn publish(&mut self, pick: u16, mut_node: u16, trop: &TropicalReport) -> Actuation {
        let mut act = Actuation::EMPTY;
        act.fair_class = pick;
        act.mutated_node = mut_node;
        act.energy0 = self.hodge.nonharmonic_energy0();
        act.epoch = self.epoch;
        self.quiver.ceiling_scales(&mut act.ceilings);
        act.n_ceilings = self.quiver.n.min(MAX_N) as u8;
        self.hodge.migration_delta(&mut act.migrate);
        act.n_migrate = self.hodge.n_v.min(MAX_V) as u8;

        act.fiedler_mask = self.fiedler_mask;
        act.fiedler_value_fp = self.fiedler_value_fp;
        act.cech_h1_dim = self.cech_h1_dim;
        act.cech_obstructed = self.cech_obstructed;
        act.zx_edges_before = self.zx_edges_before;
        act.zx_edges_after = self.zx_edges_after;
        act.tropical_chain = trop.chain;
        act.tropical_chain_len = trop.len;
        act.tropical_length_fp = trop.length_fp;

        self.last = act;
        act
    }

    fn record(&mut self, tick: u64, kind: u16, a0: u64, a1: u64) {
        let _ = self.ghost.record(tick, 0, kind, 0, a0, a1);
    }
}

// ---------------------------------------------------------------------------
// Global BSP instance (early boot, single-threaded)
// ---------------------------------------------------------------------------

static READY: AtomicBool = AtomicBool::new(false);
static EPOCH: AtomicU64 = AtomicU64::new(0);
static mut ORCH: ManifoldOrchestrator = ManifoldOrchestrator::new();

pub struct MathState {
    pub runtime: crate::certified_math::CertifiedMathRuntime,
    pub secrets: crate::certified_math::MathDomainSecrets,
    pub sheaf: crate::certified_math::CellularCapabilitySheaf,
    pub stabilizer: crate::certified_math::SymplecticStabilizer,
    pub channel: crate::certified_math::KrausChannel,
    pub rho: crate::certified_math::DensityMatrix,
}

static CERTIFIED_RUNTIME: crate::sync::SpinLock<Option<MathState>> =
    crate::sync::SpinLock::new(None);

#[derive(Debug, Eq, PartialEq)]
pub enum TensorDeferredError {
    Tensor(crate::tensor_kernel::TensorKernelError),
    Math(crate::certified_math::RuntimeError),
    CertifiedRuntimeUnavailable,
}

impl From<crate::tensor_kernel::TensorKernelError> for TensorDeferredError {
    fn from(error: crate::tensor_kernel::TensorKernelError) -> Self {
        Self::Tensor(error)
    }
}

impl From<crate::certified_math::RuntimeError> for TensorDeferredError {
    fn from(error: crate::certified_math::RuntimeError) -> Self {
        Self::Math(error)
    }
}

pub fn initialize_tensor_runtime(
    secret: u64,
) -> Result<(), crate::tensor_kernel::TensorKernelError> {
    crate::tensor_kernel::initialize(
        secret,
        crate::tensor_decomp::MultilinearPolicy::KERNEL_DEFAULT,
    )
}

/// # Safety
/// Call only from serialized BSP boot / a single tick owner.
pub unsafe fn global_mut() -> &'static mut ManifoldOrchestrator {
    unsafe { &mut *core::ptr::addr_of_mut!(ORCH) }
}

pub fn ready() -> bool {
    READY.load(Ordering::Acquire)
}

pub fn global_epoch() -> u64 {
    EPOCH.load(Ordering::Relaxed)
}

/// main.rs bolt-in after drivernet::resolve_all.
pub fn boot_after_drivernet(inv: &PciInventory, drive: &DriverNetSummary, serial: &mut SerialPort) {
    let orch = unsafe { global_mut() };
    match orch.boot(inv, drive, serial) {
        Ok(act) => {
            READY.store(true, Ordering::Release);
            EPOCH.store(act.epoch, Ordering::Relaxed);
            let _ = writeln!(
                serial,
                "Manifold: boot ok fair_class={} energy0={} ceilings={}",
                act.fair_class, act.energy0, act.n_ceilings
            );
            let tensor_secret = 0xAAAAAAAA; // Domain-separated secret
            if let Err(e) = crate::manifold_orchestrator::initialize_tensor_runtime(tensor_secret) {
                let _ = writeln!(serial, "Manifold: Tensor runtime init failed: {:?}", e);
            }

            // --- 8. Initialize the certified runtime ---
            let secrets = crate::certified_math::MathDomainSecrets {
                controller: 0x11111111,
                hodge: 0x22222222,
                optimization: 0x33333333,
                sheaf: 0x44444444,
                stabilizer: 0x55555555,
                persistence: 0x66666666,
                spectral: 0x77777777,
                tropical: 0x88888888,
                density: 0x99999999,
            };

            if let Ok(runtime) = crate::certified_math::CertifiedMathRuntime::from_manifold(
                orch,
                16,
                secrets,
                crate::certified_math::RuntimePolicy::STRICT,
                crate::certified_math::CertificationPolicy::INVARIANT_PRESERVING,
            ) {
                if let Ok(mut sheaf) =
                    crate::certified_math::CellularCapabilitySheaf::new(secrets.sheaf)
                {
                    let _ = sheaf
                        .add_open(1, 16, 0)
                        .map(|open_id| sheaf.install(1, open_id, 0));
                    if let Ok(mut stabilizer) =
                        crate::certified_math::SymplecticStabilizer::new(16, secrets.stabilizer)
                    {
                        let _ = stabilizer.add_generator(crate::certified_math::Pauli {
                            x: 0,
                            z: 0xFFFF,
                            phase: 0,
                        });
                        let mut channel = crate::certified_math::KrausChannel::EMPTY;
                        channel.dimension = 4;
                        if let Ok(id_op) = crate::certified_math::density::Operator::identity(4) {
                            let _ = channel.push(id_op);
                            if let Ok(rho) = crate::certified_math::DensityMatrix::from_diagonal(&[
                                1_073_741_824,
                                0,
                                0,
                                0,
                            ]) {
                                let state = MathState {
                                    runtime,
                                    secrets,
                                    sheaf,
                                    stabilizer,
                                    channel,
                                    rho,
                                };
                                *CERTIFIED_RUNTIME.lock() = Some(state);
                                let _ =
                                    writeln!(serial, "Manifold: CertifiedMathRuntime initialized");
                            }
                        }
                    }
                }
            }
        }
        Err(e) => {
            let _ = writeln!(serial, "Manifold: boot failed: {e:?}");
        }
    }
}

/// Timer softpath.
pub fn tick(now_tsc: u64) -> Option<Actuation> {
    if !ready() {
        return None;
    }
    let orch = unsafe { global_mut() };
    match orch.tick(now_tsc) {
        Ok(act) => {
            EPOCH.store(act.epoch, Ordering::Relaxed);
            let _ = crate::tensor_kernel::observe(&act);

            // --- 9. Execute one certified step ---
            let mut guard = CERTIFIED_RUNTIME.lock();
            if let Some(state) = &mut *guard {
                if let Ok(glue) = state.sheaf.certify(1) {
                    let mutation = crate::certified_math::Pauli {
                        x: 0,
                        z: 0,
                        phase: 0,
                    };
                    let syndrome = state.stabilizer.certify(mutation);

                    if let Ok((rho_next, dp)) =
                        state.channel.apply(state.rho, 1000, state.secrets.density)
                    {
                        state.rho = rho_next;
                        let external = crate::certified_math::ExternalProofs {
                            sheaf: &glue,
                            stabilizer: &syndrome,
                            density: crate::certified_math::DensityProof::Channel(&dp),
                        };

                        // 10. Keep capability authorization separate (Verify the step)
                        let _step_result = state.runtime.step(orch, &act, external);
                    }
                }
            }

            Some(act)
        }
        Err(_) => None,
    }
}

pub fn run_tensor_online_update_deferred()
-> Result<Option<crate::tensor_decomp::SgdCertificate>, TensorDeferredError> {
    crate::tensor_kernel::update_online_deferred().map_err(Into::into)
}

pub fn run_tensor_analysis_deferred()
-> Result<Option<crate::tensor_decomp::MultilinearDirective>, TensorDeferredError> {
    let Some(directive) = crate::tensor_kernel::analyze_deferred()? else {
        return Ok(None);
    };

    let mut guard = CERTIFIED_RUNTIME.lock();
    let state = guard
        .as_mut()
        .ok_or(TensorDeferredError::CertifiedRuntimeUnavailable)?;

    if directive.queue_charge != 0 {
        state.runtime.charge_external_class(
            directive.queue_class as usize,
            u64::from(directive.queue_charge),
        )?;
    }

    Ok(Some(directive))
}

// ---------------------------------------------------------------------------
// Seed: PCI DAG + drivernet strategies → ResourceQuiver
// ---------------------------------------------------------------------------

fn class_priority(d: &PciDevice) -> u8 {
    match d.class_code {
        0x03 => 0,
        0x02 => 1,
        0x01 => 2,
        0x0c => 3,
        0x06 => 4,
        _ => 5,
    }
}

fn kind_of(d: &PciDevice) -> NodeKind {
    match d.class_code {
        0x03 => NodeKind::Display,
        0x02 => NodeKind::Network,
        0x01 => NodeKind::Storage,
        0x0c => NodeKind::Usb,
        0x06 => NodeKind::Bridge,
        _ => NodeKind::Other,
    }
}

fn seed_x(d: &PciDevice) -> QFp {
    match d.class_code {
        0x03 => 4 * Q_ONE,
        0x02 | 0x01 => 3 * Q_ONE,
        0x06 => 2 * Q_ONE,
        _ => Q_ONE,
    }
}

fn strategy_congestion(s: DriverStrategy) -> QFp {
    match s {
        DriverStrategy::HermesNvidia => 3 * Q_ONE,
        DriverStrategy::AmdDisplay | DriverStrategy::IntelDisplay => 2 * Q_ONE,
        DriverStrategy::VirtioGpu | DriverStrategy::VirtualSvga => Q_ONE,
        DriverStrategy::FirmwareFramebuffer => Q_ONE / 2,
        DriverStrategy::Quarantine => 4 * Q_ONE,
    }
}

fn seed_quiver(
    inv: &PciInventory,
    drive: &DriverNetSummary,
    q: &mut ResourceQuiver,
) -> Result<SeedReport, ClusterFault> {
    let devices = inv.devices();
    let mut idx = [0u16; MAX_SEED_DEV];
    let mut n_keep = 0usize;

    for (i, d) in devices.iter().enumerate() {
        if n_keep < MAX_SEED_DEV {
            idx[n_keep] = i as u16;
            n_keep += 1;
            let mut j = n_keep - 1;
            while j > 0
                && class_priority(&devices[idx[j] as usize])
                    < class_priority(&devices[idx[j - 1] as usize])
            {
                idx.swap(j, j - 1);
                j -= 1;
            }
        } else {
            let worst = n_keep - 1;
            if class_priority(d) < class_priority(&devices[idx[worst] as usize]) {
                idx[worst] = i as u16;
                let mut j = worst;
                while j > 0
                    && class_priority(&devices[idx[j] as usize])
                        < class_priority(&devices[idx[j - 1] as usize])
                {
                    idx.swap(j, j - 1);
                    j -= 1;
                }
            }
        }
    }

    let mut strats = [DriverStrategy::FirmwareFramebuffer; 5];
    let mut n_strat = 0usize;
    for r in drive.resolutions() {
        if !strats[..n_strat].contains(&r.strategy) && n_strat < 5 {
            strats[n_strat] = r.strategy;
            n_strat += 1;
        }
    }

    let dma_i = n_keep + n_strat;
    let irq_i = dma_i + 1;
    let total = irq_i + 1;
    if total > MAX_N {
        return Err(ClusterFault::Dim);
    }
    *q = ResourceQuiver::new(total);

    for (local, &di) in idx.iter().take(n_keep).enumerate() {
        let d = &devices[di as usize];
        q.set_node(local, kind_of(d), di, seed_x(d))?;
        q.set_congestion(local, Q_ONE)?;
    }
    for s in 0..n_strat {
        let ni = n_keep + s;
        q.set_node(ni, NodeKind::Strategy, strats[s] as u16, 2 * Q_ONE)?;
        q.set_congestion(ni, strategy_congestion(strats[s]))?;
    }
    q.set_node(dma_i, NodeKind::DmaPool, 0xFF01, 8 * Q_ONE)?;
    q.set_node(irq_i, NodeKind::IrqBudget, 0xFF02, 8 * Q_ONE)?;
    q.set_congestion(dma_i, Q_ONE)?;
    q.set_congestion(irq_i, Q_ONE)?;

    for a in 0..n_keep {
        let da = &devices[idx[a] as usize];
        if da.class_code != 0x06 {
            continue;
        }
        for b in 0..n_keep {
            if a == b {
                continue;
            }
            let db = &devices[idx[b] as usize];
            if da.address.bus == db.address.bus && db.class_code != 0x06 {
                let _ = q.add_arrow(a as u8, b as u8, 1);
            }
        }
    }

    for a in 0..n_keep {
        let da = &devices[idx[a] as usize];
        if da.class_code != 0x03 {
            continue;
        }
        for r in drive.resolutions() {
            if let Some(si) = strats[..n_strat].iter().position(|&t| t == r.strategy) {
                let _ = q.add_arrow(a as u8, (n_keep + si) as u8, 1);
            }
        }
    }

    for a in 0..n_keep {
        let d = &devices[idx[a] as usize];
        let _ = q.add_arrow(a as u8, dma_i as u8, 1);
        if d.interrupt_pin != 0 {
            let _ = q.add_arrow(a as u8, irq_i as u8, 1);
        }
    }
    for s in 0..n_strat {
        let _ = q.add_arrow((n_keep + s) as u8, dma_i as u8, 1);
    }

    Ok(SeedReport {
        devices_kept: n_keep as u8,
        strategy_nodes: n_strat as u8,
        arrows: q.live_arrows() as u16,
        faces: 0,
    })
}

fn hodge_from_quiver(q: &ResourceQuiver, h: &mut HodgeNerve) -> u16 {
    *h = HodgeNerve::new(q.n);
    for i in 0..q.n {
        let _ = h.set_load(i, q.x[i] as HFp);
    }
    for a in q.arrows.iter().take(q.e_len) {
        if a.live {
            let _ = h.add_edge(a.from, a.to, a.mult as u16);
        }
    }
    h.fill_clique_triangles() as u16
}

fn prove_complex_identity(h: &mut HodgeNerve) -> bool {
    if h.n_f == 0 {
        return true; // vacuous
    }
    h.store_gradient_flux();
    let mut beta = [0; MAX_F];
    h.coboundary1(&h.f1, &mut beta);
    for i in 0..h.n_f {
        if beta[i] != 0 {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Host tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster_quiver::ResourceQuiver;

    #[test]
    fn complex_on_triangle() {
        let mut h = HodgeNerve::new(3);
        h.add_edge(0, 1, 1).unwrap();
        h.add_edge(1, 2, 1).unwrap();
        h.add_edge(0, 2, 1).unwrap();
        h.add_face(0, 1, 2, 1).unwrap();
        h.set_load(0, 0).unwrap();
        h.set_load(1, H_ONE).unwrap();
        h.set_load(2, 2 * H_ONE).unwrap();
        assert!(prove_complex_identity(&mut h));
    }

    #[test]
    fn orch_cold_not_ready() {
        let mut o = ManifoldOrchestrator::new();
        assert!(matches!(o.tick(0), Err(OrchFault::NotReady)));
        assert_eq!(o.phase(), OrchPhase::Cold);
    }

    #[test]
    fn hodge_from_simple_quiver() {
        let mut q = ResourceQuiver::new(3);
        q.set_node(0, NodeKind::Bridge, 0, Q_ONE).unwrap();
        q.set_node(1, NodeKind::Display, 1, 2 * Q_ONE).unwrap();
        q.set_node(2, NodeKind::DmaPool, 2, 3 * Q_ONE).unwrap();
        q.add_arrow(0, 1, 1).unwrap();
        q.add_arrow(1, 2, 1).unwrap();
        q.add_arrow(0, 2, 1).unwrap();
        let mut h = HodgeNerve::new(0);
        let f = hodge_from_quiver(&q, &mut h);
        assert_eq!(h.n_v, 3);
        assert!(h.n_e >= 3);
        assert_eq!(f, 1);
    }
}
