---
name: nestweaver-debug
description: Debug errors using NestWeaver's code+notes graph. Prefer graph queries over grep — brain_context shows the full call chain and related code in one ranked result.
---

**Use NestWeaver to trace errors through the graph.** `brain_context` + `flow_trace` show the call chain and related code without reading files. `brain_search` finds error patterns across both code AND vault notes in one call.

When debugging an error or unexpected behavior:

1. Extract key symbol names from the error message or stack trace
2. Call `brain_search` with the error message keywords
3. For each matched symbol, call `brain_context` to see its call chain and related code
4. Use `investigate` to build a focused investigation bundle around the failing symbol — then `investigate_expand` to widen if needed, `investigate_hydrate` to load full source
5. Use `flow_trace` to trace forward execution from the suspected origin
6. Use `brain_impact` to find all callers of the failing symbol — the `impact_score` shows how strongly each caller depends on it
7. Use `read_symbols` to view the source code of suspects directly
8. Optionally check `dead_code` for whether the failing symbol looks unreachable — as a *hint about test coverage or an unused path*, never as permission to delete. See the caveat below.
9. Check if any vault notes mention the error pattern via `brain_search`
10. Report: the call chain leading to the error, related code, and any existing documentation

Note: With `--track-interactions` enabled, debugging patterns are remembered across sessions — symbols involved in past debugging flows rank higher in future queries.

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

## Before you trust a ranking

NestWeaver 9.0.0 bumped `RESOLVER_GENERATION` to 4, so **any graph indexed by an
earlier release is ranked over stale edges** until it is re-indexed
(`nestweaver index --repo <path> --force`). `stale_check` will not tell you —
it compares indexed SHA against git HEAD only. `hub_nodes` and `bridge_nodes`
are the **only** tools that disclose it, via `rankings_stale` / `stale_repos`;
roughly a dozen other ranking-derived surfaces disclose nothing, so the absence
of a staleness field is not evidence of freshness.
