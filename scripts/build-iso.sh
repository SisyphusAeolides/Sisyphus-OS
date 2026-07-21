#!/usr/bin/env sh
set -eu

project_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
iso_root="$project_root/target/iso-root"
kernel="$project_root/target/x86_64-sisyphus/debug/boulder"
image="$project_root/target/sisyphus-os.iso"

cd "$project_root"
cargo kernel
mkdir -p "$iso_root/boot/grub"
install -m 0755 "$kernel" "$iso_root/boot/boulder"
install -m 0644 "$project_root/boot/grub/grub.cfg" "$iso_root/boot/grub/grub.cfg"
grub-mkrescue -o "$image" "$iso_root"
printf '%s\n' "$image"
