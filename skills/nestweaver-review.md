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
5. For each high-impact changed file, call `brain_context` with the file path as seed for surrounding context
6. Call `contract_drift` if the changes touch public API boundaries
7. Call `dead_code` to check if the changes introduce or interact with unreachable code
8. Check for vault notes that mention modified symbols via `backlinks`
9. Identify cross-repo impacts via `cross_repo_contracts` for any modified public APIs
10. Report: what each change affects (sorted by impact_score), test coverage gaps, API contract risks, and relevant design decisions from notes

## Limitations

`blast_radius` results are a **lower bound** — they over-approximate the *candidate set* over the indexed graph, not a proven set of affected code. Treat them as "at least this, possibly more":

- **False-negative classes not captured:** dynamic dispatch (trait objects / virtual calls), reflection, dependency-injection / config wiring, and macro / codegen output. Real dependents reached only through these are invisible to static traversal.
- **Pruning & depth cuts:** low-confidence paths are pruned below a `0.10` impact-score threshold, and paths deeper than `max_depth` are cut. Both are surfaced via the `coverage.traversal_truncated` flag and the `blind_spots` list — check them before trusting a "small" blast radius.
- **Freshness:** results are only as current as the index. A stale or behind-source repo shows in `coverage.stale_repos` / `coverage.repos_not_indexed`.
- **Cross-repo severities are heuristics:** the org-wide `breaking` / `warning` / `info` levels are reach-based (`severitySource: reach-only`), NOT a verified signature or contract diff — confirm real breakage before gating on them.
