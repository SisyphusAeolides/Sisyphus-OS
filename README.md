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
| Interrupt foundation | Implemented | IDT, dedicated #DF/NMI/#MC IST stacks, runtime stack-switch probe, xAPIC, I/O APIC routing, PIT-calibrated one-shot deadline ownership and periodic transition |
| PID 0 execution authority | Implemented foundation | Non-process identity, epoch leases, atomic idle/reselection handoff, non-termination invariants |
| Timer scheduling | Implemented safe-point preemption | Lock-free IRQ ticket, generation/epoch revalidation, bounded syscall consumption, stale-ticket rejection |
| Measured PID 1 transfer | Implemented | Static ELF validation, W^X mapping, CPU-local return lease, single-use Ring 3 transition certificate |
| Ring-domain authority | Implemented foundation | Unique non-kernel CR3s, valid IRETQ/SYSRETQ matrix, scoped Ring 1 hardware grants, PID1 production consumer |
| Formal authority models | Implemented | Total Idris2 lifecycle/package models, safe Agda privilege model, hash-bound build and PID1 authority attestation |
| General process creation | Fail-closed | Lifecycle registry exists; `spawn` and `wait` remain unavailable until retained address-space ownership and context switching are complete |
| Firmware display | Implemented | Multiboot framebuffer evidence, retained object, bounded MMIO mapping, write/read verification |
| Native GPU activation | Fail-closed | Compatibility proof and probe evidence exist; activation requires real generation-specific BAR, DMA, interrupt, reset, and firmware backends |
| Hermes GSP transport | Implemented foundation | Versioned wire ABI, compatibility manifests, bounded rings, deadline admission, reply tracking, fault states |
| Crest software compositor | Library foundation | Fixed-point scene evaluation, damage tiles, deterministic first-light frame; no retained input/presentation session loop yet |
| Crest hardware presentation | In progress | Firmware scanout exists in Boulder; a general userland present lease is not yet complete |
| Spectral resource geometry | Implemented | Hodge 1-skeleton, normalized-Laplacian Fiedler cut, fixed-point heat flow, periodic recomputation |
| Foreign driver personalities | Version-scoped | Object validation and narrow service tables exist; unsupported contracts reject loading |
| Automatic device census | Implemented foundation | Retained identity/class/command/BAR evidence, exact tuple claims, live authorization, queryable terminal records |
| xHCI reset-ready transport | Implemented prerequisite | Retained live claim, bounded firmware handoff/halt, exact BAR0 lease, lease-backed USB2/USB3 port map, reset-ready audit root; fixed-capacity DMA/ring machinery is now proven in isolation, while controller binding, interrupts, and USB children remain deferred |
| Functional-source gate | Implemented | Rejects tracked recovery debris, unfinished markers, simulated success, and production dead-code suppression |

A subsystem should be described as complete only after the exact commit passes
host tests, custom-target builds, and the relevant boot or hardware assertion.

## Architecture

