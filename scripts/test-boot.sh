#!/usr/bin/env sh
set -eu

project_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
image="$project_root/target/sisyphus-os.iso"
output=$(mktemp)
trap 'rm -f "$output"' EXIT HUP INT TERM

"$project_root/scripts/build-iso.sh" >/dev/null

status=0
timeout --signal=TERM 10s qemu-system-x86_64 \
    -cdrom "$image" \
    -m 256M \
    -smp 4 \
    -no-reboot \
    -no-shutdown \
    -display none \
    -monitor none \
    -serial stdio >"$output" 2>&1 || status=$?

if [ "$status" -ne 0 ] && [ "$status" -ne 124 ]; then
    cat "$output"
    exit "$status"
fi

cat "$output"
grep -Fq "Boulder: CPU topology processors=4, online=1, enclave=2, compute=1" "$output"
grep -Fq "Boulder: higher-half transition verified, low PML4 entry absent" "$output"
grep -Fq "Boulder: TSS active RSP0=" "$output"
grep -Fq "Boulder: capability-gated fabric work cycle verified" "$output"
grep -Fq "Boulder: Aether policy and bounded flight recorder verified" "$output"
grep -Fq "Boulder: Black Lab time=600 ns, heat=11000, predictions=1, epoch=2, generation=2, faults=1, artifact=64 bytes, PID1 plan entry=0x1000, install=frame-backed:1" "$output"
grep -Fq "PID1 syscall write" "$output"
grep -Fq "Boulder: Ring 3 trap rip=0x1020 cs=0x23, returning through RSP0" "$output"
grep -Fq "frames=6, retained=true, cr3_activation=validated, syscall_write=returned, syscall_yield=returned, ring3_probe=returned" "$output"
grep -Fq "Boulder: Kairos profile CPUs=4" "$output"
grep -Fq "Boulder: local APIC timer" "$output"
grep -Fq "Boulder: ignition Multiboot2 online, userland_ready=false" "$output"
grep -Fq "Boulder: interrupt-routing milestone complete" "$output"
