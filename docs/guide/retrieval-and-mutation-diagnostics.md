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
