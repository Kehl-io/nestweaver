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

When configured as an MCP server, NestWeaver exposes **42** tools across these
categories. The registry is `all_tool_schemas_undecorated()` in
`crates/nestweaver-mcp/src/tools.rs`; call `tools/list` to read the live set and
its schemas rather than trusting this table.

Direct read-only mode advertises **36** (the registry minus the six mutating
tools listed under *Admin* below). `--lite` advertises **6**.

**Context & Search:**
- **brain_context** — Task-focused subgraph via Personalized PageRank with type-aware resolution
- **code_context** — Code-only context, when vault notes would be noise
- **brain_search** — Full-text BM25 search across code and notes
- **project_context** — Project-scoped retrieval across notes, symbols, and components. Omitting `include_components` means **true** — the documented default finally governs as of 9.0.0
- **brain_guide** — Auto-generated codebase intelligence guide
- **get_summary** — Hierarchical code summaries at symbol, file, or cluster level

**Analysis:**
- **brain_impact** — Confidence-weighted blast radius analysis (impact_score decays through edges)
- **blast_radius** — File-level change impact with affected symbols and clusters
- **flow_trace** — Forward execution flow from entry points
- **detect_changes** — Map file changes to affected processes and risk
- **affected_tests** — Test-impact analysis for regression test selection
- **dead_code** — Unreachable-symbol detection. **A review aid, not a deletion list**: measured top-15 precision on Rust was 0/15 and it remains poor on C++. Treat every confidence tier as review candidates. `coverage: "degraded"` means the walk had no usable seed set, so every row is unreachable *by construction*. **Refuses on a resolver-generation-stale graph** (`refused: true`, `reason: "outdated_resolver"`, no `unreachable_symbols` key) — re-index with `nestweaver index --repo <path> --force` and retry
- **contract_drift** — API contract drift detection across repos

**Graph Structure:**
- **hub_nodes** / **bridge_nodes** — Centrality analysis (PageRank, betweenness). Disclose stale rankings (`rankings_stale`, `stale_repos`), as do `repo_map`, `ranking rank` and hub-level `get_summary`; `stale_check` reports the same condition as `status: "outdated_resolver"`
- **clusters** — Community detection (Louvain-style local moving, single-level)
- **cross_repo_contracts** — Cross-repository symbol relationships

**Investigation:**
- **investigate** — Deep-dive bundle for focused code exploration
- **investigate_expand** — Expand investigation context
- **investigate_hydrate** — Load full source for investigation targets

**Vault & Notes:**
- **note_get** — Retrieve a note with optional section filtering
- **backlinks** — Find notes that link to a target note
- **brain_topic_clusters** — Topic clusters over note wikilinks (Louvain-style)
- **brain_tag_graph** / **brain_doc_stats** — Vault structure analysis
- **brain_broken_links** / **brain_orphan_documents** — Vault health checks
- **brain_memory_lint** / **brain_memory_consolidate** / **brain_memory_related** — Memory bank tools

**Code Reading:**
- **read_symbols** — Read symbol source code by name or UID
- **regex_search** — Regex search across indexed code
- **count_patterns** — Count pattern occurrences

**Admin:**
- **brain_status** — Database and vault status
- **brain_diff** — Show graph changes since a given SHA
- **stale_check** — Check if the index needs refreshing. Note it compares indexed SHA against git HEAD only — it does **not** detect a resolver-generation upgrade
- **query_extensions** — Read custom metadata on any node

**Mutating (6)** — require the admin token when auth is configured, are never
routed upstream, and are absent from direct read-only mode. Every schema carries
MCP `annotations`, so a client can tell them apart on the wire.

- **brain_add_source** — Index new vaults or repos at runtime (additive, idempotent)
- **set_extension** — Write one `(uid, key)` annotation (idempotent)
- **compact_embeddings** — Reclaim vectors from deleted nodes (idempotent, non-destructive)
- **brain_remove_source** — *destructive*, "removal is permanent"
- **prune_stale** — *destructive*, "cannot undo — removed sources must be re-indexed"
- **brain_memory_consolidate** — *destructive* with `apply: true`; it **moves files the caller did not name**

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `NESTWEAVER_DB` | `./nestweaver.lbug` | Path to the NestWeaver database |
| `NESTWEAVER_NO_DAEMON` | unset | *Requests* a daemon bypass. On its own it does nothing — see `NESTWEAVER_ALLOW_NO_DAEMON` |
| `NESTWEAVER_ALLOW_NO_DAEMON` | unset | The only thing that *permits* the bypass. `CI` and `GITHUB_ACTIONS` confer nothing |

The daemon's idle timeout is a `daemon run --idle-timeout <secs>` flag (3600 by
default when autostarted), **not** an environment variable. See CLAUDE.md for the
full operator-facing environment-variable table.

## Daemon

NestWeaver uses a background daemon process that exclusively owns the database and serves all queries via gRPC over a Unix domain socket. The daemon auto-starts on first use and exits after 1 hour of inactivity.

```bash
nestweaver daemon status --db ./nestweaver.lbug   # check daemon state
nestweaver daemon stop --db ./nestweaver.lbug     # stop the daemon
```

Daemon logs are written to `~/.local/state/nestweaver/<instance>/daemon.log`.

For CI or environments where the daemon can't run, you need **both**:
`NESTWEAVER_ALLOW_NO_DAEMON=1` to permit the bypass and `NESTWEAVER_NO_DAEMON=1`
(or `--no-daemon`) to request it. With only the request, the flag is disclosed on
stderr and the command autostarts a daemon anyway.
