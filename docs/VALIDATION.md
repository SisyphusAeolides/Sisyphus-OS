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
bus mastering disabled, one retained reset-ready root, and zero mutation debts.
Those children remain deferred until real DMA rings, interrupts, and USB
enumeration exist.

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
