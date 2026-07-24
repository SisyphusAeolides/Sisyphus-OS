# Measured GPU and Adaptive Display Upgrade

This report describes the integrated upgrade applied after commit
`3ac1393c4ca94c2689e7c08449d5d94a855e42cf`.

## Engineering objective

The upgrade does not attempt to make unsupported hardware appear operational.
It strengthens the paths that can be implemented from existing boot evidence
and introduces reusable mathematical controllers only where they have concrete
consumers.

The resulting design has three linked control surfaces:

```text
measured PCI and firmware evidence
             |
             v
portable compatibility obligations
             |
             +--------------------+
             |                    |
             v                    v
firmware scanout            Hermes admission
                                  |
                                  v
                         min-plus service bound
                                  |
                                  v
                         conformal uncertainty

Crest damage field
      |
      v
ordered tile schedule
      |
      v
conformal frame-time plan
```

## 1. Portable GPU compatibility proof

The driver ABI now contains C-compatible representations for:

```text
PCI identity
six BAR observations
firmware scanout surface
measured topology flags
driver compatibility manifest
compatibility proof
```

A proof records satisfied, missing, and violated obligations. Eligibility
requires all mandatory obligations and no violated obligations. Native
manifests require real MMIO evidence and an isolated IOMMU topology. Firmware
scanout requires a usable boot surface. A vendor identifier alone is
insufficient.

The C header is `kernel/boulder/include/sisyphus/gpu.h`. Its C11 static layout
assertions are compiled by `scripts/check-driver-abi.sh`.

## 2. Firmware scanout object

Boulder now parses direct-color Multiboot2 framebuffer evidence for:

```text
XRGB8888
XBGR8888
RGB565
```

The framebuffer is reserved before physical allocation begins. Drivernet can
retain it as a generation-checked firmware display object. A bounded boot
signature maps at most one MiB, writes a deterministic pattern with volatile
MMIO operations, reads verification samples back, closes the aperture, and
emits a sealed image root.

This is a real fallback path. It is not represented as native GPU mode setting.

## 3. Shared service calculus

`libraries/slope/src/service_calculus.rs` is an allocation-free control core
used by both Boulder and Crest.

For backlog `q`, a rate-latency service curve supplies:

```text
windows = ceil((q + 1) / minimum completions per window)
deterministic delay = fixed latency + windows * window duration
```

The controller then adds:

```text
one-sided split-conformal residual guard
Lyapunov virtual-backlog drift penalty
```

Admission succeeds only when the complete bound fits the supplied deadline and
the arrival envelope and backlog limits remain satisfied. The returned
certificate binds the curve, calibration state, reservation sequence, delay
terms, and deadline slack.

## 4. Hermes command lifecycle

Hermes command submission now performs service admission before reserving a
pending slot or publishing to the transport ring. Failed pending insertion or
wire publication rolls the reservation back. Replies, correlated faults, and
expired requests settle the original admission evidence exactly once through
the pending table.

Hermes personality selection additionally requires a portable compatibility
manifest and proof. Unsupported generations remain rejected until a backend
owns the required BAR, DMA, interrupt, reset, firmware, and recovery contracts.

## 5. Crest adaptive frame control

Crest now combines:

```text
fixed-point signed-distance rendering
Hilbert-ordered damage tiles
three-lane frame policy
one-sided conformal frame-time guard
bounded partial-frame scheduling
```

The latency, coherence, and thermal lanes vote on frame mode, presentation
phase, and tile budget. If the selected budget is smaller than the original
damage set, the tile field materializes an exact prefix and the frame oracle
reseals the plan against that schedule root. Deferred tile signals remain live
for the next frame.

The compositor validates the schedule, rejects duplicate or out-of-range tile
indices, renders only authorized dirty tiles, and clears damage only after all
selected tiles render successfully.

## 6. Deliberate fail-closed boundaries

The following remain unavailable rather than simulated:

```text
general process spawn and wait
native AMD, Intel, or NVIDIA activation without generation backends
general Crest userland scanout lease
unresolved foreign-kernel driver contracts
```

These are tracked as engineering boundaries, not hidden behind successful
placeholder returns.
