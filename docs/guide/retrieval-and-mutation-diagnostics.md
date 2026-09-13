# Retrieval and mutation diagnostics

Repository-scoped MCP and typed gRPC callers cannot request whole-graph
aggregates (`brain_context`, `code_context`, `project_context`, `dead_code`,
`hub_nodes`, `bridge_nodes`, or `regex_search`). Both detailed and concise
formats refuse before computation. Filtering result rows cannot undo the
influence of hidden repositories on ranks, traversal, counts, or truncation.
Unrestricted callers retain these tools.

Hybrid fusion orders by descending floating-point score, then canonical UID
ascending. Candidate normalization also uses UID order. Unequal scores are
never rounded into ties. Federated brain search applies its per-kind cap after
ranking and deduplication; all `Symbol/*` subtypes share one cap. An explicit
limit wins; otherwise the stricter positive limit resolved by the two tiers is
used (20 for older peers that do not report a limit). `returned_matches`
describes the displayed rows; `total_matches` and its relation retain their
pre-cap meaning. Lexical fallback warnings retain their tier provenance.

A requested semantic leg that cannot contribute leaves graph results available
and reports `semantic_applied: false`, `degraded_components`, and
`semantic_unavailable` with a reason, stage, and remediation. Cancellation
remains an error. Pipeline mismatches include differing field names and safe
values: free-form identifiers are SHA-256 digests, while closed enums, numbers,
and booleans are shown directly. A deliberate full embedding rebuild remains
the way to change an incompatible pipeline.

Index and vault publication reconcile vector liveness against the committed
graph before retiring the durable `.index-dirty` marker. A persistent failure
reports a committed/degraded mutation and keeps the recovery fence. Recovery
reconciles the vectors before ranked queries can trust the publication again.
Repository removal also invalidates that repository's live activity scores;
sidecar read/write failures are reported as reconciliation failures.

Backup staging requires readable global and per-repository statistics. A failed
read cannot publish an archive with invented zero counts. Similarly, failed
upstream or source inventory RPCs cannot become an empty inventory or a
successful connection/removal. `brain list` uses the same inventory on direct
and daemon routes, with `null` JSON counts, human `unavailable`, and a nonzero
exit when a vault's note count cannot be read.

First-index fallback carries the completed full build's `edges_found` count,
just as explicit `--force` does: resolved relationships, inferred cross-repo
calls, and emitted `MEMBER_OF` edges. This is a build count, not all structural
relationships in the database. A true incremental run reports `null` because
it does not measure that population.

Backup packaging validates git-activity payloads before assigning a schema.
Repository-keyed v2 scores are preserved. Recognized legacy flat scores have
no reliable repository ownership, so the staged copy is excluded and the
archive manifest records a warning. Save (including daemon RPC), inspect, and
restore display that warning. The live source file is unchanged. After restore,
reindex each repository with `--with-git-activity` to rebuild these scores.
Malformed, unreadable, and unknown-version payloads fail backup creation.

Trigram refresh reports per-scope live `(UID, trigram)` additions and deletions.
An unchanged full rebuild reports zero deltas, and deleted segment documents
do not inflate counts. Standalone file-symbol deletion commits its regex
invalidation with the deletion, so a delete-only index queues cleanup even
when it has no replacement symbols to insert. Measuring a changed scope streams its prior postings
once; it does not retain a second corpus-sized posting set. If the prior shard
cannot be read, refresh still repairs it, but excludes that scope from posting
totals and names it in `posting_deltas_unavailable` (JSON and gRPC).

`ui --port 0` selects the default port, currently 3000. Successful daemon
responses must contain a nonzero, in-range port; the CLI uses that returned
endpoint for its URL and supervision. A healthy daemon with a persistently
unavailable UI gets at most three repair requests before the CLI exits with an
actionable error. Daemon-outage recovery continues to serve the degraded page.

