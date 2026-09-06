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
# field that suppresses this. This script is the enforcement point instead.
#
# SCOPED TO THE NEWEST SECTION ONLY, on purpose, after a real defect: an
# earlier version of this script tracked `### ` subsections but never reset
# on `## [version]` boundaries, so it collapsed a bullet that legitimately
# appears in MULTIPLE shipped releases (early bootstrap history genuinely
# shared across v0.2.0, v0.3.0 and v0.12.0, all real, all independently
# tagged) into one, silently deleting real change history from two already-
# published releases. Every version below the newest is FROZEN history and
# is never touched, structurally -- this script locates the first and second
# `## [` headers and only ever looks at the slice between them.
#
# A duplicate bullet is removed TOGETHER WITH ITS CONTINUATION -- every line
# after it up to (not including) the next top-level bullet or `### ` header
# -- never just its header line. This CHANGELOG has real multi-paragraph
# bullets (code fences, indented notes); deleting only the header left an
# orphaned continuation paragraph reading as coherent prose attributing
# detail from the dropped commit to whichever different, surviving bullet
# happened to come next.
#
# FAILS CLOSED: refuses (nonzero exit, no stdout) rather than emitting a
# result if the section would come out empty, or if it would remove more
# bullets than a small, generous bound -- a rewrite of generated content
# that nobody diffs by hand before it overwrites a live file must never
# silently do something drastically wrong.
#
# Usage:
#   dedupe-changelog-entries.sh <changelog-file>   dedupe only the file's
#                                                   newest `## [...]` section;
#                                                   print the WHOLE file,
#                                                   unchanged everywhere else
#   dedupe-changelog-entries.sh --self-test        verify against this
#                                                   repo's own real,
#                                                   already-shipped
#                                                   CHANGELOG.md

set -euo pipefail

BULLET_RE='^\* .*\(\[[0-9a-f]{7,40}\]\(https://github\.com/[^)]+/commit/[0-9a-f]{7,40}\)\)$'
MAX_REMOVED_ABS=10

# Dedupe bullets WITHIN a single already-extracted version section (the
# caller guarantees no `## [` line appears in $1). Tracks `### ` subsections
# so identical text under two different types is never merged. A duplicate
# bullet's ENTIRE continuation -- everything up to the next top-level bullet
# or `### ` header -- is dropped with it via `drop_mode`, not just its own
# line.
dedupe_section() {
  local input=$1
  # The bullet pattern is a LITERAL awk regex, not $BULLET_RE passed via
  # `-v`, deliberately: `-v var=value` runs `value` through awk's own
  # escape-sequence processing (POSIX: backslash-escapes in a -v assignment
  # are interpreted the same as in a string literal), which silently drops
  # backslashes awk does not recognize as C-style escapes -- `\(` survives
  # as `(`, `\*` as `*`. That turns "literal parenthesis" into "regex
  # grouping" and matched zero lines of a real CHANGELOG.md while reporting
  # nothing wrong. A `/regex/` literal is parsed once, directly, by awk's
  # own regex parser, with no intermediate string-escape pass.
  awk '
    BEGIN { section = ""; drop_mode = 0; removed = 0 }
    {
      line = $0
      if (line ~ /^### /) {
        section = line
        drop_mode = 0
        print line
        next
      }
      if (line ~ /^\* .*\(\[[0-9a-f]{7,40}\]\(https:\/\/github\.com\/[^)]+\/commit\/[0-9a-f]{7,40}\)\)$/) {
        key = line
        sub(/ \(\[[0-9a-f]{7,40}\]\(https:\/\/github\.com\/[^)]+\/commit\/[0-9a-f]{7,40}\)\)$/, "", key)
        seen_key = section "\x1f" key
        if (seen_key in seen) {
          drop_mode = 1
          removed++
          print "DROPPED DUPLICATE (with its continuation through the next entry): " line > "/dev/stderr"
          next
        }
        seen[seen_key] = 1
        drop_mode = 0
        print line
        next
      }
      # An unlinked top-level bullet (e.g. an Upgrade Notes / BREAKING
      # CHANGES entry with no trailing commit link) is a boundary but never a
      # dedup candidate: there is no reliable key for it, so it always
      # survives and always ends whatever drop was in progress.
      if (line ~ /^\* /) {
        drop_mode = 0
        print line
        next
      }
      if (drop_mode) {
        next
      }
      print line
    }
    END { print "removed=" removed > "/dev/stderr" }
  ' "$input"
}

