# NestWeaver

## Build & Test

```sh
cargo build                                                 # build all crates
cargo build --release                                       # release binary
cargo test                                                  # run all tests
just test-crate nestweaver-schema                           # test one crate — NOT `cargo test -p`
cargo clippy --workspace --all-targets -- -D warnings       # lint (zero warnings)
cargo fmt --all -- --check                                  # format check
cargo fmt --all                                             # format in place
```

**Use `just test-crate`, never a bare `cargo test -p <crate>`.** `-p` resolves
features for that package alone, while a workspace run unifies them across
dependents. Affected crates therefore run **fewer tests under `-p` and still
report `ok`**. Measured on `5e9e0f0`, bare `-p` / `--all-features` / `--workspace`:

| crate | `-p` | `--all-features` | `--workspace` |
| --- | --- | --- | --- |
| `nestweaver-daemon` | 238 | 264 | 264 |
| `nestweaver-mcp` | 154 | 180 | 180 |

The recipe uses `--all-features`, not a per-crate feature list, because every
feature unification can activate on a package is one of that package's own
features — so `--all-features` is provably a superset and can never cover less.
Guessing the list is unreliable: `--features embed` looks right and leaves
`nestweaver-mcp` at 154, since what it actually needs is `daemon`.

Two packages are exempt and run plain: `nestweaver-embed` and the root
`nestweaver`, whose `metal` feature pulls `objc2` and does not compile on Linux.

Switching between `-p` and `--workspace` also re-resolves features, which
re-fingerprints the build and forces a full `lbug` C++ rebuild. Pick one shape
per working tree and keep it.

A clean clone links with no extra linker flags. Only one copy of zstd is in the
binary: `liblbug.a` vendors it, and Rust code reaches that copy through
`nestweaver_store::zstd` rather than the `zstd` crate. Do not add a `zstd`
dependency back — that reintroduces `zstd-sys`, a second complete copy, and the
duplicate-symbol link failure that `-Wl,--allow-multiple-definition` used to
suppress.

## Daemon Architecture

All CLI commands and MCP tool calls route through a background daemon process
that owns the LadybugDB write lock. The daemon auto-starts on the first CLI
invocation and self-terminates after an idle timeout. The client auto-restarts
the daemon on version mismatch.

**One shutdown path.** Both operator-initiated routes — the gRPC `Shutdown` RPC
(`daemon restart`) and SIGTERM (`daemon stop`) — go through
`begin_shutdown_drain` in `crates/nestweaver-daemon/src/server.rs`, which runs
the same drain loop and emits the same messages. Neither broadcasts shutdown up
front, so **listeners stay up and reads keep being served while writes drain**.
An idle daemon still exits immediately: the drain tests its counters before its
first sleep. `NESTWEAVER_DRAIN_TIMEOUT_SECS` governs both routes.

**`daemon stop` does not escalate automatically.** It waits up to the stop grace
and then, if the daemon is still draining, **reports and stands down** — it
leaves the daemon running, re-probes the socket so its claim about read service
is observed rather than assumed, and exits non-zero. It does not SIGKILL.
Automatic escalation was the defect: nothing in the process can abort a
`spawn_blocking` write, and the graph store is not crash-safe, so an automatic
SIGKILL performed on a timer exactly the crash declined at the drain ceiling on
nw-126 evidence (a SIGKILLed daemon left a stale 42-byte WAL that made a live
5.6 GB database look absent). Ending an in-flight write is an explicit operator
act: `nestweaver daemon stop --force`, or `kill -9 <pid>`.

**Two client-side paths still do escalate, and this change did not touch them.**
`nestweaver_daemon::lifecycle::stop_legacy_hash_daemon` and the legacy
`$TMPDIR`-path cleanup in `nestweaver-client/src/autostart.rs` both do
SIGTERM → 2s → SIGKILL when autostart finds a *legacy* daemon holding the DB
write lock. Two seconds is far inside a drain, so either can SIGKILL a daemon
that is draining correctly — the nw-126 crash, on a 2-second fuse. Both are
migration paths (pre-SHA-256 instance IDs; pre-v0.26.2 `$TMPDIR` sockets) and
neither is on the `daemon stop` route, but "nothing escalates automatically" is
true of `daemon stop` only, not of the binary as a whole.

