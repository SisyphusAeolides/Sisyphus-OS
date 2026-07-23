//! Full multilinear kernel-analysis runtime.
//!
//! Scheduling:
//!
//! - observation: write one bounded telemetry slice;
//! - online deferred pass: SGD minibatches;
//! - full deferred pass: CP-ALS, optional CCD++, ST-HOSVD, HOOI, TT-SVD,
//!   MPS transfer contractions, PEPS patch contraction, einsum residual energy,
//!   and tensordot cross-model agreement.
//!
//! No result receives authority directly. The output is a sealed bounded queue
//! pressure proposal for the existing certified exact spectral queue.

use super::cp::{CpCertificate, CpConfig, CpModel, CpWorkspace, fit_cp_als};
use super::einsum::{EinsumCertificate, EinsumError, EinsumPlan, execute_binary};
use super::fixed;
use super::hooi::{HooiCertificate, HooiConfig, HooiWorkspace, refine_tucker_hooi};
use super::hosvd::{HosvdCertificate, HosvdConfig, HosvdWorkspace, fit_st_hosvd};
use super::network::{
    MpsCertificate, MpsWorkspace, Peps2x2, PepsCertificate, build_binary_peps_patch,
    mps_local_diagonal_expectation, mps_norm, peps_local_diagonal_expectation,
};
use super::ops::{AxisPairs, ContractionCertificate, tensordot_into};
use super::optim::{
    CcdCertificate, CcdConfig, CpOptimizerWorkspace, SgdCertificate, SgdConfig, update_cp_ccd,
    update_cp_sgd,
};
use super::telemetry::{KernelTelemetryTensor, METRICS, SUBSYSTEMS, TIME_SLOTS, TelemetrySnapshot};
use super::tensor::{DenseTensor, TensorError, TensorShape, mix, squared_error_q48};
use super::tt::{
    MAX_TT_DIMENSION, TensorTrain, TtCertificate, TtConfig, TtDense, TtShape, TtWorkspace,
    fit_tt_svd,
};
use super::tucker::TuckerModel;

const CP_DOMAIN: u64 = 0x4350_5f42_4154_4348;
const SGD_DOMAIN: u64 = 0x4350_5f4f_4e4c_494e;
const CCD_DOMAIN: u64 = 0x4350_5f43_4344_5050;
const HOSVD_DOMAIN: u64 = 0x484f_5356_445f_5354;
const HOOI_DOMAIN: u64 = 0x484f_4f49_5f52_4546;
const TT_DOMAIN: u64 = 0x5454_5f53_5644_5f43;
const MPS_DOMAIN: u64 = 0x4d50_535f_434f_4e54;
const PEPS_DOMAIN: u64 = 0x5045_5053_5f32_5832;
const EINSUM_DOMAIN: u64 = 0x4549_4e53_554d_5f52;
const DOT_DOMAIN: u64 = 0x5445_4e53_4f52_444f;
const DIRECTIVE_DOMAIN: u64 = 0x4d55_4c54_495f_4449;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MultilinearPolicy {
    pub minimum_occupied_slots: u8,
    pub online_period_epochs: u16,
    pub full_period_epochs: u16,
    pub cp: CpConfig,
    pub sgd: SgdConfig,
    pub ccd: CcdConfig,
    pub hosvd: HosvdConfig,
    pub hooi: HooiConfig,
    pub tt: TtConfig,
    pub ccd_trigger_error_q24: i64,
    pub maximum_cp_error_q24: i64,
    pub maximum_cp_normal_residual_q24: u64,
    pub maximum_sgd_gradient_q24: u64,
    pub maximum_ccd_delta_q24: u64,
    pub maximum_tucker_error_q24: i64,
    pub maximum_orthogonality_q24: u64,
    pub maximum_eigen_residual_q24: u64,
    pub maximum_tt_error_q24: i64,
    pub maximum_cross_disagreement_q24: i64,
    pub anomaly_threshold_q24: i64,
    pub maximum_queue_charge: u32,
    pub require_tucker_compression: bool,
    pub require_tt_compression: bool,
}

