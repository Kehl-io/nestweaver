---
name: nestweaver-explore
description: Explore unfamiliar code using the NestWeaver knowledge graph.
---

When the user asks to explore, understand, or navigate unfamiliar code:

1. Identify the file or symbol they're looking at
2. Call the `brain_context` MCP tool with that file path or symbol name as a seed
3. Review the returned seeds (direct matches) and connected nodes (related code)
4. Use `get_summary` at file or cluster level for a token-efficient structural overview
5. Use `hub_nodes` to find the most connected symbols in the area
6. Use `bridge_nodes` to identify architectural chokepoints
7. Use `clusters` to see which functional grouping this code belongs to
8. If vault notes appear in results, call `note_get` to read relevant notes
9. Summarize: what this code does, what calls it, what it depends on, and any design notes from the vault

Note: When `--track-interactions` is enabled on the MCP server, frequently-explored areas rank higher over time as the interaction memory learns your navigation patterns.
