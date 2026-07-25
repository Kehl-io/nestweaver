# NestWeaver

## Build & Test

```sh
cargo build                                                 # build all crates
cargo build --release                                       # release binary
cargo test                                                  # run all tests
cargo test -p nestweaver-schema                             # test one crate
cargo clippy --workspace --all-targets -- -D warnings       # lint (zero warnings)
cargo fmt --all -- --check                                  # format check
cargo fmt --all                                             # format in place
```

## Daemon Architecture

All CLI commands and MCP tool calls route through a background daemon process
that owns the LadybugDB write lock. The daemon auto-starts on the first CLI
invocation and self-terminates after an idle timeout. The client auto-restarts
the daemon on version mismatch. Shutdown is graceful: the daemon drains active
write RPCs before exiting (up to `NESTWEAVER_DRAIN_TIMEOUT_SECS`, default 660s).
Indexing is CPU-throttled to a rolling 5s duty-cycle window so a saturated
daemon stays under macOS CPU-violation limits; tune with
`NESTWEAVER_INDEX_CPU_PERCENT` (percent of one core, 1–99, default 50; `0` or
`>=100` disables throttling). The var reaches the daemon two ways: from the
shell env for directly-spawned daemons, or baked into the launchd plist's
`EnvironmentVariables` at `daemon start` time (launchd jobs don't inherit the
shell env — re-run `daemon start` after changing it).

**The daemon is the sole writer to the DB file.** Never run `sqlite3` or other
tools against the DB while the daemon is running. The `--no-daemon` flag and
`NESTWEAVER_NO_DAEMON=1` env var exist only for CI/testing. Bypassing the
daemon risks WAL corruption from concurrent access. If you see "database
locked" errors, stop the daemon (`nestweaver daemon stop`) rather than using
`--no-daemon`.

## macOS App (preferred on Mac)

On macOS, prefer the native `.app` bundle over the CLI daemon. It provides:
- Menubar status icon for quick access to the web UI
- Metal GPU acceleration (GUI session context required for MTLCompilerService)
- Automatic daemon lifecycle with crash recovery
- Daemon coexistence — detects running daemons and connects instead of duplicating

```sh
# Build and launch
cd app && bash build.sh
open target/release/NestWeaver.app

# Or download NestWeaver.app from GitHub Releases
```

The web UI defaults to port 3000 (`nestweaver ui`); the macOS .app uses 9377. The app is menubar-only (no Dock icon).

When helping users install on macOS, recommend the `.app` bundle first. Fall back to `nestweaver daemon start` only for headless/CI environments.

## Run

