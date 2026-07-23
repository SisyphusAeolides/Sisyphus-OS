use crate::drivers::drivernet::fingerprint::{TOPOLOGY_VIRTUAL_MACHINE, VENDOR_VIRTIO};
use crate::drivers::drivernet::model::DriverStrategy;
use crate::drivers::drivernet::registry::{
    PROBE_EVIDENCE_COMMAND_RING, PROBE_EVIDENCE_HEALTH, PROBE_EVIDENCE_IDENTITY,
    PROBE_EVIDENCE_IOMMU_DOMAIN, PROBE_EVIDENCE_TRANSPORT, ProbeProgram, ProbeSemantic,
    SHIM_FLAG_PRESERVE_FIRMWARE_DISPLAY, SHIM_FLAG_READ_ONLY_PROBE, SHIM_FLAG_SUPPORTS_ROLLBACK,
    SHIM_FLAG_VIRTUAL_DEVICE, ShimDescriptor, VendorGate, empty_steps, step,
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
        ProbeSemantic::VerifyIommuIsolation,
        PROBE_EVIDENCE_IOMMU_DOMAIN,
        64,
        0,
        0,
    );
    steps[2] = step(
        ProbeSemantic::VerifyParavirtualCapability,
        PROBE_EVIDENCE_TRANSPORT,
        256,
        1,
        0,
    );
    steps[3] = step(
        ProbeSemantic::VerifyCommandRingGeometry,
        PROBE_EVIDENCE_COMMAND_RING,
        256,
        0,
        0,
    );
    steps[4] = step(
        ProbeSemantic::EstablishHealthBaseline,
        PROBE_EVIDENCE_HEALTH,
        256,
        1,
        0,
    );

    ShimDescriptor {
        strategy: DriverStrategy::VirtioGpu,
        name: "virtio-gpu",
        abi_version: 2,
        vendor_gate: VendorGate {
            vendor_id: VENDOR_VIRTIO,
            device_id_mask: 0,
            device_id_value: 0,
        },
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
            5,
            PROBE_EVIDENCE_IDENTITY
                | PROBE_EVIDENCE_IOMMU_DOMAIN
                | PROBE_EVIDENCE_TRANSPORT
                | PROBE_EVIDENCE_COMMAND_RING
                | PROBE_EVIDENCE_HEALTH,
            1_024,
        ),
    }
}
