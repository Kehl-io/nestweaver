#!/usr/bin/env bash
set -euo pipefail

INPUT=$(cat)
FILE_PATH=$(echo "$INPUT" | jq -r '.file_path // empty')
[ -z "$FILE_PATH" ] && exit 0

DB="${NESTWEAVER_DB:-./nestweaver.lbug}"
[ ! -f "$DB" ] && exit 0

SYMBOLS=$(nestweaver search "$(basename "$FILE_PATH" | sed 's/\.[^.]*$//')" --json --db "$DB" 2>/dev/null || true)
if [ -n "$SYMBOLS" ] && [ "$SYMBOLS" != "[]" ]; then
  FIRST_UID=$(echo "$SYMBOLS" | jq -r '.[0].uid // empty')
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