**The drain is monotone over the gRPC surface.** Once shutdown starts,
`ConnectionGuard::write` refuses new writes with `UNAVAILABLE`. This is not a
nicety: because listeners now stay up by design, without it a webhook feed,
watcher, MCP client or agent loop could start a new write minutes into a drain
and reset the drain's exit condition indefinitely — leaving `--force`/`kill -9`
as the only way out, which would make the unsafe kill *more* likely on a busy
daemon, not less. Reads are deliberately not gated, and every gated RPC takes the
guard BEFORE the write gate so a refusal is immediate rather than queued behind
the in-flight write.

The claim is scoped to gRPC on purpose. The **web admin** routes
(`nestweaver-web/src/routes/admin.rs`) take the write gate but no
`ConnectionGuard::write`, and `AdminState` is never handed `shutdown_started`, so
they can neither be refused nor consult the flag. They are also invisible to the
drain's exit condition, which means the drain neither waits for them nor is
extended by them. Pre-existing, not introduced or fixed here.

**Under a process supervisor none of that is a guarantee.** The unbounded drain
describes what this process does with SIGTERM; a supervisor that SIGKILLs on its
own timer still does, and nothing here can prevent it. The repo ships **no
systemd unit** — on Linux `daemon start` spawns a bare detached process
supervised by nothing, and that is the case this change fully fixes. The macOS
launchd plist now sets `ExitTimeOut` to the drain ceiling + 60 (the key was
absent, so launchd applied its 20s default), and `docker-compose.yml`'s
`stop_grace_period` is raised to 720s — but both remain hard deadlines that
SIGKILL a draining daemon. **The interactive case is fixed; the supervised case
remains exposed until the graph store is crash-safe.** See
`docs/guide/daemon-shutdown.md`.

`NESTWEAVER_DRAIN_TIMEOUT_SECS` (default 660s) still means two different things
depending on what is still in flight. While a WRITE RPC is running it is a
reporting threshold, not a kill switch — those run on
`spawn_blocking` threads that cannot be aborted, so past the ceiling the daemon
keeps waiting, keeps serving reads, and logs what it is waiting on; an operator
SIGKILL (`daemon stop --force` or `kill -9`) is the only thing that ends a stuck
write, and nothing does it automatically. A genuinely running INDEX job is
treated the same way: the drain consults the worker pool's own in-flight
counter (`DaemonState::indexing_in_flight`, shared into the worker's
`IndexingStatus` so the worker — not the flag — is the authority on whether a
job is running), and a job that is genuinely in flight past the ceiling gets
the same unbounded wait with reads still served. The ceiling is a real deadline
only for a STUCK flag: `indexing_active` set with no worker job in flight,
which cannot clear once the pool is drained with a non-empty queue, so waiting
on it would hang forever — there the daemon signals shutdown at the ceiling,
which closes every listener and stops read service. One honest limit: a job
that is in-flight but truly wedged is indistinguishable from a slow one, so it
gets the unbounded wait — the same operator escape (`daemon stop --force` /
`kill -9`) applies, and reads stay up in the meantime, which the old bounded
branch could not say. During a write drain MOST reads keep being served —
`embed` and `plan_embed` are the exception, they take the same write gate and
block until the write finishes.
Indexing is CPU-throttled to a rolling 5s duty-cycle window so a saturated
daemon stays under macOS CPU-violation limits; tune with
`NESTWEAVER_INDEX_CPU_PERCENT` (percent of one core, 1–99, default 50; `0` or
`>=100` disables throttling). The var reaches the daemon two ways: from the
shell env for directly-spawned daemons, or baked into the launchd plist's
`EnvironmentVariables` at `daemon start` time (launchd jobs don't inherit the
shell env — re-run `daemon start` after changing it).

**The daemon is the sole writer to the DB file.** Never run `sqlite3` or other
tools against the DB while the daemon is running. Bypassing the daemon risks
WAL corruption from concurrent access. If you see "database locked" errors,
stop the daemon (`nestweaver daemon stop`) rather than using `--no-daemon`.

`--no-daemon` and `NESTWEAVER_NO_DAEMON=1` only **request** the bypass.
`NESTWEAVER_ALLOW_NO_DAEMON` is the only thing that **permits** it — `CI` and
`GITHUB_ACTIONS` confer nothing (they used to, and an ambient `CI=true` deciding
writer exclusivity was the defect). A requested-but-unpermitted bypass is
disclosed on stderr and the command routes through an autostarted daemon
anyway, so a CI job that passes `--no-daemon` and expects isolation gets a
daemon. See `no_daemon_allowed_from` in `src/main.rs`.

## Environment variables (operator-facing)

