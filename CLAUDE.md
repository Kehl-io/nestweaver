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
nestweaver index --repo ./client --name coyote-client  # explicit repo name for multi-repo setups
nestweaver index --stats                 # show timing and statistics after indexing
nestweaver watch                         # live re-indexing via filesystem watcher with debouncing
nestweaver watch ./my-project            # watch a specific directory
nestweaver context greet                 # task-focused subgraph via PPR
nestweaver context greet --intent find-definition          # intent-tuned PPR
nestweaver context src/main.js           # seed from all symbols in a file
nestweaver search "greet"
nestweaver symbol "greet" --json
nestweaver impact "greet" --depth 3
nestweaver impact "fetchRegions" --repo freeplay  # filter impact to a specific repo
nestweaver repo-map --token-budget 2000
nestweaver summary --level symbol        # hierarchical code summaries (symbol/file/cluster)

# Graph analysis
nestweaver hubs                          # most connected hub nodes (degree centrality + PageRank)
nestweaver bridges                       # architectural chokepoints (betweenness centrality)
nestweaver clusters                      # functional communities (adaptive resolution: 0.3 for >10K symbols, 0.5 default)
nestweaver pr-impact                     # PR blast radius with risk scoring (Low/Medium/High)
nestweaver dead-code                     # detect unreachable symbols via entry point reachability

# Export
nestweaver export --format cypher        # graph export (cypher, graphml, mermaid)
nestweaver export --format msgpack       # graph snapshot for WASM engine

# Markdown brain (`.brainignore` for glob exclusion patterns; `--ignore` flag for ad-hoc)
nestweaver brain add ~/Documents/Obsidian/MyVault
nestweaver brain search "architecture"   # searches code symbols AND vault notes
nestweaver brain context "MyProject"     # unified code + notes context
nestweaver brain status                  # vault counts, per-vault staleness
nestweaver brain watch ~/notes --refresh-wiki-hours 6 --config ./instance.toml  # periodic wiki refresh

# Projects
nestweaver list-projects --config ./nestweaver-instance.toml
nestweaver project-context "my-project" --token-budget 5000
nestweaver materialize-projects --config ./nestweaver-instance.toml
nestweaver detect-implicit-projects --vault ~/Documents/Obsidian/MyVault

# Multi-repo / instance config
nestweaver suggest-links --db ./all.lbug
nestweaver list-links --config ./nestweaver-instance.toml --db ./main.lbug
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
nestweaver setup claude-code --allow-writes                            # enable write-mode tools

# Generate tool-specific instruction files
nestweaver generate-guide --format skill                              # Claude Code skill (SKILL.md)
nestweaver generate-guide --format cursor-rule                        # Cursor .mdc rule
nestweaver generate-guide --format agents-md                          # Codex AGENTS.md

# Shell completions
nestweaver completions bash              # also: zsh, fish, powershell

# Interaction memory (opt-in, improves ranking over time)
nestweaver mcp --track-interactions --db ./nestweaver.lbug    # enable usage tracking
nestweaver interactions status --db ./nestweaver.lbug          # show memory stats
nestweaver interactions clear --db ./nestweaver.lbug           # wipe interaction data

# MCP server (38 tools, or 6 in lite mode for Cursor)
nestweaver mcp --db ./nestweaver.lbug
nestweaver mcp --lite --db ./nestweaver.lbug                          # 6 core tools only
nestweaver mcp --tools context,search,symbol --db ./nestweaver.lbug   # allowlist specific tools

# Instance config: external MCP servers with timeout
# [[mcp_servers]]
# name = "wiki-mcp"
# command = "wiki-mcp"
# timeout_secs = 60  # default 30

# Web UI
nestweaver ui --db ./nestweaver.lbug --port 8080
nestweaver ui --watch                    # live re-indexing via filesystem watcher
# Append ?engine=wasm to run graph algorithms client-side via WASM
# Requires: wasm-pack build crates/nestweaver-wasm --target web --out-dir ../../crates/nestweaver-web/frontend/src/wasm

# Web API endpoints (when ui is running)
# GET  /api/v1/version          → {"graph_generation": N, "pagerank_generation": N}
# GET  /api/v1/snapshot.msgpack → MessagePack-encoded graph (X-Graph-Generation header)
# GET  /api/v1/events           → SSE stream (graph:updated, pagerank:recomputed, full_refresh)