# Locate the newest `## [...]` section in a full CHANGELOG.md, dedupe ONLY
# that slice, and print the whole file back out -- unchanged before and
# after that slice, byte for byte.
dedupe_changelog() {
  local input=$1
  local total_lines first_line second_line preamble_end section_end
  local tmp_section tmp_deduped tmp_stderr
  local before_bullets after_bullets removed half

  total_lines=$(wc -l < "$input")
  first_line=$(grep -nE '^## \[' "$input" | head -1 | cut -d: -f1 || true)

  if [[ -z "$first_line" ]]; then
    # No version header at all -- nothing this script knows how to scope, so
    # there is nothing safe to touch. Passing through unchanged is correct:
    # an unrecognized format must never be guessed at.
    cat "$input"
    return 0
  fi

  second_line=$(grep -nE '^## \[' "$input" | sed -n '2p' | cut -d: -f1 || true)
  if [[ -z "$second_line" ]]; then
    second_line=$((total_lines + 1))
  fi
  preamble_end=$((first_line - 1))
  section_end=$((second_line - 1))

  tmp_section=$(mktemp)
  tmp_deduped=$(mktemp)
  tmp_stderr=$(mktemp)
  # Manual cleanup (a single `rm` right before every exit point), not
  # `trap ... RETURN`: that trap is NOT scoped to this function call -- once
  # set, it fires on every subsequent function return in the whole script,
  # including calls made long after this function's own locals have gone out
  # of scope, which raises "unbound variable" under `set -u` the next time
  # any OTHER function returns.
  local status=0

  sed -n "${first_line},${section_end}p" "$input" > "$tmp_section"
  dedupe_section "$tmp_section" > "$tmp_deduped" 2> "$tmp_stderr"
  cat "$tmp_stderr" >&2
  removed=$(grep -oE 'removed=[0-9]+' "$tmp_stderr" | tail -1 | cut -d= -f2)
  [[ -n "$removed" ]] || removed=0

  before_bullets=$(grep -cE "$BULLET_RE" "$tmp_section" || true)
  after_bullets=$(grep -cE "$BULLET_RE" "$tmp_deduped" || true)

  # FAIL CLOSED, before printing anything reassembled. None of these should
  # ever trigger for the actual nw-096 pattern (a small handful of exact-text
  # duplicate pairs per release); if one does, this is a bug in this script,
  # and refusing beats silently shipping a mangled CHANGELOG that nobody
  # diffs before it overwrites the live file.
  if [[ "$((before_bullets - removed))" -ne "$after_bullets" ]]; then
    echo "refusing: bullet-count arithmetic disagrees with awk's own removed count (before=$before_bullets removed=$removed after=$after_bullets) -- the two counting methods diverging is itself a bug" >&2
    status=1
  elif [[ -s "$tmp_section" && ! -s "$tmp_deduped" ]]; then
    echo "refusing: dedup produced an EMPTY section from a non-empty one" >&2
    status=1
  elif [[ "$removed" -gt "$MAX_REMOVED_ABS" ]]; then
    echo "refusing: dedup would remove $removed bullets from one release section, above the safe bound of $MAX_REMOVED_ABS" >&2
    status=1
  elif [[ "$before_bullets" -gt 0 ]]; then
    half=$(( (before_bullets + 1) / 2 ))
    if [[ "$removed" -gt "$half" ]]; then
      echo "refusing: dedup would remove more than half of this section's bullets ($removed of $before_bullets)" >&2
      status=1
    fi
  fi

  if [[ "$status" -eq 0 ]]; then
    if [[ "$preamble_end" -ge 1 ]]; then
      head -n "$preamble_end" "$input"
    fi
    cat "$tmp_deduped"
    if [[ "$second_line" -le "$total_lines" ]]; then
      tail -n "+${second_line}" "$input"
    fi
  fi

  rm -f -- "$tmp_section" "$tmp_deduped" "$tmp_stderr"
  return "$status"
}

