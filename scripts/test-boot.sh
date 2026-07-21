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
grep -Fq "Boulder: local APIC timer" "$output"
grep -Fq "Boulder: interrupt-routing milestone complete" "$output"
