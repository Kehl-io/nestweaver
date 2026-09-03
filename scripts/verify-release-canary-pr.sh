#!/usr/bin/env bash

# Create an automation-authored canary PR with GITHUB_TOKEN, prove the applied
# main-branch rules block it while Required CI is absent/action_required, and
# always close the PR and delete its temporary branch.

set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <owner/repo> <exact-base-sha>" >&2
  exit 64
fi

repo=$1
base_sha=$2
base_branch=${BASE_BRANCH:-main}
run_id=${GITHUB_RUN_ID:-local}
run_attempt=${GITHUB_RUN_ATTEMPT:-1}
branch="release-gate-canary-${run_id}-${run_attempt}"
canary_path=".release-gate-canary/${run_id}-${run_attempt}.txt"
pr_number=""
cleanup_failed=false

if [[ ! "$base_sha" =~ ^[0-9a-f]{40}$ ]]; then
  echo "canary base must be a full commit SHA: $base_sha" >&2
  exit 64
fi
if [[ -z "${GH_TOKEN:-}" ]]; then
  echo "GH_TOKEN is required" >&2
  exit 64
fi

cleanup() {
  local original_status=$?
  trap - EXIT
  set +e
  if [[ -n "$pr_number" ]]; then
    if ! gh api --method PATCH "repos/$repo/pulls/$pr_number" \
      -f state=closed --silent; then
      echo "failed to close canary PR #$pr_number" >&2
      cleanup_failed=true
    fi
  fi
  if ! gh api --method DELETE "repos/$repo/git/refs/heads/$branch" \
    --silent 2>/dev/null; then
    echo "failed to delete canary branch $branch" >&2
    cleanup_failed=true
  fi
  if [[ "$cleanup_failed" == true && $original_status -eq 0 ]]; then
    exit 1
  fi
  exit "$original_status"
}
trap cleanup EXIT

