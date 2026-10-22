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
echo "$COMMAND" | grep -qE 'git\s+commit' || exit 0

# Hooks run with the session's working directory; anchor the default database
# at the project root Claude Code reports, not wherever the shell happens to be.
DB="${NESTWEAVER_DB:-${CLAUDE_PROJECT_DIR:-.}/nestweaver.lbug}"
[ -e "$DB" ] || exit 0

# Ask the product rather than re-deriving the answer: `stale-check` exits
#   0 = every repo is fresh
#   1 = the check itself could not run  (say nothing — a broken check is not drift)
#   2 = at least one repo needs a re-index
#   64 = bad usage
# Gating on 2 specifically is what the CLI's own --help prescribes. The previous
# version compared `list-repos[0].indexed_sha` against `git rev-parse HEAD`,
# which reported "? commit(s) behind" for any repo whose indexed_sha is not a
# real SHA (e.g. the literal "local" for an untracked tree).
set +e
REPORT=$(nestweaver stale-check --db "$DB" --json 2>/dev/null)
RC=$?
set -e

if [ "$RC" -eq 2 ]; then
  # As of 9.0.0 exit 2 also covers `status: "outdated_resolver"` — a repo at
  # HEAD whose edges were written by an older resolver generation. The remedy
  # differs: incremental indexing is a NO-OP on such a repo (nothing changed),
  # so only `--force` clears it. Printing the wrong one sends the user round a
  # loop where the command succeeds and the warning returns.
  if [ -n "$(echo "${REPORT:-}" | jq -r '.resolver_stale_repos // [] | .[]' 2>/dev/null)" ]; then
    TEXT="NestWeaver graph was built by an older resolver generation.
  Run: nestweaver index --repo . --db \"$DB\" --force   (--force is required)"
  else
    TEXT="NestWeaver index needs a re-index. Run: nestweaver index --repo . --db \"$DB\""
  fi
  emit_context "$TEXT
  (details: nestweaver stale-check --db \"$DB\" --json)"
fi
