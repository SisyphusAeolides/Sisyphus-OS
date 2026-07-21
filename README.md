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
- `libraries/slope`: safe userland syscall surface.
- `userland/push`: init and service supervision.
- `userland/corinth`: networking and package management.
- `userland/crest`: compositor and input routing.

## C driver contract

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
```

The custom target is intentionally not the workspace default, which keeps host
tests usable. Current Rust releases require nightly for JSON targets. Boot-target
builds use `x86_64-sisyphus.json` with a custom `core` and `compiler_builtins`;
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
domain snapshots. Its internal object table uses non-cloneable,
generation-checked handles with rights attenuation and opaque payload handles.
Raw-pointer message transport, mutable global profile publication, and a
user-visible syscall token encoding remain disabled pending per-process handle
tables and revocation-safe ownership transfer.

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
Oureboros is currently a fixed-capacity deterministic artifact catalog, not a
replacement VFS. It unfolds versioned recipes into caller-owned writable
buffers, checks their exact SHA-256 manifest measurements, and clears output on
failure. A verified-artifact token keeps the measured source buffer immutably
borrowed while Boulder prepares it. The first executable recipe emits a fixed,
minimal x86-64 ET_DYN image whose information is part of the generator; it is
preparation evidence rather than a compressed arbitrary user program. The PID
1 preparation path rejects dynamic-linker requirements, non-user addresses,
oversized images, unreadable executable segments, and manifest/ELF entry-point
disagreement. Executable-class output is never transferred directly to
control. Boulder now defines a transactional address-space backend contract
that requires zeroed staging mappings, bounded copies, initialized-data and BSS
verification, final W^X sealing, commit-after-seal ordering, and abort on every
intermediate failure. The x86-64 bootstrap backend now allocates real physical
frames, constructs a hardware-format four-level user hierarchy, inherits only
the upper kernel PML4 half, keeps staging leaves non-present, enables EFER.NXE,
and publishes final user PTEs only after data and BSS verification. The current
validation path is bounded to 64 user pages and 128 total owned frames; it
reclaims the complete PID 1 hierarchy and proves stale-handle rejection. It
does not load the new root into CR3 or claim runtime isolation because Boulder
is still linked in low memory. Manifest measurements provide authenticity only
when the manifest root is independently protected and replicated.
The ignition sequence is a protocol-neutral phase guard around Boulder's
existing GRUB/Multiboot2 handoff. It requires validated boot information,
memory, topology, subsystems, and interrupt routing in order before declaring
the kernel online. A future Limine entry can feed the same guard without adding
a competing entry symbol. Userland remains explicitly not ready even though a
measured PID 1 image now passes static executable-format preparation: high-half
kernel relocation, retained per-process root ownership, relocation
policy, TSS privilege stacks, syscall entry, scheduling, and Ring 3 transfer
are not yet complete.

```sh
rustup component add rust-src --toolchain nightly
scripts/build-iso.sh
scripts/run-qemu.sh
scripts/test-boot.sh
```

The ISO path is `target/sisyphus-os.iso`. Building it requires GRUB and
`xorriso`; running it requires `qemu-system-x86_64`. `test-boot.sh` succeeds
only after the expected early-boot milestone appears on COM1.