rules=$(gh api "repos/$repo/rules/branches/$base_branch")
jq -e '
  any(.[]; .type == "pull_request" and
    (.parameters.required_approving_review_count // 0) >= 1 and
    (.parameters.require_code_owner_review // false) == true and
    (.parameters.dismiss_stale_reviews_on_push // false) == true) and
  any(.[]; .type == "required_status_checks" and
    (.parameters.strict_required_status_checks_policy // false) == true and
    any(.parameters.required_status_checks[]?;
      .context == "Required CI" and .integration_id == 15368))
' <<< "$rules" >/dev/null || {
  echo "main lacks the enforced PR/CODEOWNER/up-to-date Required CI rules" >&2
  exit 1
}

current_base=$(gh api "repos/$repo/git/ref/heads/$base_branch" --jq '.object.sha')
if [[ "$current_base" != "$base_sha" ]]; then
  echo "main advanced from dry-run SHA $base_sha to $current_base; refusing stale canary evidence" >&2
  exit 1
fi

base_commit=$(gh api "repos/$repo/git/commits/$base_sha")
base_tree=$(jq -er '.tree.sha' <<< "$base_commit")
blob_payload=$(jq -n \
  --arg content "release gate canary for workflow run $run_id attempt $run_attempt" \
  '{content: $content, encoding: "utf-8"}')
blob_sha=$(gh api --method POST "repos/$repo/git/blobs" \
  --input - --jq '.sha' <<< "$blob_payload")
tree_payload=$(jq -n \
  --arg base_tree "$base_tree" \
  --arg path "$canary_path" \
  --arg sha "$blob_sha" \
  '{base_tree: $base_tree,
    tree: [{path: $path, mode: "100644", type: "blob", sha: $sha}]}')
tree_sha=$(gh api --method POST "repos/$repo/git/trees" \
  --input - --jq '.sha' <<< "$tree_payload")
commit_payload=$(jq -n \
  --arg message "test: release gate canary" \
  --arg tree "$tree_sha" \
  --arg parent "$base_sha" \
  '{message: $message, tree: $tree, parents: [$parent]}')
canary_sha=$(gh api --method POST "repos/$repo/git/commits" \
  --input - --jq '.sha' <<< "$commit_payload")
ref_payload=$(jq -n \
  --arg ref "refs/heads/$branch" \
  --arg sha "$canary_sha" \
  '{ref: $ref, sha: $sha}')
gh api --method POST "repos/$repo/git/refs" --input - \
  --silent <<< "$ref_payload"

pr_payload=$(jq -n \
  --arg title "test: release gate canary $run_id/$run_attempt" \
  --arg head "$branch" \
  --arg base "$base_branch" \
  --arg body "Temporary automation-authored release-gate proof. This PR is closed automatically." \
  '{title: $title, head: $head, base: $base, body: $body, draft: false}')
pr_json=$(gh api --method POST "repos/$repo/pulls" --input - <<< "$pr_payload")
pr_number=$(jq -er '.number' <<< "$pr_json")

run_state=absent
run_id_observed=""
job_count=0
for _ in $(seq 1 18); do
  runs=$(gh api "repos/$repo/actions/workflows/ci.yml/runs?head_sha=$canary_sha&event=pull_request&per_page=20")
  run=$(jq -c --arg sha "$canary_sha" '
    [.workflow_runs[] | select(.head_sha == $sha and .event == "pull_request")]
    | sort_by(.id) | last // empty
  ' <<< "$runs")
  if [[ -n "$run" ]]; then
    run_id_observed=$(jq -er '.id' <<< "$run")
    run_state=$(jq -r '.conclusion // .status' <<< "$run")
    if [[ "$run_state" == action_required ]]; then
      jobs=$(gh api "repos/$repo/actions/runs/$run_id_observed/jobs?per_page=100")
      job_count=$(jq -er '.total_count' <<< "$jobs")
      [[ "$job_count" -eq 0 ]] || {
        echo "action_required canary run unexpectedly created $job_count jobs" >&2
        exit 1
      }
      break
    fi
    if [[ "$run_state" != queued && "$run_state" != in_progress && "$run_state" != requested && "$run_state" != waiting ]]; then
      echo "canary workflow reached unexpected state $run_state" >&2
      exit 1
    fi
  fi
  sleep 5
done
if [[ "$run_state" != action_required && "$run_state" != absent ]]; then
  echo "canary workflow never reached action_required or a stable absent state" >&2
  exit 1
fi

checks=$(gh api -H 'Accept: application/vnd.github+json' \
  "repos/$repo/commits/$canary_sha/check-runs?per_page=100")
required_successes=$(jq '[.check_runs[] |
  select(.name == "Required CI" and .conclusion == "success")] | length' <<< "$checks")
[[ "$required_successes" -eq 0 ]] || {
  echo "canary unexpectedly received a successful Required CI check" >&2
  exit 1
}

merge_state=unknown
mergeable=""
for _ in $(seq 1 12); do
  pr_json=$(gh api "repos/$repo/pulls/$pr_number")
  merge_state=$(jq -r '.mergeable_state // "unknown"' <<< "$pr_json")
  mergeable=$(jq -r '.mergeable | tostring' <<< "$pr_json")
  [[ "$merge_state" != unknown ]] && break
  sleep 5
done
[[ "$merge_state" == blocked ]] || {
  echo "canary PR was not blocked by the applied rules (state: $merge_state)" >&2
  exit 1
}
final_base=$(gh api "repos/$repo/git/ref/heads/$base_branch" --jq '.object.sha')
[[ "$final_base" == "$base_sha" ]] || {
  echo "main advanced during the canary; the blocked state is not exact-SHA evidence" >&2
  exit 1
}

jq -n \
  --argjson pr_number "$pr_number" \
  --arg branch "$branch" \
  --arg base_sha "$base_sha" \
  --arg canary_sha "$canary_sha" \
  --arg run_state "$run_state" \
  --arg run_id "$run_id_observed" \
  --argjson job_count "$job_count" \
  --arg merge_state "$merge_state" \
  --arg mergeable "$mergeable" \
  '{pr_number: $pr_number, branch: $branch, base_sha: $base_sha,
    canary_sha: $canary_sha, workflow_state: $run_state,
    workflow_run_id: $run_id, workflow_job_count: $job_count,
    required_ci_success_count: 0, merge_state: $merge_state,
    mergeable: $mergeable, cleanup: "completed-on-process-exit"}'
