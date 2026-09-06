---
name: nestweaver-review
description: Review code changes with codebase context from NestWeaver. Use blast_radius instead of manually tracing callers — it shows confidence-weighted impact over the indexed call/type graph.
---

**Use NestWeaver to understand what a change affects.** `blast_radius` gives confidence-weighted impact over the indexed call/type graph. `affected_tests` identifies test coverage. DO NOT manually grep for callers.

When reviewing a PR or set of changes:

1. Get the list of changed files from git diff
2. Call `blast_radius` with the changed files — review the `impact_score` on each affected symbol to prioritize review effort
3. Call `affected_tests` to identify which tests cover the changed code and should be run
4. Call `detect_changes` with the changed file list for process-level risk assessment
5. For each high-impact changed file, call `code_context` with the REPO-RELATIVE file path as seed for surrounding context (`brain_context` does not resolve file paths — seed it with a symbol name when you want vault notes folded in)
6. Call `contract_drift` if the changes touch public API boundaries
7. Optionally call `dead_code` to see whether the changes interact with code that looks unreachable — a review prompt, never a recommendation to delete (see Limitations)
8. Check for vault notes that mention modified symbols via `backlinks`
9. Identify cross-repo impacts via `cross_repo_contracts` for any modified public APIs
10. Report: what each change affects (sorted by impact_score), test coverage gaps, API contract risks, and relevant design decisions from notes

## Limitations

`blast_radius` results are a **lower bound** — they over-approximate the *candidate set* over the indexed graph, not a proven set of affected code. Treat them as "at least this, possibly more":

- **False-negative classes not captured:** dynamic dispatch (trait objects / virtual calls), reflection, dependency-injection / config wiring, and macro / codegen output. Real dependents reached only through these are invisible to static traversal.
- **Pruning & depth cuts:** low-confidence paths are pruned below a `0.10` impact-score threshold, and paths deeper than `max_depth` are cut. Both are surfaced via the `coverage.traversal_truncated` flag and the `blind_spots` list — check them before trusting a "small" blast radius.
- **Freshness:** results are only as current as the index. A stale or behind-source repo shows in `coverage.stale_repos` / `coverage.repos_not_indexed`. Separately, NestWeaver 9.0.0 bumped `RESOLVER_GENERATION` to 4, so a graph indexed by an earlier release has edges the current resolver would not have written. `stale_check` reports it as `status: "outdated_resolver"` (`resolver_stale: true`, exit 2); `hub_nodes`, `bridge_nodes`, `repo_map`, `ranking rank` and hub-level `get_summary` report it as `rankings_stale`. Re-index with `nestweaver index --repo <path> --force` — plain `index` is incremental and does nothing on a repo already at HEAD.
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
