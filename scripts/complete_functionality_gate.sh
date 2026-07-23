#!/usr/bin/env sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

fail() {
  printf '%s\n' "$*" >&2
  exit 1
}

tracked_artifacts=$(
  git ls-files |
    grep -E '(^|/)(scratch\.rs|.*\.(rlib|rmeta|o|a))$' || true
)
[ -z "$tracked_artifacts" ] ||
  fail "tracked build or scratch artifacts:\n$tracked_artifacts"

facades=$(
  grep -RInE \
    --include='*.rs' \
    --exclude-dir=target \
    --exclude-dir=.git \
    'STUBS FOR|Pretend |pretend |mock of|#!\[allow\(dead_code\)\]|todo!\(|unimplemented!\(' \
    . || true
)
[ -z "$facades" ] ||
  fail "facade evidence remains:\n$facades"

fake_success=$(
  grep -RInE \
    --include='*.rs' \
    --exclude-dir=target \
    --exclude-dir=.git \
    'unwrap_or\(0\).*as isize|Err\(_\) => 0|Result<\(\), \(\)> \{[[:space:]]*Ok\(\(\)\)' \
    kernel core libraries userland || true
)
[ -z "$fake_success" ] ||
  fail "error-to-success conversion remains:\n$fake_success"

cargo fmt --all --check
cargo check --workspace
cargo test --workspace

cargo +nightly user-push
cargo +nightly kernel

"$root/scripts/test-boot.sh"

printf '%s\n' "complete functionality gate passed"
