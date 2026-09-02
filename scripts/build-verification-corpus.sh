#!/usr/bin/env bash
# nw-413: build a DISPOSABLE, REPEATABLE, multi-repo verification corpus.
#
# Five open backlog items are blocked on "measure this against a real multi-repo
# graph, not a fixture" (nw-291, nw-308, nw-322, nw-351, nw-358). Every one of
# them had been measured only against Kory's personal brain DB, which nobody
# else can reproduce. This builds an equivalent graph from public sources.
#
# Usage:
#   scripts/build-verification-corpus.sh            # full corpus (~20 repos)
#   scripts/build-verification-corpus.sh --small    # 5 repos, for iterating on the script
#   scripts/build-verification-corpus.sh --teardown # delete everything
#
# Env:
#   CORPUS_DIR   where clones + the .lbug live   (default /tmp/nw-corpus)
#   NW           the nestweaver binary to index with (default: cargo run --release)
#
# Entries are pinned to RELEASE TAGS, not commit SHAs, and the difference matters:
# a tag is mutable, so this is weaker pinning than the ideal. It is recorded here
# rather than overstated because an earlier draft of this comment claimed SHA
# pinning in capitals while the manifest below used tags — a false claim in a
# comment is the same defect class as a false claim in a payload.
#
# Tags are used because they are legible and stable in practice for these
# projects; `--teardown` plus a re-run gives a fresh corpus if one ever moves.
# The script RESOLVES each ref to a concrete SHA at clone time and PRINTS it, so
# any measurement can record exactly what it ran against. Anyone needing
# reproducibility stronger than "the tag did not move" should paste the printed
# SHAs back into the manifest.
#
# One entry is a floating BRANCH (`TypeScript-Node-Starter|master`) because the
# project publishes no tags. It is flagged as such in the output rather than
# silently mixed in with the tagged entries.
set -u
cd "$(dirname "$0")/.."

CORPUS_DIR="${CORPUS_DIR:-/tmp/nw-corpus}"
DB="$CORPUS_DIR/corpus.lbug"

# repo|sha|language — chosen for language spread and for sizes that make the
# internal caps actually bind (DEFAULT_RETRIEVAL_BREADTH=30, HUB_COUNT=30,
# MAX_CLUSTER_SUMMARIES=50, bound_identifiers MAX_COUNT=1000). A corpus that
# does not exceed a cap cannot prove the cap discloses itself.
REPOS_FULL=$(cat <<'EOF'
https://github.com/BurntSushi/ripgrep|14.1.1|rust
https://github.com/serde-rs/serde|v1.0.215|rust
https://github.com/clap-rs/clap|v4.5.21|rust
https://github.com/tokio-rs/tokio|tokio-1.42.0|rust
https://github.com/expressjs/express|4.21.2|javascript
https://github.com/lodash/lodash|4.17.21|javascript
https://github.com/axios/axios|v1.7.9|javascript
https://github.com/microsoft/TypeScript-Node-Starter|master|typescript
https://github.com/pallets/flask|3.1.0|python
https://github.com/psf/requests|v2.32.3|python
https://github.com/python/mypy|v1.13.0|python
https://github.com/gin-gonic/gin|v1.10.0|go
https://github.com/spf13/cobra|v1.8.1|go
https://github.com/nlohmann/json|v3.11.3|cpp
https://github.com/fmtlib/fmt|11.0.2|cpp
https://github.com/catchorg/Catch2|v3.7.1|cpp
https://github.com/google/guava|v33.4.0-jre|java
https://github.com/square/retrofit|parent-2.11.0|java
https://github.com/rails/rails|v8.0.1|ruby
https://github.com/laravel/framework|v11.34.2|php
EOF
)
REPOS_SMALL=$(printf '%s\n' "$REPOS_FULL" | head -5)

teardown() {
  echo "removing $CORPUS_DIR"
  # Stop any daemon that autostarted against the corpus DB before deleting it,
  # otherwise the next run inherits a daemon pointing at a path that no longer
  # exists — the exact wedge nw-377 describes.
  if [ -f "$DB" ]; then
    ${NW:-cargo run --release --} daemon --db "$DB" stop >/dev/null 2>&1 || true
  fi
  rm -rf "$CORPUS_DIR"
  echo "done"
}