`admin install-hook` checks the exact NestWeaver command under the `Task`
matcher in `PreToolUse`. Competing hooks, including match-all groups with no
matcher, are preserved. Unsupported settings containers fail without writes;
a successful write is reread and verified before reporting installation.
`admin instructions --for-subagent` prints markdown on a TTY. When stdin is a
PreToolUse JSON event for Task or Agent (Claude Code or Cursor), it prints
dual-format hook JSON with context and a guidance-prefixed prompt, preserving
the other tool input fields. Both response formats retain `allow`; Claude's
explicit deny/ask rules still apply. Other recognizable hook payloads, including
Task/Agent events without a phase, receive `{}` with no decision or mutation.
Empty, non-JSON, or unrelated JSON stdin retains the Markdown output.

Claude's current PreToolUse contract does not inject plain-text stdout as
context. Cursor can import the Claude settings when third-party hooks are
enabled, and accepts either the flat or nested JSON response format. See the
[Claude hook reference](https://code.claude.com/docs/en/hooks#pretooluse-decision-control)
and [Cursor compatibility reference](https://cursor.com/docs/reference/third-party-hooks).
`admin install-hook` installs only Claude settings; it does not register Codex hooks or
implement a shared SubagentStart adapter across hosts.

### Scoped review and exclusion contracts

`detect_changes` keeps the display limit separate from its computation budget.
It now limits reverse/forward traversal to 2,000,000 node/edge inspections and
uses a shared 60-second deadline for database reads and traversal. A stopped
analysis returns `status: partial`, `gate_state: degraded_unknown`, notifications,
`work_budget_exceeded` and/or `deadline_exceeded`. Counts marked `gte` are lower
bounds, including an empty result after a database deadline; they do not mean
no impact. Raising `limit` does not raise these budgets. Split the request or run
the full test suite. This is bounded partial analysis, not a continuation token.
Database timeouts are cooperative in LadybugDB; they cannot preempt an OS-level
storage stall. The client RPC deadline remains the final transport safeguard.

`phase_millis` separates planning, graph loading, traversal, and sorting. For
reference, the first 56 Rust files from `git diff --name-only v7.0.0 5149d22c --
'*.rs'` completed on the pre-change local graph in 46.076 seconds at `limit=1000`,
returning 361,171 bytes. This is an equivalent historical-delta fixture, not a
claim to have recovered the original timed-out request or a portable benchmark.
The regression suite also exercises a deterministic smaller work budget over a
56-file graph and verifies that partial analysis cannot yield a clean gate.

Code index results and each `brain_status.repos` entry disclose
`exclusion_inventory` separately from skipped/failed source files. Its
`tracked_files` is the exact number of Git-index paths matched by configured
repository excludes; null means Git inventory was unavailable. It does not
estimate untracked descendants of pruned directories. `patterns` states the
configured boundary; index reports also include `observed_paths` encountered by
the walk. Status does not walk pruned subtrees to populate that list. Explicit
excludes do not trigger parse-failure or size-policy warnings. Default skip
directories and genuine parse failures retain their existing coverage warnings.
Full, incremental, unchanged, forced and watcher updates apply the same
compiled repository policy, including deletion of obsolete watcher rows.

Impact JSON preserves both `local_impact` and `org_wide_impact` when federated.
Within an impact result, `nodes` and `impact_nodes` are additive aliases of the
same list; `symbol` is the caller's query, `target` is a resolved UID or null,
and `note` is always present. Runtime status has
`runtime_telemetry: unavailable` when a transport has no daemon-runtime values;
null runtime fields do not establish health. Deliberately disabling semantic
retrieval is distinct from a missing semantic capability.

After `setup` writes or reconciles a registration, it reads back the stored
command, arguments and environment and performs a bounded MCP `initialize` /
`tools/list` probe. It never invokes a tool or claims that the host session has
activated the server. Missing databases skip the probe to avoid creating a
new database. A failed probe is reported independently of the configuration
write; restart the host and inspect its available tools. The ten-second total
probe budget includes both requests, and individual responses cannot exceed
256 KiB. Cursor's generated guide lists only the registered lite capabilities.
