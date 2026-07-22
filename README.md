# Sisyphus OS

Sisyphus OS is a Rust-first operating system organized around narrow, explicit
boundaries between memory management, the kernel, drivers, system libraries,
and userland.

## Architecture

- `core/abyss`: physical and virtual memory primitives.
- `core/aether`: explicit effect machines, bounded flight recording, and policy execution.
- `core/blacklab`: logical-time, inference, and adaptation planning.
- `core/kairos`: ABI negotiation, normalized machine profiles, and object authority.
- `kernel/boulder`: kernel core and C driver host.
- `libraries/driver-abi`: canonical Rust definition of the stable C driver ABI.
- `libraries/slope`: syscall, shared-memory, DMA-ring, time, and storage contracts.
- `userland/push`: measured PID 1, bounded supervision, and capability policy.
- `userland/corinth`: bounded package DNA, synthesis, integration, and swarm transport.
- `userland/crest`: display/input contracts and fixed-point semantic rendering.

## Exotic Subsystems

Sisyphus OS embraces chaotic, biological, and quantum-mechanical metaphors for operating system design:
- **Wormhole IPC**: Causal time-reversal message passing using CTC (Closed Timelike Curve) ring buffers and holographic semantic hashing.
- **Chronovore**: A time-eating entropy engine that harvests CPU jitter to feed a Rule 30 cellular automaton, searching for temporal crystals to predict optimal preemption windows.
- **Eigenthread Scheduler**: A workload Hamiltonian scheduler that models processes as quantum states, dynamically recalculating their coupling energy to solve for the optimal ground-state co-schedule.
- **Fabric Weave**: A causal spacetime execution graph that tracks all kernel events as nodes in a light cone, detecting causal violations and deadlocks via Kahn's topological sort.
- **Schrödinger Core**: A quantum speculative execution sandbox that physically bifurcates the execution state to evaluate both sides of an uncertain branch simultaneously, collapsing the wave function only when the truth is observed.
- **VoidFS**: A filesystem modeled as a Kerr rotating black hole. Files orbit an accretion disk and slowly spiral into an event horizon where they are spaghettified into a 2D holographic hash, eventually evaporating as Hawking radiation into the Chronovore engine.
- **Fractal Page Tables**: An iterated function system (IFS) memory space where virtual addresses are mapped to physical frames via Mandelbrot-like chaotic dynamics on the complex plane, rendering ROP chains mathematically impossible.
- **Macrophage IPC Firewall**: A biological immune system where patrolling white blood cells inspect IPC channels, phagocytize malicious packets, and broadcast extracted antigens to trigger apoptosis in rogue processes.
- **Symbiosis Scheduler**: Inspired by mitochondrial evolution, this module forces highly communicative processes into endosymbiosis—quantum-entangling their page tables and permanently collapsing their IPC overhead into a single eukaryotic host.
- **Prometheus Morphic Transpiler**: Auto-detects foreign C driver ABIs via machine-code prologue analysis and generates live JIT trampolines to seamlessly bridge Windows/Linux/BSD calling conventions to the kernel on the fly.
- **Golem Behavioral Fingerprinter**: A no_std Naive Bayes classifier that watches a transpiled driver's first 1000 syscalls to deduce its hardware class (GPU, NIC, Storage) without source code, automatically granting exactly the right capability subsets.
- **Lazarus Membrane**: A self-healing transactional barrier surrounding the driver ABI. If a closed-source driver crashes mid-execution, Lazarus rolls back the kernel state and re-animates the driver from its last known-good checkpoint—literally bringing dead code back to life.

## C driver contract

```text
C DRIVER BLOB (.so / .ko / .dll / .elf)
  │
  ▼
PROMETHEUS: scan_elf64_for_entry() → PrologueDecoder → CallingConv detected
  │  if MsX64: gen_msx64_to_sysv thunk (live x86 machine code)
  │  if SysV:  gen_passthrough thunk
  ▼
KernelApi vtable handed to driver with all fn pointers wrapped through...
  │
  ▼
LAZARUS: arm_watchdog() → driver calls mmio_map / dma_alloc / irq_register
  │  each call journaled → on crash: rollback() → resurrect() → re-probe
  │
  ▼
GOLEM: records every KernelApi call → after 1024 calls: classify()
  │  NaiveBayes → DriverArchetype → recommended_caps() + irq_core_hint()
  │  kernel auto-updates DriverDescriptor.required_capabilities
  ▼
DRIVER IS ALIVE, CLASSIFIED, CRASH-PROOF — ZERO SOURCE CODE REQUIRED
```

