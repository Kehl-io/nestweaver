#!/usr/bin/env bash
set -euo pipefail

INPUT=$(cat)
FILE_PATH=$(echo "$INPUT" | jq -r '.file_path // empty')
[ -z "$FILE_PATH" ] && exit 0

DB="${NESTWEAVER_DB:-./nestweaver.lbug}"
[ ! -f "$DB" ] && exit 0

SYMBOLS=$(nestweaver search "$(basename "$FILE_PATH" | sed 's/\.[^.]*$//')" --json --db "$DB" 2>/dev/null || true)
# `nestweaver search --json` returns {"results": [...]} on 6.4+ and a bare
# array before it. Normalise so this hook works against either binary on PATH.
# `impact --json` is a bare array on both and is left alone.
ROWS=$(printf '%s' "$SYMBOLS" | jq -c 'if type == "object" then (.results // []) else . end' 2>/dev/null || printf '[]')
if [ "$(printf '%s' "$ROWS" | jq 'length')" -gt 0 ]; then
  FIRST_UID=$(printf '%s' "$ROWS" | jq -r '.[0].uid // empty')
  if [ -n "$FIRST_UID" ]; then
    IMPACT=$(nestweaver impact "$FIRST_UID" --depth 2 --json --db "$DB" 2>/dev/null || true)
    # `impact --json` is an OBJECT: {nodes, total, returned, truncated,
    # truncated_by_depth, truncated_by_limit, truncated_by_threshold, status,
    # symbol}. It was a bare array before 6.4. Normalise so this hook works
    # against either binary on PATH — a hook installed by `nestweaver setup`
    # outlives any one release. Running `jq 'length'` on the object counted its
    # KEYS, not its dependents, and the `.[0:3]` slice then failed outright.
    NODES=$(printf '%s' "$IMPACT" | jq -c 'if type == "object" then (.nodes // []) else . end' 2>/dev/null || printf '[]')
    COUNT=$(printf '%s' "$NODES" | jq 'length' 2>/dev/null || echo 0)
    if [ "$COUNT" -gt 0 ]; then
      # `total` is the pre-cut count; `--limit` defaults to 50 and maxes at
      # 1000, so a wide blast radius is truncated and says so.
      TOTAL=$(printf '%s' "$IMPACT" | jq -r 'if type == "object" then (.total // empty) else empty end' 2>/dev/null || true)
      SUFFIX=""
      if [ -n "$TOTAL" ] && [ "$TOTAL" != "$COUNT" ]; then
        SUFFIX=" (showing $COUNT of $TOTAL — result set was capped)"
      fi
      echo "--- Impact Analysis ---" >&2
      echo "Edit may affect $COUNT dependent symbol(s)$SUFFIX:" >&2
      printf '%s' "$NODES" | jq -r '.[0:3] | .[] | "  -> \(.name) (\(.edge_type), depth \(.depth))"' >&2
      echo "-----------------------" >&2
    fi
  fi
fi