self_test() {
  local repo_root=${1:-$(cd "$(dirname "$0")/.." && pwd)}
  local changelog="$repo_root/CHANGELOG.md"
  local output before_total after_total removed_total

  before_total=$(grep -cE "$BULLET_RE" "$changelog")
  output=$(dedupe_changelog "$changelog" 2> /tmp/dedupe-self-test-stderr.$$)
  removed_total=$(grep -oE 'removed=[0-9]+' /tmp/dedupe-self-test-stderr.$$ | tail -1 | cut -d= -f2)
  rm -f /tmp/dedupe-self-test-stderr.$$
  after_total=$(printf '%s\n' "$output" | grep -cE "$BULLET_RE")

  echo "before=$before_total after=$after_total removed=$removed_total" >&2

  # 1. The newest section ends up with NO duplicate titles left, and exactly
  #    as many bullets were removed as there were duplicates to remove.
  #
  #    The expected count is DERIVED from the input here, by a different
  #    method than the script itself uses (strip each bullet's commit link,
  #    then count titles beyond the first occurrence). It used to be the
  #    literal `3` -- the three nw-096 pairs that happened to sit in the
  #    9.1.0 section when this test was written -- checked against the REAL
  #    CHANGELOG.md. That coupled a self-test to data that changes on EVERY
  #    release: cutting 9.2.0 made the newest section a different section
  #    with one duplicate, and this assertion failed on a script that was
  #    working correctly, blocking the release. A self-test must not depend
  #    on which release happens to be newest.
  local newest_section expected_removed leftover_dupes
  newest_section=$(awk '/^## \[/{n++} n==1' "$changelog")
  expected_removed=$(printf '%s\n' "$newest_section" \
    | grep -E "$BULLET_RE" \
    | sed -E 's/ \(\[[0-9a-f]{7,40}\].*$//' \
    | sort | uniq -c | awk '$1>1 {total += $1 - 1} END {print total+0}')

  if [[ "$removed_total" -ne "$expected_removed" ]]; then
    echo "expected $expected_removed removed bullets (duplicate titles in the newest section of the real CHANGELOG.md), got $removed_total" >&2
    exit 1
  fi
  if [[ "$((before_total - expected_removed))" -ne "$after_total" ]]; then
    echo "bullet count arithmetic does not add up: before=$before_total after=$after_total removed=$expected_removed" >&2
    exit 1
  fi

  # And the point of the whole exercise: nothing duplicated survives in the
  # newest section of the OUTPUT.
  leftover_dupes=$(printf '%s\n' "$output" | awk '/^## \[/{n++} n==1' \
    | grep -E "$BULLET_RE" \
    | sed -E 's/ \(\[[0-9a-f]{7,40}\].*$//' \
    | sort | uniq -d)
  if [[ -n "$leftover_dupes" ]]; then
    echo "duplicate titles survived in the newest section:" >&2
    printf '%s\n' "$leftover_dupes" >&2
    exit 1
  fi

  # 2. THE REGRESSION THIS SELF-TEST EXISTS TO CATCH: a bullet that
  #    legitimately ships in multiple already-released versions (v0.12.0,
  #    nestweaver-v0.3.0, nestweaver-v0.2.0 all really do carry
  #    "resolve TypeScript compilation errors" from bf67137c) must survive
  #    at its full original count. A cross-version dedup bug collapses this
  #    to 1; the fix is that dedup never looks outside the newest section.
  cross_version_occurrences=$(grep -c 'resolve TypeScript compilation errors' "$changelog")
  output_cross_version_occurrences=$(printf '%s\n' "$output" | grep -c 'resolve TypeScript compilation errors')
  if [[ "$output_cross_version_occurrences" -ne "$cross_version_occurrences" ]]; then
    echo "cross-version regression: bf67137's bullet appears $cross_version_occurrences times in the source but $output_cross_version_occurrences times in the output -- historical, already-released sections must never be touched" >&2
    exit 1
  fi
  if [[ "$cross_version_occurrences" -lt 3 ]]; then
    echo "test fixture assumption broke: expected the real CHANGELOG.md to still carry at least 3 occurrences of the bf67137 bullet across old releases" >&2
    exit 1
  fi

  # 3. Everything below the newest section is BYTE-FOR-BYTE unchanged. Locate
  #    the second version header by its own TEXT in each file rather than by
  #    line number: removing bullets shrinks the output, so the same header
  #    sits at a different line number than in the source, and comparing by
  #    line number alone would either false-fail or -- worse -- silently
  #    compare the wrong slices.
  local second_header second_line_source second_line_output
  second_header=$(grep -E '^## \[' "$changelog" | sed -n '2p')
  second_line_source=$(grep -nFx -- "$second_header" "$changelog" | head -1 | cut -d: -f1)
  second_line_output=$(printf '%s\n' "$output" | grep -nFx -- "$second_header" | head -1 | cut -d: -f1)
  local expected_rest actual_rest
  expected_rest=$(mktemp)
  actual_rest=$(mktemp)
  tail -n "+${second_line_source}" "$changelog" > "$expected_rest"
  printf '%s\n' "$output" | tail -n "+${second_line_output}" > "$actual_rest"
  if ! diff -u "$expected_rest" "$actual_rest" >&2; then
    rm -f -- "$expected_rest" "$actual_rest"
    echo "history below the newest section was modified; it must be byte-for-byte identical" >&2
    exit 1
  fi
  rm -f -- "$expected_rest" "$actual_rest"

  echo "real-CHANGELOG.md self-test passed (removed $expected_removed duplicate title(s) from the newest section; $cross_version_occurrences legitimate cross-version duplicates preserved; history below the newest section untouched byte-for-byte)"
}