# Global flags: --stats, --quiet, --verbose, --no-color, --plain
```

Default database: `./nestweaver.lbug`. Override with `--db <path>` or `NESTWEAVER_DB` env var.

Sidecar files written alongside the database:
- `<db>.pagerank.json` — in-memory PageRank cache (written on `index`, loaded on open)
- `<db>.manifests.json` — parsed manifest data (package.json, go.mod, Cargo.toml, pyproject.toml, requirements.txt, composer.json, Gemfile, pubspec.yaml, Package.swift, *.csproj, build.gradle.kts, CMakeLists.txt)
- `<db>.filemeta.json` — per-file mtime/size/hash cache for tiered change detection (skips unchanged files on re-index)
- `<db>.summaries.json` — hierarchical code summaries cache (symbol/file/cluster levels)
- `<db>.tantivy/` — BM25 full-text search index for notes and sections
- `<db>.clusters.json` — community/cluster detection output
- `<db>.extensions.json` — user-defined extension properties on nodes
- `<db>.aliases.json` — taxonomy alias mappings from vault files
- `<db>.interactions.json` — agent interaction memory (query patterns, access frequency, follow-up signals)
- `<db>.perspectives.json` — saved web UI perspectives (web crate only)

## Architecture

Cargo workspace with 10 crates + root binary:

```
nestweaver/                     # CLI entry point (src/main.rs)
crates/
  nestweaver-schema/            # node/edge types, UIDs, confidence scoring, schema versioning
  nestweaver-parser/            # Tree-sitter + regex parsing for 32 languages
  nestweaver-resolver/          # cross-file import resolution with confidence scoring
  nestweaver-store/             # LadybugDB graph store, PageRank, hybrid search (BM25 + vector)
  nestweaver-storage/           # pluggable snapshot storage backends (local, S3, GitLab)
  nestweaver-engine/            # indexing pipeline, query dispatch, config, registry, snapshots, LLM pipelines
  nestweaver-algorithms/        # pure-compute graph algorithms (PPR, impact BFS) — WASM-compatible
  nestweaver-mcp/               # optional MCP wrapper (feature-gated, delegates to engine)
  nestweaver-web/               # web UI (Three.js/R3F + Axum API) with GPU-accelerated graph rendering
  nestweaver-wasm/              # browser-side WASM module wrapping nestweaver-algorithms
```

### Edge types and weighting

The graph has four edge kinds: **CALLS** (function calls + JSX `<Component />` usage), **IMPORTS**, **USES** (type references), and **ACCESSES** (field access). PPR applies per-edge-type weights (CALLS=1.0, IMPORTS=0.8, USES=0.5, ACCESSES=0.4). Dead-code BFS uses edge confidence thresholds to avoid false positives.

### Key resolver behaviors

- Monorepo workspace packages and tsconfig path aliases are resolved automatically
- Wiki/HTML content from brain vaults is auto-converted to markdown during ingestion

### Dependency flow

```
schema              (zero internal deps)
  <- parser
  <- resolver
  <- store
algorithms          (zero internal deps — WASM target)
  <- wasm
storage             (zero internal deps)
       <- engine <- (parser, resolver, store, storage, algorithms)
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

<!-- gitnexus:start -->
# GitNexus — Code Intelligence

This project is indexed by GitNexus as **nestweaver** (8036 symbols, 17663 relationships, 300 execution flows). Use the GitNexus MCP tools to understand code, assess impact, and navigate safely.

> If any GitNexus tool warns the index is stale, run `npx gitnexus analyze` in terminal first.

## Always Do

- **MUST run impact analysis before editing any symbol.** Before modifying a function, class, or method, run `gitnexus_impact({target: "symbolName", direction: "upstream"})` and report the blast radius (direct callers, affected processes, risk level) to the user.
- **MUST run `gitnexus_detect_changes()` before committing** to verify your changes only affect expected symbols and execution flows.
- **MUST warn the user** if impact analysis returns HIGH or CRITICAL risk before proceeding with edits.
- When exploring unfamiliar code, use `gitnexus_query({query: "concept"})` to find execution flows instead of grepping. It returns process-grouped results ranked by relevance.
- When you need full context on a specific symbol — callers, callees, which execution flows it participates in — use `gitnexus_context({name: "symbolName"})`.

## Never Do

- NEVER edit a function, class, or method without first running `gitnexus_impact` on it.
- NEVER ignore HIGH or CRITICAL risk warnings from impact analysis.
- NEVER rename symbols with find-and-replace — use `gitnexus_rename` which understands the call graph.
- NEVER commit changes without running `gitnexus_detect_changes()` to check affected scope.

## Resources

| Resource | Use for |
|----------|---------|
| `gitnexus://repo/nestweaver/context` | Codebase overview, check index freshness |
| `gitnexus://repo/nestweaver/clusters` | All functional areas |
| `gitnexus://repo/nestweaver/processes` | All execution flows |
| `gitnexus://repo/nestweaver/process/{name}` | Step-by-step execution trace |

## CLI

| Task | Read this skill file |
|------|---------------------|
| Understand architecture / "How does X work?" | `.claude/skills/gitnexus/gitnexus-exploring/SKILL.md` |
| Blast radius / "What breaks if I change X?" | `.claude/skills/gitnexus/gitnexus-impact-analysis/SKILL.md` |
| Trace bugs / "Why is X failing?" | `.claude/skills/gitnexus/gitnexus-debugging/SKILL.md` |
| Rename / extract / split / refactor | `.claude/skills/gitnexus/gitnexus-refactoring/SKILL.md` |
| Tools, resources, schema reference | `.claude/skills/gitnexus/gitnexus-guide/SKILL.md` |
| Index, status, clean, wiki CLI commands | `.claude/skills/gitnexus/gitnexus-cli/SKILL.md` |

<!-- gitnexus:end -->