impl MultilinearPolicy {
    pub const KERNEL_DEFAULT: Self = Self {
        minimum_occupied_slots: TIME_SLOTS as u8,
        online_period_epochs: 2,
        full_period_epochs: TIME_SLOTS as u16,
        cp: CpConfig::KERNEL_DEFAULT,
        sgd: SgdConfig::KERNEL_DEFAULT,
        ccd: CcdConfig::KERNEL_DEFAULT,
        hosvd: HosvdConfig::KERNEL_DEFAULT,
        hooi: HooiConfig::KERNEL_DEFAULT,
        tt: TtConfig::KERNEL_DEFAULT,
        ccd_trigger_error_q24: fixed::ONE / 8,
        maximum_cp_error_q24: fixed::ONE,
        maximum_cp_normal_residual_q24: fixed::ONE as u64 / 16,
        maximum_sgd_gradient_q24: 64 * fixed::ONE as u64,
        maximum_ccd_delta_q24: 64 * fixed::ONE as u64,
        maximum_tucker_error_q24: fixed::ONE,
        maximum_orthogonality_q24: fixed::ONE as u64 / 128,
        maximum_eigen_residual_q24: fixed::ONE as u64 / 8,
        maximum_tt_error_q24: fixed::ONE,
        maximum_cross_disagreement_q24: 3 * fixed::ONE / 4,
        anomaly_threshold_q24: fixed::ONE / 128,
        maximum_queue_charge: 4096,
        require_tucker_compression: true,
        require_tt_compression: true,
    };

