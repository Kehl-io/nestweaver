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

# `--` ends option parsing: a file named `--db=x.py` must stay a search term.
SYMBOLS=$(nestweaver search --json --db "$DB" -- "$(basename -- "$FILE_PATH" | sed 's/\.[^.]*$//')" 2>/dev/null || true)
# `nestweaver search --json` returns {"results": [...]} on 6.4+ and a bare
# array before it. Normalise to an array so this hook works against either
# binary on PATH — a hook installed by `nestweaver setup` outlives any one
# release.
ROWS=$(printf '%s' "$SYMBOLS" | jq -c 'if type == "object" then (.results // []) else . end' 2>/dev/null || printf '[]')
if [ "$(printf '%s' "$ROWS" | jq 'length')" -gt 0 ]; then
  TEXT="NestWeaver: symbols related to $(basename -- "$FILE_PATH"):
$(printf '%s' "$ROWS" | jq -r '.[0:5] | .[] | "  \(.name) (\(.kind)) @ \(.file_path):\(.start_line)"')"
  emit_context "$TEXT"
fi
