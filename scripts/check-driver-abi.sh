#!/usr/bin/env sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT HUP INT TERM

cc=${CC:-cc}

"$cc" \
  -std=c11 \
  -ffreestanding \
  -fno-stack-protector \
  -fvisibility=hidden \
  -Wall \
  -Wextra \
  -Werror \
  -I"$root/kernel/boulder/include" \
  -c "$root/kernel/boulder/drivers/reference/reference_driver.c" \
  -o "$work/reference_driver.o"

test -s "$work/reference_driver.o"
printf '%s\n' "driver ABI reference compile PASS"
