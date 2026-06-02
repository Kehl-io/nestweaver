<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-full-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/logo-full-light.svg">
    <img src="assets/logo-full-dark.svg" width="400" alt="NestWeaver">
  </picture>
</p>

<p align="center">
  <strong>Your codebase as a queryable graph — built for AI agents.</strong>
</p>

<p align="center">
  <a href="https://github.com/Kehl-io/nestweaver/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/Kehl-io/nestweaver/ci.yml?branch=main&label=CI&style=flat-square" alt="CI"></a>
  <a href="https://github.com/Kehl-io/nestweaver/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue?style=flat-square" alt="MIT License"></a>
  <a href="https://github.com/Kehl-io/nestweaver/releases"><img src="https://img.shields.io/github/v/release/Kehl-io/nestweaver?style=flat-square&label=release" alt="Latest Release"></a>
  <img src="https://img.shields.io/badge/rust-1.85%2B-orange?style=flat-square" alt="Rust 1.85+">
</p>

<p align="center">
  NestWeaver parses 32 languages, resolves cross-file references with confidence scoring,<br>
  and gives agents precomputed answers about symbols, dependencies, call graphs,<br>
  type usage, and field access — no source reading required.
</p>

---

<p align="center">
  <img src="assets/demo.svg" width="700" alt="NestWeaver terminal demo">
</p>

<p align="center"><em>Index a repo and query it in seconds</em></p>

---

<table>
<tr>
<td width="50%" valign="top">

**32 Languages**<br>
Tree-sitter parsing for JS, TS, Go, Python, Rust, Java, C/C++, Lua, Scala, Elixir, Zig, Vue, Svelte, and 19 more. Tracks CALLS, IMPORTS, USES, and ACCESSES edges. Resolves monorepo workspaces and tsconfig path aliases.

</td>
<td width="50%" valign="top">

**Markdown Brain**<br>
Index Obsidian vaults alongside code. Unified knowledge graph across notes and symbols.

</td>
</tr>
<tr>
<td width="50%" valign="top">

**Intent-Aware Context**<br>
Personalized PageRank with per-edge-type weights (CALLS, IMPORTS, USES, ACCESSES) and `--intent` tuning surfaces exactly the symbols relevant to your task.

</td>
<td width="50%" valign="top">

**38-Tool MCP Server**<br>
Model Context Protocol tools for AI agents. Drop-in for any MCP client, lite mode for Cursor. Daemon architecture enables concurrent access from multiple AI tools without lock contention.

</td>
</tr>
<tr>
<td width="50%" valign="top">

**PR Impact & Dead Code**<br>
Confidence-weighted blast radius with `impact_score` decay through edges; co-change mining from git history (Jaccard-scored file pairs); type-aware call resolution via AST-extracted type bindings; dead code detection with type exclusion and manifest-driven entry points; hub/bridge analysis and graph export.

</td>
<td width="50%" valign="top">

**16 AI Tool Integrations**<br>
One-command setup for Claude Code, Cursor, Codex, Gemini CLI, Copilot CLI, Aider, Kiro, and more.

</td>
</tr>
<tr>
<td width="50%" valign="top">

**Agent Interaction Memory**<br>
Opt-in usage tracking that learns from agent query patterns to improve PPR ranking over time. Privacy-first: local-only, records UIDs and timestamps only, no content capture. Enable with `--track-interactions`.

</td>
<td width="50%" valign="top">
</td>
</tr>
</table>

## Quick Start

```sh
# Install (requires Rust 1.85+)
cargo install --path .

# Index a repository (auto-detects repo root from .git)
nestweaver index

# Get task-focused context for a symbol
nestweaver context processPayment

# Live re-indexing as you code
nestweaver watch
```

```sh
# Configure for your AI tool (16 supported: Claude Code, Cursor, Codex, Gemini CLI, and more)
nestweaver setup
nestweaver setup --force   # regenerate skill/guide files even if customized
```

Run `nestweaver --help` for the full command list. All commands support `--json` for machine-readable output.

