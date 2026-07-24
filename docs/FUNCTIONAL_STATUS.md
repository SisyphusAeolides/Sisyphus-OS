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
| PID 0 authority | Integrated foundation | Epoch-bound non-process identity, atomic idle handoff, timer-issued safe-point tickets, non-termination tests | Full interrupt-context process switch |
| Timer safe-point preemption | Integrated foundation | APIC IRQ publication, PID/generation/epoch revalidation, bounded syscall consumption, stale-ticket rejection | Per-process XSAVE and FS/GS ownership for direct IRQ switching |
| Measured PID 1 | Integrated foundation | Static image validation and certified Ring 3 transfer observed in QEMU | Retained process resources and general process switching |
| CPU-local privilege entry | Integrated foundation | Per-CPU record, unique TSS binding, GS syscall entry, return leases | AP startup and per-CPU GDT/TSS/IST publication |
| BSP fault containment | Integrated foundation | Separate #DF/NMI/#MC IST stacks, descriptor validation, live NMI-vector stack-switch probe | Guard pages and per-AP GDT/TSS/IST publication |
| Ring-domain authority | Integrated foundation | Distinct non-kernel CR3 roots, bounded Ring 1 hardware grants, certified IRETQ/SYSRETQ matrix, PID1 consumer | Isolated Ring 1/2 images and syscall-broker integration |
| Idris2/Agda authority models | Integrated | Total/safe compiler gates, source attestation, build rejection, PID1 root binding, privilege-root and transition proofs | Generated ABI witnesses and broader driver-lifecycle refinement |
| General process creation | Fail-closed | Fixed-capacity lifecycle admission record | Retained address spaces, kernel stacks, context switching, wait queues |
| Tensor and predictive control | Foundation | Bounded fixed-point implementations and queue integration | Workload comparison and target timing evidence |
| Foreign driver personalities | Fail-closed by contract | Bounded ABI and object validation | Explicit supported-version service implementations |

No row should be promoted solely because its source compiles. Promotion requires
its caller, target test, resource bounds, and failure path to pass on the exact
commit.
