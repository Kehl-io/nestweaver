---
name: nestweaver-impact
description: Check blast radius before modifying code using NestWeaver. DO NOT grep for callers — brain_impact shows confidence-weighted impact over the indexed call/type graph in one call.
---

**Use NestWeaver instead of grep to find callers/dependents.** `brain_impact` returns confidence-weighted impact over the indexed call/type graph — no need to manually grep for usages.

Before modifying a function, class, or module:

1. Call `blast_radius` with the changed files for a confidence-weighted risk assessment — each affected symbol has an `impact_score` (0-1) showing how strongly the change propagates through the call graph
2. Call `brain_impact` with the symbol name for a focused symbol-level impact view
3. Call `affected_tests` to identify which tests cover the changed code (test-impact analysis)
4. Use `detect_changes` with the list of files you expect to modify for process-level risk
5. Optionally call `dead_code` to see whether any impacted symbols look unreachable — a hint to weight review effort, never a deletion list (see Limitations)
6. Call `contract_drift` if the symbol is part of a cross-service API contract
7. Call `cross_repo_contracts` if the symbol might be used across services
8. Call `backlinks` if the symbol is referenced in vault notes
9. Report: what will break (sorted by impact_score), what tests to run, what notes document this decision

## Limitations

Impact results are a **lower bound** — they over-approximate the *candidate set* over the indexed graph, not a proven set of affected code. Treat them as "at least this, possibly more":

- **False-negative classes not captured:** dynamic dispatch (trait objects / virtual calls), reflection, dependency-injection / config wiring, and macro / codegen output. Dependents reached only through these are invisible to static traversal.
- **Pruning & depth cuts:** low-confidence paths are pruned below a `0.10` impact-score threshold, and paths deeper than `max_depth` are cut. Both are surfaced via the `coverage.traversal_truncated` flag and the `blind_spots` list — check them before trusting a "small" blast radius.
- **Freshness:** results are only as current as the index. A stale or behind-source repo shows in `coverage.stale_repos` / `coverage.repos_not_indexed`. Separately, NestWeaver 9.0.0 bumped `RESOLVER_GENERATION` to 4, so a graph indexed by an earlier release has edges the current resolver would not have written — including C/C++ `MEMBER_OF` and C++ `IMPORTS` edges that did not exist at all. `stale_check` reports it as `status: "outdated_resolver"` (`resolver_stale: true`, exit 2); `hub_nodes`, `bridge_nodes`, `repo_map`, `ranking rank` and hub-level `get_summary` report it as `rankings_stale`. Re-index with `nestweaver index --repo <path> --force` — plain `index` is incremental and does nothing on a repo already at HEAD.
- **Result-set caps:** `brain_impact` returns at most 50 rows by default and 1000 at the maximum, on **both** the daemon and direct routes. Read `total` (the pre-cut count) and the `truncated_by_*` flags — a symbol with more depth-1 dependents than the cap has rows that are unreachable at any limit.
- **Cross-repo severities are heuristics:** the org-wide `breaking` / `warning` / `info` levels are reach-based (`severitySource: reach-only`), NOT a verified signature or contract diff — confirm real breakage before gating on them.

## `dead_code` is a review aid, not a deletion list

`dead_code` reports symbols **no entry point reaches**, which is not the same as
"nothing references it" — a reference the parser does not capture is
indistinguishable from no reference at all. Measured top-15 precision on Rust
was **0/15**, and it remains poor on C++.

- Treat **every** confidence tier as review candidates. The caveat is not scoped
  to `low`; confidence ranks how unaddressable a symbol is from outside its
  file, never how sure the reachability walk is.
- When the payload reports `coverage: "degraded"`, the walk had no usable seed
  set, so every row below is unreachable **by construction**. That is the
  absence of a finding, not a finding.
- Never delete on its say-so, and never present its output to the user as a
  list of code that is safe to remove.
- On a resolver-generation-stale graph it **refuses**: the response is
  `refused: true` with `reason: "outdated_resolver"` and a `remedies` array, and
  carries **no `unreachable_symbols` key at all**. Do not read that as "nothing
  is dead" — nothing was computed. Run each `remedies[].command`
  (`nestweaver index --repo <path> --force`) and call the tool again.