```text
                 checked Idris2/Agda models     measured boot image
                            │                          │
                            └────────────┬─────────────┘
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

formal/
  idris2/       total driver-lifecycle and package-transaction models
  agda/         safe privilege-ring and transition model
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

The boot device census keeps the raw PCI class tuple, command state, and BAR
assignments intact across every function, including display-plus-audio
multifunction devices. It deliberately
labels `02/80` as an other network controller rather than Wi-Fi,
multimedia-video as a controller rather than a camera, and xHCI as a USB host
rather than a keyboard or trackpad. Route manifests mask and match the exact
class/subclass/programming-interface tuple before issuing a non-cloneable,
live-slot authorization. Each display route is claimed before probe, then
committed or quarantined from DriverNet's measured result. A failed
rollback is terminal and prevents every later candidate from touching that
requester.

The first USB transport step is similarly narrow and real. Boulder claims an
xHCI-class PCI function before access, revalidates its identity, command state,
class tuple, header type, and every BAR word against the retained census, then
decodes BAR0 without pretending its length is known. A provisional one-access
MMIO transport reads the capability header and a bounded, current-relative
extended-capability header chain. The journal is sealed to the live,
non-cloneable authorization; pre-aperture code does not parse capability bodies.

The retained APIC deadline owner then drives a bounded takeover machine. When
the optional USB legacy capability exists, Boulder claims firmware ownership,
masks every legacy SMI enable and verifies the mask by readback. It waits for
CNR to clear, drains observed port resets (all ports when no leased protocol
body is available), and proves HCHalted. Only at that quiescent point may a
serialized PCI transaction disable decode and bus mastering, size all BAR
words, restore them with readback checks, and issue an exact BAR0 aperture
lease. Every later register access is derived from that lease. Reset completion
must re-establish CNR clear, HCHalted, and HSE clear before Boulder retains a
reset-ready controller root; any post-mutation failure instead retains explicit
mutation debt. QEMU currently proves a 16 KiB aperture and bus mastering off.
After reset, a second walk may read complete Supported Protocol bodies only
through ranges derived from that aperture lease. It validates the BCD protocol
revision, PSI tail, slot type, and non-overlapping MaxPorts coverage, then folds
the USB2/USB3 port map into the retained controller root. QEMU proves two
protocol capabilities covering four USB2 and four USB3 ports.
The fixed-capacity DMA arena and cycle-last command/event ring are now real
Rust machinery with generation-bound storage, self-link/ERST geometry, exact
completion correlation, and rollback-oriented tests. A consuming runtime seed
and halted-register programming bridge now retain the PCI/BAR/protocol proof
chain through DCBAAP/CRCR/CONFIG/ERST/ERDP preparation. The Intel-IOMMU
Q35 lane now freshly proves its routed unit disabled, allocates the real DMA
arena, enables a single-requester VT-d domain, maps every present controller
DMA region, enables PCI bus mastering, and retains both authorities across a
bounded Run-to-Halted session. It scrubs the runtime registers, resets the
controller, re-observes ready/Halted/HSE-free state, revokes bus mastering,
then revokes the mappings and releases every domain/table/frame resource.
Scratchpad backing,
when required by a controller, is accepted only as a complete pointer-array
and buffer pair. The bounded VT-d ledger covers the full xHCI maximum of 1,023
scratchpad buffers as six exact DMA spans without weakening the per-page
translation or teardown receipts. This is a reversible proof of the complete
DMA data path. The exact PCI bus-master enable/readback/revoke transaction
restores the retained reset-ready controller before the port census. No
interrupter lifecycle, command completion, USB enumeration, or input support
is claimed yet.
While still halted and before DMA, Boulder performs a bounded root-port census;
the QEMU lane currently measures two connected ports without treating them as
enumerated children.
The VT-d substrate now supports exact IOVA reservation and `map_dma_at`,
allowing a future scoped requester domain to prove IOVA==physical mappings
without relocating them; the default no-DMAR lane remains deliberately
deferred.

## Formal authority bridge

The dependent-type models are build inputs, not essays. Idris2 checks total
driver lifecycle and package transaction witnesses without holes or totality
escape hatches. Driver matching proves equality between the observed and
selected PCI class tuples rather than accepting a Boolean assertion. Agda
checks the privilege-ring transition model under `--safe`
and `--without-K`, without postulates or imported libraries.
The driver lifecycle model also rejects unresolved, nonzero-segment, and
misrouted DMAR scopes, and represents a published context by both its selected
requester and target so another requester's context cannot be constructed.

The checker emits an attestation containing the exact SHA-256 root of each
accepted source. A bare-metal Boulder build fails if that attestation is absent,
stale, built by a different pinned compiler version, or contradicts the current
sources. The kernel embeds the three roots, validates their combined authority
root at boot, and folds it into PID1's capability root. This binds the running
authority to the exact typechecked models; it does not claim automatic theorem
extraction into Rust.

The privilege model also makes the long-mode boundary explicit: Ring 1 and
Ring 2 must use address-space roots distinct from Ring 0, `SYSRETQ` may return
only to Ring 3, and direct hardware grants belong only to a bounded Ring 1
domain. Boulder's fixed-capacity domain registry and single-use transition
certificates mirror those constraints in the runtime path.

## Process model

Boulder represents PID 0 as a non-process execution authority with a nonzero,
epoch-bound identity. It cannot acquire a user-return lease, terminate, be
reaped, or alias a reused process generation. The idle wake path either renews
PID0 atomically or transfers authority to a runnable user process.

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

Syscall entry no longer uses global scratch stacks or a global saved user RSP.
The BSP owns a registered CPU-local record, unique TSS binding, entry nesting
state, and generation-checked return lease. Application processors remain
offline until their own GDT, TSS, IST, GS bases, and syscall MSRs can be
published transactionally.

Before periodic scheduling begins, Boulder calibrates the bootstrap processor's
local APIC countdown against the exact programmed PIT divisor. A non-cloneable,
CPU-bound owner can issue one generation-checked relative deadline at a time,
split intervals wider than the 32-bit APIC counter without early expiry, and is
then consumed into periodic mode. This provides the bounded early-boot clock
needed for hardware ownership transitions; it is intentionally not advertised
as a continuous monotonic epoch.

Once periodic mode begins, the local APIC interrupt publishes a lock-free PID,
generation, and scheduler-epoch ticket. Syscall safe points revalidate that
ticket under the scheduler lock, service at most one bounded scheduling pass,
and preserve the interrupted call's return value. This is real deferred
preemption, but not yet a direct interrupt-frame process switch: that promotion
waits for per-process XSAVE and FS/GS ownership plus complete interrupt
return-state capture.

General `spawn` and `wait` are intentionally fail-closed today. Completing them
requires full interrupt-frame context switching, parent wakeup, exact resource
reclamation, and application-processor startup. PID allocation alone is not
treated as execution.

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

The formal toolchains and Rust nightly are exact-version pinned. On Linux
x86_64, the bootstrap command downloads hash-verified Idris2 and Agda releases,
builds Idris2 with Chez Scheme, checks all models, and emits the build
attestation:

```sh
scripts/bootstrap-formal-toolchains.sh
```

If the pinned compilers are already installed, run the narrower gate directly:

```sh
scripts/check-formal-models.sh
```

Bare-metal checks use `nightly-2026-07-20` and `rust-src` from
`rust-toolchain.toml`:

```sh
cargo user-push
cargo kernel
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
