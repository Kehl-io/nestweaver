# NestWeaver Claude Code Integration

Deep integration between NestWeaver's code knowledge graph and Claude Code.

## Quick Setup

1. Index your repository:
   ```bash
   nestweaver index --repo .
   ```

2. Add NestWeaver as an MCP server in `.claude/settings.json`:
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

## Hooks

| Hook | Trigger | Effect |
|------|---------|--------|
| `pre-read-enrich.sh` | Claude reads a file | Shows related symbols and their graph position |
| `pre-search-enrich.sh` | Claude runs grep/rg | Adds PageRank-ordered graph results |
| `post-commit-stale-check.sh` | After `git commit` | Warns if index is stale |
| `post-edit-impact.sh` | After file edits | Shows blast radius of the change |

## MCP Tools

When configured as an MCP server, NestWeaver exposes:

- **brain_context** — Task-focused subgraph via Personalized PageRank
- **brain_search** — Full-text BM25 search across code and notes
- **brain_impact** — Blast radius analysis for any symbol
- **flow_trace** — Forward execution flow from entry points
- **detect_changes** — Map file changes to affected processes and risk
- **clusters** — Community detection results (Leiden algorithm)
- **stale_check** — Check if the index needs refreshing
- **cross_repo_contracts** — Cross-repository symbol relationships
- **note_get** / **backlinks** — Markdown vault queries

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `NESTWEAVER_DB` | `./nestweaver.lbug` | Path to the NestWeaver database |
