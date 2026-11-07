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

printf 'download benchmark runner tests passed\n'