The kernel exposes a versioned function table to drivers. Drivers receive no
Rust types and no direct access to kernel data structures. Every public
structure starts with ABI metadata, all resources use opaque handles, and
optional services are advertised through capability bits and nullable function
pointers.

This contract supports portable freestanding C drivers written for Sisyphus.
Drivers written for Linux, BSD, Windows, or another kernel still need an API
compatibility layer because their source code calls that kernel's internal
interfaces.

The canonical C header is
`kernel/boulder/include/sisyphus/driver.h`. A linked reference driver lives in
`kernel/boulder/drivers/reference` and is exercised by the Rust test suite.

Boulder also contains compatibility foundations for externally built C code:

- bounds-checked ELF64 relocatable-object and shared-object validation;
- a W^X load-plan validator and bounded AMD64 relocation engine;
- an explicit external-symbol allowlist and a small versioned Linux KPI subset;
- serialized C device and network vtable adapters;
- backend-gated IOMMU domains, deferred hotplug events, and device typestates.

The Mirage personality registry selects a versioned object format, calling
convention, service table, and external-symbol allowlist for each foreign
environment. The current Linux personalities expose a deliberately small
symbol subset. Windows NT personalities expose Win64 pool allocation/free and
a bounded PE32+ load-plan validator, while IRP layouts remain behind a
version-specific opaque bridge. FreeBSD personalities fail closed until their
native contracts and service implementations are installed. Mirage thunk pages
follow a writable-then-executable lifecycle;
cross-ABI thunk generation remains disabled until complete register, stack,
floating-point, and unwind translation is available.

VT-d table support uses 128-bit root and context entries with explicit
context-cache and IOTLB invalidation hooks. Translation activation remains
behind a platform backend so firmware DMAR discovery and capability checks
must succeed before hardware registers are touched.

These components do not claim that an arbitrary foreign kernel module can run
unchanged. Linux, BSD, and Windows drivers depend on large, version-specific
kernel interfaces and execution assumptions. Each compatibility personality
must resolve and validate those contracts before executable module loading is
enabled.

`DriverHost` derives its capability mask and callback table directly from the
installed services. A capability cannot be advertised without its backend.
`AbyssAllocator` connects the C allocation callbacks to Abyss's bootstrap bump
allocator. The default early-boot host intentionally exposes logging only;
MMIO, DMA, IRQ, clocks, sleeping, and device publication become visible to
drivers only after Boulder installs their initialized Ring 0 services.

## Local checks

```sh
cargo check --workspace
cargo test -p boulder
cargo user-push
cargo kernel
```

The custom target is intentionally not the workspace default, which keeps host
tests usable. Current Rust releases require nightly for JSON targets. Boot-target
builds use `x86_64-sisyphus.json` with custom `core`, `alloc`, and
`compiler_builtins`;
that stage also requires the `rust-src` component.

## Booting Boulder

Boulder boots through Multiboot2, creates an identity map for the first GiB,
enters x86-64 long mode, and reports early boot state on COM1. GRUB's memory map
is validated and copied into Abyss before the bootstrap heap is selected. Abyss
then builds a reclaiming bitmap frame allocator with typed reservations for low
memory, the kernel, boot information, heap, and allocator metadata. Boulder also
installs a higher-half physical alias and a dedicated cache-disabled MMIO window.
The bootstrap CPU then installs a 256-entry IDT, remaps and masks the legacy
8259 PIC, and exposes generation-checked IRQ registrations to C drivers. APIC
capability is detected at boot, the local xAPIC is enabled through the uncached
MMIO window, and a self-IPI validates local routing. Boulder validates the
Multiboot ACPI root pointer, traverses the RSDT or XSDT, parses the MADT, applies
interrupt-source overrides, and programs every I/O APIC redirection entry from
a masked state. The local APIC timer is calibrated against PIT channel 2 and
must deliver repeated periodic interrupts before boot can complete. Legacy PCI
configuration-space discovery records all present functions while respecting
multifunction headers.