## Install

```sh
# npm (recommended — no Rust needed)
npm install -g @kehl-io/nestweaver

# or Cargo
cargo install --path .
```

<details>
<summary>Pre-built binaries</summary>

Download a pre-built binary for your platform from [GitHub Releases](https://github.com/Kehl-io/nestweaver/releases). Binaries are available for Linux and macOS on both x86_64 and aarch64.

</details>

<details>
<summary>Build from source</summary>

```sh
git clone https://github.com/Kehl-io/nestweaver.git
cd nestweaver
cargo build --release
# Binary is at target/release/nestweaver
```

</details>

## CLI Reference

<details>
<summary>Core Commands</summary>

| Command | Description |
|---------|-------------|
| `index` | Parse and index a repository (auto-detects repo root from `.git`). Use `--name` to set a custom repo name for multi-repo setups. |
| `watch` | Live re-indexing via filesystem watcher with debouncing |
| `context` | Get task-focused context via PPR (supports `--intent` for tuned retrieval) |
| `search` | Full-text search across indexed symbols and notes |
| `symbol` | Look up a symbol by name and display its metadata |
| `impact` | Trace the blast radius of a symbol through the dependency graph |
| `repo-map` | Generate a token-budgeted map of the repository structure |
| `summary` | Hierarchical code summaries at symbol, file, or cluster level |

</details>

<details>
<summary>Brain Commands</summary>

| Command | Description |
|---------|-------------|
| `brain add` | Add an Obsidian vault or markdown directory to the knowledge graph |
| `brain search` | Search across code symbols, notes, headings, sections, and tags |
| `brain context` | Get unified context spanning both code symbols and notes |
| `brain list` | List all registered vaults |
| `brain status` | Show vault counts, per-vault staleness, and index health |
| `brain watch` | Watch vaults for changes and re-index automatically |
| `brain refresh` | Force re-index of all registered vaults |
| `memory lint` | Health checks over the vault (stale notes, broken links, orphans) |
| `memory consolidate` | Propose/apply tier promotions (logs → ideas → project files) |
| `memory related` | Typed-edge traversal from a note (supersedes, depends-on, etc.) |

</details>

<details>
<summary>Analysis Commands</summary>

| Command | Description |
|---------|-------------|
| `hubs` | Find most connected hub nodes (degree centrality + PageRank) |
| `bridges` | Find architectural chokepoints (betweenness centrality) |
| `pr-impact` | PR blast radius analysis with risk scoring (Low/Medium/High/Critical) |
| `dead-code` | Detect unreachable symbols via entry point reachability |
| `export` | Export the graph in Cypher, GraphML, Mermaid, or MessagePack format |

</details>

<details>
<summary>Multi-Repo and Projects</summary>

| Command | Description |
|---------|-------------|
| `list-projects` | List all projects defined in the instance config |
| `project-context` | Get context scoped to a specific project |
| `materialize-projects` | Materialize declared projects, wiki sources, and cross-repo links from instance config |
| `detect-implicit-projects` | Detect implicit projects from vault structure and code patterns |
| `suggest-links` | Discover potential cross-repo links between symbols |
| `list-links` | List all cross-repo links in the instance |
| `list-features` | List features spanning multiple repositories |
| `clusters` | Detect community clusters in the dependency graph |
| `cross-repo-refs` | Find references that cross repository boundaries |

</details>

<details>
<summary>Server and Admin</summary>

| Command | Description |
|---------|-------------|
| `mcp` | Start the MCP server (38 tools, or 6 in lite mode; auto-starts daemon) |
| `daemon` | Manage the background daemon (`start`, `stop`, `status`, `restart`) |
| `ui` | Launch the interactive web UI |
| `setup` | Auto-detect and configure AI tools (16 supported). Use `--force` to regenerate customized files |
| `generate-guide` | Generate tool-specific instruction files (skill, cursor-rule, agents-md) |
| `completions` | Generate shell completions (bash, zsh, fish, powershell) |
| `embed` | Generate vector embeddings for indexed symbols |
| `pull` | Pull a snapshot from a remote storage backend |
| `instance` | Manage instance configuration |
| `snapshot` | Manage graph snapshots (build, verify, push) |
| `list-repos` | List all indexed repositories |
| `list-services` | List all detected services |
| `service-summary` | Display a summary of a specific service |

</details>

## Features

<details>
<summary>Markdown Brain</summary>

Index Obsidian vaults and markdown directories alongside your code. NestWeaver builds a unified knowledge graph that connects notes, sections, and tags to code symbols — letting you query across both worlds. Wiki/HTML content is auto-converted to markdown during ingestion. Use `.brainignore` for glob exclusion patterns or `--ignore` for ad-hoc filtering.

```sh
# Add a vault to the knowledge graph
nestweaver brain add ~/Documents/Obsidian/MyVault

# Search across code symbols and vault notes
nestweaver brain search "architecture"

# Get unified context spanning code and notes
nestweaver brain context "MyProject"
```

</details>

<details>
<summary>Projects</summary>

Group repositories, features, and configuration into named projects using TOML config files. Projects scope queries and context to just the code that matters.

```toml
# nestweaver-instance.toml
[[projects]]
name = "payments"
repos = ["payments-api", "payments-worker"]
components = ["checkout", "refunds"]
```

```sh
nestweaver project-context "payments" --token-budget 5000
```

</details>

<details>
<summary>Multi-Repo and Instance Config</summary>

Manage multiple repositories as a single graph. NestWeaver discovers cross-repo dependencies, suggests links between related symbols, and lets you query across repository boundaries.

See the [Instance Config Guide](docs/guide/instance-config.md) for full configuration options.

```sh
# Discover cross-repo links
nestweaver suggest-links --db ./all.lbug

# Get context for a feature spanning multiple repos
nestweaver context --feature device-pairing --config ./nestweaver-instance.toml --db ./all.lbug
```

</details>

## MCP Server

NestWeaver exposes 38 tools via the [Model Context Protocol](https://modelcontextprotocol.io), giving any MCP-compatible AI agent structured access to your codebase graph without reading source files directly.

```sh
nestweaver mcp --db ./nestweaver.lbug
nestweaver mcp --tools context,search,symbol --db ./nestweaver.lbug   # allowlist specific tools
nestweaver mcp --no-daemon --db ./nestweaver.lbug                     # bypass daemon (CI/testing)
```

The MCP server automatically starts a background daemon that owns the database. Multiple MCP servers, CLI commands, and IDE integrations can share the same database concurrently without lock contention. The daemon exits after 1 hour of inactivity.

```sh
nestweaver daemon status --db ./nestweaver.lbug   # check daemon state
nestweaver daemon stop --db ./nestweaver.lbug     # stop the daemon manually
pgrep -a nestweaver-daemon                        # find running daemons (Linux)
```

38 tools including type-aware context retrieval, confidence-weighted impact analysis (`impact_score` shows how strongly changes propagate), investigation bundles, co-change detection, dead code analysis, community detection, and vault/notes integration. Use `--tools` to expose only the tools you need.

### Key capabilities

- **Type-aware call resolution** — AST-extracted type bindings (annotations, constructors, self/this, return types) resolve `obj.method()` calls to the correct target class
- **Confidence-weighted blast radius** — `impact_score` decays multiplicatively through edges; low-confidence paths are pruned
- **Co-change mining** — Jaccard-scored file pairs from git history surface files that always change together
- **MRO walk** — inherited methods resolved via class hierarchy traversal (depth 5, cycle-safe)

External MCP servers can be configured in your instance config with `timeout_secs` (default 30):

```toml
[[mcp_servers]]
name = "wiki-mcp"
command = "wiki-mcp"
timeout_secs = 60
```

## Web UI

Launch an interactive graph visualization powered by Three.js with GPU-accelerated rendering. Nodes glow with per-kind colors, radial gradients, and bloom post-processing on a deep dark canvas. Features force-directed layout, community detection overlays, semantic zoom, accessible list view, and full search.

```sh
nestweaver ui --db ./nestweaver.lbug --port 8080
nestweaver ui --db ./nestweaver.lbug --port 8080 --watch  # live re-indexing
```

<p align="center">
  <img src="assets/web-ui-screenshot.png" width="700" alt="NestWeaver Web UI — context graph with glowing nodes">
</p>
<p align="center">
  <img src="assets/web-ui-graph.png" width="700" alt="NestWeaver Web UI — class hierarchy with bloom effects">
</p>

**Graph features:**
- GPU-rendered nodes with SDF circles, radial gradients, outer glow halos, and breathing animation
- Bloom post-processing for a premium atmospheric feel
- Edge particles with directional flow
- Click to inspect: callers, callees, source code with syntax highlighting
- Accessible node list view (Ctrl+L) with keyboard navigation
- Community overlay with Louvain detection
- Reduced effects toggle for accessibility (`prefers-reduced-motion` auto-detected)
- URL deep-linking for shareable views
- Navigation history (Ctrl+Z / Ctrl+Shift+Z)
- Glassmorphism panels with cursor-responsive lighting
- Dark/light/system theme with kehl.io-inspired dark palette

**WASM mode** — Append `?engine=wasm` to the URL to run graph algorithms client-side via WebAssembly. The browser downloads a MessagePack snapshot and executes PPR locally — no server round-trips for queries. Requires `wasm-pack build crates/nestweaver-wasm` first.

**Keyboard shortcuts:**
- `Tab` / `Shift+Tab` — cycle forward/backward through nodes
- `Arrow keys` — navigate to neighboring nodes
- `Ctrl+L` — toggle accessible node list view
- `Ctrl+Z` / `Ctrl+Shift+Z` — navigate history backward/forward

## Architecture

<details>
<summary>Cargo workspace with 13 crates compiling to a single static binary + optional WASM module</summary>

| Crate | Description |
|-------|-------------|
| `nestweaver-schema` | Node/edge types, UIDs, confidence scoring, schema versioning |
| `nestweaver-parser` | Tree-sitter + regex parsing for 32 languages |
| `nestweaver-resolver` | Cross-file symbol resolution with import graphs, monorepo workspace packages, and tsconfig path aliases |
| `nestweaver-store` | LadybugDB graph store, PageRank, hybrid search |
| `nestweaver-storage` | Pluggable snapshot storage backends (local, S3, GitLab) |
| `nestweaver-engine` | Indexing pipeline, query dispatch, config, snapshots, LLM integration |
| `nestweaver-algorithms` | Pure-compute graph algorithms (PPR, impact BFS) — WASM-compatible, no I/O |
| `nestweaver-proto` | gRPC service definition (protobuf) for daemon IPC |
| `nestweaver-daemon` | Background daemon — owns the database, serves gRPC over Unix socket |
| `nestweaver-client` | Daemon client with auto-start, flock-based race prevention, version check |
| `nestweaver-mcp` | MCP server — proxies tool calls through the daemon |
| `nestweaver-web` | Web UI (Three.js/R3F) and Axum API backend |
| `nestweaver-wasm` | Browser-side WASM module wrapping nestweaver-algorithms |

```
schema              (zero internal deps)
  <- parser
  <- resolver
  <- store
algorithms          (zero internal deps — WASM target)
  <- wasm
storage             (zero internal deps)
proto               (generated gRPC types)
       <- engine <- (parser, resolver, store, storage, algorithms)
            <- daemon <- (engine, store, mcp, proto)
            <- client <- (proto, daemon)
            <- mcp    <- (client, proto)  [daemon proxy mode]
            <- web
```

</details>

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines on building, testing, and submitting changes.

## License

[MIT](LICENSE)

---

<p align="center">
  <a href="https://kehl.io">
    <img src="assets/kehl-io/kehl-icon.png" width="56" alt="kehl.io" />
  </a>
  <br>
  <sub>Built by <a href="https://kehl.io">kehl.io</a></sub>
</p>
