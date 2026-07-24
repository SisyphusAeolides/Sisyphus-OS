# Validation Record

## Checks completed in the reconstruction environment

The following checks passed on the returned tree:

```text
git diff whitespace/error check
dependency-free Rust delimiter and module audit
tracked patch-debris and compiler-artifact audit
production unfinished-marker audit
module-wide dead-code-suppression audit
C11 freestanding reference-driver compilation
GPU C ABI static layout assertions
repository provenance/trailer scan
```

Reference-driver command:

```sh
scripts/check-driver-abi.sh
```

## Formal authority gate

The bare-metal build consumes `target/formal/verified.lock`. Produce it with:

```sh
scripts/bootstrap-formal-toolchains.sh
```

The bootstrap verifies pinned release hashes before executing Idris2 0.8.0 and
Agda 2.8.0. `scripts/check-formal-models.sh` rejects unresolved holes, unsafe
escape hatches, compiler-version drift, generated files in `formal/`, and any
source digest that is not represented by the emitted attestation.

A clean-room bootstrap was also exercised from an empty temporary prefix: it
self-hosted the pinned Idris2 compiler through Chez Scheme, verified the pinned
Agda binary, checked both total Idris2 modules and the safe Agda privilege
model, and emitted the attestation consumed by the kernel build.

The QEMU boot assertion additionally observes the live IST stack-switch probe,
the formal authority root, the calibrated one-shot APIC deadline owner and its
periodic transition, the certified Ring 3 PID1 transfer, a timer-issued
safe-point preemption receipt, and PID1's bounded recovery transition.
It also checks the conservative all-function device census, exact xHCI tuple
selection, retained PCI configuration evidence, and the queryable
detected/operational/quarantined binding reconciliation before PID1 entry.
The QEMU machine includes an xHCI controller with keyboard and tablet children;
the boot check requires a reset-ready controller with a measured 16 KiB BAR0,
two lease-decoded protocol bodies covering four USB2 and four USB3 ports, bus
mastering disabled, one retained reset-ready root, and zero mutation debts.
The fixed-capacity DMA arena, cycle-last command/event ring, and halted
register-programming bridge are covered by focused Rust tests. The Q35 lane
also freshly re-proves that the routed VT-d unit is disabled, allocates the
real arena, enables its single-requester domain, maps every present controller
DMA region (requiring a complete scratchpad pair when scratchpads exist),
programs and scrubs the halted controller, then revokes and releases the
domain, tables, and every frame. It remains a reversible preparation epoch: a
persistent DMA domain, Run/Stop, interrupts, and USB enumeration are still
absent. The same lane also performs the real PCI bus-master
enable/readback/revoke transaction, then revalidates and restores the exact
reset-ready controller before the port census.
Before that transition, the QEMU boot lane performs a read-only halted-port
census and currently observes two connected root ports; this is evidence only,
not USB child enumeration or input-driver support.
The VT-d substrate now also has an exact-range IOVA reservation and
`map_dma_at` path; tests prove an IOVA==physical mapping is retained and
cannot be relocated or overlapped. The xHCI binding retains each mapping
receipt through release debt, so a failed teardown can be retried without
reconstructing authority. This is preparation for the scoped requester
witness, not a claim that the default no-DMAR QEMU lane is isolated.
VT-d construction additionally rejects nonempty context entries across every
PCI bus before it installs the sole requester context; a stale entry on any
bus therefore blocks activation rather than escaping the isolation check.
The production direct-map table allocator applies the same all-bus rule before
reclaiming its pinned root/context pages, preventing table release while a
nonzero-bus translation entry remains live.
Include-all DRHD units have a separate policy type: only segment-zero,
resolved, endpoint-free firmware routing may enter it. Resolved shared DRHD
units also have an explicit policy type: the selected requester must be one of
that unit's exact endpoints. Backend tests prove that either policy starts
from empty tables and publishes exactly one requester context before
translation.
The device census keeps PCI segment identity; the current VT-d backend rejects
nonzero-segment requesters explicitly until its requester and context-table
model carries segment-aware identity end to end.
The dedicated Q35 + Intel IOMMU lane measures QEMU's actual one-unit DMAR
shape (seven explicit endpoints) and selects its xHCI endpoint through the
shared-unit policy. It exercises the reversible VT-d enable/map/revoke/release
epoch above, but remains deferred: it is not a persistent DMA domain, bus
mastering, Run/Stop, interrupt routing, or USB child enumeration.

## Required validation on the receiving machine

Run in this order:

```sh
python3 scripts/static-repository-audit.py
scripts/check-driver-abi.sh
scripts/check-formal-models.sh
cargo fmt --all --check
cargo check --workspace
cargo test --workspace
cargo check -p crest --features os-bin
cargo user-push
cargo kernel
scripts/test-boot.sh
```

Any failure should be repaired before describing the corresponding subsystem as
complete.

## Required hardware qualification

Native GPU promotion additionally requires, per supported generation:

```text
cold boot and warm reboot
IOMMU isolation proof
BAR ownership and bounds
DMA mapping and teardown
interrupt delivery and masking
firmware authentication
mode set and scanout
reset and post-reset health
fault injection and rollback
resource leak accounting
```
