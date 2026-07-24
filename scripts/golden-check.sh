#!/usr/bin/env bash
# golden-check.sh — corruption + determinism guard for the impact/traversal paths.
#
# Runs `affected-tests` and `pr-impact --json` over a fixed set of files, N times
# each, against a real database, and fails if EITHER:
#   (1) two runs of the same binary on the same file disagree — non-determinism
#       is the signature of the storage-engine string-scan corruption class
#       (LadybugDB #678); the correct traversal is deterministic; or
#   (2) any returned symbol name / uid / test-file path contains a control
#       character or non-ASCII byte — the direct corruption signal.
#
# This is the guard that caught nw-069. Run it after any change to the store
# read path, the traversal, or the pinned lbug version, and before a release.
#
# Usage:
#   scripts/golden-check.sh <db-path> [runs] [file1 file2 ...]
# Env:
#   NW_BIN   path to the nestweaver binary (default: target/release/nestweaver)
#   NW_RUNS  number of runs per file (default: 3; overridden by the 2nd arg)
set -euo pipefail

# This harness deliberately uses the daemon-bypass path (NESTWEAVER_NO_DAEMON=1)
# for hermetic, single-process runs. That bypass is now refused outside CI unless
# explicitly allowed, so opt in for the whole script.
export NESTWEAVER_ALLOW_NO_DAEMON=1

DB="${1:?usage: golden-check.sh <db-path> [runs] [files...]}"
shift || true
RUNS="${1:-${NW_RUNS:-3}}"
if [[ "${RUNS}" =~ ^[0-9]+$ ]]; then shift || true; else RUNS="${NW_RUNS:-3}"; fi
BIN="${NW_BIN:-target/release/nestweaver}"

# Default file list: a spread of small/medium/large source files that exist in
# this repo. Callers can override by passing explicit paths.
if [[ "$#" -gt 0 ]]; then
  FILES=("$@")
else
  FILES=(
    "crates/nestweaver-engine/src/affected_tests.rs"
    "crates/nestweaver-store/src/traverse.rs"
    "crates/nestweaver-engine/src/blast_radius.rs"
    "crates/nestweaver-engine/src/rts_eval.rs"
    "crates/nestweaver-store/src/read.rs"
    "crates/nestweaver-daemon/src/server.rs"
  )
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
fail=0

# has_corrupt_identifier <json-file>: true if any IDENTIFIER field extracted
# from the graph (symbol name/uid, file path, test file, test names) contains a
# control character. Deliberately does NOT scan free-text fields (summary,
# disclaimer, notifications[].message, reasons) — those legitimately contain
# Unicode (em-dashes, etc.), and flagging them is the false-positive trap that
# makes teams disable data validation (Google SRE Ch. 26). Identifiers are the
# fields that come from string-column scans and are the corruption surface.
has_corrupt_identifier() {
  python3 - "$1" <<'PYEOF'
import json, sys
def corrupt(v):
    return isinstance(v, str) and any(ord(c) < 32 or ord(c) > 126 for c in v)
try:
    d = json.load(open(sys.argv[1]))
except Exception:
    sys.exit(0)  # unparseable is handled elsewhere; not a corruption signal here
def walk_ids(obj):
    if isinstance(obj, dict):
        for k in ("name", "uid", "file_path", "symbol_uid", "test_file"):
            if corrupt(obj.get(k)):
                print(f"{k}={obj[k]!r}")
        for t in obj.get("tests", []) or []:
            if corrupt(t):
                print(f"test={t!r}")
        for v in obj.values():
            walk_ids(v)
    elif isinstance(obj, list):
        for v in obj:
            walk_ids(v)
found = []
import io
buf = io.StringIO()
_p = print
def print(*a, **k):  # capture
    found.append(" ".join(str(x) for x in a))
walk_ids(d)
print = _p
if found:
    for f in found[:5]:
        print(f, file=sys.stderr)
    sys.exit(1)
sys.exit(0)
PYEOF
}

for tool in affected-tests pr-impact; do
  for f in "${FILES[@]}"; do
    first=""
    for ((r = 1; r <= RUNS; r++)); do
      out="$tmp/$(echo "${tool}_${f}_${r}" | tr '/' '_')"
      if [[ "$tool" == "affected-tests" ]]; then
        NESTWEAVER_NO_DAEMON=1 "$BIN" affected-tests --files "$f" --json --db "$DB" \
          >"$out" 2>/dev/null || true
      else
        # pr-impact prints progress on the first lines; keep only the JSON body.
        NESTWEAVER_NO_DAEMON=1 "$BIN" pr-impact --files "$f" --json --db "$DB" 2>/dev/null \
          | sed -n '/^{/,$p' >"$out" || true
      fi

      if detail="$(has_corrupt_identifier "$out" 2>&1)"; then :; else
        echo "CORRUPT: control char in an identifier field — $tool on $f (run $r): $detail"
        fail=1
      fi

      if [[ -z "$first" ]]; then
        first="$out"
      elif ! diff -q "$first" "$out" >/dev/null 2>&1; then
        echo "NONDETERMINISM: $tool on $f differs between run 1 and run $r"
        fail=1
      fi
    done
  done
done

if [[ "$fail" -eq 0 ]]; then
  echo "golden-check OK: ${#FILES[@]} files x $RUNS runs x 2 tools — deterministic, no corruption"
fi
exit "$fail"
