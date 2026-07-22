//! Shared early-process initialization for Sisyphus userland binaries.

use crate::env::{EnvSnapshot, QuantumArgv, QuantumEnv};
use crate::kairos::{
    CpuAffinityHint, KairosBootError, KairosInit, WorkPartition, WorkloadClass, features,
};

pub const DEFAULT_REQUIRED_FEATURES: u64 =
    features::SYSCALL_BASIC;
pub const DEFAULT_OPTIONAL_FEATURES: u64 = features::ASYNC_IO
    | features::THERMAL_PAGE
    | features::KAIROS_PAGE
    | features::OFFLOAD_DISPATCH
    | features::HOLOGRAM_FS;
pub const DEFAULT_WORK_UNITS: usize = 1024;

pub struct ProcessRuntime {
    pub argv: QuantumArgv,
    pub environment: EnvSnapshot,
    pub kairos: KairosInit,
    pub affinity: CpuAffinityHint,
    pub partition: WorkPartition,
}

impl ProcessRuntime {
    /// Performs the common argument, environment, ABI, and topology sequence.
    ///
    /// # Safety
    ///
    /// `stack_ptr` must reference Boulder's documented process-entry stack
    /// layout for the lifetime of the process.
    pub unsafe fn initialize(
        stack_ptr: *const u8,
        required_features: u64,
        optional_features: u64,
        workload: WorkloadClass,
        work_units: usize,
    ) -> Result<Self, KairosBootError> {
        // SAFETY: The caller establishes the process-entry stack contract.
        let argv = unsafe { QuantumArgv::from_stack(stack_ptr) };
        // SAFETY: `envp_base` derives from the same validated entry layout.
        let environment_view = unsafe { QuantumEnv::from_ptr(argv.envp_base()) };
        let environment = EnvSnapshot::collapse(&environment_view);
        let kairos = KairosInit::run(required_features, optional_features)?;
        let affinity = kairos.topology.compute_affinity(workload);
        let partition = kairos.topology.partition_work(work_units);
        Ok(Self {
            argv,
            environment,
            kairos,
            affinity,
            partition,
        })
    }

    /// Performs initialization with the standard native-process policy.
    ///
    /// # Safety
    ///
    /// The requirements are identical to [`Self::initialize`].
    pub unsafe fn initialize_native(stack_ptr: *const u8) -> Result<Self, KairosBootError> {
        // SAFETY: Forwarded unchanged from the caller.
        unsafe {
            Self::initialize(
                stack_ptr,
                DEFAULT_REQUIRED_FEATURES,
                DEFAULT_OPTIONAL_FEATURES,
                WorkloadClass::Compute,
                DEFAULT_WORK_UNITS,
            )
        }
    }
}
