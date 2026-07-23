use crate::drivers::drivernet::fingerprint::TOPOLOGY_VIRTUAL_MACHINE;
use crate::drivers::drivernet::model::DriverStrategy;
use crate::drivers::drivernet::registry::{
    PROBE_EVIDENCE_DISPLAY_ENGINE, PROBE_EVIDENCE_HEALTH, PROBE_EVIDENCE_IDENTITY,
    PROBE_EVIDENCE_TRANSPORT, ProbeProgram, ProbeSemantic, SHIM_FLAG_PRESERVE_FIRMWARE_DISPLAY,
    SHIM_FLAG_READ_ONLY_PROBE, SHIM_FLAG_SUPPORTS_ROLLBACK, SHIM_FLAG_VIRTUAL_DEVICE,
    ShimDescriptor, VendorGate, empty_steps, step,
};

pub const fn descriptor() -> ShimDescriptor {
    let mut steps = empty_steps();
    steps[0] = step(
        ProbeSemantic::ValidateIdentity,
        PROBE_EVIDENCE_IDENTITY,
        32,
        0,
        0,
    );
    steps[1] = step(
        ProbeSemantic::VerifyParavirtualCapability,
        PROBE_EVIDENCE_TRANSPORT,
        256,
        1,
        0,
    );
    steps[2] = step(
        ProbeSemantic::VerifyDisplayEngine,
        PROBE_EVIDENCE_DISPLAY_ENGINE,
        256,
        1,
        0,
    );
    steps[3] = step(
        ProbeSemantic::EstablishHealthBaseline,
        PROBE_EVIDENCE_HEALTH,
        256,
        1,
        0,
    );

    ShimDescriptor {
        strategy: DriverStrategy::VirtualSvga,
        name: "virtual-svga",
        abi_version: 2,
        vendor_gate: VendorGate::ANY,
        required_topology: TOPOLOGY_VIRTUAL_MACHINE,
        forbidden_topology: 0,
        minimum_confidence_q16: 4_000,
        flags: SHIM_FLAG_PRESERVE_FIRMWARE_DISPLAY
            | SHIM_FLAG_READ_ONLY_PROBE
            | SHIM_FLAG_SUPPORTS_ROLLBACK
            | SHIM_FLAG_VIRTUAL_DEVICE,
        activation_budget_ticks: 8_192,
        health_budget_ticks: 2_048,
        program: ProbeProgram::new(
            steps,
            4,
            PROBE_EVIDENCE_IDENTITY
                | PROBE_EVIDENCE_TRANSPORT
                | PROBE_EVIDENCE_DISPLAY_ENGINE
                | PROBE_EVIDENCE_HEALTH,
            1_024,
        ),
    }
}
