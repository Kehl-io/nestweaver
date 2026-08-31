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
nestweaver stale-check --db "$DB" >/dev/null 2>&1
RC=$?
set -e

if [ "$RC" -eq 2 ]; then
  echo "NestWeaver index needs a re-index. Run: nestweaver index --repo . --db $DB" >&2
  echo "  (details: nestweaver stale-check --db $DB --json)" >&2
fi

# stale-check compares indexed SHA against git HEAD only. It does NOT detect a
# resolver-generation upgrade, so it exits 0 on a graph built by an older
# NestWeaver whose edges — and therefore every PageRank-ordered answer — are
# stale. After upgrading NestWeaver, re-index once regardless of what this says.
# `nestweaver hubs --json` reports `rankings_stale` / `stale_repos`.