Timeouts and tuning an operator may actually need. Every one of these is named
by an error message the tool can print, so they belong somewhere findable.

| Variable | Default | Purpose |
|----------|---------|---------|
| `NESTWEAVER_DAEMON_BOOT_TIMEOUT_SECS` | 30 | How long a client waits for the daemon to bind its socket. Raise it on a slow cold start; the boot-failure message names it. Boot phase timings (`boot_ms`, `store_open_ms`, `extension_reconcile_ms`, `unattributed_ms`) are logged at bind so a slow boot is diagnosable rather than guessed at. |
| `NESTWEAVER_INDEX_TIMEOUT_SECS` | 1800 | Overall ceiling for one index. On expiry the daemon requests cancellation and reports a non-terminal warning naming this variable. Cancellation is COOPERATIVE and only observed up to the pre-write boundary, so the final stream event says whether the run aborted before writing or committed anyway (committed-after-cancellation names `index --force` as the repair). |
| `NESTWEAVER_DRAIN_TIMEOUT_SECS` | 660 | Drain ceiling for BOTH shutdown routes — the gRPC `Shutdown` RPC (`daemon restart`) and SIGTERM (`daemon stop`), which share one drain. With an in-flight write OR a genuinely running index job (the drain reads the worker pool's own in-flight counter, not the `indexing_active` flag) it is NOT a deadline: the daemon cannot abort either, so past this point it keeps waiting, keeps serving most reads (`embed`/`plan_embed` excepted — they take the write gate), logs the in-flight count, and names `daemon stop --force` / `kill -9` as the escapes. With only a STUCK `indexing_active` flag (set, but no worker job in flight) it IS a deadline: the daemon signals shutdown at the ceiling, which stops read service, because that flag cannot clear once the worker pool is drained with a non-empty queue. Also derives `NESTWEAVER_STOP_GRACE_SECS`, the launchd plist's `ExitTimeOut` (ceiling + 60 — deliberately later than the stop grace, so the CLI gives up watching before launchd kills), and the client's owner-release wait (ceiling + 5s). |
| `NESTWEAVER_STOP_GRACE_SECS` | drain ceiling + 30 (690) | How long `daemon stop` waits for the daemon to exit. This is NOT a kill deadline: when it expires with the daemon still draining, `daemon stop` re-probes the socket, reports what it observed, leaves the daemon running, and exits non-zero. It does not SIGKILL — see the daemon-architecture section for why an automatic escalation was the defect. Listeners stay up for the whole window in a write drain (and in a genuinely-running-index drain), so waiting is not an outage (individual reads can still stall for seconds while a write commits); only in a stuck-flag index drain does the ceiling broadcast close them 30s before this expires, and the message says so. `daemon stop --force` ignores this variable and uses a short fixed 10s window before SIGKILL, abandoning any in-flight write. |
| `NESTWEAVER_INDEX_CPU_PERCENT` | 50 | Index CPU duty cycle, percent of one core (1–99; `0` or `>=100` disables). Also see the launchd note below. |
| `NESTWEAVER_ALLOW_NO_DAEMON` | unset | Opt-in required to honour `--no-daemon` / `NESTWEAVER_NO_DAEMON` outside CI. Without it the bypass is REFUSED, because it circumvents the single-writer lock. Not for normal use. |
| `NESTWEAVER_SOCK_FALLBACK_DIR` | `/tmp/nw-sock-<uid>` | Root of the /tmp socket-fallback tree (used when the runtime socket path would exceed the 104-byte `sun_path` limit). Test support: daemon, client, and `daemon gc` all read it, so a test points every one of them at one scratch directory and never sweeps the operator's real fallback root. Not for normal use. |
| `NESTWEAVER_LBUG_MAX_THREADS` | 1 | Engine thread-pool size. `1` closes the nw-073 eviction-vs-read race; raise only if you measure a query-latency cost. |
| `NESTWEAVER_LBUG_BUFFER_POOL_BYTES` | auto | Buffer pool size. A larger pool avoids eviction when the working set fits. |
| `NESTWEAVER_LBUG_AUTO_CHECKPOINT` | on | `0`/`false` defers auto-checkpoints; reduces the #678 corruption trigger during bulk load. |
| `NESTWEAVER_LBUG_MAX_DB_SIZE` | engine default | Max database size in bytes. Also bounds the VIRTUAL ADDRESS RESERVATION each open takes, so a smaller value allows more concurrent opens — lbug's own test config bounds it for exactly that reason. `.cargo/config.toml` pins 16 GiB for anything cargo runs, because the suite opens dozens of stores at default parallelism and exhausted address space, failing unrelated tests with an mmap error (nw-137). Raise it for a brain approaching the bound. |
| `NESTWEAVER_RPC_TIMEOUT_SECS` | 300 | Client-side ceiling on a daemon RPC; `0` disables. `--max-millis` is enforced SERVER-side and does not bound the client's wall clock, so without this a daemon that accepted the connection and then stopped answering parked the CLI indefinitely (nw-162). With `--max-millis` the ceiling is that budget plus a transport margin. |
| `NESTWEAVER_INDEX_PUBLICATION_WAIT_MS` | 3000 | How long a ranked query waits out an in-flight index publication before failing closed. Named in the error message itself, so it must be discoverable here. |
| `NESTWEAVER_CRASH_REPORT_DIRS` | platform | Extra directories scanned by `diagnostics capabilities` for nw-073 crash recurrence. |
| `NESTWEAVER_GIT_CLONE_TIMEOUT_SECS` / `NESTWEAVER_GIT_NET_TIMEOUT_SECS` | — | Bounds on git clone / network operations during pull. |

Server mode additionally reads `NESTWEAVER_BIND`, `NESTWEAVER_TOKEN`,
`NESTWEAVER_ADMIN_TOKEN`, `NESTWEAVER_UPSTREAM`, and
`NESTWEAVER_WEBHOOK_SECRET` / `NESTWEAVER_WEBHOOK_SECRET_OLD` (rotation).

## macOS App (preferred on Mac)

On macOS, prefer the native `.app` bundle over the CLI daemon. It provides:
- Menubar status icon for quick access to the web UI
- Metal GPU acceleration (GUI session context required for MTLCompilerService)
- Automatic daemon lifecycle with crash recovery
- Daemon coexistence — detects running daemons and connects instead of duplicating

```sh
# Build and launch
bash app/build.sh
open target/release/NestWeaver.app

# NestWeaver.app is source-build-only until a release job publishes a .app or DMG.
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
nestweaver dead-code                     # REVIEW AID, not a deletion list — measured 0/15 top-15 precision on Rust, poor on C++
#                                        REFUSES (exit 2) on a resolver-generation-stale graph — see the sidecar bullet

# Export
nestweaver export --format cypher        # graph export (cypher, graphml, mermaid)
nestweaver export --format msgpack       # graph snapshot for WASM engine

# Markdown brain (`.brainignore` for glob exclusion patterns; `--ignore` flag for ad-hoc)
nestweaver brain add ~/Documents/Obsidian/MyVault
nestweaver brain add ~/vault --config ./instance.toml  # uses config's instance_id and db field
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
nestweaver interactions forget <uid> --db ./nestweaver.lbug    # drop one node's memory, keep the rest
nestweaver extensions list --db ./nestweaver.lbug              # read back what agents wrote via set_extension
nestweaver extensions unset --uid <uid> --key <key>            # remove one extension property from one node

# MCP server (42 tools; 36 in direct read-only mode; 6 with --lite, e.g. for Cursor).
# The count is derivable, not typed: `all_tool_schemas_undecorated()` in
# crates/nestweaver-mcp/src/tools.rs is the registry, and
# tools::tool_doc_tests::all_tools_have_doc_categories asserts the doc table
# covers exactly tool_list(false)["tools"].len(). Read it back with tools/list
# rather than restating it.
nestweaver mcp --db ./nestweaver.lbug
nestweaver mcp --lite --db ./nestweaver.lbug                          # 6 core tools only
# --tools takes exact, case-sensitive REGISTRY names, not CLI verb names.
# `--tools context,search,symbol` is rejected at startup: those are CLI
# subcommands, not tools.
nestweaver mcp --tools brain_context,brain_search,read_symbols --db ./nestweaver.lbug

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
- `<db>.filemeta.json` — per-file mtime/size/hash cache for tiered change detection (skips unchanged files on re-index). **v3**: the mtime is NANOSECONDS, not seconds — truncating to seconds made same-second edits permanently invisible. Versioned in lockstep with `resolution_cache::CACHE_VERSION`; a stale version is discarded, costing one full re-index rather than risking a mis-classification
- `<db>.summaries.json` — hierarchical code summaries cache (symbol/file/cluster levels)
- `<db>.tantivy/` — BM25 full-text search index for notes and sections
- `<db>.clusters.json` — community/cluster detection output
- `<db>.extensions.json` — user-defined extension properties on nodes
- `<db>.aliases.json` — taxonomy alias mappings from vault files
- `<db>.interactions.json` — agent interaction memory (query patterns, access frequency, follow-up signals)
- `<db>.perspectives.json` — saved web UI perspectives (web crate only)
- `<db>.cache` — MCP response cache (binary: MessagePack + ZSTD; falls back to legacy JSON on read). Every entry also records the response-SHAPE version of the binary that wrote it (derived by `nestweaver-mcp/build.rs` from the shape-relevant crate sources). Foreign-shape entries are dropped at open and refused on lookup, so a release that adds a response field cannot serve the old shape from cache across an upgrade. The digest is deliberately over-broad: a comment-only edit in a hashed crate also invalidates the cache, costing one recompute
- `<db>.parsed_cache.bin` — Cached parse results (symbols, references, type bindings) keyed by content hash, for skipping re-parsing unchanged files
- `<db>.resolution_deps.bin` — Per-file resolution dependency tracker for incremental cross-file resolution
- `<db>.resolver_generation.json` — per-repo record of which resolver generation built that repo's edges. A repo with no entry predates the record and reads as generation 0. A repo below `RESOLVER_GENERATION` is reported as stale by `hubs`/`bridges`, because a resolver fix that changes edge SHAPE cannot repair edges already written — only re-indexing can. **`RESOLVER_GENERATION` is 4 as of 9.0.0** (`crates/nestweaver-engine/src/resolver_generation.rs`, which carries the per-generation rationale); every graph built by an earlier release must be re-indexed before rankings, `MEMBER_OF` edges and C++ `IMPORTS` edges are correct. **`stale-check` consults this sidecar as of 9.0.0** — a repo below `RESOLVER_GENERATION` reports `status: "outdated_resolver"`, `resolver_stale: true`, `needs_reindex: true`, and the command exits 2 (through 8.x its ladder was SHA-vs-HEAD only and a generation-3 graph exited 0). The remedy needs `--force`: a generation-stale repo is at HEAD with nothing modified, so plain `nestweaver index --repo <path>` takes the incremental path and writes nothing. `hubs`, `bridges`, `repo-map`, `ranking rank` and `summary --level hub` disclose it too (`rankings_stale` / `stale_repos`). **`dead-code` REFUSES as of 9.0.0** on every route (CLI direct, CLI daemon, `--json`, MCP `dead_code`): it returns `refused: true` with `reason: "outdated_resolver"`, a `remedies` array of ready-to-run `nestweaver index --repo <path> --force` commands, and NO `unreachable_symbols` key, and the CLI exits 2. It refuses rather than disclosing because its output is a list of symbols to DELETE computed by a forward reachability walk, so a missing edge can only move a LIVE symbol onto it — the error is one-directional and the deletion is not recoverable. The response cache is salted with this sidecar (`resolver_generation_cache_salt`) so a pre-bump list cannot be replayed past the bump. `clusters`, `blast-radius`, `affected-tests`, `generate-guide`, PPR-backed `context` and the web UI still do not disclose. Vaults are not `Repo` nodes and carry no generation

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

Canonical list: the `EXIT_*` constants at the top of `src/main.rs`.

| Code | Meaning |
|------|---------|
| 0 | Success |
| 1 | Error — including the check itself failing |
| 2 | Not found (symbol, service) · `stale-check`: at least one repo needs a re-index — behind HEAD, incomplete, missing, or built by an older resolver generation · `pr-impact --strict`: blocked on a contract-verified breaking change |
| 3 | Ambiguous match (multiple symbols with same name) |
| 4 | Unauthorized (pull) |
| 5 | Unavailable (pull) |
| 64 | Usage error — unknown flag, bad value, missing argument (`EX_USAGE` from BSD `sysexits.h`) |

**64, not clap's 2.** Clap's default collided with `EXIT_NEEDS_REINDEX`, so a CI
gate could not tell `nestweaver stale-chekc` (a typo) from "your graph is
stale". Anything that gates on exit codes must treat 64 as a usage bug, never as
drift. Note that 2 is overloaded across commands — `case $rc in 2)` is not
portable between `stale-check` and `pr-impact --strict`.

Measured on this branch: `export --format json` → 64 · `export --scope bogus` →
64 · `impact --limit 5000` → 64 · `impact <unknown>` → 2 · `export --format
cypher --scope vault` (valid enums, unsupported *combination*) → 1.
