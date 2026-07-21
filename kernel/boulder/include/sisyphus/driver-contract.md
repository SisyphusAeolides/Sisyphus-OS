# Sisyphus C Driver ABI Contract

The header in this directory is the source-level contract for freestanding C
drivers. The matching Rust layouts live in the `sisyphus-driver-abi` crate.

## Compatibility rules

1. The upper 16 bits of `abi_version` are the major version. A major mismatch
   is incompatible. The lower 16 bits are additive minor revisions.
2. `struct_size` says how many leading bytes are valid. New fields are appended;
   existing fields are never reordered, removed, or repurposed.
   `out_driver_size` is the writable descriptor capacity supplied by the
   kernel; a driver must never write beyond it.
3. Drivers must require only capabilities they actually need. They must check a
   capability bit and its function pointer before calling an optional service.
4. Integer widths are explicit. Public enums are represented as `uint32_t`,
   status values as `int32_t`, and resource identities as opaque `uint64_t`
   handles. No compiler-specific C bitfields cross the boundary.
5. A pointer plus a length represents every byte string and buffer. Text is
   UTF-8 unless a field explicitly says otherwise. NUL termination is never
   implied.
6. Memory remains owned by the side that supplied it unless a function states
   otherwise. Driver names and callback pointers must remain valid while the
   driver is loaded. Device descriptions remain valid only for the callback.
7. A successful resource-creation call writes every output. On failure, outputs
   are unspecified and must not be consumed.
8. IRQ callbacks may run concurrently and must not block. Calls not documented
   as IRQ-safe are forbidden from an IRQ callback.
9. Drivers are trusted Ring 0 components. ABI validation catches mistakes, not
   hostile pointers or malicious code.
10. A driver must pass the exact `kernel_context` value from its API table back
    to every kernel callback. Flags not defined by the negotiated ABI revision
    must be zero.

## Porting other kernels' drivers

This ABI can host any architecture-neutral C driver written to this contract.
A Linux driver cannot be linked directly merely because it is C: it depends on
Linux symbols, object lifetimes, locking, device models, and scheduler rules.
Such drivers need a separate Linux compatibility module that translates those
interfaces into this ABI.
