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

if [[ ${1:-} == --self-test ]]; then
  baseline='[{"type":"pull_request","parameters":{"required_approving_review_count":0,"require_code_owner_review":false}},{"type":"required_status_checks","parameters":{"strict_required_status_checks_policy":true,"required_status_checks":[{"context":"Required CI","integration_id":15368}]}}]'
  verify_rules <<< "$baseline"
  for mutation in \
    'map(select(.type != "pull_request"))' \
    'map(select(.type != "required_status_checks"))' \
    '.[1].parameters.strict_required_status_checks_policy = false' \
    '.[1].parameters.required_status_checks[0].integration_id = 42' \
    '.[1].parameters.required_status_checks[0].context = "Some other CI"'; do
    if jq "$mutation" <<< "$baseline" | verify_rules; then
      echo "main rules self-test accepted invalid policy: $mutation" >&2
      exit 1
    fi
  done
  echo "main rules self-test passed"
elif [[ $# -eq 0 ]]; then
  verify_rules || { echo 'main requires pull requests and up-to-date Required CI from GitHub Actions' >&2; exit 1; }
else
  echo 'usage: verify-main-rules.sh [--self-test] (effective rules JSON on stdin)' >&2
  exit 64
fi