self_test_continuation_block() {
  local fixture output
  fixture=$(mktemp)
  cat > "$fixture" << 'FIXTURE'
# Changelog

## [1.0.0](https://x/compare/v0.9.0...v1.0.0) (2026-01-01)

### Bug Fixes

* fix: duplicate with a continuation ([1111111](https://github.com/o/r/commit/1111111111111111111111111111111111111111))
* fix: unrelated survivor ([2222222](https://github.com/o/r/commit/2222222222222222222222222222222222222222))
* fix: duplicate with a continuation ([3333333](https://github.com/o/r/commit/3333333333333333333333333333333333333333))

  Extra paragraph explaining the SECOND (dropped) occurrence in detail,
  including a code block:

  ```sh
  nestweaver do-a-thing
  ```
FIXTURE

  output=$(dedupe_changelog "$fixture" 2>/tmp/dedupe-continuation-stderr.$$)
  rm -f /tmp/dedupe-continuation-stderr.$$ "$fixture"

  if printf '%s\n' "$output" | grep -q 'Extra paragraph'; then
    echo "continuation-block regression: the dropped duplicate's continuation paragraph survived, orphaned under a different bullet" >&2
    exit 1
  fi
  if printf '%s\n' "$output" | grep -q 'nestweaver do-a-thing'; then
    echo "continuation-block regression: the dropped duplicate's code fence survived" >&2
    exit 1
  fi
  if ! printf '%s\n' "$output" | grep -qF '1111111'; then
    echo "the FIRST occurrence (and its own continuation) must survive" >&2
    exit 1
  fi
  if printf '%s\n' "$output" | grep -qF '3333333'; then
    echo "the SECOND (duplicate) occurrence must be dropped" >&2
    exit 1
  fi
  if ! printf '%s\n' "$output" | grep -qF '2222222'; then
    echo "the unrelated survivor bullet must be completely unaffected" >&2
    exit 1
  fi
  echo "continuation-block self-test passed"
}

self_test_floor_guard() {
  local fixture
  fixture=$(mktemp)
  {
    echo "# Changelog"
    echo
    echo "## [1.0.0](https://x/compare/v0.9.0...v1.0.0) (2026-01-01)"
    echo
    echo "### Bug Fixes"
    echo
    for sha in 1111111111111111111111111111111111111111 \
      2222222222222222222222222222222222222222 \
      3333333333333333333333333333333333333333 \
      4444444444444444444444444444444444444444; do
      # All four bullets carry the IDENTICAL description on purpose -- an
      # adversarial input engineered to remove 3 of 4 bullets (75%), which
      # must trip the floor guard regardless of the absolute-count bound.
      echo "* fix: identical text every time ([${sha:0:7}](https://github.com/o/r/commit/$sha))"
    done
  } > "$fixture"

  if dedupe_changelog "$fixture" > /tmp/dedupe-floor-stdout.$$ 2>/tmp/dedupe-floor-stderr.$$; then
    rm -f "$fixture" /tmp/dedupe-floor-stdout.$$ /tmp/dedupe-floor-stderr.$$
    echo "floor-guard regression: removing 3 of 4 bullets (75%) must be refused, not silently applied" >&2
    exit 1
  fi
  if [[ -s /tmp/dedupe-floor-stdout.$$ ]]; then
    rm -f "$fixture" /tmp/dedupe-floor-stdout.$$ /tmp/dedupe-floor-stderr.$$
    echo "floor-guard regression: a refused dedup must not print partial output to stdout" >&2
    exit 1
  fi
  rm -f "$fixture" /tmp/dedupe-floor-stdout.$$ /tmp/dedupe-floor-stderr.$$
  echo "floor-guard self-test passed (refused to remove 75% of one section's bullets, no stdout produced)"
}

case "${1:-}" in
  --self-test)
    self_test "${2:-}"
    self_test_continuation_block
    self_test_floor_guard
    ;;
  "")
    echo "usage: $0 <changelog-file> | --self-test [repo-root]" >&2
    exit 64
    ;;
  *)
    if [[ $# -ne 1 ]]; then
      echo "usage: $0 <changelog-file> | --self-test [repo-root]" >&2
      exit 64
    fi
    dedupe_changelog "$1"
    ;;
esac
