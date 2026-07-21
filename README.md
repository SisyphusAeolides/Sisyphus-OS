# Sisyphus OS

Sisyphus OS is a Rust-first operating system organized around narrow, explicit
boundaries between memory management, the kernel, drivers, system libraries,
and userland.

## Architecture

- `core/abyss`: physical and virtual memory primitives.
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
MMIO window, and a self-IPI validates local routing. External I/O APIC routing
waits for ACPI MADT discovery.

```sh
rustup component add rust-src --toolchain nightly
scripts/build-iso.sh
scripts/run-qemu.sh
scripts/test-boot.sh
```

The ISO path is `target/sisyphus-os.iso`. Building it requires GRUB and
`xorriso`; running it requires `qemu-system-x86_64`. `test-boot.sh` succeeds
only after the expected early-boot milestone appears on COM1.
