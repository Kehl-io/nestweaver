#!/usr/bin/env bash
# Classify what a downstream job resolves from the `release-pr` job's outputs.
#
# nw-426 built a diagnostic that failed whenever `release-pr` succeeded and its
# `number` output read empty. nw-465: that test cannot tell the two apart.
#
#   * OUTPUT LOSS   -- the producer located a PR and the value did not survive
#                      the job-output mapping. This is the nw-426 fault.
#   * NO RELEASE PR -- the producer looked, correctly found none, logged
#                      `located release PR: <none>` and exited 0. This is the
#                      ordinary state right after a release PR is merged.
#
# Both present as `result=success, number=""`, so the diagnostic turned every
# successful publication into a red workflow. Reproduced on the v9.3.0 release,
# run 34187421838: publication and npm jobs both succeeded and all artifacts
# verified, yet the workflow was red.
#
# The producer therefore also emits `located`, an explicit statement of what it
# BELIEVED, and this script fails only on a contradiction between that belief
# and the values that actually arrived.
#
# Kept as a script rather than inline YAML so the four cases are testable --
# see tests/release_pr_diagnostic_test.rs.
#
# Inputs (environment):
#   RESULT   -- needs.release-pr.result
#   LOCATED  -- needs.release-pr.outputs.located ("true" | "false")
#   NUMBER, BRANCH, HEAD_SHA -- the corresponding outputs
#
# Exit: 0 = consistent, 1 = contradictory (a real propagation fault).
set -euo pipefail

RESULT="${RESULT:-}"
LOCATED="${LOCATED:-}"
NUMBER="${NUMBER:-}"
BRANCH="${BRANCH:-}"
HEAD_SHA="${HEAD_SHA:-}"

# Bracketed so an empty string is visually obvious in the log.
echo "release-pr.result=[$RESULT]"
echo "release-pr.outputs.located=[$LOCATED]"
echo "release-pr.outputs.number=[$NUMBER]"
echo "release-pr.outputs.branch=[$BRANCH]"
echo "release-pr.outputs.head_sha=[$HEAD_SHA]"

fail() {
  echo "::error::$1" >&2
  exit 1
}

# A producer that did not succeed is already a red job. Re-failing here would
# add a second red job with a less specific message and no new information.
if [ "$RESULT" != "success" ]; then
  echo "release-pr did not succeed (result=[$RESULT]); its own job reports the cause."
  exit 0
fi

case "$LOCATED" in
  true)
    [ -n "$NUMBER" ] || fail \
      "release-pr located a release PR but its 'number' output reads empty here -- this is nw-426's job-output propagation failure reproducing. prepare-release-lockfile will skip and a manual Cargo.lock sync will be needed again."
    # A located PR must carry its full identity. A partial mapping is the same
    # class of fault and would otherwise reach the lockfile job as a valid PR.
    [ -n "$BRANCH" ] || fail \
      "release-pr located PR #$NUMBER but its 'branch' output reads empty here -- partial job-output loss (nw-426 class)."
    [ -n "$HEAD_SHA" ] || fail \
      "release-pr located PR #$NUMBER but its 'head_sha' output reads empty here -- partial job-output loss (nw-426 class). The exact-head lease cannot be taken without it."
    echo "consistent: release PR #$NUMBER ($BRANCH @ $HEAD_SHA) propagated intact."
    ;;
  false)
    # nw-465: the valid post-release state. Not a fault, and not silence --
    # it is stated, so a reader can tell it from a lost output.
    [ -z "$NUMBER" ] || fail \
      "release-pr reported locating no release PR yet a 'number' output of [$NUMBER] arrived here -- the producer's belief and its outputs contradict each other."
    echo "consistent: no open release PR, and no PR identity was propagated. This is the expected state immediately after a release PR is merged."
    ;;
  *)
    # `located` is itself a job output. If it is missing the mapping lost it,
    # which is the very fault this job exists to catch -- passing here would
    # reintroduce the blind spot one level up.
    fail "release-pr succeeded but its 'located' output reads [$LOCATED]; expected \"true\" or \"false\". The job-output mapping lost it (nw-426 class), so no claim about the release PR can be trusted."
    ;;
esac
