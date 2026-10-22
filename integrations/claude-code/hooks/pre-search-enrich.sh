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

COMMAND=$(printf '%s' "$INPUT" | jq -r '.tool_input.command // empty')
echo "$COMMAND" | grep -qE '(grep|rg)\s' || exit 0

# Hooks run with the session's working directory; anchor the default database
# at the project root Claude Code reports, not wherever the shell happens to be.
DB="${NESTWEAVER_DB:-${CLAUDE_PROJECT_DIR:-.}/nestweaver.lbug}"
[ -e "$DB" ] || exit 0

# Best-effort extraction: takes the last whitespace-delimited or double-quoted
# token from the command. Does not handle single-quoted strings or escaped
# quotes — acceptable for a non-blocking enrichment hook.
# `|| true`: under `set -euo pipefail` a command with no token to extract
# (e.g. one ending in whitespace) would otherwise fail the whole hook.
SEARCH_TERM=$(echo "$COMMAND" | grep -oE '"[^"]+"|[^ ]+$' | tail -1 | tr -d '"' || true)
if [ -n "$SEARCH_TERM" ]; then
  RESULTS=$(nestweaver search --json --db "$DB" -- "$SEARCH_TERM" 2>/dev/null || true)
  # `nestweaver search --json` returns {"results": [...]} on 6.4+ and a bare
  # array before it. Normalise so this hook works against either binary.
  ROWS=$(printf '%s' "$RESULTS" | jq -c 'if type == "object" then (.results // []) else . end' 2>/dev/null || printf '[]')
  if [ "$(printf '%s' "$ROWS" | jq 'length')" -gt 0 ]; then
    TEXT="NestWeaver graph results for \"$SEARCH_TERM\" (brain_search / regex_search cover indexed repos):
$(printf '%s' "$ROWS" | jq -r '.[0:3] | .[] | "  \(.name) @ \(.file_path):\(.start_line)"')"
    emit_context "$TEXT"
  fi
fi
