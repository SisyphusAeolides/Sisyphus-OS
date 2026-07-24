# Sisyphus OS

**A Rust-first research operating system built around measured boot, explicit
capabilities, bounded state, mathematical control, and fail-closed hardware
activation.**

Sisyphus OS is intentionally experimental. Its unusual names are not a promise
of magic: every production path is expected to identify its inputs, authority,
resource bounds, failure behavior, observable result, and validation evidence.
Unsupported operations remain unavailable rather than reporting work that did
not happen.

## Current engineering status

| Area | Status | Evidence |
|---|---|---|
| Multiboot2 long-mode boot | Implemented | Memory map, modules, ACPI roots, framebuffer tag, COM1 boot trace |
| Physical memory | Implemented | Typed reservations and bounded bitmap frame allocation |
| Interrupt foundation | Implemented | IDT, PIC fallback, xAPIC, I/O APIC routing, timer calibration |
| Measured PID 1 transfer | Implemented | Static ELF validation, W^X mapping, retained user stack, Ring 3 entry |
| General process creation | Fail-closed | Lifecycle registry exists; `spawn` and `wait` remain unavailable until retained address-space ownership and context switching are complete |
| Firmware display | Implemented | Multiboot framebuffer evidence, retained object, bounded MMIO mapping, write/read verification |
| Native GPU activation | Fail-closed | Compatibility proof and probe evidence exist; activation requires real generation-specific BAR, DMA, interrupt, reset, and firmware backends |
| Hermes GSP transport | Implemented foundation | Versioned wire ABI, compatibility manifests, bounded rings, deadline admission, reply tracking, fault states |
| Crest software compositor | Implemented | Fixed-point scene evaluation, damage tiles, deterministic first-light frame |
| Crest hardware presentation | In progress | Firmware scanout exists in Boulder; a general userland present lease is not yet complete |
| Spectral resource geometry | Implemented | Hodge 1-skeleton, normalized-Laplacian Fiedler cut, fixed-point heat flow, periodic recomputation |
| Foreign driver personalities | Version-scoped | Object validation and narrow service tables exist; unsupported contracts reject loading |
| Functional-source gate | Implemented | Rejects tracked recovery debris, unfinished markers, simulated success, and production dead-code suppression |

A subsystem should be described as complete only after the exact commit passes
host tests, custom-target builds, and the relevant boot or hardware assertion.

## Architecture

```text
                           measured boot image
                                  │
                                  ▼
┌──────────────────────────────── BOULDER ────────────────────────────────┐
│ Multiboot2  ACPI  PCI  memory  interrupts  syscalls  capabilities      │
│      │       │    │      │         │          │           │            │
│      └───────┴────┴──────┴─────────┴──────────┴───────────┘            │
│                                  │                                      │
│                    evidence-bearing kernel state                        │
│                                  │                                      │
│     ┌────────────────────────────┼────────────────────────────┐         │
│     ▼                            ▼                            ▼         │
│ Drivernet                 Manifold control              Nexus runtime   │
│ compatibility proofs      certified queue pressure      lease admission │
│ transactional brokers     predictive containment        rollback/replay │
└─────┬────────────────────────────┬────────────────────────────┬─────────┘
      │                            │                            │
      ▼                            ▼                            ▼
  driver ABI                    Slope ABI                  measured PID 1
      │                            │                            │
      └────────────────────────────┴────────────────────────────┘
                                   │
                     ┌─────────────┼─────────────┐
                     ▼             ▼             ▼
                   Push         Corinth         Crest
                supervision     synthesis     compositor
```

### Repository map

```text
core/
  abyss/        physical and virtual memory primitives
  aether/       bounded effects, policy execution, causal recording
  blacklab/     logical-time models, inference, adaptation proposals
  kairos/       ABI negotiation, topology snapshots, object authority

kernel/
  boulder/      boot, memory, interrupts, scheduling, drivers, syscalls

libraries/
  driver-abi/   stable C and GPU compatibility wire contracts
  slope/        user/kernel ABI, shared pages, time, service calculus

userland/
  push/         measured PID 1 and bounded supervision
  corinth/      package synthesis and integration machinery
  crest/        fixed-point desktop and damage-tracked compositor
  cerebral/     shared control-plane client

tools/
  reality-gate/ source functionality ledger and façade detection
```

