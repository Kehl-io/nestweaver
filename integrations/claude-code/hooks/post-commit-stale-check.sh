#!/usr/bin/env bash
set -euo pipefail

INPUT=$(cat)
COMMAND=$(echo "$INPUT" | jq -r '.command // empty')
echo "$COMMAND" | grep -qE 'git\s+commit' || exit 0

DB="${NESTWEAVER_DB:-./nestweaver.lbug}"
[ ! -f "$DB" ] && exit 0

CURRENT_SHA=$(git rev-parse HEAD 2>/dev/null || exit 0)
INDEXED_SHA=$(nestweaver list-repos --json --db "$DB" 2>/dev/null | jq -r '.[0].indexed_sha // empty' || true)

if [ -n "$INDEXED_SHA" ] && [ "$CURRENT_SHA" != "$INDEXED_SHA" ]; then
  BEHIND=$(git rev-list --count "$INDEXED_SHA".."$CURRENT_SHA" 2>/dev/null || echo "?")
  echo "NestWeaver index is ${BEHIND} commit(s) behind HEAD. Run: nestweaver index --repo . --db $DB" >&2
fi
