#!/usr/bin/env sh
set -eu

project_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
image="$project_root/target/sisyphus-os.iso"

if [ ! -f "$image" ]; then
    "$project_root/scripts/build-iso.sh"
fi

exec qemu-system-x86_64 \
    -cdrom "$image" \
    -m 256M \
    -no-reboot \
    -no-shutdown \
    -display none \
    -monitor none \
    -serial stdio
