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
        sha256sum "$repo_root/Cargo.lock" || printf 'Cargo.lock checksum unavailable\n'
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
    temp_file=$(mktemp "$output_dir/.${label}.XXXXXX")
    if ! cargo run --locked --manifest-path "$repo_root/Cargo.toml" --release --bin bench_memory \
        --features 'bench-alloc-stats bench-diagnostics' -- \
        "$@" --json >"$temp_file"; then
        rm -f -- "$temp_file"
        return 1
    fi
    mv -- "$temp_file" "$output_file"
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
