#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
lock_file="$repository_root/formal/toolchains.lock"

fail() {
    printf 'formal-model check failed: %s\n' "$1" >&2
    exit 1
}

[[ -f "$lock_file" ]] || fail "missing formal/toolchains.lock"

expected_lock=$(cat <<'LOCK'
format = 1
idris2_version = "0.8.0"
idris2_commit = "15a3e4e70843f7a34100f6470c04b791330788df"
idris2_source_sha256 = "940a283cb66b0097cab0d24fe10341274fab75cb3af58dc715944d6ca7230665"
agda_version = "2.8.0"
agda_linux_sha256 = "824081b8dcbe431289a50ac6bd83e451f390c51c3884ac7a8c4a5c0df2632faf"
LOCK
)
actual_lock="$(cat -- "$lock_file")"
[[ "$actual_lock" == "$expected_lock" ]] || fail "toolchain lock differs from the pinned identities"

idris2_command="${IDRIS2:-idris2}"
agda_command="${AGDA:-agda}"
command -v "$idris2_command" >/dev/null 2>&1 || fail "Idris2 0.8.0 is not available"
command -v "$agda_command" >/dev/null 2>&1 || fail "Agda 2.8.0 is not available"

idris2_version="$($idris2_command --version)"
agda_version_output="$($agda_command --version)"
agda_version="${agda_version_output%%$'\n'*}"
[[ "$idris2_version" == "Idris 2, version 0.8.0" ]] ||
    fail "wrong Idris2 compiler version: $idris2_version"
[[ "$agda_version" == "Agda version 2.8.0" ]] ||
    fail "wrong Agda compiler version: $agda_version"

for source in \
    "$repository_root/formal/idris2/DriverLifecycle.idr" \
    "$repository_root/formal/idris2/PackageTransaction.idr" \
    "$repository_root/formal/agda/PrivilegeRings.agda"
do
    [[ -f "$source" ]] || fail "missing ${source#"$repository_root/"}"
done

for source in \
    "$repository_root/formal/idris2/DriverLifecycle.idr" \
    "$repository_root/formal/idris2/PackageTransaction.idr"
do
    grep -Fxq '%default total' "$source" ||
        fail "totality is not the module default in ${source#"$repository_root/"}"
done

if grep -En \
    'believe_me|assert_total|assert_smaller|unsafe|(^|[^[:alnum:]_])partial([^[:alnum:]_]|$)|[?][A-Za-z_]|[?][?][?]' \
    "$repository_root/formal/idris2/DriverLifecycle.idr" \
    "$repository_root/formal/idris2/PackageTransaction.idr"
then
    fail "Idris2 escape hatch or unresolved hole detected"
fi

if grep -En '^[[:space:]]*postulate\b|\{![^!]*!\}|TERMINATING|NON_TERMINATING|NO_TERMINATION_CHECK' \
    "$repository_root/formal/agda/PrivilegeRings.agda"
then
    fail "Agda postulate, termination escape, or unresolved hole detected"
fi
grep -Fxq '{-# OPTIONS --safe --without-K #-}' \
    "$repository_root/formal/agda/PrivilegeRings.agda" ||
    fail "Agda safety options are missing"
if grep -En '^[[:space:]]*(open[[:space:]]+)?import[[:space:]]' \
    "$repository_root/formal/agda/PrivilegeRings.agda"
then
    fail "Agda model must remain self-contained and library-free"
fi

generated="$(find "$repository_root/formal" -type f \
    \( -name '*.ttc' -o -name '*.agdai' -o -name '*.ibc' \) -print -quit)"
[[ -z "$generated" ]] || fail "generated artifact is present: ${generated#"$repository_root/"}"

scratch="$(mktemp -d "${TMPDIR:-/tmp}/sisyphus-formal.XXXXXXXX")"
trap 'rm -rf -- "$scratch"' EXIT
mkdir -p "$scratch/idris2" "$scratch/agda" "$scratch/agda-data" "$scratch/agda-config"
cp -- "$repository_root/formal/idris2/DriverLifecycle.idr" "$scratch/idris2/"
cp -- "$repository_root/formal/idris2/PackageTransaction.idr" "$scratch/idris2/"
cp -- "$repository_root/formal/agda/PrivilegeRings.agda" "$scratch/agda/"

(
    cd -- "$scratch/idris2"
    "$idris2_command" --check DriverLifecycle.idr
    "$idris2_command" --check PackageTransaction.idr
)
(
    cd -- "$scratch/agda"
    XDG_DATA_HOME="$scratch/agda-data" \
        XDG_CONFIG_HOME="$scratch/agda-config" \
        "$agda_command" --no-libraries --safe --without-K PrivilegeRings.agda
)

driver_digest="$(sha256sum -- "$repository_root/formal/idris2/DriverLifecycle.idr")"
driver_digest="${driver_digest%% *}"
package_digest="$(sha256sum -- "$repository_root/formal/idris2/PackageTransaction.idr")"
package_digest="${package_digest%% *}"
privilege_digest="$(sha256sum -- "$repository_root/formal/agda/PrivilegeRings.agda")"
privilege_digest="${privilege_digest%% *}"
attestation_directory="$repository_root/target/formal"
attestation="$scratch/verified.lock"
printf '%s\n' \
    'format=1' \
    'idris2_version=0.8.0' \
    'agda_version=2.8.0' \
    "driver_lifecycle_sha256=$driver_digest" \
    "package_transaction_sha256=$package_digest" \
    "privilege_rings_sha256=$privilege_digest" \
    > "$attestation"
mkdir -p "$attestation_directory"
install -m 0644 "$attestation" "$attestation_directory/verified.lock"

printf 'formal-model check passed: Idris2 0.8.0 and Agda 2.8.0\n'
