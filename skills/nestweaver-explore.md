---
name: nestweaver-explore
description: Explore unfamiliar code using the NestWeaver knowledge graph.
---

When the user asks to explore, understand, or navigate unfamiliar code:

1. Identify the file or symbol they're looking at
2. Call `brain_context` with that file path or symbol name as a seed — returns a type-aware subgraph of related code ranked by Personalized PageRank
3. For deeper exploration, use `investigate` to build a focused investigation bundle, then `investigate_expand` to widen the scope
4. Use `project_context` if exploring within a specific project boundary (filters to that project's repos and notes)
5. Use `flow_trace` to follow execution from a specific entry point forward
6. Use `hub_nodes` to find the most connected symbols in the area
7. Use `bridge_nodes` to identify architectural chokepoints
8. Use `clusters` to see which functional grouping this code belongs to
9. Use `read_symbols` to view the source code of specific symbols
10. If vault notes appear in results, call `note_get` to read relevant notes
11. Summarize: what this code does, what calls it, what it depends on, and any design notes from the vault

Note: When `--track-interactions` is enabled on the MCP server, frequently-explored areas rank higher over time as the interaction memory learns your navigation patterns.
