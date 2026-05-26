#!/usr/bin/env bash
set -euo pipefail

INPUT=$(cat)
FILE_PATH=$(echo "$INPUT" | jq -r '.file_path // empty')
[ -z "$FILE_PATH" ] && exit 0

DB="${NESTWEAVER_DB:-./nestweaver.lbug}"
[ ! -f "$DB" ] && exit 0

SYMBOLS=$(nestweaver search "$(basename "$FILE_PATH" | sed 's/\.[^.]*$//')" --json --db "$DB" 2>/dev/null || true)
if [ -n "$SYMBOLS" ] && [ "$SYMBOLS" != "[]" ]; then
  echo "--- NestWeaver Context ---" >&2
  echo "Symbols related to $(basename "$FILE_PATH"):" >&2
  echo "$SYMBOLS" | jq -r '.[0:5] | .[] | "  \(.name) (\(.kind)) @ \(.file_path):\(.start_line)"' >&2
  echo "--------------------------" >&2
fi
