# NestWeaver Claude Code Integration

Deep integration between NestWeaver's code knowledge graph and Claude Code.

## Quick Setup

1. Index your repository:
   ```bash
   nestweaver index --repo .
   ```

2. Add NestWeaver as an MCP server in `.mcp.json` (project root):
   ```json
   {
     "mcpServers": {
       "nestweaver": {
         "command": "nestweaver",
         "args": ["mcp", "--db", "./nestweaver.lbug", "--allow-mcp-add-sources"]
       }
     }
   }
   ```

3. (Optional) Enable hooks for enriched context:
   ```bash
   # Merge the hook config into your existing .claude/settings.json
   cat integrations/claude-code/settings.json
   ```

> **Recommended:** Use `nestweaver setup claude-code` to automatically install the MCP server config (`.mcp.json`) and skill (`.claude/skills/nestweaver/SKILL.md`).

## Hooks

| Hook | Trigger | Effect |
|------|---------|--------|
| `SessionStart` | Session begins | Prints brain status and staleness check |
| `pre-read-enrich.sh` | Claude reads a file | Shows related symbols and their graph position |
| `pre-write-impact` | Claude writes/edits a file | Shows blast radius of the file being modified |
| `pre-search-enrich.sh` | Claude runs grep/rg | Adds PageRank-ordered graph results |
| `post-commit-stale-check.sh` | After `git commit` | Warns if index is stale |
| `post-edit-impact.sh` | After file edits | Shows blast radius of the change |

## MCP Tools

When configured as an MCP server, NestWeaver exposes:

- **brain_context** — Task-focused subgraph via Personalized PageRank (supports filters: repos, vaults, kinds, tags, path_prefix; tunable hybrid weights)
- **brain_search** — Full-text BM25 search across code and notes
- **brain_impact** — Blast radius analysis for any symbol (accepts name or UID)
- **brain_status** — Database and vault status with per-vault staleness
- **brain_guide** — Auto-generated codebase intelligence guide with repos, features, links, and projects
- **brain_add_source** — Index new vaults or repos at runtime (enabled by default)
- **brain_diff** — Show what changed in the graph since a given SHA
- **flow_trace** — Forward execution flow from entry points (accepts name or UID)
- **detect_changes** — Map file changes to affected processes and risk
- **clusters** — Community detection results (Leiden algorithm)
- **stale_check** — Check if the index needs refreshing (supports SSH/HTTPS URLs)
- **cross_repo_contracts** — Cross-repository symbol relationships
- **project_context** — Project-scoped retrieval across notes, symbols, and components
- **note_get** — Retrieve a note with optional section filtering
- **backlinks** — Find notes that link to a target note
- **set_extension** / **query_extensions** — Attach and query custom metadata on any node

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `NESTWEAVER_DB` | `./nestweaver.lbug` | Path to the NestWeaver database |
