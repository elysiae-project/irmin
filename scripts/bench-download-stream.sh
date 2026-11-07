#!/usr/bin/env bash
set -euo pipefail
umask 077

usage() {
    printf 'Usage: %s [OUTPUT_DIR] [REPEATS] [--test]\n' "$0"
    printf 'Runs the opt-in in-memory download write/hash Criterion benchmark repeatedly.\n'
    printf 'Default output: /tmp/opencode/irmin-download-stream-<UTC timestamp>\n'
    printf 'Environment probes use Linux /proc and /sys when available; unavailable probes are recorded.\n'
    printf 'Optional DOWNLOAD_BENCH_MAX_LOAD marks samples provisional when 1-minute load exceeds the value.\n'
    printf 'Optional DOWNLOAD_BENCH_MAX_SPREAD sets the maximum acceptable relative throughput spread (default: 0.05).\n'
    printf 'Requires Bash 4.4 or newer.\n'
}

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
timestamp=$(date -u +%Y%m%dT%H%M%SZ)
token_file=$(mktemp "/tmp/dl-${timestamp}-XXXXXXXX")
run_token=${token_file##*/}
rm -- "$token_file"
baseline_prefix="$run_token"
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
if [[ -n ${DOWNLOAD_BENCH_MAX_LOAD:-} ]] && [[ ! $DOWNLOAD_BENCH_MAX_LOAD =~ ^([0-9]+([.][0-9]*)?|[.][0-9]+)$ ]]; then
    printf 'DOWNLOAD_BENCH_MAX_LOAD must be a nonnegative decimal number\n' >&2
    exit 2
fi
max_spread=${DOWNLOAD_BENCH_MAX_SPREAD:-0.05}
if [[ ! $max_spread =~ ^([0-9]+([.][0-9]*)?|[.][0-9]+)$ ]] || ! LC_ALL=C awk -v value="$max_spread" 'BEGIN { exit !(value >= 0 && value < 1) }'; then
    printf 'DOWNLOAD_BENCH_MAX_SPREAD must be a decimal in [0, 1)\n' >&2
    exit 2
fi
if [[ $test_mode != true ]] && ! command -v python3 >/dev/null 2>&1; then
    printf 'python3 is required to aggregate Criterion reports\n' >&2
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
    printf 'baseline_prefix=%s\n' "$baseline_prefix"
    printf 'DOWNLOAD_BENCH_MAX_LOAD=%s\n' "${DOWNLOAD_BENCH_MAX_LOAD:-<unset>}"
    printf 'DOWNLOAD_BENCH_MAX_SPREAD=%s\n' "$max_spread"
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
    if [[ -n ${CARGO_HOME:-} ]]; then
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
        load_before=$(awk '{print $1}' /proc/loadavg 2>/dev/null || true)
        printf '%s\n' "${load_before:-unavailable}"
    } >"$log_file"
    extra_args=()
    if [[ $test_mode != true ]]; then
        baseline=$(printf '%s-%02d' "$baseline_prefix" "$run")
        extra_args+=(-- --save-baseline "$baseline")
    fi
    if [[ $test_mode == true ]]; then
        extra_args+=(-- --test)
    fi
    {
        printf "command: cargo bench --locked --bench download_stream_bench --features 'benchmark bench-internals'"
        printf ' %q' "${extra_args[@]}"
        printf '\n'
    } >>"$log_file"
    if ! (cd "$repo_root" && cargo bench --locked --bench download_stream_bench --features 'benchmark bench-internals' "${extra_args[@]}") >>"$log_file" 2>&1; then
        printf 'Benchmark failed; see %s\n' "$log_file" >&2
        exit 1
    fi
    {
        printf '\nload_after='
        load_after=$(awk '{print $1}' /proc/loadavg 2>/dev/null || true)
        printf '%s\n' "${load_after:-unavailable}"
        numeric='^([0-9]+([.][0-9]*)?|[.][0-9]+)$'
        if [[ -n ${DOWNLOAD_BENCH_MAX_LOAD:-} && ${load_before:-} =~ $numeric && ${load_after:-} =~ $numeric ]]; then
            if LC_ALL=C awk -v before="$load_before" -v after="$load_after" -v max="$DOWNLOAD_BENCH_MAX_LOAD" \
                'BEGIN { exit !(before <= max && after <= max) }'; then
                printf 'load_verdict=below_user_threshold\n'
            else
                printf 'load_verdict=above_user_threshold; treat timing as provisional\n'
            fi
        else
            printf 'load_verdict=not_scored\n'
        fi
    } >>"$log_file"
    if [[ $test_mode != true ]]; then
        report_root="${CARGO_TARGET_DIR:-$repo_root/target}/criterion"
        run_report_dir="$output_dir/run-$(printf '%02d' "$run")-criterion"
        mkdir -- "$run_report_dir"
        report_list="$output_dir/.reports-${run}.list"
        if ! find "$report_root" -type f \( -path "*/$baseline/benchmark.json" -o -path "*/$baseline/estimates.json" \) -print0 >"$report_list"; then
            printf 'Failed to enumerate Criterion reports under %s\n' "$report_root" >&2
            exit 1
        fi
        benchmark_reports=0
        while IFS= read -r -d '' report; do
            relative=${report#"$report_root"/}
            destination="$run_report_dir/$relative"
            mkdir -p -- "${destination%/*}"
            cp -- "$report" "$destination"
            if [[ ${report##*/} == benchmark.json ]]; then
                benchmark_reports=$((benchmark_reports + 1))
                if [[ ! -f ${report%/*}/estimates.json ]]; then
                    printf 'Criterion benchmark report has no sibling estimates.json: %s\n' "$report" >&2
                    exit 1
                fi
            elif [[ ${report##*/} == estimates.json && ! -f ${report%/*}/benchmark.json ]]; then
                printf 'Criterion estimates report has no sibling benchmark.json: %s\n' "$report" >&2
                exit 1
            fi
        done <"$report_list"
        rm -- "$report_list"
        if (( benchmark_reports == 0 )); then
            printf 'No Criterion reports found for %s; check CARGO_TARGET_DIR and benchmark filters.\n' "$baseline" >&2
            exit 1
        fi
    fi
done

if [[ $test_mode == true ]]; then
    printf '{"verdict":"test-only","repeats":%s}\n' "$repeats" >"$output_dir/summary.json"
else
    python3 "$repo_root/scripts/summarize_download_bench.py" \
        "$output_dir" "$repeats" "$max_spread" "${DOWNLOAD_BENCH_MAX_LOAD:-}"
fi

printf 'Results saved under %s\n' "$output_dir"
printf 'Criterion reports are under %s/criterion (or CARGO_TARGET_DIR/criterion).\n' "${CARGO_TARGET_DIR:-$repo_root/target}"
