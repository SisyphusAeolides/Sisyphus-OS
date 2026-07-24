#!/usr/bin/env sh
set -eu

project_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
image="$project_root/target/sisyphus-os.iso"
output=$(mktemp)
trap 'rm -f "$output"' EXIT HUP INT TERM

"$project_root/scripts/build-iso.sh" >/dev/null

status=0
timeout --signal=TERM 10s qemu-system-x86_64 \
    -machine q35 \
    -cdrom "$image" \
    -m 256M \
    -smp 4 \
    -device intel-iommu,intremap=on \
    -device qemu-xhci,id=xhci \
    -device usb-kbd,bus=xhci.0 \
    -device usb-tablet,bus=xhci.0 \
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
grep -Fq "Boulder: DMAR host-width=48 units=1, presence-only" "$output"
grep -Fq "Boulder: DMAR unit segment=0 base=0xfed90000 include-all=false endpoints=7 unresolved-requester-scopes=false" "$output"
grep -Fq "Boulder: xHCI VT-d requester candidate PciAddress { bus: 0, slot: 3, function: 0 } policy=isolated-shared-unit unit=0xfed90000 include-all=false" "$output"
grep -Fq "Boulder: xHCI scoped VT-d epoch enabled/mapped/revoked/released domain=4294967297 mappings=4" "$output"
grep -Fq "Boulder: xHCI reversible Run/Stop/reset epoch started/halted/reset-ready start-polls=" "$output"
grep -Fq "Boulder: xHCI reversible DMA epoch prepared/scrubbed/reclaimed regions=4 pages=4" "$output"
grep -Fq "Boulder: xHCI bus-master epoch enabled/readback/revoked/restored bus-master=false" "$output"
grep -Fq "Boulder: xHCI halted port census connected=2 enabled=0 resetting=0 overcurrent=0" "$output"
grep -Fq "Boulder: xHCI reset-ready DeviceAddress { segment: 0, bus: 0, slot: 3, function: 0 }" "$output"
grep -Fq "bus-master=false" "$output"
grep -Fq "deferred=true" "$output"
if grep -Fq "Boulder panic:" "$output"; then
    exit 1
fi
