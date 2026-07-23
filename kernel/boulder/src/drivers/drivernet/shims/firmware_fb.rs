use crate::drivers::drivernet::model::DriverStrategy;
use crate::drivers::drivernet::registry::{
    PROBE_EVIDENCE_FIRMWARE_LEASE, PROBE_EVIDENCE_HEALTH, ProbeProgram, ProbeSemantic,
    SHIM_FLAG_READ_ONLY_PROBE, SHIM_FLAG_SUPPORTS_ROLLBACK, SHIM_FLAG_TERMINAL_FALLBACK,
    ShimDescriptor, VendorGate, empty_steps, firmware_required_topology, step,
};

pub const fn descriptor() -> ShimDescriptor {
    let mut steps = empty_steps();
    steps[0] = step(
        ProbeSemantic::VerifyFirmwareLease,
        PROBE_EVIDENCE_FIRMWARE_LEASE,
        64,
        0,
        0,
    );
    steps[1] = step(
        ProbeSemantic::EstablishHealthBaseline,
        PROBE_EVIDENCE_HEALTH,
        64,
        0,
        0,
    );

    ShimDescriptor {
        strategy: DriverStrategy::FirmwareFramebuffer,
        name: "firmware-framebuffer",
        abi_version: 2,
        vendor_gate: VendorGate::ANY,
        required_topology: firmware_required_topology(),
        forbidden_topology: 0,
        minimum_confidence_q16: 0,
        flags: SHIM_FLAG_READ_ONLY_PROBE
            | SHIM_FLAG_SUPPORTS_ROLLBACK
            | SHIM_FLAG_TERMINAL_FALLBACK,
        activation_budget_ticks: 1_024,
        health_budget_ticks: 512,
        program: ProbeProgram::new(
            steps,
            2,
            PROBE_EVIDENCE_FIRMWARE_LEASE | PROBE_EVIDENCE_HEALTH,
            256,
        ),
    }
}
