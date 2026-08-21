#!/usr/bin/env bash
set -euo pipefail

INPUT=$(cat)
FILE_PATH=$(echo "$INPUT" | jq -r '.file_path // empty')
[ -z "$FILE_PATH" ] && exit 0

DB="${NESTWEAVER_DB:-./nestweaver.lbug}"
[ ! -f "$DB" ] && exit 0

SYMBOLS=$(nestweaver search "$(basename "$FILE_PATH" | sed 's/\.[^.]*$//')" --json --db "$DB" 2>/dev/null || true)
# `nestweaver search --json` returns {"results": [...]} on 6.4+ and a bare
# array before it. Normalise to an array so this hook works against either
# binary on PATH — a hook installed by `nestweaver setup` outlives any one
# release.
ROWS=$(printf '%s' "$SYMBOLS" | jq -c 'if type == "object" then (.results // []) else . end' 2>/dev/null || printf '[]')
if [ "$(printf '%s' "$ROWS" | jq 'length')" -gt 0 ]; then
  echo "--- NestWeaver Context ---" >&2
  echo "Symbols related to $(basename "$FILE_PATH"):" >&2
  printf '%s' "$ROWS" | jq -r '.[0:5] | .[] | "  \(.name) (\(.kind)) @ \(.file_path):\(.start_line)"' >&2
  echo "--------------------------" >&2
fi
