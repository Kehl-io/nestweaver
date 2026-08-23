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
         "args": ["mcp", "--db", "./nestweaver.lbug"]
       }
     }
   }
   ```

   The MCP server automatically starts a background daemon that owns the database. Multiple MCP servers and CLI commands can share the same database concurrently — no lock contention.

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

When configured as an MCP server, NestWeaver exposes 41 tools across these categories:

**Context & Search:**
- **brain_context** — Task-focused subgraph via Personalized PageRank with type-aware resolution
- **brain_search** — Full-text BM25 search across code and notes
- **project_context** — Project-scoped retrieval across notes, symbols, and components
- **brain_guide** — Auto-generated codebase intelligence guide

**Analysis:**
- **brain_impact** — Confidence-weighted blast radius analysis (impact_score decays through edges)
- **blast_radius** — File-level change impact with affected symbols and clusters
- **flow_trace** — Forward execution flow from entry points
- **detect_changes** — Map file changes to affected processes and risk
- **affected_tests** — Test-impact analysis for regression test selection
- **dead_code** — Confidence-aware unreachable code detection
- **contract_drift** — API contract drift detection across repos

**Graph Structure:**
- **hub_nodes** / **bridge_nodes** — Centrality analysis (PageRank, betweenness)
- **clusters** — Community detection (Louvain-style local moving, single-level)
- **cross_repo_contracts** — Cross-repository symbol relationships

**Investigation:**
- **investigate** — Deep-dive bundle for focused code exploration
- **investigate_expand** — Expand investigation context
- **investigate_hydrate** — Load full source for investigation targets

**Vault & Notes:**
- **note_get** — Retrieve a note with optional section filtering
- **backlinks** — Find notes that link to a target note
- **brain_tag_graph** / **brain_doc_stats** — Vault structure analysis
- **brain_broken_links** / **brain_orphan_documents** — Vault health checks
- **brain_memory_lint** / **brain_memory_consolidate** / **brain_memory_related** — Memory bank tools

**Code Reading:**
- **read_symbols** — Read symbol source code by name or UID
- **regex_search** — Regex search across indexed code
- **count_patterns** — Count pattern occurrences

**Admin:**
- **brain_status** — Database and vault status
- **brain_add_source** — Index new vaults or repos at runtime
- **brain_diff** — Show graph changes since a given SHA
- **stale_check** — Check if the index needs refreshing
- **set_extension** / **query_extensions** — Custom metadata on any node

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `NESTWEAVER_DB` | `./nestweaver.lbug` | Path to the NestWeaver database |
| `NESTWEAVER_NO_DAEMON` | unset | If set, bypass daemon and open the DB directly |
| `NESTWEAVER_DAEMON_IDLE_TIMEOUT` | `3600` | Seconds before an idle daemon exits |

## Daemon

NestWeaver uses a background daemon process that exclusively owns the database and serves all queries via gRPC over a Unix domain socket. The daemon auto-starts on first use and exits after 1 hour of inactivity.

```bash
nestweaver daemon status --db ./nestweaver.lbug   # check daemon state
nestweaver daemon stop --db ./nestweaver.lbug     # stop the daemon
```

Daemon logs are written to `~/.local/state/nestweaver/<instance>/daemon.log`.

For CI or environments where the daemon can't run, set `NESTWEAVER_NO_DAEMON=1` or pass `--no-daemon`.