## Black-lab control mathematics

The current upgrade unifies Boulder command admission and Crest frame planning
around one allocation-free service-calculus core in
`libraries/slope/src/service_calculus.rs`.

### Deterministic min-plus service bound

A rate-latency service curve provides a hard delay envelope for a new job with
backlog `q`:

```text
windows(q) = ceil((q + 1) / minimum_completions_per_window)
deterministic_delay(q) = latency + windows(q) × window_ticks
```

Admission is rejected if the complete delay cannot fit the caller's deadline.
The controller also enforces a bounded arrival envelope and maximum live
backlog.

### One-sided conformal calibration

Completed work contributes only positive underprediction residuals. A bounded
order statistic supplies an uncertainty guard without assuming Gaussian noise:

```text
residual = max(0, observed_delay - predicted_delay)
guard = conformal_quantile(last N residuals) + fixed_slack
```

The same fixed-capacity implementation is consumed by:

- Hermes GSP command admission in Boulder;
- Crest's frame oracle and first-light diagnostic.

### Drift-plus-penalty pressure

A virtual Lyapunov backlog records sustained under-service. It raises admission
pressure when work completes later than the certified bound and drains as
service windows advance. This does not replace the deterministic safety bound;
it adds an adaptive penalty while keeping the hard admission rule explicit.

Every accepted job receives a sealed admission certificate containing:

```text
reservation sequence
window position
backlog and arrival count
deterministic delay
conformal uncertainty guard
drift penalty
deadline slack
service-curve root
calibration root
certificate root
```

## Spectral resource geometry

Boulder treats resource relationships as an executable discrete geometry, not
as decorative terminology. The Manifold orchestrator constructs a bounded
Hodge complex from the live resource quiver and applies several independent
operators to it:

```text
normalized graph Laplacian     Lsym = I - D^(-1/2) A D^(-1/2)
Fiedler vector                 deterministic resource bipartition
Hodge heat flow                bounded load diffusion
Čech first cohomology          cycle and obstruction evidence
tropical critical path        max-plus dependency pressure
```

The Fiedler solver uses allocation-free Q16.16 arithmetic, a deterministic
seed, degree-weighted removal of the null mode, normalized adjacency
iteration, and a Rayleigh quotient for algebraic connectivity. Manifold
recomputes the partition every 32 control epochs and exports the resulting
mask and connectivity value as actuation evidence. A bridge-cut test verifies
that two dense regions are separated across their lightest connection.

These mechanisms are classical numerical algorithms implemented in Rust. The
quantum-prefixed Crest interfaces describe discrete observations, sealed state
transitions, and bounded command channels; they do not claim quantum hardware.
Future quantum algorithms belong here only when they have an executable model,
a production consumer, explicit resource bounds, and falsifiable tests.

## GPU universality without false universality

Sisyphus OS does not select a driver from a vendor ID alone. The portable GPU
ABI describes both measured device evidence and a driver's compatibility
obligations:

```text
PCI identity and class
revision interval
BAR type and minimum size
topology requirements
IOMMU isolation
required and optional features
firmware-surface policy
architecture hint
```

`GpuCompatibilityProof` records satisfied, missing, and violated obligations.
A strategy is eligible only when its complete proof accepts. Native activation
still remains unavailable unless a generation-specific backend can establish
real isolation, transport, reset, interrupt, and health evidence.

The firmware framebuffer is a real fallback, not a fabricated driver. Boulder
parses the Multiboot2 direct-color framebuffer tag, creates a generation-checked
object, maps at most one MiB for the boot signature, performs volatile writes,
reads verification samples back, unmaps the aperture, and reports a sealed
image root.

