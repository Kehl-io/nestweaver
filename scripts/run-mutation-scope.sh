#!/usr/bin/env bash
# Shared actual mutation command. Status is captured evidence; the reporter owns verdicts.
set -euo pipefail
# Runner-context checks prevent accidental local use; environment values are
# not proof of execution origin. Never assign CI markers to bypass this guard.
# BEGIN RUNNER PREDICATE
github_runner_context() {
  [ "$1" = true ] && [ -n "$2" ] && [ -n "$3" ] && [ -n "$4" ]
}
# END RUNNER PREDICATE
if ! github_runner_context "${GITHUB_ACTIONS:-}" "${RUNNER_TEMP:-}" "${RUNNER_OS:-}" "${GITHUB_RUN_ID:-}"; then
  echo 'mutation helpers require GitHub Actions runner context' >&2
  exit 1
fi
packages='' diff='' output='' filter='' expected_plan=''
budget=35m
test_timeout=600
kill_grace=120s
while [ "$#" -gt 0 ]; do
  [ "$#" -ge 2 ] || { echo 'missing argument value' >&2; exit 1; }
  case "$1" in
    --packages) packages=$2 ;; --diff) diff=$2 ;; --output) output=$2 ;;
    --filter) filter=$2 ;; --expected-plan) expected_plan=$2 ;; --budget) budget=$2 ;; --test-timeout) test_timeout=$2 ;;
    --kill-grace) kill_grace=$2 ;; *) echo "unknown argument: $1" >&2; exit 1 ;;
  esac
  shift 2
done
for input in "$packages" "$diff"; do
  [[ "$input" = /* && -f "$input" ]] || { echo 'inputs must be absolute files' >&2; exit 1; }
done
[[ "$output" = /* && ! -e "$output" ]] || { echo 'output must be an unused absolute directory' >&2; exit 1; }
[[ "$budget" =~ ^[1-9][0-9]*[sm]$ && "$kill_grace" =~ ^[1-9][0-9]*s$ && "$test_timeout" =~ ^[1-9][0-9]*$ ]] || exit 1
if [ -n "$filter" ]; then [[ "$filter" = /* && -f "$filter" ]] || exit 1; fi
if [ -n "$expected_plan" ]; then [[ "$expected_plan" = /* && -f "$expected_plan" ]] || exit 1; fi
mkdir -p "$output"
cp "$packages" "$output/packages.txt"
cp "$diff" "$output/input.diff"
pkg_args=()
while IFS= read -r pkg; do
  [[ "$pkg" =~ ^[a-zA-Z0-9_-]+$ ]] || { echo 'invalid package name' >&2; exit 1; }
  pkg_args+=(-p "$pkg")
done < "$packages"
filter_args=()
if [ -n "$filter" ]; then
  cp "$filter" "$output/filter.txt"
  filter_args=(--re "$(cat "$filter")")
fi
status=0
discovery_status=0
no_packages=false
started=$(date +%s)
if [ "${#pkg_args[@]}" -eq 0 ]; then
  no_packages=true
  : > "$output/discovery.json"
  : > "$output/discovery.log"
  echo 'no Rust package in this diff' > "$output/console.log"
else
  set +e
  timeout --signal=INT --kill-after="$kill_grace" "$budget" \
    cargo-mutants mutants "${pkg_args[@]}" --in-diff "$diff" "${filter_args[@]}" \
      --list --json > "$output/discovery.json" 2> "$output/discovery.log"
  discovery_status=$?
  set -e
  if [ "$discovery_status" -eq 0 ] && [ -n "$expected_plan" ]; then
    # Validate the exact filtered discovery before any mutation compile/test.
    python3 - "$expected_plan" "$output/discovery.json" <<'PY_CHECK'
import json
import sys
expected, actual = [json.load(open(path)) for path in sys.argv[1:]]
def identities(plan):
    if not isinstance(plan, list):
        raise ValueError('expected a discovered mutant list')
    result = [json.dumps({k: v for k, v in mutant.items() if k != 'diff'}, sort_keys=True) for mutant in plan]
    if len(set(result)) != len(result):
        raise ValueError('duplicate discovered identities')
    return result
if identities(expected) != identities(actual):
    raise SystemExit('filtered discovery did not match the exact expected plan')
PY_CHECK
  fi
  if [ "$discovery_status" -eq 0 ]; then
    # Discovery consumes the same total outer budget, rather than adding another
    # full production budget. The pinned tool writes its own plan before tests.
    elapsed=$(( $(date +%s) - started ))
    seconds=${budget%[sm]}
    if [[ "$budget" = *m ]]; then seconds=$((seconds * 60)); fi
    remaining=$((seconds - elapsed))
    if [ "$remaining" -gt 0 ]; then
      set +e
      timeout --signal=INT --kill-after="$kill_grace" "${remaining}s" \
        cargo-mutants mutants "${pkg_args[@]}" --in-diff "$diff" "${filter_args[@]}" \
          --output "$output" --in-place --build-timeout 300 \
          --baseline skip --timeout "$test_timeout" --no-shuffle -- --no-fail-fast \
        2>&1 | tee "$output/console.log"
      pipeline_status=("${PIPESTATUS[@]}")
      set -e
      status=${pipeline_status[0]}
      if [ "${pipeline_status[1]}" -ne 0 ] && [ "$status" -ne 124 ] && [ "$status" -ne 137 ]; then status=1; fi
    else
      status=124
      echo 'discovery exhausted outer mutation budget' > "$output/console.log"
    fi
  else
    status=$discovery_status
    cp "$output/discovery.log" "$output/console.log"
  fi
fi
python3 - "$output/status.json" "$status" "$discovery_status" "$no_packages" "$started" <<'PY'
import json
from pathlib import Path
import sys
import time
path, status, discovery_status, no_packages, started = sys.argv[1:]
Path(path).write_text(json.dumps(dict(status=int(status), discovery_status=int(discovery_status),
    truncated=int(status) in (124, 137), no_packages=no_packages == 'true',
    started_epoch=int(started), finished_epoch=time.time()), indent=2) + '\n')
PY
