#!/usr/bin/env bash
# Validate effective branch rules, including the source of the required check.
# Reviewer counts are a separate governance choice: requiring a second owner in
# a single-owner repository would make every release impossible to merge.
set -euo pipefail

verify_rules() {
  jq -e '
    type == "array" and
    any(.[]; .type == "pull_request") and
    any(.[]; .type == "required_status_checks" and
      .parameters.strict_required_status_checks_policy == true and
      any(.parameters.required_status_checks[]?;
        .context == "Required CI" and .integration_id == 15368))
  ' >/dev/null
}

verify_ruleset() {
  jq -e '
    .target == "branch" and .enforcement == "active" and
    .bypass_actors == [] and
    .conditions.ref_name.include == ["refs/heads/main"] and
    .conditions.ref_name.exclude == [] and
    any(.rules[]; .type == "deletion") and
    any(.rules[]; .type == "non_fast_forward")
  ' "$1" >/dev/null && jq '.rules' "$1" | verify_rules
}

if [[ ${1:-} == --self-test && $# -eq 1 ]]; then
  policy="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)/.github/main-ruleset.json"
  verify_ruleset "$policy" || { echo 'committed main ruleset is invalid' >&2; exit 1; }
  baseline=$(jq '.rules' "$policy")
  verify_rules <<< "$baseline"
  for mutation in \
    'map(select(.type != "pull_request"))' \
    'map(select(.type != "required_status_checks"))' \
    'map(if .type == "required_status_checks" then .parameters.strict_required_status_checks_policy = false else . end)' \
    'map(if .type == "required_status_checks" then .parameters.required_status_checks[0].integration_id = 42 else . end)' \
    'map(if .type == "required_status_checks" then .parameters.required_status_checks[0].context = "Some other CI" else . end)'; do
    if jq "$mutation" <<< "$baseline" | verify_rules; then
      echo "main rules self-test accepted invalid policy: $mutation" >&2
      exit 1
    fi
  done
  echo "main rules self-test passed (committed ruleset and counterexamples)"
elif [[ ${1:-} == --ruleset && $# -eq 2 ]]; then
  verify_ruleset "$2" || { echo 'main ruleset must enforce the committed PR/check and no-bypass policy' >&2; exit 1; }
elif [[ $# -eq 0 ]]; then
  verify_rules || { echo 'main requires pull requests and up-to-date Required CI from GitHub Actions' >&2; exit 1; }
else
  echo 'usage: verify-main-rules.sh [--self-test | --ruleset FILE] (effective rules JSON on stdin otherwise)' >&2
  exit 64
fi
