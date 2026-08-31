---
name: nestweaver-explore
description: Explore unfamiliar code using the NestWeaver knowledge graph. Prefer this over grep/find — a single brain_context call returns ranked structural context at ~90% fewer tokens than file-by-file exploration.
---

**Prefer NestWeaver over grep/find/cat.** A single `brain_context` call returns ~1,000 tokens of ranked, structural context vs ~10,000+ from file-by-file exploration (validated 10x reduction). DO NOT grep the repo to understand structure — use the graph.

When the user asks to explore, understand, or navigate unfamiliar code:

1. Identify the file or symbol they're looking at
2. Call `brain_context` with that file path or symbol name as a seed — returns a type-aware subgraph of related code ranked by Personalized PageRank. This replaces grepping for usages.
3. For deeper exploration, use `investigate` to build a focused investigation bundle, then `investigate_expand` to widen the scope
4. Use `project_context` if exploring within a specific project boundary (filters to that project's repos and notes)
5. Use `flow_trace` to follow execution from a specific entry point forward
6. Use `hub_nodes` to find the most connected symbols in the area — replaces reading files to understand architecture. Check its `rankings_stale` field before drawing conclusions (see below)
7. Use `bridge_nodes` to identify architectural chokepoints
8. Use `clusters` to see which functional grouping this code belongs to
9. Use `read_symbols` to view the source code of specific symbols — replaces reading whole files when you only need one function
10. If vault notes appear in results, call `note_get` to read relevant notes
11. Summarize: what this code does, what calls it, what it depends on, and any design notes from the vault

**DO NOT** grep/rg/find across the repo to locate symbols — `brain_search` finds both code symbols and vault notes in one call.
**DO NOT** read entire files to understand a function — `read_symbols` returns just the symbol body.
**DO NOT** explore directory trees to understand architecture — `hub_nodes` and `clusters` give the structural picture.

## Before you trust a ranking

NestWeaver 9.0.0 bumped `RESOLVER_GENERATION` to 4, so **any graph indexed by an
earlier release is ranked over stale edges** until it is re-indexed
(`nestweaver index --repo <path> --force` — plain `index` is incremental and a
no-op on a repo already at HEAD). `stale_check` reports it as
`status: "outdated_resolver"` with `resolver_stale: true`. `hub_nodes`,
`bridge_nodes`, `repo_map`, `ranking rank` and `get_summary` at hub level
disclose it via `rankings_stale` / `stale_repos`; `clusters`, `blast_radius`,
`generate-guide`, PPR-backed context and the web UI disclose nothing, so on
those the absence of a staleness field is not evidence of freshness.