MADT processor records retain firmware UIDs, APIC/x2APIC IDs, enabled state,
and online-capable state. Boulder assigns the boot processor to Aegis, reserves
up to two discovered APs for Mirage enclaves, and leaves a compute core when
the machine is large enough. APs remain in the discovered state until a later
INIT/SIPI startup handshake marks each one online. Role enforcement is an
explicit execution authorization check; it does not disable the interrupts
needed for timers, IPIs, or watchdog delivery.

The real-time scheduler implements fixed-capacity EDF admission for independent,
preemptible, implicit-deadline periodic tasks using conservative integer
utilization accounting. Runtime budgets, absolute deadlines, missed releases,
and overruns are reported explicitly. RTM wrappers are CPUID-gated rollback
aids only and are not treated as memory-security boundaries.

Boulder has a compile-time architecture contract for CPU identity, local
counter sampling, interrupt-state preservation, local TLB invalidation, and idle
behavior. The x86-64 backend is implemented; additional architecture backends
remain gated until their boot, interrupt-controller, and page-table paths are
complete. Scoped authority proofs make privileged operations explicit without
claiming that kernel bootstrap cannot construct the root authority.

The bounded heterogeneous fabric routes fixed-size work descriptors to CPU,
firmware, copy, compute, media, or remote nodes by capability and NUMA domain.
Node queues and work slots use generation-checked handles, explicit completion
transitions, and bounded capacity. The initial implementation serializes
metadata and is restricted to thread context; interrupt handlers must defer
work through an IRQ handoff queue.

Aether provides allocation-free effect state machines whose handlers fail
closed on unknown operations, a bounded single-writer flight recorder with
stable logical tickets, and a policy VM with verified registers and branches,
bounded execution fuel, serialized program replacement, and explicit
host-call registration. Boulder supplies architecture timestamps and CPU IDs
and gates policy installation through scoped authority. Software provenance,
speculative journals, and linear session IPC remain disabled until their
cross-CPU ownership and revocation models are complete.

Kairos strictly negotiates self-describing ABI layouts and feature
intersections, constructs bounded machine profiles from Boulder's real ACPI,
boot-memory, and PCI observations, and synthesizes immutable machine and NUMA
domain snapshots. Its shared C wire layouts are defined once in `core/kairos`.
Boulder exposes bounded topology-query and ABI-negotiation syscalls that copy
through validated user-writable mappings without placing the 70 KiB topology
reply on the kernel entry stack. Slope retains the raw reply in process-wide
storage, validates counts, domain membership, parents, and feature grants, and
provides zero-allocation CPU/domain iterators, workload affinity hints, and
CPU-proportional NUMA work partitions. The kernel advertises only implemented
features, so required capabilities fail closed while optional capabilities are
reported as unavailable. Slope also centralizes argv/environment collapse and
Kairos setup in `ProcessRuntime` for binaries using the process-entry stack ABI.
Boulder now allocates a retained multi-page user stack, materializes
`[argc][argv][envp]` with C strings, and passes its base through the Ring 3
entry trampoline. The remaining integration seam is the kernel ABI-reply
user-copy path; until that is corrected, Push reports the accepted entry ABI
and continues its bounded supervisor loop without claiming topology negotiation
succeeded. Kairos's internal object table uses
non-cloneable, generation-checked handles with rights attenuation and opaque
payload handles. Raw-pointer message transport and mutable global profile
publication remain disabled pending per-process handle tables and
revocation-safe ownership transfer.