    fn validate(self) -> Result<(), TensorError> {
        if self.minimum_occupied_slots == 0
            || self.minimum_occupied_slots as usize > TIME_SLOTS
            || self.online_period_epochs == 0
            || self.full_period_epochs == 0
            || self.maximum_cp_error_q24 < 0
            || self.maximum_tucker_error_q24 < 0
            || self.maximum_tt_error_q24 < 0
            || self.maximum_cross_disagreement_q24 < 0
            || self.anomaly_threshold_q24 < 0
            || self.anomaly_threshold_q24 >= fixed::ONE
            || self.maximum_queue_charge == 0
        {
            return Err(TensorError::InvalidDimension);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MultilinearDirective {
    pub epoch: u64,
    pub queue_class: u8,
    pub queue_charge: u32,
    pub suspect_subsystem: u8,
    pub suspect_metric: u8,
    pub anomaly_q24: i64,
    pub cross_disagreement_q24: i64,
    pub mps_subsystem_mass_q24: i64,
    pub mps_metric_mass_q24: i64,
    pub peps_fault_expectation_q24: i64,
    pub cp_model_root: u64,
    pub tucker_model_root: u64,
    pub train_root: u64,
    pub certificate_root: u64,
    pub root: u64,
}

impl MultilinearDirective {
    pub const EMPTY: Self = Self {
        epoch: 0,
        queue_class: 0,
        queue_charge: 0,
        suspect_subsystem: 0,
        suspect_metric: 0,
        anomaly_q24: 0,
        cross_disagreement_q24: 0,
        mps_subsystem_mass_q24: 0,
        mps_metric_mass_q24: 0,
        peps_fault_expectation_q24: 0,
        cp_model_root: 0,
        tucker_model_root: 0,
        train_root: 0,
        certificate_root: 0,
        root: 0,
    };

    pub fn verify(&self, secret: u64) -> bool {
        self.root == directive_root(mix(secret, DIRECTIVE_DOMAIN), self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MultilinearCertificate {
    pub epoch: u64,
    pub telemetry_root: u64,
    pub analysis_root: u64,
    pub cp: CpCertificate,
    pub online_sgd: Option<SgdCertificate>,
    pub ccd: Option<CcdCertificate>,
    pub hosvd: HosvdCertificate,
    pub hooi: HooiCertificate,
    pub tt: TtCertificate,
    pub mps_norm: MpsCertificate,
    pub mps_subsystem: MpsCertificate,
    pub mps_metric: MpsCertificate,
    pub peps: PepsCertificate,
    pub residual_einsum: EinsumCertificate,
    pub model_tensordot: ContractionCertificate,
    pub cross_disagreement_q24: i64,
    pub anomaly_q24: i64,
    pub suspect_subsystem: u8,
    pub suspect_metric: u8,
    pub directive_root: u64,
    pub root: u64,
}

impl MultilinearCertificate {
    pub fn verify(&self, secret: u64, policy: MultilinearPolicy) -> bool {
        let cp_ok = self.cp.verify(
            mix(secret, CP_DOMAIN),
            policy.maximum_cp_error_q24,
            policy.maximum_cp_normal_residual_q24,
        );
        let sgd_ok = self
            .online_sgd
            .map(|certificate| {
                certificate.verify(
                    mix(secret, SGD_DOMAIN),
                    policy.maximum_cp_error_q24,
                    policy.maximum_sgd_gradient_q24,
                )
            })
            .unwrap_or(true);
        let ccd_ok = self
            .ccd
            .map(|certificate| {
                certificate.verify(
                    mix(secret, CCD_DOMAIN),
                    policy.maximum_cp_error_q24,
                    policy.maximum_ccd_delta_q24,
                )
            })
            .unwrap_or(true);

        cp_ok
            && sgd_ok
            && ccd_ok
            && self.hosvd.verify(
                mix(secret, HOSVD_DOMAIN),
                policy.maximum_tucker_error_q24,
                policy.maximum_orthogonality_q24,
                policy.maximum_eigen_residual_q24,
            )
            && (!policy.require_tucker_compression || self.hosvd.compresses())
            && self.hooi.verify(
                mix(secret, HOOI_DOMAIN),
                policy.maximum_tucker_error_q24,
                policy.maximum_orthogonality_q24,
                policy.maximum_eigen_residual_q24,
            )
            && self.tt.verify(
                mix(secret, TT_DOMAIN),
                policy.maximum_tt_error_q24,
                policy.maximum_orthogonality_q24,
                policy.maximum_eigen_residual_q24,
            )
            && (!policy.require_tt_compression || self.tt.compresses())
            && self.mps_norm.train_root == self.tt.train_root
            && self.mps_subsystem.train_root == self.tt.train_root
            && self.mps_metric.train_root == self.tt.train_root
            && self.mps_norm.verify(mix(secret, MPS_DOMAIN))
            && self.mps_subsystem.verify(mix(secret, MPS_DOMAIN ^ 1))
            && self.mps_metric.verify(mix(secret, MPS_DOMAIN ^ 2))
            && self.peps.verify(mix(secret, PEPS_DOMAIN))
            && self.residual_einsum.verify(mix(secret, EINSUM_DOMAIN))
            && self.model_tensordot.verify(mix(secret, DOT_DOMAIN))
            && self.cross_disagreement_q24 <= policy.maximum_cross_disagreement_q24
            && self.root == certificate_root(secret, self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MultilinearRejectionReason {
    InsufficientWindow,
    Cp,
    Tucker,
    TensorTrain,
    TensorNetwork,
    CrossDisagreement,
    Certificate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MultilinearRejection {
    pub epoch: u64,
    pub reason: MultilinearRejectionReason,
    pub telemetry_root: u64,
    pub detail_q24: i64,
    pub evidence_root: u64,
    pub root: u64,
}

impl MultilinearRejection {
    pub fn verify(&self, secret: u64) -> bool {
        self.root == rejection_root(secret, self)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum MultilinearError {
    Tensor(TensorError),
    Einsum(EinsumError),
    Rejected(MultilinearRejection),
}

impl From<TensorError> for MultilinearError {
    fn from(error: TensorError) -> Self {
        Self::Tensor(error)
    }
}

impl From<EinsumError> for MultilinearError {
    fn from(error: EinsumError) -> Self {
        Self::Einsum(error)
    }
}

pub struct MultilinearRuntime {
    telemetry: KernelTelemetryTensor,
    analysis: DenseTensor,
    consensus_residual: DenseTensor,
    energy_map: DenseTensor,
    scalar: DenseTensor,
    residual_plan: EinsumPlan,
    dot_axes: AxisPairs,

    cp_model: CpModel,
    cp_workspace: CpWorkspace,
    cp_optimizer_workspace: CpOptimizerWorkspace,

    tucker_model: TuckerModel,
    hosvd_workspace: HosvdWorkspace,
    hooi_workspace: HooiWorkspace,

    tt_input: TtDense,
    tt_train: TensorTrain,
    tt_workspace: TtWorkspace,
    mps_workspace: MpsWorkspace,
    peps_patch: Peps2x2,

    policy: MultilinearPolicy,
    secret: u64,
    last_online_epoch: u64,
    last_full_epoch: u64,
    last_sgd: Option<SgdCertificate>,
    last_certificate: Option<MultilinearCertificate>,
    last_directive: Option<MultilinearDirective>,
}

impl MultilinearRuntime {
    pub fn new(secret: u64, policy: MultilinearPolicy) -> Result<Self, MultilinearError> {
        if secret == 0 {
            return Err(TensorError::ZeroSecret.into());
        }
        policy.validate()?;

        let telemetry = KernelTelemetryTensor::new(mix(secret, 0x5445_4c45))?;
        let shape = telemetry.tensor().shape();
        let analysis = DenseTensor::zeros(shape);
        let consensus_residual = DenseTensor::zeros(shape);
        let energy_shape = TensorShape::new(2, [SUBSYSTEMS as u8, METRICS as u8, 0, 0])?;
        let energy_map = DenseTensor::zeros(energy_shape);
        let scalar_shape = TensorShape::new(1, [1, 0, 0, 0])?;
        let scalar = DenseTensor::zeros(scalar_shape);
        let residual_plan = EinsumPlan::binary(b"tsm,tsm->sm", shape, shape)?;
        let dot_axes = AxisPairs::new(&[0, 1, 2], &[0, 1, 2])?;

        let cp_model = CpModel::new(shape, policy.cp.rank as usize)?;
        let cp_workspace = CpWorkspace::new(shape, policy.cp.rank as usize)?;
        let cp_optimizer_workspace = CpOptimizerWorkspace::new(shape, policy.cp.rank as usize)?;

        let tucker_model = TuckerModel::new(shape, policy.hosvd.ranks)?;
        let hosvd_workspace = HosvdWorkspace::new(shape)?;
        let hooi_workspace = HooiWorkspace::new(shape)?;

        let mut tt_dimensions = [0_u8; 8];
        for mode in 0..shape.order() {
            tt_dimensions[mode] = shape.dimension(mode) as u8;
        }
        let tt_shape = TtShape::new(shape.order(), tt_dimensions)?;
        let tt_input = TtDense::zeros(tt_shape);
        let tt_train = TensorTrain::new(tt_shape);
        let tt_workspace = TtWorkspace::new(tt_shape);

        Ok(Self {
            telemetry,
            analysis,
            consensus_residual,
            energy_map,
            scalar,
            residual_plan,
            dot_axes,
            cp_model,
            cp_workspace,
            cp_optimizer_workspace,
            tucker_model,
            hosvd_workspace,
            hooi_workspace,
            tt_input,
            tt_train,
            tt_workspace,
            mps_workspace: MpsWorkspace::new(),
            peps_patch: Peps2x2::new(2, 2)?,
            policy,
            secret,
            last_online_epoch: 0,
            last_full_epoch: 0,
            last_sgd: None,
            last_certificate: None,
            last_directive: None,
        })
    }

    pub fn observe_manifold(
        &mut self,
        actuation: &crate::manifold_orchestrator::Actuation,
    ) -> Result<TelemetrySnapshot, TensorError> {
        self.telemetry.observe_manifold(actuation)
    }

    pub fn record_external_q24(
        &mut self,
        metric: usize,
        value_q24: i64,
    ) -> Result<(), TensorError> {
        self.telemetry
            .record_q24(super::telemetry::subsystem::EXTERNAL, metric, value_q24)
    }

    pub fn update_online_deferred(&mut self) -> Result<Option<SgdCertificate>, MultilinearError> {
        let snapshot = self.telemetry.snapshot()?;
        if snapshot.occupied_slots < self.policy.minimum_occupied_slots
            || (self.last_online_epoch != 0
                && snapshot.epoch.saturating_sub(self.last_online_epoch)
                    < u64::from(self.policy.online_period_epochs))
            || !self.cp_model.initialized()
        {
            return Ok(None);
        }

        self.telemetry.copy_chronological_into(&mut self.analysis)?;
        let certificate = update_cp_sgd(
            &self.analysis,
            &mut self.cp_model,
            &mut self.cp_optimizer_workspace,
            self.policy.sgd,
            mix(self.secret, SGD_DOMAIN),
        )?;

        self.last_online_epoch = snapshot.epoch;
        self.last_sgd = if certificate.committed {
            Some(certificate)
        } else {
            None
        };
        Ok(Some(certificate))
    }

    pub fn analyze_full_deferred(
        &mut self,
    ) -> Result<Option<MultilinearDirective>, MultilinearError> {
        let snapshot = self.telemetry.snapshot()?;
        if snapshot.occupied_slots < self.policy.minimum_occupied_slots {
            return Ok(None);
        }
        if self.last_full_epoch != 0
            && snapshot.epoch.saturating_sub(self.last_full_epoch)
                < u64::from(self.policy.full_period_epochs)
        {
            return Ok(None);
        }

        self.telemetry.copy_chronological_into(&mut self.analysis)?;
        self.tt_input.copy_from_dense(&self.analysis)?;

        let cp = fit_cp_als(
            &self.analysis,
            &mut self.cp_model,
            &mut self.cp_workspace,
            self.policy.cp,
            mix(self.secret, CP_DOMAIN),
        )?;
        if !cp.verify(
            mix(self.secret, CP_DOMAIN),
            self.policy.maximum_cp_error_q24,
            self.policy.maximum_cp_normal_residual_q24,
        ) {
            return Err(self.reject(
                snapshot,
                MultilinearRejectionReason::Cp,
                cp.relative_error_q24,
                cp.root,
            ));
        }

        let ccd = if cp.relative_error_q24 > self.policy.ccd_trigger_error_q24 {
            let certificate = update_cp_ccd(
                &self.analysis,
                &mut self.cp_model,
                &mut self.cp_optimizer_workspace,
                self.policy.ccd,
                mix(self.secret, CCD_DOMAIN),
            )?;
            if certificate.committed {
                Some(certificate)
            } else {
                None
            }
        } else {
            None
        };
        self.cp_model
            .reconstruct_into(self.cp_workspace.reconstruction_mut())?;

        let hosvd = fit_st_hosvd(
            &self.analysis,
            &mut self.tucker_model,
            &mut self.hosvd_workspace,
            self.policy.hosvd,
            mix(self.secret, HOSVD_DOMAIN),
        )?;
        if !hosvd.verify(
            mix(self.secret, HOSVD_DOMAIN),
            self.policy.maximum_tucker_error_q24,
            self.policy.maximum_orthogonality_q24,
            self.policy.maximum_eigen_residual_q24,
        ) {
            return Err(self.reject(
                snapshot,
                MultilinearRejectionReason::Tucker,
                hosvd.relative_error_q24,
                hosvd.root,
            ));
        }

        let hooi = refine_tucker_hooi(
            &self.analysis,
            &mut self.tucker_model,
            &mut self.hooi_workspace,
            self.policy.hooi,
            mix(self.secret, HOOI_DOMAIN),
        )?;
        if !hooi.verify(
            mix(self.secret, HOOI_DOMAIN),
            self.policy.maximum_tucker_error_q24,
            self.policy.maximum_orthogonality_q24,
            self.policy.maximum_eigen_residual_q24,
        ) {
            return Err(self.reject(
                snapshot,
                MultilinearRejectionReason::Tucker,
                hooi.relative_error_q24,
                hooi.root,
            ));
        }

        let tt = fit_tt_svd(
            &self.tt_input,
            &mut self.tt_train,
            &mut self.tt_workspace,
            self.policy.tt,
            mix(self.secret, TT_DOMAIN),
        )?;
        if !tt.verify(
            mix(self.secret, TT_DOMAIN),
            self.policy.maximum_tt_error_q24,
            self.policy.maximum_orthogonality_q24,
            self.policy.maximum_eigen_residual_q24,
        ) {
            return Err(self.reject(
                snapshot,
                MultilinearRejectionReason::TensorTrain,
                tt.relative_error_q24,
                tt.root,
            ));
        }

        self.build_consensus_residual()?;
        let residual_einsum = execute_binary(
            self.residual_plan,
            &self.consensus_residual,
            &self.consensus_residual,
            &mut self.energy_map,
            mix(self.secret, EINSUM_DOMAIN),
        )?;
        let (suspect_subsystem, suspect_metric, anomaly) = self.locate_anomaly()?;

        let model_tensordot = tensordot_into(
            self.cp_workspace.reconstruction(),
            self.hooi_workspace.reconstruction(),
            self.dot_axes,
            &mut self.scalar,
            mix(self.secret, DOT_DOMAIN),
        )?;
        let cross_disagreement = self.maximum_model_disagreement()?;
        if cross_disagreement > self.policy.maximum_cross_disagreement_q24 {
            return Err(self.reject(
                snapshot,
                MultilinearRejectionReason::CrossDisagreement,
                cross_disagreement,
                model_tensordot.root,
            ));
        }

        let mps_norm_certificate = mps_norm(
            &self.tt_train,
            &mut self.mps_workspace,
            mix(self.secret, MPS_DOMAIN),
        )?;
        let subsystem_diagonal = projector_diagonal(suspect_subsystem as usize);
        let metric_diagonal = projector_diagonal(suspect_metric as usize);
        let mps_subsystem = mps_local_diagonal_expectation(
            &self.tt_train,
            1,
            &subsystem_diagonal,
            &mut self.mps_workspace,
            mix(self.secret, MPS_DOMAIN ^ 1),
        )?;
        let mps_metric = mps_local_diagonal_expectation(
            &self.tt_train,
            2,
            &metric_diagonal,
            &mut self.mps_workspace,
            mix(self.secret, MPS_DOMAIN ^ 2),
        )?;

        self.build_peps_patch(suspect_subsystem as usize, suspect_metric as usize)?;
        let fault_diagonal = [0, fixed::ONE, 0, 0];
        let peps = peps_local_diagonal_expectation(
            &self.peps_patch,
            0,
            &fault_diagonal,
            mix(self.secret, PEPS_DOMAIN),
        )?;

        if !mps_norm_certificate.verify(mix(self.secret, MPS_DOMAIN))
            || !mps_subsystem.verify(mix(self.secret, MPS_DOMAIN ^ 1))
            || !mps_metric.verify(mix(self.secret, MPS_DOMAIN ^ 2))
            || !peps.verify(mix(self.secret, PEPS_DOMAIN))
        {
            return Err(self.reject(
                snapshot,
                MultilinearRejectionReason::TensorNetwork,
                anomaly,
                self.tt_train.root(),
            ));
        }

        let queue_charge = queue_charge(
            anomaly,
            mps_subsystem.normalized_expectation_q24,
            mps_metric.normalized_expectation_q24,
            peps.normalized_expectation_q24,
            self.policy,
        )?;

        let mut directive = MultilinearDirective {
            epoch: snapshot.epoch,
            queue_class: suspect_subsystem,
            queue_charge,
            suspect_subsystem,
            suspect_metric,
            anomaly_q24: anomaly,
            cross_disagreement_q24: cross_disagreement,
            mps_subsystem_mass_q24: mps_subsystem.normalized_expectation_q24,
            mps_metric_mass_q24: mps_metric.normalized_expectation_q24,
            peps_fault_expectation_q24: peps.normalized_expectation_q24,
            cp_model_root: self.cp_model.root(),
            tucker_model_root: self.tucker_model.root(),
            train_root: self.tt_train.root(),
            certificate_root: 0,
            root: 0,
        };
        directive.root = directive_root(mix(self.secret, DIRECTIVE_DOMAIN), &directive);

        let mut certificate = MultilinearCertificate {
            epoch: snapshot.epoch,
            telemetry_root: snapshot.tensor_root,
            analysis_root: self.analysis.root(self.secret)?,
            cp,
            online_sgd: self.last_sgd,
            ccd,
            hosvd,
            hooi,
            tt,
            mps_norm: mps_norm_certificate,
            mps_subsystem,
            mps_metric,
            peps,
            residual_einsum,
            model_tensordot,
            cross_disagreement_q24: cross_disagreement,
            anomaly_q24: anomaly,
            suspect_subsystem,
            suspect_metric,
            directive_root: directive.root,
            root: 0,
        };
        certificate.root = certificate_root(self.secret, &certificate);
        directive.certificate_root = certificate.root;

        if !directive.verify(self.secret) || !certificate.verify(self.secret, self.policy) {
            return Err(self.reject(
                snapshot,
                MultilinearRejectionReason::Certificate,
                anomaly,
                certificate.root,
            ));
        }

        self.last_full_epoch = snapshot.epoch;
        self.last_certificate = Some(certificate);
        self.last_directive = Some(directive);
        Ok(Some(directive))
    }

    pub const fn last_certificate(&self) -> Option<MultilinearCertificate> {
        self.last_certificate
    }

    pub const fn last_directive(&self) -> Option<MultilinearDirective> {
        self.last_directive
    }

    fn build_consensus_residual(&mut self) -> Result<(), TensorError> {
        for index in 0..self.analysis.shape().length() {
            let observed = self.analysis.get_linear(index)?;
            let cp = self.cp_workspace.reconstruction().get_linear(index)?;
            let tucker = self.hooi_workspace.reconstruction().get_linear(index)?;
            let tt = self.tt_workspace.reconstruction().values()[index];

            let residual = observed
                .abs_diff(cp)
                .min(observed.abs_diff(tucker))
                .min(observed.abs_diff(tt));
            self.consensus_residual.set_linear(
                index,
                i64::try_from(residual).map_err(|_| TensorError::Arithmetic)?,
            )?;
        }
        Ok(())
    }

    fn locate_anomaly(&self) -> Result<(u8, u8, i64), TensorError> {
        let mut best_subsystem = 0_usize;
        let mut best_metric = 0_usize;
        let mut best_energy_q24 = 0_i64;

        for subsystem in 0..SUBSYSTEMS {
            for metric in 0..METRICS {
                let energy = self.energy_map.get(&[subsystem, metric, 0, 0])?;
                if energy > best_energy_q24 {
                    best_energy_q24 = energy;
                    best_subsystem = subsystem;
                    best_metric = metric;
                }
            }
        }

        let source_energy = self.analysis.frobenius_squared_q48()?.max(1);
        let selected_q48 = (best_energy_q24.max(0) as u128)
            .checked_shl(fixed::FRACTION_BITS)
            .ok_or(TensorError::Arithmetic)?;
        let anomaly = fixed::ratio_u128(selected_q48, source_energy)?;

        Ok((best_subsystem as u8, best_metric as u8, anomaly))
    }

    fn maximum_model_disagreement(&self) -> Result<i64, TensorError> {
        let source_energy = self.analysis.frobenius_squared_q48()?.max(1);
        let cp_tucker = squared_error_q48(
            self.cp_workspace.reconstruction(),
            self.hooi_workspace.reconstruction(),
        )?;
        let cp_tt = dense_tt_error_q48(
            self.cp_workspace.reconstruction(),
            self.tt_workspace.reconstruction(),
        )?;
        let tucker_tt = dense_tt_error_q48(
            self.hooi_workspace.reconstruction(),
            self.tt_workspace.reconstruction(),
        )?;

        fixed::ratio_u128(cp_tucker.max(cp_tt).max(tucker_tt), source_energy).map_err(Into::into)
    }

    fn build_peps_patch(&mut self, subsystem: usize, metric: usize) -> Result<(), TensorError> {
        let next_subsystem = (subsystem + 1) % SUBSYSTEMS;
        let next_metric = (metric + 1) % METRICS;
        let coordinates = [
            (subsystem, metric),
            (next_subsystem, metric),
            (subsystem, next_metric),
            (next_subsystem, next_metric),
        ];

        let mut energies = [0_i64; 4];
        let mut maximum = 1_i64;
        for (index, (subsystem, metric)) in coordinates.iter().copied().enumerate() {
            energies[index] = self.energy_map.get(&[subsystem, metric, 0, 0])?.max(0);
            maximum = maximum.max(energies[index]);
        }

        let mut probabilities = [0_i64; 4];
        for index in 0..4 {
            probabilities[index] = fixed::div(energies[index], maximum)?.clamp(0, fixed::ONE);
        }

        let horizontal = [
            similarity(probabilities[0], probabilities[1])?,
            similarity(probabilities[2], probabilities[3])?,
        ];
        let vertical = [
            similarity(probabilities[0], probabilities[2])?,
            similarity(probabilities[1], probabilities[3])?,
        ];

        build_binary_peps_patch(
            probabilities,
            horizontal,
            vertical,
            &mut self.peps_patch,
            mix(self.secret, PEPS_DOMAIN ^ 0x4255_494c),
        )
    }

    fn reject(
        &self,
        snapshot: TelemetrySnapshot,
        reason: MultilinearRejectionReason,
        detail_q24: i64,
        evidence_root: u64,
    ) -> MultilinearError {
        let mut rejection = MultilinearRejection {
            epoch: snapshot.epoch,
            reason,
            telemetry_root: snapshot.tensor_root,
            detail_q24,
            evidence_root,
            root: 0,
        };
        rejection.root = rejection_root(self.secret, &rejection);
        MultilinearError::Rejected(rejection)
    }
}

fn projector_diagonal(selected: usize) -> [i64; MAX_TT_DIMENSION] {
    let mut diagonal = [0_i64; MAX_TT_DIMENSION];
    if selected < MAX_TT_DIMENSION {
        diagonal[selected] = fixed::ONE;
    }
    diagonal
}

fn dense_tt_error_q48(dense: &DenseTensor, tt: &TtDense) -> Result<u128, TensorError> {
    if dense.shape().length() != tt.shape().length() || dense.shape().order() != tt.shape().order()
    {
        return Err(TensorError::ShapeMismatch);
    }

    let mut error = 0_u128;
    for index in 0..dense.shape().length() {
        let difference = dense
            .get_linear(index)?
            .checked_sub(tt.values()[index])
            .ok_or(TensorError::Arithmetic)?;
        let magnitude = difference.unsigned_abs() as u128;
        error = error
            .checked_add(
                magnitude
                    .checked_mul(magnitude)
                    .ok_or(TensorError::Arithmetic)?,
            )
            .ok_or(TensorError::Arithmetic)?;
    }
    Ok(error)
}

fn similarity(left_q24: i64, right_q24: i64) -> Result<i64, TensorError> {
    fixed::ONE
        .checked_sub(
            i64::try_from(left_q24.abs_diff(right_q24)).map_err(|_| TensorError::Arithmetic)?,
        )
        .map(|value| value.clamp(0, fixed::ONE))
        .ok_or(TensorError::Arithmetic)
}

fn queue_charge(
    anomaly_q24: i64,
    subsystem_mass_q24: i64,
    metric_mass_q24: i64,
    peps_expectation_q24: i64,
    policy: MultilinearPolicy,
) -> Result<u32, TensorError> {
    if anomaly_q24 <= policy.anomaly_threshold_q24 {
        return Ok(0);
    }

    let confidence = subsystem_mass_q24
        .checked_add(metric_mass_q24)
        .and_then(|value| value.checked_add(peps_expectation_q24))
        .ok_or(TensorError::Arithmetic)?
        / 3;
    let weighted_anomaly = fixed::mul(
        anomaly_q24,
        fixed::HALF
            .checked_add(confidence.clamp(0, fixed::ONE) / 2)
            .ok_or(TensorError::Arithmetic)?,
    )?;

    let numerator = weighted_anomaly
        .checked_sub(policy.anomaly_threshold_q24)
        .ok_or(TensorError::Arithmetic)?
        .max(0);
    let denominator = fixed::ONE
        .checked_sub(policy.anomaly_threshold_q24)
        .ok_or(TensorError::Arithmetic)?;
    let fraction = fixed::div(numerator, denominator)?.clamp(0, fixed::ONE);
    let charge = (fraction as u128)
        .checked_mul(policy.maximum_queue_charge as u128)
        .ok_or(TensorError::Arithmetic)?
        >> fixed::FRACTION_BITS;

    Ok(charge.min(policy.maximum_queue_charge as u128) as u32)
}

fn certificate_root(secret: u64, certificate: &MultilinearCertificate) -> u64 {
    let mut state = mix(secret, certificate.epoch);
    state = mix(state, certificate.telemetry_root);
    state = mix(state, certificate.analysis_root);
    state = mix(state, certificate.cp.root);
    state = mix(
        state,
        certificate.online_sgd.map(|value| value.root).unwrap_or(0),
    );
    state = mix(state, certificate.ccd.map(|value| value.root).unwrap_or(0));
    state = mix(state, certificate.hosvd.root);
    state = mix(state, certificate.hooi.root);
    state = mix(state, certificate.tt.root);
    state = mix(state, certificate.mps_norm.root);
    state = mix(state, certificate.mps_subsystem.root);
    state = mix(state, certificate.mps_metric.root);
    state = mix(state, certificate.peps.root);
    state = mix(state, certificate.residual_einsum.root);
    state = mix(state, certificate.model_tensordot.root);
    state = mix(state, certificate.cross_disagreement_q24 as u64);
    state = mix(state, certificate.anomaly_q24 as u64);
    state = mix(
        state,
        certificate.suspect_subsystem as u64 | ((certificate.suspect_metric as u64) << 8),
    );
    mix(state, certificate.directive_root)
}

fn directive_root(secret: u64, directive: &MultilinearDirective) -> u64 {
    let mut state = mix(secret, directive.epoch);
    state = mix(
        state,
        directive.queue_class as u64 | ((directive.queue_charge as u64) << 8),
    );
    state = mix(
        state,
        directive.suspect_subsystem as u64 | ((directive.suspect_metric as u64) << 8),
    );
    state = mix(state, directive.anomaly_q24 as u64);
    state = mix(state, directive.cross_disagreement_q24 as u64);
    state = mix(state, directive.mps_subsystem_mass_q24 as u64);
    state = mix(state, directive.mps_metric_mass_q24 as u64);
    state = mix(state, directive.peps_fault_expectation_q24 as u64);
    state = mix(state, directive.cp_model_root);
    state = mix(state, directive.tucker_model_root);
    mix(state, directive.train_root)
}

fn rejection_root(secret: u64, rejection: &MultilinearRejection) -> u64 {
    let mut state = mix(secret, rejection.epoch);
    state = mix(state, rejection.reason as u8 as u64);
    state = mix(state, rejection.telemetry_root);
    state = mix(state, rejection.detail_q24 as u64);
    mix(state, rejection.evidence_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_charge_is_bounded() {
        let policy = MultilinearPolicy::KERNEL_DEFAULT;
        let charge = queue_charge(fixed::ONE, fixed::ONE, fixed::ONE, fixed::ONE, policy).unwrap();
        assert_eq!(charge, policy.maximum_queue_charge);
    }

    #[test]
    fn projector_selects_one_physical_state() {
        let diagonal = projector_diagonal(3);
        assert_eq!(diagonal[3], fixed::ONE);
        assert_eq!(diagonal[2], 0);
    }
}
