#!/usr/bin/env bash
set -euo pipefail

INPUT=$(cat)
COMMAND=$(echo "$INPUT" | jq -r '.command // empty')
echo "$COMMAND" | grep -qE 'git\s+commit' || exit 0

DB="${NESTWEAVER_DB:-./nestweaver.lbug}"
[ ! -f "$DB" ] && exit 0

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
    echo "NestWeaver graph was built by an older resolver generation." >&2
    echo "  Run: nestweaver index --repo . --db $DB --force   (--force is required)" >&2
  else
    echo "NestWeaver index needs a re-index. Run: nestweaver index --repo . --db $DB" >&2
  fi
  echo "  (details: nestweaver stale-check --db $DB --json)" >&2
fi