case "${1:-}" in
  --teardown) teardown; exit 0 ;;
  --small)    REPOS="$REPOS_SMALL" ;;
  "")         REPOS="$REPOS_FULL" ;;
  *)          echo "unknown argument: $1" >&2; exit 64 ;;
esac

NW="${NW:-cargo run --release --}"
mkdir -p "$CORPUS_DIR/src"

echo "corpus dir: $CORPUS_DIR"
echo "database:   $DB"
echo

# --- clone (shallow, pinned) -------------------------------------------------
while IFS='|' read -r url sha lang; do
  [ -z "$url" ] && continue
  name=$(basename "$url")
  dest="$CORPUS_DIR/src/$name"
  if [ -d "$dest/.git" ]; then
    echo "have    $name ($lang)"
    continue
  fi
  echo "clone   $name ($lang) @ $sha"
  # --filter=blob:none keeps the checkout small without losing the ability to
  # resolve the pinned ref; a plain --depth 1 cannot check out an arbitrary sha.
  if ! git clone --quiet --filter=blob:none --no-checkout "$url" "$dest" 2>/dev/null; then
    echo "  SKIP (clone failed — network or renamed repo)" >&2
    rm -rf "$dest"
    continue
  fi
  if ! git -C "$dest" checkout --quiet "$sha" 2>/dev/null; then
    echo "  SKIP (pinned ref '$sha' not found — repo moved its tags)" >&2
    rm -rf "$dest"
    continue
  fi
  # Record what the mutable ref actually resolved to, so a measurement taken on
  # this corpus can be reproduced even if the tag later moves.
  resolved=$(git -C "$dest" rev-parse HEAD 2>/dev/null || echo unknown)
  printf '        resolved %s -> %s\n' "$sha" "${resolved:0:12}"
  if [ "$sha" = "master" ] || [ "$sha" = "main" ]; then
    echo "        NOTE: floating branch, not a tag — this entry is not reproducible by ref alone" >&2
  fi
done <<< "$REPOS"

# --- index -------------------------------------------------------------------
echo
indexed=0
for dest in "$CORPUS_DIR"/src/*/; do
  [ -d "$dest" ] || continue
  name=$(basename "$dest")
  printf 'index   %-28s ' "$name"
  if $NW index --repo "$dest" --db "$DB" --no-embed >/dev/null 2>&1; then
    echo ok
    indexed=$((indexed + 1))
  else
    # A failed repo is reported, never silently dropped — a corpus that quietly
    # indexed 12 of 20 repos would make every measurement taken on it wrong in
    # a direction nobody could see.
    echo "FAILED"
  fi
done

# --- report ------------------------------------------------------------------
echo
echo "=== corpus built ==="
echo "repos indexed: $indexed"
$NW list-repos --db "$DB" 2>/dev/null | tail -5
echo
echo "Scale check — these are the numbers that decide whether the corpus is big"
echo "enough for the caps under test to actually bind:"
$NW search "" --db "$DB" --limit 10000 --json 2>/dev/null > "$CORPUS_DIR/.scale.json" || true
python3 - "$CORPUS_DIR/.scale.json" <<'PYEOF' || echo "  (symbol count unavailable)"
import json, sys
try:
    d = json.load(open(sys.argv[1]))
except Exception:
    print("  (symbol count unavailable)"); raise SystemExit(0)
n = d.get("returned", 0)
print(f"  symbols in capped view: {n} (truncated={d.get('truncated')})")
# The 10000 presentation ceiling is itself a cap; hitting it means the corpus is
# at least that big, which is the property the blocked items actually need.
print("  scale: SUFFICIENT — exceeds the 10k presentation ceiling" if d.get("truncated")
      else f"  scale: thin at {n} symbols; add repos before trusting a cap measurement")
PYEOF
echo
echo "Use with:  --db $DB"
echo "Teardown:  scripts/build-verification-corpus.sh --teardown"
