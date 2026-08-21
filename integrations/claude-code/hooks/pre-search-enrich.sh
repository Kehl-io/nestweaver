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
  # `nestweaver search --json` returns {"results": [...]} on 6.4+ and a bare
  # array before it. Normalise so this hook works against either binary.
  ROWS=$(printf '%s' "$RESULTS" | jq -c 'if type == "object" then (.results // []) else . end' 2>/dev/null || printf '[]')
  if [ "$(printf '%s' "$ROWS" | jq 'length')" -gt 0 ]; then
    echo "--- NestWeaver Graph Results ---" >&2
    printf '%s' "$ROWS" | jq -r '.[0:3] | .[] | "  \(.name) @ \(.file_path):\(.start_line)"' >&2
    echo "--------------------------------" >&2
  fi
fi
