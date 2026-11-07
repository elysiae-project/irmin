#!/usr/bin/env bash
set -euo pipefail

if [[ ${1:-} == "--help" || ${1:-} == "-h" ]]; then
    printf 'Usage: %s [OUTPUT_DIR]\n' "$0"
    printf 'Runs the synthetic 4/64/256 MiB HDiff phase and standard preinstall matrix, plus one cgroup-limited 256 MiB case.\n'
    printf 'Set CGROUP_MEMORY_MAX_MIB to change the cgroup limit (default: 64 MiB).\n'
    printf 'OUTPUT_DIR must be absent or empty; default: /tmp/opencode or /tmp with a timestamped name.\n'
    exit 0
fi
if (( $# > 1 )); then
    printf 'Usage: %s [OUTPUT_DIR]\n' "$0" >&2
    exit 2
fi

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
timestamp=$(date -u +%Y%m%dT%H%M%SZ)
default_parent=/tmp
if [[ -d /tmp/opencode && -w /tmp/opencode ]]; then
    default_parent=/tmp/opencode
fi
output_dir=${1:-"${default_parent}/irmin-preinstall-memory-${timestamp}"}
if [[ $output_dir != /* ]]; then
    output_dir="$(pwd)/$output_dir"
fi
cgroup_limit_mib=${CGROUP_MEMORY_MAX_MIB:-64}
if [[ ! $cgroup_limit_mib =~ ^[1-9][0-9]*$ ]]; then
    printf 'CGROUP_MEMORY_MAX_MIB must be a positive integer\n' >&2
    exit 2
fi

if [[ -e $output_dir ]]; then
    if [[ ! -d $output_dir || -n $(find "$output_dir" -mindepth 1 -maxdepth 1 -print -quit) ]]; then
        printf 'Output directory must be absent or empty: %s\n' "$output_dir" >&2
        exit 2
    fi
else
    mkdir -p "$output_dir"
fi

record_environment() {
    local output=$1
    shift
    if ! "$@" >>"$output" 2>&1; then
        printf 'Environment probe failed: %s\n' "$*" >&2
        printf 'Environment probe failed: %s\n' "$*" >>"$output"
    fi
}

environment_file="$output_dir/environment.txt"
: >"$environment_file"
record_environment "$environment_file" date -u
record_environment "$environment_file" uname -a
record_environment "$environment_file" lscpu
compiler=${RUSTC:-rustc}
if ! (cd "$repo_root" && "$compiler" -Vv) >>"$environment_file" 2>&1; then
    printf 'configured rustc version probe failed (RUSTC is treated as one executable path)\n' >>"$environment_file"
fi
if ! (cd "$repo_root" && cargo -V) >>"$environment_file" 2>&1; then
    printf 'Cargo version probe failed\n' >>"$environment_file"
fi
{
    printf '\nBuild environment:\n'
    printf 'profile=release (--release)\n'
    printf 'provenance_scope=script command and repository-local/CARGO_HOME Cargo and toolchain config; ancestor Cargo config may also affect the effective build\n'
    printf 'CARGO_BUILD_TARGET=%s\n' "${CARGO_BUILD_TARGET:-<host default or cargo config>}"
    for name in RUSTFLAGS CARGO_ENCODED_RUSTFLAGS CARGO_BUILD_RUSTFLAGS RUSTC RUSTC_WRAPPER; do
        value=${!name:-}
        if [[ ${!name+x} ]]; then
            hash=$(printf '%s' "$value" | hash_stdin || printf 'unavailable')
            printf '%s_set=true sha256=%s\n' "$name" "$hash"
        else
            printf '%s_set=false\n' "$name"
        fi
    done
    for name in CARGO_PROFILE_RELEASE_OPT_LEVEL CARGO_PROFILE_RELEASE_LTO CARGO_PROFILE_RELEASE_CODEGEN_UNITS CARGO_PROFILE_RELEASE_DEBUG CARGO_PROFILE_RELEASE_PANIC; do
        value=${!name:-}
        printf '%s=%s\n' "$name" "${value:-<unset>}"
    done
    for config in \
        "$repo_root"/.cargo/config \
        "$repo_root"/.cargo/config.toml \
        "$repo_root"/rust-toolchain \
        "$repo_root"/rust-toolchain.toml \
        "${CARGO_HOME:-$HOME/.cargo}/config" \
        "${CARGO_HOME:-$HOME/.cargo}/config.toml"; do
        if [[ -f $config ]]; then
            hash_file "$config"
        fi
    done
} >>"$environment_file"
{
    printf '\nFilesystem:\n'
    if ! df -T "$repo_root" "$output_dir" /tmp; then
        printf 'Filesystem probe failed\n' >&2
    fi
    printf '\nCgroup membership:\n'
    if ! cat /proc/self/cgroup; then
        printf 'Cgroup membership probe failed\n' >&2
    fi
    printf '\nCgroup mount:\n'
    if ! grep ' - cgroup2 ' /proc/self/mountinfo; then
        printf 'Cgroup v2 mount unavailable\n'
    fi
    printf '\nSwap:\n'
    if ! cat /proc/swaps; then
        printf 'Swap probe failed\n' >&2
    fi
    printf '\nSwap usage summary (MiB):\n'
    if ! awk 'NR > 1 { size += $3; used += $4 } END { printf "total=%.1f used=%.1f\n", size / 1024, used / 1024 }' /proc/swaps; then
        printf 'Swap usage summary unavailable\n' >&2
    fi
    printf '\nBuild revision:\n'
    git -C "$repo_root" rev-parse HEAD || printf 'Git revision unavailable\n'
    printf '\nWorktree status:\n'
    if status=$(git -C "$repo_root" status --porcelain 2>/dev/null); then
        tracked_status=$(git -C "$repo_root" status --porcelain --untracked-files=no 2>/dev/null || true)
        if [[ -n $tracked_status ]]; then
            printf 'tracked_dirty=true\n'
        else
            printf 'tracked_dirty=false\n'
        fi
        if [[ -n $status ]]; then
            printf 'untracked_paths_present=true\n'
        else
            printf 'untracked_paths_present=false\n'
        fi
    else
        printf 'Git status unavailable\n'
    fi
    printf '\nCargo.lock checksum:\n'
    if [[ -f $repo_root/Cargo.lock ]]; then
        hash_file "$repo_root/Cargo.lock"
    else
        printf 'Cargo.lock missing\n'
    fi
} >>"$environment_file" 2>&1

run_benchmark() {
    local label=$1
    local output_file="$output_dir/${label}.json"
    local temp_file
    shift
    printf 'Running %s\n' "$label"
    local -a command=(
        cargo run --locked --manifest-path "$repo_root/Cargo.toml" --release
        --bin bench_memory --features 'bench-alloc-stats bench-diagnostics' --
        "$@" --json
    )
    {
        printf '\nRun %s:\n' "$label"
        printf ' cd %q &&' "$repo_root"
        printf ' %q' "${command[@]}"
        printf '\n'
    } >>"$environment_file"
    temp_file=$(mktemp "$output_dir/.${label}.XXXXXX")
    if ! (cd "$repo_root" && "${command[@]}" >"$temp_file"); then
        rm -f -- "$temp_file"
        return 1
    fi
    mv -- "$temp_file" "$output_file"
}

hash_stdin() {
    local result
    if command -v sha256sum >/dev/null 2>&1; then
        result=$(sha256sum | awk '{print $1}') && [[ -n $result ]] || return 1
    elif command -v shasum >/dev/null 2>&1; then
        result=$(shasum -a 256 | awk '{print $1}') && [[ -n $result ]] || return 1
    else
        return 1
    fi
    printf '%s' "$result"
}

hash_file() {
    local file=$1
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$file" || printf 'Checksum unavailable for %s\n' "${file##*/}"
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$file" || printf 'Checksum unavailable for %s\n' "${file##*/}"
    else
        printf 'Checksum utility unavailable for %s\n' "${file##*/}"
    fi
}

for size_mib in 4 64 256; do
    common=(
        --scenario preinstall-apply
        --size-mib "$size_mib"
        --chunks 4
        --preinstall-mode patch
        --asset-count 1
        --repeat 3
        --fixture-backing disk
    )
    run_benchmark "phase-${size_mib}m" "${common[@]}" --diagnose-hdiff-phases
    run_benchmark "standard-${size_mib}m" "${common[@]}"
done

run_benchmark "cgroup-256m-${cgroup_limit_mib}m" \
    --scenario preinstall-apply \
    --size-mib 256 \
    --chunks 4 \
    --preinstall-mode patch \
    --asset-count 1 \
    --repeat 1 \
    --fixture-backing disk \
    --sweep-sizes-mib 256 \
    --memory-max-mib "$cgroup_limit_mib"

printf 'Results saved to %s\n' "$output_dir"
