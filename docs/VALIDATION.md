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

## Tooling unavailable in the reconstruction environment

The environment did not contain:

```text
rustc
cargo
rustfmt
QEMU
GRUB image tooling
```

Therefore Rust compilation, Rust tests, custom-target construction, and boot
success are not claimed by this report.

## Required validation on the receiving machine

Run in this order:

```sh
python3 scripts/static-repository-audit.py
scripts/check-driver-abi.sh
cargo fmt --all --check
cargo check --workspace
cargo test --workspace
cargo check -p crest --features os-bin
cargo +nightly user-push
cargo +nightly kernel
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
