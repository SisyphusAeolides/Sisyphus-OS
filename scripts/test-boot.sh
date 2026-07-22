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
grep -Fq "Boulder: measured Push module" "$output"
grep -Fq "PID1 plan entry=" "$output"
grep -Fq "install=frame-backed:1" "$output"
grep -Fq "segments=3, retained=true, cr3_activation=validated, launch=pending" "$output"
grep -Fq "Boulder: Kairos profile CPUs=4" "$output"
grep -Fq "Boulder: local APIC timer" "$output"
grep -Fq "Boulder: ignition Multiboot2 online, userland_ready=true" "$output"
grep -Fq "Boulder: interrupt-routing milestone complete" "$output"
grep -Fq "Boulder: transferring permanently to measured Push PID1 at " "$output"
grep -Fq "[PID 1] measured push engine online" "$output"
grep -Fq "[PID 1] requesting 'slope-net' critical=true restart=0/5" "$output"
grep -Fq "[PID 1] requesting 'slope-net' critical=true restart=5/5" "$output"
grep -Fq "[PID 1] critical service SlopeNet exhausted; entering recovery mode" "$output"
