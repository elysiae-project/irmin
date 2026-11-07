#!/usr/bin/env bash
set -euo pipefail
umask 077

usage() {
    printf 'Usage: %s [OUTPUT_DIR] [REPEATS] [--test]\n' "$0"
    printf 'Runs the opt-in in-memory download write/hash Criterion benchmark repeatedly.\n'
    printf 'Default output: /tmp/opencode/irmin-download-stream-<UTC timestamp>\n'
    printf 'Environment probes use Linux /proc and /sys when available; unavailable probes are recorded.\n'
    printf 'Requires Bash 4.4 or newer.\n'
}

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
timestamp=$(date -u +%Y%m%dT%H%M%SZ)
output_parent=/tmp
if [[ -d /tmp/opencode && -w /tmp/opencode ]]; then
    output_parent=/tmp/opencode
fi
output_dir=
repeats=
test_mode=false
for arg in "$@"; do
    if [[ $arg == --help || $arg == -h ]]; then
        usage
        exit 0
    elif [[ -z $arg ]]; then
        printf 'Arguments must not be empty\n' >&2
        exit 2
    elif [[ $arg == --test ]]; then
        if [[ $test_mode == true ]]; then
            printf 'Duplicate --test option\n' >&2
            exit 2
        fi
        test_mode=true
    elif [[ $arg == -* ]]; then
        usage >&2
        exit 2
    elif [[ -z $output_dir ]]; then
        output_dir=$arg
    elif [[ -z $repeats ]]; then
        repeats=$arg
    else
        usage >&2
        exit 2
    fi
done
output_dir=${output_dir:-"${output_parent}/irmin-download-stream-${timestamp}"}
repeats=${repeats:-5}

if [[ ! $repeats =~ ^[1-9][0-9]*$ ]] || (( ${#repeats} > 2 )) || (( 10#$repeats > 50 )); then
    printf 'REPEATS must be an integer from 1 to 50\n' >&2
    exit 2
fi
if [[ $output_dir != /* ]]; then
    output_dir="$(pwd)/$output_dir"
fi
if ! mkdir -- "$output_dir"; then
    printf 'Could not create output directory: %s\n' "$output_dir" >&2
    exit 2
fi

environment_file="$output_dir/environment.txt"
{
    date -u || printf 'date unavailable\n'
    printf '\nHost and CPU:\n'
    uname -srm || printf 'OS and architecture unavailable\n'
    if command -v lscpu >/dev/null 2>&1; then
        lscpu || printf 'lscpu unavailable\n'
    else
        printf 'lscpu unavailable\n'
    fi
    printf '\nLoad averages:\n'
    cat /proc/loadavg 2>/dev/null || printf 'load average unavailable\n'
    printf '\nCPU governor:\n'
    shopt -s nullglob
    governors=(/sys/devices/system/cpu/cpu*/cpufreq/scaling_governor)
    shopt -u nullglob
    if (( ${#governors[@]} == 0 )); then
        printf 'CPU governor unavailable\n'
    else
        for governor in "${governors[@]}"; do
            if [[ -r $governor ]]; then
                printf '%s=%s\n' "$governor" "$(<"$governor")"
            else
                printf '%s=unavailable\n' "$governor"
            fi
        done
    fi
    printf '\nRust toolchain:\n'
    (cd "$repo_root" && "${RUSTC:-rustc}" -Vv) || printf 'rustc version unavailable\n'
    (cd "$repo_root" && cargo -V) || printf 'Cargo version unavailable\n'
    if [[ ${RUSTUP_TOOLCHAIN+x} ]]; then
        printf 'RUSTUP_TOOLCHAIN_set=true\n'
    else
        printf 'RUSTUP_TOOLCHAIN_set=false\n'
    fi
    printf '\nBuild identity:\n'
    git -C "$repo_root" rev-parse HEAD || printf 'Git revision unavailable\n'
    if status=$(git -C "$repo_root" status --porcelain 2>/dev/null); then
        if [[ -n $status ]]; then
            printf 'worktree_dirty=true\n'
        else
            printf 'worktree_dirty=false\n'
        fi
    else
        printf 'worktree_dirty=unavailable\n'
    fi
    printf '\nRun configuration:\nrepeats=%s\n' "$repeats"
    if [[ ${CARGO_TARGET_DIR+x} ]]; then
        printf 'target_dir_set=true\n'
    else
        printf 'target_dir_set=false\n'
    fi
    printf 'provenance_scope=script, repository/user Cargo and toolchain settings; ancestor config and system wrappers may also affect the build\n'
    for name in RUSTFLAGS CARGO_ENCODED_RUSTFLAGS RUSTC RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER; do
        if [[ ${!name+x} ]]; then
            printf '%s_set=true\n' "$name"
        else
            printf '%s_set=false\n' "$name"
        fi
    done
    if [[ ${CARGO_HOME+x} ]]; then
        cargo_home=${CARGO_HOME}
        cargo_home_available=true
    elif [[ ${HOME+x} ]]; then
        cargo_home="$HOME/.cargo"
        cargo_home_available=true
    else
        cargo_home_available=false
        cargo_home=''
    fi
    printf 'cargo_home_available=%s\n' "$cargo_home_available"
    configs=(
        "$repo_root/.cargo/config|repo/.cargo/config"
        "$repo_root/.cargo/config.toml|repo/.cargo/config.toml"
        "$cargo_home/config|cargo_home/config"
        "$cargo_home/config.toml|cargo_home/config.toml"
        "$repo_root/rust-toolchain|repo/rust-toolchain"
        "$repo_root/rust-toolchain.toml|repo/rust-toolchain.toml"
    )
    for entry in "${configs[@]}"; do
        config=${entry%%|*}
        label=${entry#*|}
        if [[ $label == cargo_home/* && $cargo_home_available == false ]]; then
            continue
        fi
        if [[ -f $config ]]; then
            printf 'setting=%s present=true\n' "$label"
        else
            printf 'setting=%s present=false\n' "$label"
        fi
    done
} >"$environment_file"

for ((run = 1; run <= repeats; run++)); do
    log_file=$(printf '%s/run-%02d.log' "$output_dir" "$run")
    {
        printf 'Run %d/%d at ' "$run" "$repeats"
        date -u || printf 'date unavailable\n'
        printf 'load_before='
        cat /proc/loadavg 2>/dev/null || printf 'unavailable\n'
        printf "command: cargo bench --locked --bench download_stream_bench --features 'benchmark bench-internals'"
        [[ $test_mode == true ]] && printf ' -- --test'
        printf '\n'
    } >"$log_file"
    extra_args=()
    if [[ $test_mode == true ]]; then
        extra_args+=(-- --test)
    fi
    if ! (cd "$repo_root" && cargo bench --locked --bench download_stream_bench --features 'benchmark bench-internals' "${extra_args[@]}") >>"$log_file" 2>&1; then
        printf 'Benchmark failed; see %s\n' "$log_file" >&2
        exit 1
    fi
    {
        printf '\nload_after='
        cat /proc/loadavg 2>/dev/null || printf 'unavailable\n'
    } >>"$log_file"
done

printf 'Results saved under %s\n' "$output_dir"
printf 'Criterion reports are under %s/criterion (or CARGO_TARGET_DIR/criterion).\n' "${CARGO_TARGET_DIR:-$repo_root/target}"
