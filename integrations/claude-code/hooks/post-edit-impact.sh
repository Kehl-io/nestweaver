#!/usr/bin/env bash
set -euo pipefail

# Every hook here is advisory: without jq there is nothing to parse, so stay
# silent rather than fail.
command -v jq >/dev/null 2>&1 || exit 0

INPUT=$(cat)
# Claude Code sends the tool's arguments under `tool_input`, not at the top
# level of the event JSON.
EVENT=$(printf '%s' "$INPUT" | jq -r '.hook_event_name // "PreToolUse"')

# Context for Claude must be JSON on stdout under
# hookSpecificOutput.additionalContext. stderr from a hook that exits 0 is never
# shown to Claude, and a top-level `additionalContext` key is ignored.
emit_context() {
  jq -n --arg event "$EVENT" --arg ctx "$1" \
    '{hookSpecificOutput: {hookEventName: $event, additionalContext: $ctx}}'
}

FILE_PATH=$(printf '%s' "$INPUT" | jq -r '.tool_input.file_path // empty')
[ -z "$FILE_PATH" ] && exit 0

# Hooks run with the session's working directory; anchor the default database
# at the project root Claude Code reports, not wherever the shell happens to be.
DB="${NESTWEAVER_DB:-${CLAUDE_PROJECT_DIR:-.}/nestweaver.lbug}"
[ -e "$DB" ] || exit 0

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
      TEXT="NestWeaver impact: editing $(basename "$FILE_PATH") may affect $COUNT dependent symbol(s)$SUFFIX:
$(printf '%s' "$NODES" | jq -r '.[0:3] | .[] | "  -> \(.name) (\(.edge_type), depth \(.depth))"')"
      emit_context "$TEXT"
    fi
  fi
fi
