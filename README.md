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
  NestWeaver parses 16 languages, resolves cross-file references with confidence scoring,<br>
  and gives agents precomputed answers about symbols, dependencies, and call graphs —<br>
  no source reading required.
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

**16 Languages**<br>
Tree-sitter parsing for JavaScript, TypeScript, Go, Python, Rust, Java, C/C++, and 8 more.

</td>
<td width="50%" valign="top">

**Markdown Brain**<br>
Index Obsidian vaults alongside code. Unified knowledge graph across notes and symbols.

</td>
</tr>
<tr>
<td width="50%" valign="top">

**Task-Focused Context**<br>
Personalized PageRank surfaces only the symbols relevant to your current work.

</td>
<td width="50%" valign="top">

**MCP Server**<br>
17 tools for AI agents via the Model Context Protocol. Drop-in for any MCP client.

</td>
</tr>
<tr>
<td width="50%" valign="top">

**Blast-Radius Analysis**<br>
Trace the impact of a change through the full dependency graph before you ship.

</td>
<td width="50%" valign="top">

**Interactive Web UI**<br>
Graph visualization with force physics, community detection, and semantic zoom.

</td>
</tr>
</table>

## Quick Start

```sh
# Install (requires Rust 1.85+)
cargo install --path .

# Index a repository
nestweaver index --repo ./my-project

# Get task-focused context for a symbol
nestweaver context processPayment
```

Run `nestweaver --help` for the full command list. All commands support `--json` for machine-readable output.

## Install

```sh
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
| `index` | Parse and index a repository into the graph database |
| `context` | Get task-focused context for a symbol or file via Personalized PageRank |
| `search` | Full-text search across indexed symbols and notes |
| `symbol` | Look up a symbol by name and display its metadata |
| `impact` | Trace the blast radius of a symbol through the dependency graph |
| `repo-map` | Generate a token-budgeted map of the repository structure |

</details>

<details>
<summary>Brain Commands</summary>

| Command | Description |
|---------|-------------|
| `brain add` | Add an Obsidian vault or markdown directory to the knowledge graph |
| `brain search` | Search across indexed notes and sections |
| `brain context` | Get unified context spanning both code symbols and notes |
| `brain list` | List all registered vaults |
| `brain status` | Show vault counts, per-vault staleness, and index health |
| `brain watch` | Watch vaults for changes and re-index automatically |
| `brain refresh` | Force re-index of all registered vaults |

</details>

<details>
<summary>Multi-Repo and Projects</summary>

| Command | Description |
|---------|-------------|
| `list-projects` | List all projects defined in the instance config |
| `project-context` | Get context scoped to a specific project |
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
| `mcp` | Start the MCP server for AI agent integration |
| `ui` | Launch the interactive web UI |
| `generate-guide` | Generate a guide document for the indexed codebase |
| `embed` | Generate vector embeddings for indexed symbols |
| `pull` | Pull a snapshot from a remote storage backend |
| `instance` | Manage instance configuration |
| `snapshot` | Create or restore database snapshots |
| `list-repos` | List all indexed repositories |
| `list-services` | List all detected services |
| `service-summary` | Display a summary of a specific service |

</details>

## Features

<details>
<summary>Markdown Brain</summary>

Index Obsidian vaults and markdown directories alongside your code. NestWeaver builds a unified knowledge graph that connects notes, sections, and tags to code symbols — letting you query across both worlds.

```sh
# Add a vault to the knowledge graph
nestweaver brain add ~/Documents/Obsidian/MyVault

# Search across all indexed notes
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

NestWeaver exposes 17 tools via the [Model Context Protocol](https://modelcontextprotocol.io), giving any MCP-compatible AI agent structured access to your codebase graph without reading source files directly.

```sh
nestweaver mcp --db ./nestweaver.lbug
```

Tools include symbol lookup, impact analysis, context generation, search, repo-map, brain queries, project scoping, and more. Point any MCP client at the server to get started.

## Web UI

Launch an interactive graph visualization to explore your codebase visually. The web UI includes force-directed layout, community detection, semantic zoom, and full search.

```sh
nestweaver ui --db ./nestweaver.lbug --port 8080
```

<!-- Screenshot will be added once the web UI is running against a larger dataset -->

## Architecture

<details>
<summary>Cargo workspace with 8 internal crates compiling to a single static binary</summary>

| Crate | Description |
|-------|-------------|
| `nestweaver-schema` | Node/edge types, UIDs, confidence scoring, schema versioning |
| `nestweaver-parser` | Tree-sitter + regex parsing for 16 languages |
| `nestweaver-resolver` | Cross-file symbol resolution with import graphs |
| `nestweaver-store` | LadybugDB graph store, PageRank, hybrid search |
| `nestweaver-storage` | Pluggable snapshot storage backends (local, S3, GitLab) |
| `nestweaver-engine` | Indexing pipeline, query dispatch, config, snapshots, LLM integration |
| `nestweaver-mcp` | Optional MCP server for non-shell AI clients |
| `nestweaver-web` | Optional web UI and API backend |

```
schema          (zero internal deps)
  <- parser
  <- resolver
  <- store
storage         (zero internal deps)
       <- engine <- (parser, resolver, store, storage)
            <- mcp
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
