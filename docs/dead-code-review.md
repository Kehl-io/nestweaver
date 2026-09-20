# Reviewing unreachable symbols

`dead-code` lists review candidates from static reachability analysis. Missing references, dynamic calls, framework registration and external consumers can make live code appear unreachable. No confidence tier authorizes deletion.

The existing `low`, `medium`, and `high` filters remain accepted. Low includes all candidates; Medium excludes explicitly public candidates. High has no validated population and returns no rows with `high_confidence_available: false` and `confidence_filter_status: "unavailable_no_validated_population"`. An empty High response does not establish that the codebase has no dead code. All responses containing candidates carry `review_only: true`.

Check `coverage` and its disclosures before using counts. Resolver-stale graphs refuse analysis and name the required reindex operations. Repository scope filters the results and totals after the global reachability walk; it does not remove other repositories' entry points from the walk.

```sh
nestweaver dead-code --repo my-library --limit 100 --json
```

For a complete result set, retain the first response's `graph_generation` and `page_token`. If `next_offset` is non-null, request it with the same repository and confidence filters:

```sh
nestweaver dead-code --repo my-library --limit 100 --offset 100 --generation GENERATION --page-token TOKEN --json
```

Use actual returned values in place of `GENERATION`, `TOKEN`, and the example offset. Repeat until `next_offset` is null. Preserve UIDs when comparing sets. Every response stays bounded to 1–1000 rows; each request still performs a whole-graph reachability walk. `truncated` retains its population-count meaning (`returned < matching_count`); `has_more` and `next_offset` identify whether another page remains.

Pages belong to the selected local database. CLI and hybrid MCP dead-code requests do not merge or substitute upstream graphs. Directly connecting to a remote MCP endpoint queries that endpoint's own database. A token from a different database, repository scope, confidence filter, graph generation, or changed candidate population refuses the continuation. Index publication in progress also refuses pages. Restart at offset zero after publication finishes. Refusal responses have `refused: true` and no candidate array; CLI exits 2.

MCP uses `repos`, `limit`, `offset`, `expected_generation`, and `page_token` for the same contract. Restricted callers remain subject to the existing whole-graph authorization refusal. The token is a consistency marker, not authorization.

JS/TS list-form exports root their local top-level runtime declarations, including aliases and `as default`. Re-exports from another module, type-only exports, and nested names are excluded from that rule. Existing indexes require the release's coordinated resolver migration and daemon-owned reindex to receive changed persisted entry-point flags.
