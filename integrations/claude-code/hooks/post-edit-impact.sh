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
    if [ -n "$IMPACT" ] && [ "$IMPACT" != "[]" ]; then
      COUNT=$(echo "$IMPACT" | jq 'length')
      if [ "$COUNT" -gt 0 ]; then
        echo "--- Impact Analysis ---" >&2
        echo "Edit may affect $COUNT dependent symbol(s):" >&2
        echo "$IMPACT" | jq -r '.[0:3] | .[] | "  -> \(.name) (\(.edge_type), depth \(.depth))"' >&2
        echo "-----------------------" >&2
      fi
    fi
  fi
fi
