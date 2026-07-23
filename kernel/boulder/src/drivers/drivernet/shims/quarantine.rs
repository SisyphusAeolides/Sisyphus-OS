use crate::drivers::drivernet::model::DriverStrategy;
use crate::drivers::drivernet::registry::{
    PROBE_EVIDENCE_HEALTH, ProbeProgram, ProbeSemantic, SHIM_FLAG_READ_ONLY_PROBE,
    SHIM_FLAG_TERMINAL_FALLBACK, ShimDescriptor, VendorGate, empty_steps, step,
};

pub const fn descriptor() -> ShimDescriptor {
    let mut steps = empty_steps();
    steps[0] = step(
        ProbeSemantic::EstablishHealthBaseline,
        PROBE_EVIDENCE_HEALTH,
        32,
        0,
        0,
    );

    ShimDescriptor {
        strategy: DriverStrategy::Quarantine,
        name: "quarantine",
        abi_version: 2,
        vendor_gate: VendorGate::ANY,
        required_topology: 0,
        forbidden_topology: 0,
        minimum_confidence_q16: 0,
        flags: SHIM_FLAG_READ_ONLY_PROBE | SHIM_FLAG_TERMINAL_FALLBACK,
        activation_budget_ticks: 256,
        health_budget_ticks: 256,
        program: ProbeProgram::new(steps, 1, PROBE_EVIDENCE_HEALTH, 64),
    }
}
