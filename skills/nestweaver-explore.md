---
name: nestweaver-explore
description: Explore unfamiliar code using the NestWeaver knowledge graph.
---

When the user asks to explore, understand, or navigate unfamiliar code:

1. Identify the file or symbol they're looking at
2. Call the `brain_context` MCP tool with that file path or symbol name as a seed
3. Review the returned seeds (direct matches) and connected nodes (related code)
4. If vault notes appear in results, call `note_get` to read relevant notes
5. Summarize: what this code does, what calls it, what it depends on, and any design notes from the vault