```sh
# Index a repo and query it
nestweaver index                         # auto-detects repo root from .git
nestweaver index --repo ./testdata/js    # explicit repo path
nestweaver index --repo ./client --name my-client  # explicit repo name for multi-repo setups
nestweaver index --stats                 # show timing and statistics after indexing
nestweaver watch                         # live re-indexing via filesystem watcher with debouncing
nestweaver watch ./my-project            # watch a specific directory
nestweaver context greet                 # task-focused subgraph via PPR
nestweaver context greet --intent find-definition          # intent-tuned PPR
nestweaver context greet --limit 20                        # cap connected nodes
nestweaver context src/main.js           # seed from all symbols in a file
nestweaver search "greet"
nestweaver symbol "greet" --json
nestweaver impact "greet" --depth 3
nestweaver impact "fetchRegions" --repo my-service  # filter impact to a specific repo
nestweaver repo-map --token-budget 2000
nestweaver summary --level symbol        # hierarchical code summaries (symbol/file/cluster)

# Graph analysis
nestweaver hubs                          # most connected hub nodes (degree centrality + PageRank)
nestweaver bridges                       # architectural chokepoints (betweenness centrality)
nestweaver clusters                      # functional communities (adaptive resolution: 0.3 for >10K symbols, 0.5 default)
nestweaver pr-impact                     # PR blast radius with risk scoring (Low/Medium/High)
nestweaver pr-impact --sarif             # SARIF 2.1.0 for GitHub code scanning / VS Code SARIF viewer
nestweaver pr-impact --strict            # exit 2 on a contract-verified breaking change (advisory by default)
nestweaver affected-tests --base-ref main  # tiered regression-test selection for a diff
nestweaver rts-eval record-truth --sha X --failed-test-files a.test.ts  # CI reports full-suite outcome
nestweaver rts-eval report               # measured recall/breadth of past selections (nw-037 loop)
nestweaver dead-code                     # detect unreachable symbols via entry point reachability

# Export
nestweaver export --format cypher        # graph export (cypher, graphml, mermaid)
nestweaver export --format msgpack       # graph snapshot for WASM engine

# Markdown brain (`.brainignore` for glob exclusion patterns; `--ignore` flag for ad-hoc)
nestweaver brain add ~/Documents/Obsidian/MyVault
nestweaver brain add ~/vault --config ./instance.toml  # uses config's instance_id and db_path
nestweaver brain search "architecture"   # searches code symbols AND vault notes
nestweaver brain context "MyProject"     # unified code + notes context
nestweaver brain status                  # vault counts, per-vault staleness
nestweaver brain stale-check             # compare indexed SHAs against git HEAD
nestweaver brain stale-check --json      # JSON output
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
nestweaver instance merge --from default --to my-instance  # fix misconfigured instance_ids

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
nestweaver generate-guide --format claude-md                          # Claude Code CLAUDE.md

# Shell completions
nestweaver completions bash              # also: zsh, fish, powershell

# Interaction memory (opt-in, improves ranking over time)
nestweaver mcp --track-interactions --db ./nestweaver.lbug    # enable usage tracking
nestweaver interactions status --db ./nestweaver.lbug          # show memory stats
nestweaver interactions clear --db ./nestweaver.lbug           # wipe interaction data

# MCP server (40 tools, or 6 in lite mode for Cursor)
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
# Append ?engine=wasm to run graph algorithms client-side via WASM.
# Build with --remap-path-prefix so the build machine's home path (and the
# username in .cargo/registry panic-location strings) is NOT baked into the
# committed .wasm artifact:
#   RUSTFLAGS="--remap-path-prefix=$HOME=/build" \
#     wasm-pack build crates/nestweaver-wasm --target web \
#       --out-dir ../../crates/nestweaver-web/frontend/src/wasm

# Web API endpoints (when ui is running)
# GET  /api/v1/version          → {"graph_generation": N, "pagerank_generation": N}
# GET  /api/v1/snapshot.msgpack → MessagePack-encoded graph (X-Graph-Generation header)
# GET  /api/v1/events           → SSE stream (graph:updated, pagerank:recomputed, full_refresh)

# Global flags: --stats, --quiet, --verbose, --no-color, --plain
```

Default database: `./nestweaver.lbug`. Override with `--db <path>` or `NESTWEAVER_DB` env var.

Sidecar files written alongside the database:
- `<db>.pagerank.json` — PageRank score cache (computed and saved at index time on a full re-index and on incremental updates, loaded on open; a single-flight lazy compute is the fallback for DBs indexed before this or with no sidecar yet)
- `<db>.manifests.json` — parsed manifest data (package.json, go.mod, Cargo.toml, pyproject.toml, requirements.txt, composer.json, Gemfile, pubspec.yaml, Package.swift, *.csproj, build.gradle.kts, CMakeLists.txt)
- `<db>.filemeta.json` — per-file mtime/size/hash cache for tiered change detection (skips unchanged files on re-index)
- `<db>.summaries.json` — hierarchical code summaries cache (symbol/file/cluster levels)
- `<db>.tantivy/` — BM25 full-text search index for notes and sections
- `<db>.clusters.json` — community/cluster detection output
- `<db>.extensions.json` — user-defined extension properties on nodes
- `<db>.aliases.json` — taxonomy alias mappings from vault files
- `<db>.interactions.json` — agent interaction memory (query patterns, access frequency, follow-up signals)
- `<db>.perspectives.json` — saved web UI perspectives (web crate only)
- `<db>.cache` — MCP response cache (binary: MessagePack + ZSTD; falls back to legacy JSON on read)
- `<db>.parsed_cache.bin` — Cached parse results (symbols, references, type bindings) keyed by content hash, for skipping re-parsing unchanged files
- `<db>.resolution_deps.bin` — Per-file resolution dependency tracker for incremental cross-file resolution

## Architecture

Cargo workspace with 15 crates + root binary:

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
  nestweaver-embed/             # local embedding models (candle; Metal GPU on macOS) for vector search
  nestweaver-proto/             # gRPC protobuf definitions and generated Rust types
  nestweaver-federation/        # federation coordinator: upstream routing, health/ejection, two-tier merge, staleness (leaf; used by client + daemon-mode mcp)
  nestweaver-daemon/            # background daemon process for persistent graph serving
  nestweaver-client/            # gRPC client for daemon communication
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
            <- mcp   <- (federation, under the `daemon` feature)
            <- web
federation          (leaf: schema + proto only)
  <- client
  <- mcp (daemon feature) <- daemon
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