Black Lab evaluates a checked rational logical-time model, stores bounded
semantic memory relationships through object handles and page numbers, ranks
validated hardware-personality transforms, and emits resonance plans from
immutable snapshots. Its fixed-shape INT8 inference uses checked configuration,
i64 accumulators, and bounded operation counts. Thermal forecasts become power
advice only after offline validation metadata passes the configured quality
gate. Its Q16.16 PA-I learner publishes atomic weight snapshots and provides
telemetry only; it cannot authorize memory access. Resonance outputs remain
advisory and never directly remap memory or reconfigure hardware. Aion's
bounded evolution chamber uses an explicit deterministic seed, scores each
forecast once against later observations, preserves four elites, and requires
minimum evidence before producing a new generation. Evolved candidates remain
non-actionable until independently validated through the thermal quality gate.
Echidna represents temporary cross-address-space sharing as generation-checked,
expiring metadata leases; the memory manager must separately validate and map
their opaque object handles. Tartarus tracks retired ranges in software and
returns quarantine decisions while placing learning samples on a bounded queue.
It neither overloads architecture page-table bits nor trains a model in an
exception handler.
Oureboros is a fixed-capacity measured-artifact catalog, not a replacement VFS
or a claim that arbitrary binaries can be reconstructed from tiny random seeds.
It verifies immutable boot artifacts against independently embedded SHA-256
manifests and clears generated output on failure. GRUB supplies the separately
built `push` ELF as a named Multiboot2 module; Boulder reserves that physical
range, verifies its exact size, digest, and entry-point manifest, and rejects
dynamic-linker requirements, non-user addresses, overlapping or oversized
segments, and writable-executable mappings. The address-space installer uses
zeroed staging pages, bounded copies, initialized-data and BSS verification,
final W^X sealing, commit-after-seal ordering, and abort on every intermediate
failure. Its hardware-format four-level hierarchy inherits only the upper
kernel PML4 half and is retained for the lifetime of PID 1.
Boulder now enters through a physical low bootstrap island, transfers code and
stack execution to the higher half, removes PML4 entry zero, and checks that
transition before initializing the IDT. During serialized bootstrap it also
switches to PID 1's frame-backed CR3, confirms execution survives solely on the
inherited higher-half mappings, and restores the kernel root. The process
hierarchy is now retained with a separate zeroed RW+NX user stack. A higher-half
GDT and 64-bit TSS provide SYSRET-compatible DPL3 selectors and a dedicated
RSP0 entry stack. The kernel programs EFER/STAR/LSTAR/FMASK and enters native
syscalls on a separate 16 KiB kernel stack. The bounded write path walks every
user page through the active hierarchy, requires user permission at all four
levels, copies at most 256 bytes through the retained direct map, and never
dereferences a raw user pointer. Boulder switches to Push's CR3 and transfers
permanently to its measured ET_EXEC entry. Push then exercises native bounded
write and cooperative-yield syscalls from a persistent Ring 3 supervision loop.
Manifest measurements provide authenticity only when the manifest root is
independently protected and replicated.

Push models Slope-Net, Corinth, and Crest as a fixed dependency graph with
bounded restart/backoff policy, saturating per-service failure mass, telemetry,
and a deadlock detector that observes stalled launch work rather than mistaking
a stable healthy system for failure. Gordian request pages use explicit atomic
states and return opaque generation-checked capability handles. The kernel
hardware broker and process-spawn capability are not implemented, so the
current boot deliberately reports launch failure and proves the critical
recovery path instead of claiming child services are running.

Slope provides bounded syscall wrappers, revocation-aware pointer resolution,
borrow-scoped shared-memory access, split-ownership DMA rings, cooperative
network futures, and opaque pinned-block storage contracts. Corinth validates
fixed-capacity package DNA, drives backend-supplied code lowering and artifact
publication, commits dependency updates transactionally, and advances swarm
assimilation one verified fragment per poll. Crest keeps display, input, and
framebuffer authority behind backend traits. Its Obsidian path validates a
bounded fixed-point SDF instruction set; Heliosphere consumes telemetry
snapshots, and the orbital cortex solves Kepler's equation with a fixed six-step
integer Newton budget. None of these userland contracts grant raw MMIO, NVMe,
HID, or CRTC pointers.

The ignition sequence is a protocol-neutral phase guard around Boulder's
existing GRUB/Multiboot2 handoff. It requires validated boot information,
memory, topology, subsystems, interrupt routing, and the measured PID 1 install
before declaring userland ready. A future Limine entry can feed the same guard
without adding a competing entry symbol. Scheduler-owned process exit,
preemptive user scheduling, per-process syscall capabilities, the hardware
capability broker, and measured child-process spawn/wait remain incomplete.

```sh
rustup component add rust-src --toolchain nightly
scripts/build-iso.sh
scripts/run-qemu.sh
scripts/test-boot.sh
```

The ISO path is `target/sisyphus-os.iso`. Building it requires GRUB and
`xorriso`; running it requires `qemu-system-x86_64`. `test-boot.sh` succeeds
only after COM1 proves measured installation, permanent Ring 3 transfer, native
Push syscalls, and bounded transition into recovery mode.
