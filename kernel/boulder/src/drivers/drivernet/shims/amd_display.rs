use crate::drivers::drivernet::fingerprint::VENDOR_AMD;
use crate::drivers::drivernet::model::DriverStrategy;
use crate::drivers::drivernet::registry::{
    PROBE_EVIDENCE_DISPLAY_ENGINE, PROBE_EVIDENCE_HEALTH, PROBE_EVIDENCE_IDENTITY,
    PROBE_EVIDENCE_IOMMU_DOMAIN, PROBE_EVIDENCE_MMIO_DECODE, PROBE_EVIDENCE_RESET_PATH,
    ProbeProgram, ProbeSemantic, SHIM_FLAG_PRESERVE_FIRMWARE_DISPLAY, SHIM_FLAG_READ_ONLY_PROBE,
    SHIM_FLAG_REQUIRES_EXCLUSIVE_DEVICE, SHIM_FLAG_SUPPORTS_ROLLBACK, ShimDescriptor, VendorGate,
    empty_steps, native_required_topology, step,
};

pub const fn descriptor() -> ShimDescriptor {
    let mut steps = empty_steps();
    steps[0] = step(
        ProbeSemantic::ValidateIdentity,
        PROBE_EVIDENCE_IDENTITY,
        64,
        0,
        0,
    );
    steps[1] = step(
        ProbeSemantic::VerifyIommuIsolation,
        PROBE_EVIDENCE_IOMMU_DOMAIN,
        128,
        0,
        0,
    );
    steps[2] = step(
        ProbeSemantic::MapControlBarReadOnly,
        PROBE_EVIDENCE_MMIO_DECODE,
        256,
        0,
        0,
    );
    steps[3] = step(
        ProbeSemantic::VerifyDisplayEngine,
        PROBE_EVIDENCE_DISPLAY_ENGINE,
        768,
        2,
        0,
    );
    steps[4] = step(
        ProbeSemantic::VerifyResetMechanism,
        PROBE_EVIDENCE_RESET_PATH,
        512,
        1,
        0,
    );
    steps[5] = step(
        ProbeSemantic::EstablishHealthBaseline,
        PROBE_EVIDENCE_HEALTH,
        512,
        1,
        0,
    );

    ShimDescriptor {
        strategy: DriverStrategy::AmdDisplay,
        name: "amd-display",
        abi_version: 2,
        vendor_gate: VendorGate {
            vendor_id: VENDOR_AMD,
            device_id_mask: 0,
            device_id_value: 0,
        },
        required_topology: native_required_topology(),
        forbidden_topology: 0,
        minimum_confidence_q16: 8_000,
        flags: SHIM_FLAG_PRESERVE_FIRMWARE_DISPLAY
            | SHIM_FLAG_READ_ONLY_PROBE
            | SHIM_FLAG_REQUIRES_EXCLUSIVE_DEVICE
            | SHIM_FLAG_SUPPORTS_ROLLBACK,
        activation_budget_ticks: 24_576,
        health_budget_ticks: 8_192,
        program: ProbeProgram::new(
            steps,
            6,
            PROBE_EVIDENCE_IDENTITY
                | PROBE_EVIDENCE_IOMMU_DOMAIN
                | PROBE_EVIDENCE_MMIO_DECODE
                | PROBE_EVIDENCE_DISPLAY_ENGINE
                | PROBE_EVIDENCE_RESET_PATH
                | PROBE_EVIDENCE_HEALTH,
            3_072,
        ),
    }
}
