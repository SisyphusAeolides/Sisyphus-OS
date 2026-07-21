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

Boulder also contains compatibility foundations for externally built C code:

- bounds-checked ELF64 relocatable-object and shared-object validation;
- a W^X load-plan validator and bounded AMD64 relocation engine;
- an explicit external-symbol allowlist and a small versioned Linux KPI subset;
- serialized C device and network vtable adapters;
- backend-gated IOMMU domains, deferred hotplug events, and device typestates.

The Mirage personality registry selects a versioned object format, calling
convention, service table, and external-symbol allowlist for each foreign
environment. The current Linux personalities expose a deliberately small
symbol subset. Windows NT and FreeBSD personalities fail closed until their
native object loaders, structure contracts, and service implementations are
installed. Mirage thunk pages follow a writable-then-executable lifecycle;
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

```sh
rustup component add rust-src --toolchain nightly
scripts/build-iso.sh
scripts/run-qemu.sh
scripts/test-boot.sh
```

The ISO path is `target/sisyphus-os.iso`. Building it requires GRUB and
`xorriso`; running it requires `qemu-system-x86_64`. `test-boot.sh` succeeds
only after the expected early-boot milestone appears on COM1.
