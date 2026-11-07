#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
tmp_dir=$(mktemp -d /tmp/opencode/bench-runner-test.XXXXXX)
trap 'rm -rf -- "$tmp_dir"' EXIT

fake_bin="$tmp_dir/bin"
mkdir -- "$fake_bin"
cat >"$fake_bin/cargo" <<'FAKE_CARGO'
#!/usr/bin/env bash
set -euo pipefail
baseline=
while (( $# )); do
    if [[ $1 == --save-baseline ]]; then
        baseline=$2
        shift 2
    else
        shift
    fi
done
[[ -n $baseline ]]
if [[ ${FAKE_NO_REPORT:-false} == true ]]; then
    exit 0
fi
counter_file="$CARGO_TARGET_DIR/run-count"
count=0
[[ -f $counter_file ]] && read -r count <"$counter_file"
count=$((count + 1))
printf '%s\n' "$count" >"$counter_file"
values=( $FAKE_THROUGHPUTS )
value=${values[$((count - 1))]}
dir="$CARGO_TARGET_DIR/criterion/group/case/$baseline"
mkdir -p -- "$dir"
cat >"$dir/benchmark.json" <<JSON
{"full_id":"group/case","throughput":{"Bytes":1000}}
JSON
cat >"$dir/estimates.json" <<JSON
{"median":{"point_estimate":$((1000000000000 / value))}}
JSON
FAKE_CARGO
chmod +x "$fake_bin/cargo"
cat >"$fake_bin/awk" <<'FAKE_AWK'
#!/usr/bin/env bash
if [[ ${FAKE_DELAY_FIRST_LOAD_PROBE:-false} == true && ${1:-} == '{print $1}' && ${2:-} == "$DOWNLOAD_BENCH_LOADAVG_FILE" ]]; then
    sleep 2
fi
exec /usr/bin/awk "$@"
FAKE_AWK
chmod +x "$fake_bin/awk"

run_case() {
    local name=$1
    local values=$2
    local expected=$3
    local target="$tmp_dir/target-$name"
    local output="$tmp_dir/output-$name"
    mkdir -p -- "$target"
    FAKE_THROUGHPUTS="$values" \
    CARGO_TARGET_DIR=$target \
    DOWNLOAD_BENCH_MAX_LOAD=999999 \
    PATH="$fake_bin:$PATH" \
        "$repo_root/scripts/bench-download-stream.sh" "$output" 5
    local verdict
    verdict=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["verdict"])' "$output/summary.json")
    [[ $verdict == "$expected" ]]
}

run_case stable '100 101 99 100 100' quiet
run_case variable '70 85 100 115 130' noisy

missing_target="$tmp_dir/target-missing"
mkdir -p -- "$missing_target/criterion"
missing_output="$tmp_dir/output-missing"
if FAKE_NO_REPORT=true CARGO_TARGET_DIR="$missing_target" PATH="$fake_bin:$PATH" \
    "$repo_root/scripts/bench-download-stream.sh" "$missing_output" 1 2>"$tmp_dir/missing.err"; then
    printf 'expected missing Criterion reports to fail\n' >&2
    exit 1
fi
grep -q 'No Criterion reports found' "$tmp_dir/missing.err"

quiet_load="$tmp_dir/loadavg-quiet"
printf '0.1 0.1 0.1 1/1 1\n' >"$quiet_load"
quiet_wait_output="$tmp_dir/output-wait-quiet"
if DOWNLOAD_BENCH_LOADAVG_FILE="$quiet_load" DOWNLOAD_BENCH_MAX_LOAD=1 \
    "$repo_root/scripts/bench-download-stream.sh" "$quiet_wait_output" 1 --test --wait-quiet 1; then
    quiet_status=0
else
    quiet_status=$?
fi
[[ $quiet_status == 0 ]]
grep -q 'quiet load observed' "$quiet_wait_output/quiet-wait.log"

busy_load="$tmp_dir/loadavg-busy"
printf '2.0 2.0 2.0 1/1 1\n' >"$busy_load"
busy_wait_output="$tmp_dir/output-wait-timeout"
if DOWNLOAD_BENCH_LOADAVG_FILE="$busy_load" DOWNLOAD_BENCH_MAX_LOAD=1 \
    FAKE_DELAY_FIRST_LOAD_PROBE=true PATH="$fake_bin:$PATH" \
    "$repo_root/scripts/bench-download-stream.sh" "$busy_wait_output" 1 --test --wait-quiet 1 \
    2>"$tmp_dir/wait-timeout.err"; then
    printf 'expected wait-for-quiet to time out\n' >&2
    exit 1
else
    timeout_status=$?
fi
[[ $timeout_status == 3 ]]
grep -q 'Timed out waiting for load' "$tmp_dir/wait-timeout.err"
grep -q 'waiting, load=' "$busy_wait_output/quiet-wait.log"

if "$repo_root/scripts/bench-download-stream.sh" --test "$tmp_dir/output-missing-timeout" 1 --wait-quiet --test \
    2>"$tmp_dir/missing-timeout.err"; then
    printf 'expected --wait-quiet without a timeout to fail\n' >&2
    exit 1
else
    missing_timeout_status=$?
fi
[[ $missing_timeout_status == 2 ]]
if "$repo_root/scripts/bench-download-stream.sh" --test "$tmp_dir/output-zero-timeout" 1 --wait-quiet 0 \
    2>"$tmp_dir/zero-timeout.err"; then
    printf 'expected zero wait timeout to fail\n' >&2
    exit 1
else
    zero_status=$?
fi
[[ $zero_status == 2 ]]
if "$repo_root/scripts/bench-download-stream.sh" --test "$tmp_dir/output-octal-timeout" 1 --wait-quiet 08 \
    2>"$tmp_dir/octal-timeout.err"; then
    printf 'expected invalid octal-looking timeout to fail\n' >&2
    exit 1
else
    octal_status=$?
fi
[[ $octal_status == 2 ]]
if "$repo_root/scripts/bench-download-stream.sh" --test "$tmp_dir/output-large-timeout" 1 --wait-quiet 999999999999999999999 \
    2>"$tmp_dir/large-timeout.err"; then
    printf 'expected oversized timeout to fail\n' >&2
    exit 1
else
    large_status=$?
fi
[[ $large_status == 2 ]]
grep -q 'positive integer' "$tmp_dir/large-timeout.err"
if "$repo_root/scripts/bench-download-stream.sh" --test "$tmp_dir/output-invalid-timeout" 1 --wait-quiet invalid \
    2>"$tmp_dir/invalid-timeout.err"; then
    printf 'expected nonnumeric wait timeout to fail\n' >&2
    exit 1
else
    invalid_status=$?
fi
[[ $invalid_status == 2 ]]
grep -q 'requires a following timeout' "$tmp_dir/missing-timeout.err"
grep -q 'positive integer' "$tmp_dir/zero-timeout.err"
grep -q 'positive integer' "$tmp_dir/invalid-timeout.err"
if DOWNLOAD_BENCH_MAX_LOAD=1 DOWNLOAD_BENCH_LOADAVG_FILE="$quiet_load" \
    "$repo_root/scripts/bench-download-stream.sh" --test "$tmp_dir/output-padded-timeout" 1 \
    --wait-quiet 00000000000000000001 2>"$tmp_dir/padded-timeout.err"; then
    padded_status=0
else
    padded_status=$?
fi
[[ $padded_status == 0 ]]
if "$repo_root/scripts/bench-download-stream.sh" "$tmp_dir/output-no-threshold" 1 --wait-quiet 1 \
    2>"$tmp_dir/missing-threshold.err"; then
    printf 'expected --wait-quiet to require DOWNLOAD_BENCH_MAX_LOAD\n' >&2
    exit 1
fi
grep -q 'requires DOWNLOAD_BENCH_MAX_LOAD' "$tmp_dir/missing-threshold.err"

printf 'download benchmark runner tests passed\n'
