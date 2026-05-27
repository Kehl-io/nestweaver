# NestWeaver

## Build & Test

```sh
cargo build                                                 # build all crates
cargo build --release                                       # release binary
cargo test                                                  # run all tests
cargo test -p nestweaver-schema                             # test one crate
cargo clippy --all-targets --all-features -- -D warnings    # lint (zero warnings)
cargo fmt --all -- --check                                  # format check
cargo fmt --all                                             # format in place
```

## Run

```sh
# Index a repo and query it
nestweaver index                         # auto-detects repo root from .git
nestweaver index --repo ./testdata/js    # explicit repo path
nestweaver index --stats                 # show timing and statistics after indexing
nestweaver watch                         # live re-indexing via filesystem watcher with debouncing
nestweaver context greet                 # task-focused subgraph via PPR
nestweaver context greet --intent find-definition          # intent-tuned PPR
nestweaver context src/main.js           # seed from all symbols in a file
nestweaver search "greet"
nestweaver symbol "greet" --json
nestweaver impact "greet" --depth 3
nestweaver repo-map --token-budget 2000
nestweaver summary --level symbol        # hierarchical code summaries (symbol/file/cluster)

# Graph analysis
nestweaver hubs                          # most connected hub nodes (degree centrality + PageRank)
nestweaver bridges                       # architectural chokepoints (betweenness centrality)
nestweaver pr-impact                     # PR blast radius with risk scoring (Low/Medium/High/Critical)
nestweaver dead-code                     # detect unreachable symbols via entry point reachability

# Export
nestweaver export --format cypher        # graph export (cypher, graphml, mermaid)

# Markdown brain
nestweaver brain add ~/Documents/Obsidian/MyVault
nestweaver brain search "architecture"
nestweaver brain context "MyProject"     # unified code + notes context
nestweaver brain status                  # vault counts, per-vault staleness

# Projects
nestweaver list-projects --config ./nestweaver-instance.toml
nestweaver project-context "my-project" --token-budget 5000

# Multi-repo / instance config
nestweaver suggest-links --db ./all.lbug
nestweaver list-links --config ./nestweaver-instance.toml
nestweaver list-features --config ./nestweaver-instance.toml
nestweaver context --feature device-pairing --config ./nestweaver-instance.toml --db ./all.lbug

# Recency-aware retrieval
nestweaver brain context "status" --since 2026-05-20T00:00:00Z       # only recent notes
nestweaver brain context "project" --recency-weight 0.7               # boost recent content

# Auto-setup for AI tools (16 supported)
# Claude Code, Cursor, Codex, Windsurf, JetBrains, VS Code,
# Gemini CLI, GitHub Copilot CLI, Aider, Kiro, Continue.dev,
# Cline, OpenCode, Trae, Devin, Hermes
nestweaver setup                                                      # auto-detect and configure all
nestweaver setup claude-code                                           # configure specific tool

# Generate tool-specific instruction files
nestweaver generate-guide --format skill                              # Claude Code skill (SKILL.md)
nestweaver generate-guide --format cursor-rule                        # Cursor .mdc rule
nestweaver generate-guide --format agents-md                          # Codex AGENTS.md

# Shell completions
nestweaver completions bash              # also: zsh, fish, powershell

# MCP server (22 tools, or 6 in lite mode for Cursor)
nestweaver mcp --db ./nestweaver.lbug
nestweaver mcp --lite --db ./nestweaver.lbug                          # 6 core tools only

# Web UI
nestweaver ui --db ./nestweaver.lbug --port 8080

# Global flags: --stats, --quiet, --verbose, --no-color, --plain
```

Default database: `./nestweaver.lbug`. Override with `--db <path>` or `NESTWEAVER_DB` env var.

Sidecar files written alongside the database:
- `<db>.pagerank.json` — in-memory PageRank cache (written on `index`, loaded on open)
- `<db>.manifests.json` — parsed manifest data (package.json, go.mod, Cargo.toml, pyproject.toml, requirements.txt, composer.json, Gemfile, pubspec.yaml, Package.swift, *.csproj, build.gradle.kts, CMakeLists.txt)
- `<db>.filemeta.json` — per-file mtime/size/hash cache for tiered change detection (skips unchanged files on re-index)
- `<db>.summaries.json` — hierarchical code summaries cache (symbol/file/cluster levels)
- `<db>.tantivy/` — BM25 full-text search index for notes and sections
- `<db>.extensions.json` — user-defined extension properties on nodes
- `<db>.aliases.json` — taxonomy alias mappings from vault files

## Architecture

Cargo workspace with 8 crates + root binary:

```
nestweaver/                     # CLI entry point (src/main.rs)
crates/
  nestweaver-schema/            # node/edge types, UIDs, confidence scoring, schema versioning
  nestweaver-parser/            # Tree-sitter + regex parsing for 32 languages
  nestweaver-resolver/          # cross-file import resolution with confidence scoring
  nestweaver-store/             # LadybugDB graph store, PageRank, hybrid search (BM25 + vector)
  nestweaver-storage/           # pluggable snapshot storage backends (local, S3, GitLab)
  nestweaver-engine/            # indexing pipeline, query dispatch, config, registry, snapshots, LLM pipelines
  nestweaver-mcp/               # optional MCP wrapper (feature-gated, delegates to engine)
  nestweaver-web/               # optional web UI and API backend (Axum + React)
```

### Dependency flow

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

## Conventions

- Rust edition 2024, resolver 2
- `thiserror` for public errors in library crates; `anyhow` only in binary/engine
- `tracing` for structured logging; no `println!` in library crates
- No `unwrap()` or `expect()` in library code outside of tests
- Parameterized queries for all LadybugDB operations (no string interpolation)
- Conventional commits enforced by pre-commit hook (see `.commitlintrc.yml` for scopes)

## CI

- `ci.yml` — cargo fmt, clippy, test, coverage (`cargo-llvm-cov`), security audit (`cargo-audit`) (on every PR and push to main)
- `release-please.yml` — automated releases, binary builds for x86_64/aarch64 x linux/darwin

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | Success |
| 1 | Error |
| 2 | Not found (symbol, service) |
| 3 | Ambiguous match (multiple symbols with same name) |
| 4 | Unauthorized (pull) |
| 5 | Unavailable (pull) |
