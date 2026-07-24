#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
lock_file="$repository_root/formal/toolchains.lock"
toolchain_root="${FORMAL_TOOLCHAIN_ROOT:-$repository_root/target/formal/toolchains}"

fail() {
    printf 'formal-toolchain bootstrap failed: %s\n' "$1" >&2
    exit 1
}

lock_value() {
    local key="$1"
    local value
    value="$(sed -n "s/^${key} = \"\([^\"]*\)\"$/\1/p" "$lock_file")"
    [[ -n "$value" ]] || fail "missing $key in formal/toolchains.lock"
    printf '%s' "$value"
}

[[ "$(uname -s)" == "Linux" && "$(uname -m)" == "x86_64" ]] ||
    fail "the pinned Agda artifact currently supports Linux x86_64 only"
[[ -f "$lock_file" ]] || fail "missing formal/toolchains.lock"
grep -Fxq 'format = 1' "$lock_file" || fail "unsupported toolchain-lock format"

idris_version="$(lock_value idris2_version)"
idris_digest="$(lock_value idris2_source_sha256)"
agda_version="$(lock_value agda_version)"
agda_digest="$(lock_value agda_linux_sha256)"
idris_archive_url="https://www.idris-lang.org/releases/idris2-${idris_version}.tgz"
agda_archive_url="https://github.com/agda/agda/releases/download/v${agda_version}/Agda-v${agda_version}-linux.tar.xz"
idris_source="$toolchain_root/Idris2-${idris_version}"
idris_compiler="$idris_source/build/exec/idris2"
agda_directory="$toolchain_root/Agda-v${agda_version}-linux"
agda_compiler="$agda_directory/agda"
download_directory="$toolchain_root/downloads"

mkdir -p "$download_directory"

if [[ ! -x "$idris_compiler" ]] ||
    [[ "$($idris_compiler --version 2>/dev/null || true)" != "Idris 2, version ${idris_version}" ]]
then
    scheme_command="${SCHEME:-}"
    if [[ -z "$scheme_command" ]]; then
        for candidate in chezscheme chez scheme; do
            if command -v "$candidate" >/dev/null 2>&1; then
                scheme_command="$candidate"
                break
            fi
        done
    fi
    [[ -n "$scheme_command" ]] || fail "threaded Chez Scheme is required to bootstrap Idris2"

    idris_archive="$download_directory/idris2-${idris_version}.tgz"
    curl --fail --location --retry 3 --output "$idris_archive" "$idris_archive_url"
    printf '%s  %s\n' "$idris_digest" "$idris_archive" | sha256sum --check --strict

    staging="$(mktemp -d "$toolchain_root/.idris2-build.XXXXXXXX")"
    trap 'rm -rf -- "$staging"' EXIT
    tar -xzf "$idris_archive" -C "$staging"
    extracted="$staging/Idris2-${idris_version}"
    [[ -d "$extracted" ]] || fail "Idris2 archive layout is unexpected"
    make -C "$extracted" bootstrap "SCHEME=$scheme_command"
    [[ "$($extracted/build/exec/idris2 --version)" == "Idris 2, version ${idris_version}" ]] ||
        fail "bootstrapped Idris2 compiler has the wrong identity"
    [[ ! -e "$idris_source" ]] || fail "incomplete Idris2 destination already exists: $idris_source"
    mv "$extracted" "$idris_source"
    trap - EXIT
    rm -rf -- "$staging"
fi

if [[ ! -x "$agda_compiler" ]] ||
    [[ "$($agda_compiler --version 2>/dev/null | sed -n '1p')" != "Agda version ${agda_version}" ]]
then
    agda_archive="$download_directory/Agda-v${agda_version}-linux.tar.xz"
    curl --fail --location --retry 3 --output "$agda_archive" "$agda_archive_url"
    printf '%s  %s\n' "$agda_digest" "$agda_archive" | sha256sum --check --strict

    staging="$(mktemp -d "$toolchain_root/.agda-install.XXXXXXXX")"
    trap 'rm -rf -- "$staging"' EXIT
    tar -xJf "$agda_archive" -C "$staging"
    [[ -x "$staging/agda" ]] || fail "Agda archive layout is unexpected"
    [[ ! -e "$agda_directory" ]] || fail "incomplete Agda destination already exists: $agda_directory"
    mkdir "$agda_directory"
    mv "$staging/agda" "$agda_compiler"
    trap - EXIT
    rm -rf -- "$staging"
fi

IDRIS2="$idris_compiler" \
IDRIS2_PATH="$idris_source/libs/prelude/build/ttc:$idris_source/libs/base/build/ttc" \
AGDA="$agda_compiler" \
    "$repository_root/scripts/check-formal-models.sh"
