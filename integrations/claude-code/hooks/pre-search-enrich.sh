#!/usr/bin/env bash
set -euo pipefail

INPUT=$(cat)
COMMAND=$(echo "$INPUT" | jq -r '.command // empty')
echo "$COMMAND" | grep -qE '(grep|rg)\s' || exit 0

DB="${NESTWEAVER_DB:-./nestweaver.lbug}"
[ ! -f "$DB" ] && exit 0

# Best-effort extraction: takes the last whitespace-delimited or double-quoted
# token from the command. Does not handle single-quoted strings or escaped
# quotes — acceptable for a non-blocking enrichment hook.
TERM=$(echo "$COMMAND" | grep -oE '"[^"]+"|[^ ]+$' | tail -1 | tr -d '"')
if [ -n "$TERM" ]; then
  RESULTS=$(nestweaver search "$TERM" --json --db "$DB" 2>/dev/null || true)
  if [ -n "$RESULTS" ] && [ "$RESULTS" != "[]" ]; then
    echo "--- NestWeaver Graph Results ---" >&2
    echo "$RESULTS" | jq -r '.[0:3] | .[] | "  \(.name) @ \(.file_path):\(.start_line)"' >&2
    echo "--------------------------------" >&2
  fi
fi
