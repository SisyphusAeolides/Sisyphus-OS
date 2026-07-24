# Functional Status Matrix

Status meanings:

- **Integrated** — real implementation and production caller exist.
- **Foundation** — bounded implementation exists, but target integration is
  incomplete.
- **Fail-closed** — public operation rejects use until its backend contract is
  present.
- **Research** — real algorithm exists; production effect must be demonstrated
  before promotion.

| Subsystem | Status | Current evidence | Remaining promotion gate |
|---|---|---|---|
| Multiboot2 framebuffer discovery | Integrated | Validated tag parser and boot reservation | QEMU and hardware mode matrix |
| Firmware scanout object | Integrated | Retained generation, bounded mapping, write/read verification | Broader firmware-format and platform tests |
| GPU compatibility ABI | Integrated | Rust/C layouts, C11 static assertions, proof tests | Independent ABI runner on target architectures |
| Native GPU activation | Fail-closed | Evidence and compatibility gate | Generation-specific BAR, DMA, IRQ, reset, firmware backend |
| Hermes normalized transport | Foundation | Versioned codec boundary, bounded rings, pending correlation | Real platform implementation and device qualification |
| Hermes service admission | Integrated foundation | Min-plus bound, arrival envelope, conformal guard, rollback | Transport workload and fault-injection tests |
| Crest software compositor | Integrated | Fixed-point scene, damage tracking, partial schedule tests | Target executable build and first-light run |
| Crest frame oracle | Integrated foundation | Three policy lanes, conformal feedback, sealed plans | Hardware presentation timing feedback |
| Crest general presentation | Fail-closed/in progress | Boulder firmware scanout is real | Capability lease, protected mapping, present receipt, device-loss recovery |
| Measured PID 1 | Integrated foundation | Static image validation and Ring 3 transfer path | Re-run complete boot suite on exact commit |
| General process creation | Fail-closed | Fixed-capacity lifecycle admission record | Retained address spaces, kernel stacks, context switching, wait queues |
| Tensor and predictive control | Foundation | Bounded fixed-point implementations and queue integration | Workload comparison and target timing evidence |
| Foreign driver personalities | Fail-closed by contract | Bounded ABI and object validation | Explicit supported-version service implementations |

No row should be promoted solely because its source compiles. Promotion requires
its caller, target test, resource bounds, and failure path to pass on the exact
commit.