The boot-domain schedule binds the measured image, boot counter, PCI inventory,
and framebuffer transcript. It provides deterministic domain separation and
transcript binding; it is not secret entropy unless a platform randomness
source is mixed into the root material.

## Crest: deterministic first light

The Crest executable performs a real bounded rendering diagnostic:

1. construct two fixed-point signed-distance scenes;
2. compile a Hilbert-ordered tile schedule;
3. obtain a conformal frame-time plan;
4. render a complete frame;
5. feed the measured counter delta back into the frame oracle;
6. invalidate a partial rectangle;
7. re-plan and render only the affected tiles;
8. publish frame roots, plan roots, predicted ticks, and the learned guard.

Expected serial evidence has this shape:

```text
[CREST] first-light PASS root0=0x... root1=0x... tiles=N/M \
frames=2 skipped=N plan0=0x... plan1=0x... predicted=N/M guard=N
```

This proves software composition and adaptive frame planning. It does not claim
that a standalone Crest process is already presenting through every native GPU.

## Driver contract

The stable C driver boundary is defined in:

```text
kernel/boulder/include/sisyphus/driver.h
libraries/driver-abi/
```

Drivers receive a versioned function table, opaque handles, and only the
capabilities backed by initialized kernel services. The early host exposes
logging; MMIO, DMA, IRQ, clocks, sleeping, and publication are advertised only
when their backends exist.

Foreign personalities are explicit and version-scoped. A Linux, BSD, or
Windows driver depends on that kernel's internal object, synchronization,
allocation, interrupt, and unwind contracts. Unresolved contracts reject the
module. Cross-ABI thunks must prove register, stack, floating-point, and unwind
translation before executable use.

## Process model

Boulder can install and enter the measured Push image as PID 1. The lifecycle
registry is fixed-capacity and generation-checked; it admits a runnable process
only after receiving a complete launch record containing:

```text
address-space root
user entry and stack
kernel stack
image measurement root
capability root
service class and priority
```

General `spawn` and `wait` are intentionally fail-closed today. Completing them
requires retained per-process address spaces, kernel stacks, saved trap
contexts, timer-driven selection, CR3/TSS switching, parent wakeup, and exact
resource reclamation. PID allocation alone is not treated as execution.

## Build and validation

Host and workspace checks:

```sh
cargo check --workspace
cargo test --workspace
cargo check -p crest --features os-bin
python3 scripts/static-repository-audit.py
cargo run -p sisyphus-reality-gate -- \
  --root . \
  --ledger target/sisyphus-functionality-ledger.tsv
```

Bare-metal checks require nightly and `rust-src`:

```sh
cargo +nightly user-push
cargo +nightly kernel
scripts/test-boot.sh
```

The complete workflow also rejects tracked compiler archives, patch recovery
files, production dead-code and unused-code suppression, unfinished macros,
and explicit simulated-success markers.

## Experimental Rust from Boulder to Crest

Sisyphus OS is meant to look unfamiliar because its mechanisms are unusually
composed, not because its vocabulary is mysterious. Boulder turns topology,
cohomology, spectral partitions, service curves, compatibility proofs, and
measured boot evidence into bounded kernel decisions. Slope carries those
decisions across versioned contracts. Push supervises the resulting services.
Crest converts the same discipline into scene transactions, Hilbert-ordered
damage schedules, conformal frame plans, and deterministic presentation
evidence.

Unconventional work is welcome when it has conventional accountability. Every
production subsystem must provide:

```text
purpose
production caller
measured input
observable output
bounded memory and execution
explicit authority
failure and rollback behavior
tests
target evidence
```

Real mathematics may be speculative. Kernel success paths may not be.

## Engineering records

- [`docs/UPGRADE_REPORT.md`](docs/UPGRADE_REPORT.md) — integrated design and control mathematics
- [`docs/FUNCTIONAL_STATUS.md`](docs/FUNCTIONAL_STATUS.md) — subsystem promotion matrix
- [`docs/VALIDATION.md`](docs/VALIDATION.md) — completed checks and required target gates
