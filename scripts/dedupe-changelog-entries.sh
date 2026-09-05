#!/usr/bin/env bash

# nw-096: release-please parses commits with a FULL-ANCESTRY walk (GitHub's
# GraphQL `history` on a ref includes every reachable commit, not just the
# first-parent chain -- verified live against this repository's own history:
# a non-first-parent branch commit (429e563) appears in the same `history`
# page as the merge commit that brought it in). This repository's PRs land
# with real merge commits (not squashes) because staging/main merges must
# preserve the conventional commits release-please reads -- squashing THAT
# merge would destroy them. But a merge commit's own message is
# "Merge pull request #N from branch\n\n<PR title>", and release-please's
# `splitMessages()` (src/commit.ts) splits on a blank line followed by a
# conventional-commit prefix, extracting the PR-title paragraph as its OWN
# synthetic conventional commit -- IN ADDITION to walking the actual branch
# commit(s) that already carry that same or a matching description. Two
# release-please-visible commits, one deliverable, one changelog line
# rendered twice. Confirmed recurring 4 times (2.6.3, 7.0.0, 8.0.0, 9.1.0);
# checked upstream (googleapis/release-please#2476): the maintainers'
# position is "we recommend squash-merge and do not have current plans to
# attempt to handle merge commits" -- there is no release-please-config.json
# field that suppresses this. This script is the enforcement point instead:
# it runs on whatever markdown release-please generates (a CHANGELOG.md
# section, or a GitHub Release body) and removes a later bullet whose
# rendered text is byte-identical to an earlier one in the same subsection,
# keeping the first (typically the chronologically earlier, more specific
# commit) and its link.
#
# Usage:
#   dedupe-changelog-entries.sh <input-file>        write deduped text to stdout
#   dedupe-changelog-entries.sh --self-test          verify against this repo's
#                                                     own already-shipped 9.1.0
#                                                     CHANGELOG.md section
#
# Duplicate/removed-count summary goes to stderr so stdout stays exactly the
# markdown the caller wants to write back.

set -euo pipefail

dedupe() {
  local input=$1
  awk '
    BEGIN { removed = 0 }
    {
      line = $0
      # Track the current changelog subsection ("### Features", "### Bug
      # Fixes", ...) so dedup never merges entries across sections -- an
      # identical description under two different types is a coincidence,
      # not the nw-096 mechanism, and must not be collapsed.
      if (line ~ /^### /) {
        section = line
        print line
        next
      }
      # A release-please bullet: "* [**scope:**] text ([sha](.../commit/sha))".
      # Strip only the trailing commit link to get the dedup key; everything
      # else about the line (including any inline PR links in the text) is
      # part of the description and must match exactly to count as the same
      # deliverable.
      if (line ~ /^\* .*\(\[[0-9a-f]{7,40}\]\(https:\/\/github\.com\/[^)]+\/commit\/[0-9a-f]{7,40}\)\)$/) {
        key = line
        sub(/ \(\[[0-9a-f]{7,40}\]\(https:\/\/github\.com\/[^)]+\/commit\/[0-9a-f]{7,40}\)\)$/, "", key)
        seen_key = section "\x1f" key
        if (seen[seen_key]++) {
          removed++
          print "DROPPED DUPLICATE: " line > "/dev/stderr"
          next
        }
      }
      print line
    }
    END { print "removed=" removed > "/dev/stderr" }
  ' "$input"
}

case "${1:-}" in
  --self-test)
    repo_root=$(cd "$(dirname "$0")/.." && pwd)
    section_file=$(mktemp)
    trap 'rm -f -- "$section_file"' EXIT
    # Extract exactly the shipped 9.1.0 section: from its header to (but not
    # including) the next "## [" release header.
    awk '
      /^## \[9\.1\.0\]/ { printing = 1 }
      printing && /^## \[9\.0\.5\]/ { exit }
      printing { print }
    ' "$repo_root/CHANGELOG.md" > "$section_file"

    before_count=$(grep -c '^\* ' "$section_file")
    output=$(dedupe "$section_file" 2> "$section_file.stderr")
    after_count=$(printf '%s\n' "$output" | grep -c '^\* ')
    removed=$(grep -oE 'removed=[0-9]+' "$section_file.stderr" | cut -d= -f2)

    echo "before=$before_count after=$after_count removed=$removed" >&2

    # The 9.1.0 section, as actually shipped, is known (nw-096) to carry
    # exactly 3 duplicate pairs:
    #   disclosure, bounds and coverage sweep across 13 items
    #   security, coverage-disclosure and merge-gate sweep across 12 items
    #   release: let release-context read the draft it validates
    [ "$removed" -eq 3 ]
    [ "$((before_count - removed))" -eq "$after_count" ]
    for text in \
      'disclosure, bounds and coverage sweep across 13 items' \
      'security, coverage-disclosure and merge-gate sweep across 12 items' \
      'let release-context read the draft it validates'; do
      occurrences=$(printf '%s\n' "$output" | grep -Fc "$text")
      if [ "$occurrences" -ne 1 ]; then
        echo "expected exactly one surviving entry for: $text (found $occurrences)" >&2
        exit 1
      fi
    done
    # Every other bullet must survive untouched -- this is a dedup, not a
    # generic filter. Every non-duplicate line from the input must still be
    # present in the output exactly once.
    while IFS= read -r bullet; do
      case "$bullet" in
        *'disclosure, bounds and coverage sweep across 13 items'*) continue ;;
        *'security, coverage-disclosure and merge-gate sweep across 12 items'*) continue ;;
        *'let release-context read the draft it validates'*) continue ;;
      esac
      count=$(printf '%s\n' "$output" | grep -Fc -- "$bullet")
      if [ "$count" -ne 1 ]; then
        echo "non-duplicate bullet was not preserved exactly once: $bullet" >&2
        exit 1
      fi
    done < <(grep '^\* ' "$section_file")
    rm -f -- "$section_file.stderr"
    echo "dedupe-changelog-entries self-test passed (removed 3 confirmed nw-096 duplicates, all else preserved)"
    ;;
  "")
    echo "usage: $0 <changelog-section-file> | --self-test" >&2
    exit 64
    ;;
  *)
    if [ $# -ne 1 ]; then
      echo "usage: $0 <changelog-section-file> | --self-test" >&2
      exit 64
    fi
    dedupe "$1"
    ;;
esac
