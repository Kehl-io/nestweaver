#!/usr/bin/env bash

# Reduce path-filtered CI jobs to one stable, branch-protectable conclusion.
# The workflow supplies GitHub's `needs` object. Any absent, cancelled, failed,
# or unexpectedly skipped job that should have run rejects the candidate.

set -euo pipefail

verify_required_ci() {
  local needs_json=$1
  local rust metal frontend

  result() {
    jq -er --arg job "$1" '.[$job].result // "missing"' <<< "$needs_json"
  }

  require_success() {
    local job=$1
    local actual
    actual=$(result "$job") || return 1
    if [[ "$actual" != success ]]; then
      echo "required CI job '$job' was $actual, expected success" >&2
      return 1
    fi
  }

  flag() {
    local name=$1
    local value
    value=$(jq -er --arg name "$name" '.changes.outputs[$name] // "missing"' <<< "$needs_json")
    if [[ "$value" != true && "$value" != false ]]; then
      echo "change detector output '$name' was $value, expected true or false" >&2
      return 1
    fi
    printf '%s' "$value"
  }

  require_success changes || return 1
  require_success secret-scan || return 1

  rust=$(flag rust) || return 1
  metal=$(flag metal) || return 1
  frontend=$(flag frontend) || return 1

  if [[ "$rust" == true ]]; then
    require_success fmt || return 1
    require_success build-and-check || return 1
    require_success clippy || return 1
    require_success daemon-tests || return 1
    require_success audit || return 1
  fi
  if [[ "$metal" == true ]]; then
    require_success metal-smoke || return 1
  fi
  if [[ "$rust" == true || "$frontend" == true ]]; then
    require_success e2e || return 1
  fi
}

self_test() {
  local baseline failed missing malformed
  baseline='{
    "changes": {"result":"success", "outputs":{"rust":"true", "metal":"true", "frontend":"true"}},
    "secret-scan": {"result":"success"},
    "fmt": {"result":"success"},
    "build-and-check": {"result":"success"},
    "clippy": {"result":"success"},
    "daemon-tests": {"result":"success"},
    "audit": {"result":"success"},
    "metal-smoke": {"result":"success"},
    "e2e": {"result":"success"}
  }'

  verify_required_ci "$baseline"

  # A no-code-change run legitimately skips conditional jobs, but the change
  # detector and unconditional secret scan must still succeed.
  verify_required_ci "$(jq '.changes.outputs = {rust:"false", metal:"false", frontend:"false"}
    | .fmt.result = "skipped"
    | .["build-and-check"].result = "skipped"
    | .clippy.result = "skipped"
    | .["daemon-tests"].result = "skipped"
    | .audit.result = "skipped"
    | .["metal-smoke"].result = "skipped"
    | .e2e.result = "skipped"' <<< "$baseline")"

  failed=$(jq '.clippy.result = "failure"' <<< "$baseline")
  if verify_required_ci "$failed" >/dev/null 2>&1; then
    echo "self-test failed: a failed required job was accepted" >&2
    return 1
  fi

  missing=$(jq 'del(.e2e)' <<< "$baseline")
  if verify_required_ci "$missing" >/dev/null 2>&1; then
    echo "self-test failed: a missing required job was accepted" >&2
    return 1
  fi

  malformed=$(jq '.changes.outputs.rust = ""' <<< "$baseline")
  if verify_required_ci "$malformed" >/dev/null 2>&1; then
    echo "self-test failed: an absent change-detector output was accepted" >&2
    return 1
  fi

  echo "required CI verifier self-test passed"
}

case "${1:-}" in
  --self-test)
    self_test
    ;;
  -)
    verify_required_ci "$(cat)"
    ;;
  "")
    echo "usage: $0 <needs-json-file> | - | --self-test" >&2
    exit 64
    ;;
  *)
    if [[ $# -ne 1 || ! -f "$1" ]]; then
      echo "usage: $0 <needs-json-file> | - | --self-test" >&2
      exit 64
    fi
    verify_required_ci "$(<"$1")"
    ;;
esac
