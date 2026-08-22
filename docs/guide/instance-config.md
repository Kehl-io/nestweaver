# Instance Config Guide

An instance config tells NestWeaver which repos to index, how they relate
to each other, and what cross-cutting features span them.

## Quick Start

Start with the canonical minimal fixture, then validate it before using it:

```sh
cp examples/minimal-instance.toml nestweaver-instance.toml
nestweaver config validate nestweaver-instance.toml
```

It contains every required setting and no repository declarations:

```toml
instance_id = "minimal-example"

[snapshot_storage]
backend = "local"
path = "~/.local/share/nestweaver/minimal/snapshots"

[workspace]
backend = "local"
path = "~/.local/share/nestweaver/minimal/workspace"

[inference]
endpoint = "http://localhost:11434"
embedding_model = "nomic-embed-text"
summary_model = "qwen2.5-coder:7b"

[git]
credential_method = "gh"
```

Add one or more `[[repos]]` entries when the instance should declare remote
repositories, then:

```sh
# Index each repo
nestweaver index --repo ./frontend --db ./my-project.lbug
nestweaver index --repo ./backend --db ./my-project.lbug

# Query across both
nestweaver search "UserService" --db ./my-project.lbug
nestweaver context UserService --db ./my-project.lbug
```

## Config Reference

### Required settings

#### `instance_id`

A unique name for this instance. Used in UIDs and registry.

```toml
instance_id = "my-project"
```

#### `db` (optional)

Selects the graph database for commands invoked with `--config`, so callers do
not also need `--db`.

```toml
db = "/path/to/brain.lbug"
```

An explicit `--db` takes precedence. When `db` is absent, NestWeaver uses
`NESTWEAVER_DB` and then `./nestweaver.lbug`.

#### `expected_brain_uuid` (optional)

Binds the config to one logical brain, independently of its path or
`instance_id`. When set, NestWeaver refuses to query, index, or start a daemon
against a database with a different graph-owned identity.

```toml
expected_brain_uuid = "2ad44321-c93b-49b1-953c-626a00586673"
```

This is an assertion, not an adoption mechanism. If the configured database is
missing, NestWeaver will not create a new brain under the expected UUID. Restore
or rebuild the expected brain explicitly. UUID spellings are canonicalized
during config validation, and malformed or nil UUIDs are rejected.

Inspect and intentionally bind identity with:

```sh
nestweaver instance identity --db /path/to/brain.lbug
nestweaver instance adopt-identity ./instance.toml --db /path/to/brain.lbug --dry-run
nestweaver instance adopt-identity ./instance.toml --db /path/to/brain.lbug
```

`adopt-identity` changes the config assertion; it never rewrites graph identity.
NestWeaver validates the complete edited TOML and durably replaces the config
while preserving its comments and formatting.

#### `[snapshot_storage]`

Where snapshots are stored for distribution.

```toml
[snapshot_storage]
backend = "local"          # "local" | "s3" | "gitlab"
path = "/path/to/snapshots"
```

#### `[workspace]`

Where source code is pulled on demand.

```toml
[workspace]
backend = "local"
path = "/path/to/workspace"
```

#### `[inference]` (required)

Required connection settings for the remote inference subsystem, including the
models used for remote embeddings and summaries. It has no global default, so
each instance explicitly selects its endpoint and models. It is separate from
the semantic-search subsystem in `[embedding]`.

```toml
[inference]
endpoint = "http://localhost:11434"   # Ollama, vLLM, or any OpenAI-compatible API
embedding_model = "nomic-embed-text"
summary_model = "qwen2.5-coder:7b"
```

#### `[embedding]` (optional; defaults apply)

`[embedding]` independently configures local or external semantic search and
hybrid retrieval weights. Its defaults apply when the section is omitted; it
does not inherit settings from `[inference]`. The semantic layer finds related
symbols, notes, and headings as seeds for graph retrieval.

`model_id` here is the default for a *fresh* database. When you run `nestweaver
embed`, NestWeaver records the model actually used, and the daemon loads that
recorded model at startup regardless of this setting — so a database always uses
a model matching its stored vectors. The default `all-MiniLM-L6-v2` is 384-dim,
fast, and CPU-friendly (best for most users); `thenlper/gte-base` is 768-dim for
higher-quality retrieval. Any mean-pooled BERT-compatible HuggingFace model
works.

```toml
[embedding]
model_id = "sentence-transformers/all-MiniLM-L6-v2"  # default for fresh DBs; the embedded model is recorded & auto-loaded
# cache_dir defaults to the platform-native cache directory:
# ~/Library/Caches/nestweaver/models on macOS, $XDG_CACHE_HOME/nestweaver/models
# (or ~/.cache/nestweaver/models) on Linux. Explicit values may use ~/.
# If unavailable or non-UTF-8, a UTF-8 HOME uses ~/.cache first. The final
# fallback is /var/cache/nestweaver/models on Unix or C:\ProgramData\nestweaver\models
# on Windows; set cache_dir explicitly if that system location is not writable.
# cache_dir = "~/Library/Caches/nestweaver/models"
accelerator = "auto" # auto | metal | cpu

# Optional: use an authoritative external API instead of the local model.
# external_endpoint = "https://api.openai.com"
# external_model = "text-embedding-3-small"

# Retrieval fusion weights (must sum to 1.0)
weight_ppr = 0.40         # graph structure (Personalized PageRank)
weight_bm25 = 0.25        # text match
weight_semantic = 0.35    # embedding similarity

# Seed selection
always_blend_semantic = true   # add semantic matches to PPR seeds even when name resolution succeeds
semantic_seed_limit = 5        # top-k semantic hits used as PPR seeds
semantic_search_limit = 200    # top-k semantic hits fed into fusion
```

| Field | Default | Description |
|-------|---------|-------------|
| `model_id` | `"sentence-transformers/all-MiniLM-L6-v2"` | Default local model for fresh DBs (any mean-pooled BERT-compatible HF model). The model a DB was embedded with is recorded and auto-loaded, overriding this. |
| `cache_dir` | platform-native | Hugging Face cache root. Default: `~/Library/Caches/nestweaver/models` on macOS, `$XDG_CACHE_HOME/nestweaver/models` (or `~/.cache/nestweaver/models`) on Linux. If unavailable or non-UTF-8, a UTF-8 home supplies `~/.cache/nestweaver/models`; only then does it fall back to `/var/cache/nestweaver/models` on Unix or `C:\ProgramData\nestweaver\models` on Windows. Set `cache_dir` explicitly if the final system location is not writable. An explicit leading `~/` is expanded against the user's home directory. |
| `accelerator` | `"auto"` | Local device policy; exact behavior is below. Ignored for an external backend. |
| `external_endpoint` | — | Optional authoritative external embedding API endpoint |
| `external_model` | — | Model name for the external endpoint |
| `weight_ppr` | `0.40` | Fusion weight for graph structure (Personalized PageRank) |
| `weight_bm25` | `0.25` | Fusion weight for BM25 text match |
| `weight_semantic` | `0.35` | Fusion weight for embedding similarity |
| `always_blend_semantic` | `true` | Add semantic matches to PPR seeds even when name resolution finds results |
| `semantic_seed_limit` | `5` | Top-k semantic hits injected as PPR seeds |
| `semantic_search_limit` | `200` | Top-k semantic hits fed into fusion scoring |

Device policies for the local backend are exact:

| Value | Behavior |
|-------|----------|
| `auto` | Metal in a Metal-enabled build; CPU only when Metal is not compiled. A Metal failure is reported; `auto` does not retry on CPU. |
| `metal` | Requires Metal to be compiled and both device creation and the full model inference probe to succeed. Failure leaves embedding state `failed`; CPU is never selected. |
| `cpu` | Selects CPU directly and never probes Metal. Use this for an intentional CPU deployment or to opt out of Metal. |

An external endpoint is authoritative. NestWeaver does not load or invoke the
local backend after an external load, readiness, or request failure. Fix the
endpoint, or follow the forced re-embedding procedure below to switch to a local
backend.

#### Daemon embedding preflight

For the normal daemon route, `nestweaver embed --db <path>` first reports an
embedding plan: the number of nodes in scope, the number eligible to embed, and
the number already present in the authoritative embedding sidecar. With the
default incremental behavior, sidecar entries are skipped; `--force` makes every
scoped node eligible. If the plan has no eligible nodes, the command succeeds
without loading the embedding model.

The plan is a point-in-time observation. Files indexed after the plan are
handled by a later embed or the daemon's incremental watcher. If the command
reports that the daemon does not support preflight after installing an updated
binary, restart that daemon for the same database:

```sh
nestweaver daemon --db <path> restart
```

Daemon startup is cache-only. It does not contact Hugging Face or download
missing model files. To populate a new cache, stop the daemon (which owns the DB
write lock) and run the direct local command. Its required form is
`nestweaver embed --db <path> --local --model-id <id> --cache-dir <path>`.
The direct path downloads missing files into that cache and records the model
used by the database:

```sh
CONFIG=/absolute/path/to/nestweaver-instance.toml
DB=/absolute/path/to/brain.lbug
MODEL=sentence-transformers/all-MiniLM-L6-v2
CACHE="$HOME/.cache/nestweaver/models"
nestweaver daemon --db "$DB" stop
nestweaver embed --db "$DB" --local --model-id "$MODEL" --cache-dir "$CACHE"
nestweaver daemon --db "$DB" start --config "$CONFIG"
```

Do not omit `--local`: without it, `embed` routes to the configured cache-only
daemon and cannot populate missing model files. The direct command receives
`--cache-dir` from the shell, so prefer an absolute path or `$HOME/...`; the
leading-tilde expansion described above applies to the TOML setting.

Switching from an external backend to a local model requires more than removing
`external_endpoint`: the database records the external model that produced its
vectors, and that recorded model overrides the configured local default at
daemon startup. Remove `external_endpoint`/`external_model`, stop the daemon,
and replace both vectors and recorded metadata with a forced direct-local embed:

```sh
CONFIG=/absolute/path/to/nestweaver-instance.toml
DB=/absolute/path/to/brain.lbug
MODEL=sentence-transformers/all-MiniLM-L6-v2
CACHE="$HOME/.cache/nestweaver/models"
nestweaver daemon --db "$DB" stop
nestweaver embed --db "$DB" --local --model-id "$MODEL" --cache-dir "$CACHE" --force
nestweaver daemon --db "$DB" start --config "$CONFIG"
```

#### `[ranking]` — Retrieval ranking policy

```toml
[ranking]
track_interactions = false   # default
enable_prf = false           # default
```

| Field | Default | Description |
|-------|---------|-------------|
| `track_interactions` | `false` | Record MCP interaction telemetry to the `<db>.interactions.json` sidecar, which feeds usage-based ranking. Local-only: UIDs and timestamps, no content. The CLI `--track-interactions` / `--no-track-interactions` flags override it per invocation; passing neither inherits this value. |
| `enable_prf` | `false` | Pseudo-relevance-feedback query expansion on BM25 searches. The CLI `--prf` flag and MCP `prf: true` override per call. |
| `dampen` / `boost` | — | Path-glob priors on result relevance. |
| `test_path_patterns` | built-in list | Substrings that deboost test/fixture paths in symbol-name ranking. |
| `git_activity_weight` | `1.2` | Weight of the git-activity recency rescale on `pagerank_score`. Applies only when a `<db>.gitactivity.json` sidecar exists. |

`track_interactions` lives here rather than existing only as a CLI flag because
it is a durable per-brain policy, and `nestweaver setup` regenerates every
`.mcp.json` it manages — so a flag hand-added to a generated config was dropped
on the next setup run, leaving the feature unreachable through the supported
install path. Setting it here reaches every generated registration at once.

Note this is not only telemetry: the recorded scores feed PPR personalization,
so enabling it changes retrieval output. The blend is deliberately capped (an
exploration floor limits interaction history to a small fraction of the
personalization mass, with sublinear scoring, time decay, and invalidation on
content change), but treat it as a ranking change rather than pure logging.

For a generated MCP registration to see any of this, it must be started with
`--config`. `nestweaver setup` emits that automatically.

#### `[git]`

How to authenticate git operations.

```toml
[git]
credential_method = "gh"   # "gh" | "ssh" | "credential-helper"
```

#### `[[repos]]`

List of repositories to index. `url` is required. `name` is an optional display
alias, and `type` is optional: omit it (or set `type = "code"`) for source code,
or set `type = "vault"` for a markdown vault.

```toml
[[repos]]
url = "https://github.com/myorg/frontend"
name = "frontend"       # optional display alias

[[repos]]
url = "https://github.com/myorg/docs"
type = "vault"          # optional: "code" (default) | "vault"

[[repos]]
url = "https://github.com/myorg/backend"
sparse = false        # optional: disable sparse checkout (default: true)
pin_sha = "abc123"    # optional: pin to a specific commit
```

### Optional sections

#### `[daemon]` — Daemon lifecycle policy

```toml
[daemon]
# macOS only. Emit RunAtLoad into the generated launch agent so the daemon
# is *started* at login, not merely registered. Off by default.
start_at_login = false  # default
```

Without `RunAtLoad`, launchd registers the agent at login but never starts it.
`nestweaver daemon start` compensates with an explicit `launchctl kickstart`,
which covers install time but **not a reboot** — after restarting, the daemon
comes back only if something else starts it (typically the desktop app running
as a login item). If you use the CLI or MCP without the app, enable this.

It is opt-in rather than on by default because `RunAtLoad` boots a daemon that
loads an embedding model at *every* login, including sessions that never touch
NestWeaver. The one-hour idle exit bounds that cost but does not remove it.

The setting is read from the config the launch agent will run with, so it only
takes effect on a `daemon start` that was passed a `--config` (directly or via
persisted config intent). Do not hand-edit the generated plist: every
`daemon start` overwrites it with generated content.

#### `[pr_impact]` — Pre-push / CI strict-gate policy

Controls what `nestweaver pr-impact --strict` (and the strict `hooks --install
--strict` pre-push hook) blocks a push on. Absent ⇒ the default policy below.

```toml
[pr_impact]
# Block --strict on a contract-verified breaking change — a decidable API
# signature break (BreakTier::Breaking). Precision-first: on by default.
strict_block_on_breaking = true    # default
# Also block --strict on a *complete* High-risk (heuristic) run
# (GateState::RiskFlagged). Off by default — the risk score stays advisory so a
# legitimate change to a central symbol isn't blocked by a high score.
strict_block_on_high_risk = false  # default
```

A degraded/incomplete run is never blocked on risk regardless of these settings
(an incomplete traversal can't be trusted to have found the risk). With both
switches `false`, `--strict` is advisory (exit 0). `pr-impact` discovers this
config at `<repo>/.nestweaver/instance.toml`.

#### `[authz]` — Per-repo authorization (Blast Radius)

Optional per-repo scoping for blast-radius results on the MCP-HTTP and daemon
gRPC surfaces (host-agnostic — matched against each repo's `url`/`uid`). **Absent
or empty ⇒ disabled ⇒ every caller sees all repos (`VisibleRepos::All`) — zero
behavior change.** Add any rule and the policy is enabled and **fail-closed**: an
unknown/unlisted token sees nothing.

```toml
[authz.rules]
# query token -> list of repo globs it may see (matched against Repo.url / uid)
"team-frontend-token" = ["github.com/acme/frontend*", "github.com/acme/design-*"]
"team-backend-token"  = ["github.com/acme/backend*"]
```

Blast-radius responses (affected/changed symbols, `org_wide` impact, coverage,
cluster/summary counts) are redacted to the caller's visible repos. Stdio MCP and
the local CLI are single-user and always see all repos.

#### `[[links]]` — Cross-repo relationships

Declare how repos communicate. This is how NestWeaver knows that your
React app talks to your Node.js backend, or that your mobile app connects
to firmware over BLE.

```toml
[[links]]
from = "frontend"                 # repo name (last segment of URL)
to = "backend"
type = "http-api"                 # see link types below
description = "React app calls backend REST API"
endpoints = ["/api/users", "/api/sessions", "/api/devices"]

[[links]]
from = "mobile-app"
to = "device-firmware"
type = "ble"
description = "App connects to device via BLE UART"
identifiers = ["6E400001-B5A3-F393-E0A9-E50E24DCCA9E"]
```

**Link types** (any string is accepted, these are conventions):

| Type | Use for |
|------|---------|
| `http-api` | REST/HTTP communication between services |
| `grpc` | gRPC/protobuf communication |
| `graphql` | GraphQL API |
| `ble` | Bluetooth Low Energy |
| `websocket` | WebSocket connections |
| `shared-db` | Services sharing a database |
| `event-bus` | Message queue, pub/sub, event-driven |
| `shared-types` | Shared type/schema package |

**Optional fields:**

```toml
[[links]]
from = "frontend"
to = "backend"
type = "http-api"
description = "..."
endpoints = ["/api/users"]       # API route patterns
identifiers = ["UUID-HERE"]      # Protocol identifiers (BLE UUIDs, etc.)
contract = "openapi/api.yaml"    # Path to shared contract/schema
```

#### `[[features]]` — Cross-cutting feature bundles

Group entry points across repos into named features. When an agent says
"I'm working on device pairing," NestWeaver returns context from all
relevant repos.

```toml
[[features]]
name = "device-pairing"
description = "Device discovery, BLE pairing, and registration"
repos = ["mobile-app", "device-firmware", "backend"]
entry_points = ["BLEProvider", "connectToDevice", "registerDevice"]

[[features]]
name = "user-auth"
description = "Login, signup, password reset, session management"
repos = ["frontend", "backend"]
entry_points = ["LoginForm", "useAuth", "authMiddleware", "createSession"]
```

Then query by feature:

```sh
nestweaver context --feature device-pairing --config ./nestweaver-instance.toml --db ./project.lbug
```

#### `[schema_extensions]` — Custom node properties

Add custom properties to node types (additive only — cannot remove or
redefine core properties).

```toml
[schema_extensions]
extra_node_properties = { Symbol = { team_owner = "string", deprecated = "bool" } }
```

### Change detection

Indexing skips a file when its cached metadata proves it has not changed. That
check compares the file's **modification time in nanoseconds** and its **size**;
only when one of those moves is the file read and content-hashed.

Both parts matter, and neither is redundant:

- **Nanoseconds.** The timestamp used to be truncated to whole seconds, so any
  edit landing in the same second as the cached value compared equal and the
  file was classified unchanged. Because the cached value then kept matching, it
  stayed misclassified on every later index — it never self-healed; only a
  further mtime change recovered it.
- **Size.** How much sub-second precision you actually get is platform-
  dependent. On macOS/APFS it is effectively per-write (measured: 200 rapid
  writes, 200 distinct stamps). On Linux the VFS stamps mtime from a clock
  updated once per jiffy, so the granularity there is ~1–10ms depending on
  `CONFIG_HZ`, not 1ns. On filesystems with no sub-second field at all (HFS+,
  ext3, FAT, some network mounts) it stays whole seconds. Size is what still
  catches size-changing edits on every one of those.

What remains uncovered is an edit inside a single timestamp tick that also
leaves the size byte-for-byte identical — Git's "racily clean" case. Bounding it
completely requires a per-run index timestamp, the way Git bounds its window by
the index file's own mtime.

An over-sensitive timestamp is safe here. If a filesystem reports a sub-second
value that moves when nothing changed, the file falls through to the content
hash, compares equal, and is reported unchanged — it costs a read, not a wrong
answer. (This is why Git's caution about `USE_NSEC` on CEPH/CIFS/NTFS/UDF does
not transfer: there, an erroneous value makes Git report a file dirty.) The
observed stat is then written back, so the file is not re-hashed on every
subsequent index.

#### Upgrading

The change-detection sidecar (`<db>.filemeta.json`) is versioned, and the
nanosecond change moves it to **v3**. A v2 entry stores seconds in the field v3
reads as nanoseconds, so it is **discarded rather than reinterpreted** —
reusing it would be indistinguishable from a real edit.

**The first index after upgrading is therefore a full re-index of every existing
database.** This is a deliberate fail-open: it costs one re-index and can never
mis-classify a file. `resolution_cache::CACHE_VERSION` is bumped in lockstep for
the same reason. Embeddings are keyed separately and are **not** invalidated, so
no re-embed is required.

Integrators implementing `ContentReader` should note the trait method is now
`file_meta_nanos` and must return nanoseconds. It was renamed because the tuple
shape did not change: an implementation still returning seconds would otherwise
compile, write seconds into a valid v3 cache, and silently keep the bug.

### `[indexing]` (optional)

Source code and Markdown notes have independent size policies. Source files
default to 2 MiB and can be raised for repositories with unusually large,
legitimate files:

```toml
[indexing]
max_source_file_bytes = 8388608 # 8 MiB
```

The accepted range is 1 KiB through a fixed 64 MiB safety ceiling; invalid
values fail config loading rather than being clamped. Markdown notes retain
their separate 1 MiB policy. Every policy skip is reported by `nestweaver
index`; use `--json` for a typed terminal result or `--fail-on-skip` when CI
must reject degraded coverage.

#### Trigram pre-filter

`regex-search` uses a trigram posting table when one exists and falls back to a
full scan otherwise. Set `with_trigrams` so indexing keeps that table fresh
instead of relying on the flag being remembered on every run:

```toml
[indexing]
with_trigrams = true
```

`with_trigrams` is the master switch for whether trigram acceleration is
maintained at all. **It no longer means "refresh during indexing."** Maintenance
is owned by a reconcile loop in the daemon; see below.

Defaults to `false`, preserving the opt-in storage cost. Precedence for one-off
runs:

| Invocation | Result |
| --- | --- |
| `index` with no `--config` | inherits the DAEMON's setting |
| `index --config <path>` | that config decides |
| `index --with-trigrams` | refreshes, whatever any config says |
| `index --no-trigrams` | skips, even when a config enables it |
| `index --rebuild-trigrams` | full rebuild; implies a refresh |
| `index --no-trigrams --rebuild-trigrams` | rejected — contradictory |

The first row matters: the policy is carried over the daemon RPC as a
three-state value (inherit / on / off), not a bool. A bool could not
distinguish "the caller said no" from "the caller said nothing", so a configless
`index` against a daemon configured with `with_trigrams = true` used to send
`false` and the daemon discarded its own configuration. The MCP index path
inherits for the same reason — it has no flags with which to state a policy.

A refresh failure never fails an otherwise successful index: a stale posting
table is detected and bypassed at query time, so the cost is a slower
`regex-search`, not a wrong one.

##### Who refreshes trigrams

The daemon runs a **reconcile loop** that drains pending trigram work on an
interval. Writers do not refresh; they only enqueue.

```toml
[indexing]
with_trigrams = true
trigram_reconcile_interval = "30s"   # default; "0" disables the loop
```

| Field | Default | Description |
|-------|---------|-------------|
| `trigram_reconcile_interval` | `"30s"` | How often the daemon drains pending trigram work. Humanized duration (`"30s"`, `"5m"`); a bare `"0"` disables the loop. Gated on `with_trigrams`. An unparseable value fails config loading rather than silently disabling maintenance. |

This is why the loop exists rather than each writer refreshing. Invalidation was
already universal and transactional — the store's own write path marks a scope
dirty inside the mutating transaction, so no caller can forget. What was missing
was anything that *drained* that queue: the refresh ran only as a side effect of
two write handlers, so the vault-refresh path and both file watchers enqueued
work nobody picked up, and their scopes stayed dirty until an unrelated repo
index happened to run. That is a transactional outbox with no relay, and a
level-triggered design invoked edge-triggered.

Consequences worth knowing:

- **An index no longer refreshes trigrams implicitly.** Explicit intent still
  wins: `index --with-trigrams` and `--rebuild-trigrams` refresh inline, which
  is what CI and scripted reindex want. `--no-trigrams` still forces none.
- **The direct (`--local` / `--no-daemon`) path still refreshes inline**, because
  no daemon exists there to drain for it.
- **The file watchers need no trigram handling at all.** They mutate, the
  mutation enqueues, the loop drains. Refreshing per watcher batch would run a
  punishing duty cycle: refresh cost is dominated by fixed overhead (measured
  ~918ms for 2 changed nodes) while the watcher debounces into ~2s batches.
- Inspect the current state with `regex-search --json`, which reports
  `ready_scopes`, `dirty_scopes`, `error_scopes`, `stale_index` and
  `scanned_fallback`.

## Manifest Parsing (automatic)

When you run `nestweaver index`, NestWeaver automatically reads the manifest
file at the root of each repo and extracts the package name (what the repo
publishes) and its dependencies (what it consumes). This data powers the
high-confidence layer of `suggest-links`.

### Supported formats

**package.json** (npm / Node.js):
```json
{
  "name": "@myorg/api-client",
  "dependencies": { "@myorg/shared-types": "^2.0.0", "axios": "^1.6.0" },
  "devDependencies": { "jest": "^29.0.0" }
}
```
Extracts: `name` as package identity, keys from `dependencies`,
`devDependencies`, and `peerDependencies` as dependency list.

**go.mod** (Go modules):
```
module github.com/myorg/api-service

require (
    github.com/myorg/shared-lib v1.2.0
    github.com/gin-gonic/gin v1.9.1
)
```
Extracts: `module` as package identity, `require` entries as dependencies.

**Cargo.toml** (Rust / Cargo):
```toml
[package]
name = "my-service"

[dependencies]
my-shared-types = { path = "../shared-types" }
serde = "1"
```
Extracts: `[package] name` as identity, keys from `[dependencies]`,
`[dev-dependencies]`, and `[build-dependencies]`.

**pyproject.toml** (Python PEP 517+):
```toml
[project]
name = "my-data-pipeline"
dependencies = ["myorg-shared-models>=1.0.0", "fastapi>=0.104.0"]
```
Extracts: `[project] name` as identity, `[project] dependencies` entries
(package name parsed before version specifier).

**requirements.txt** (Python legacy):
```
myorg-shared-models>=1.0.0
fastapi>=0.104.0
```
Extracts: dependency names only (no package identity — requirements.txt
doesn't declare what the repo publishes).

### How it works

The parsed data is saved as a JSON sidecar (`<db>.manifests.json`) and
loaded by `suggest-links`. When repo A's dependency list includes repo B's
package name, a high-confidence `package-dependency` link is suggested.

See `examples/manifests/` for complete examples of each format.

## How cross-repo relationship detection works

`suggest-links` uses three detection layers in order of confidence:

1. **Manifest dependencies (high confidence)** — If repo A's manifest declares
   a dependency on repo B's package name, that is a direct, authoritative link.
   These are emitted first and marked `high` confidence.

2. **Instance config `[[links]]` (authoritative)** — You can declare protocol-level
   relationships that can't be auto-detected: HTTP API endpoints, BLE UUIDs,
   shared databases, message queues, etc. These are the relationships that have
   no structural representation in source code, so they must be stated explicitly.

3. **IDF-filtered shared symbol names (low confidence)** — Symbols with the same
   name across repos may indicate a shared contract. Noise names (e.g., `get`,
   `main`, `config`), common framework patterns (e.g., `ButtonProps`,
   `useContext`), and names that appear in too many repos (IDF threshold) are
   filtered out. At least two non-noise shared symbols with sufficient specificity
   are required before a link is suggested.

Use `[[links]]` in your instance config for relationships that manifest parsing
and name matching cannot detect — primarily protocol-level connections like HTTP
API routes, BLE UUIDs, WebSocket channels, and shared databases.

## Bootstrapping with suggest-links

Don't know how your repos relate? After indexing, NestWeaver can analyze
the graph and suggest relationships:

```sh
# Index all your repos into one database
nestweaver index --repo ./frontend --db ./all.lbug
nestweaver index --repo ./backend --db ./all.lbug
nestweaver index --repo ./mobile-app --db ./all.lbug

# Ask NestWeaver to suggest links and features
nestweaver suggest-links --db ./all.lbug
```

Output:

```toml
# Suggested links (review and add to your instance config)

# Manifest dependency detected (high confidence)
[[links]]
from = "frontend"
to = "backend"
type = "package-dependency"
description = "Depends on @myorg/backend (from manifest)"
# Confidence: high

# Shared symbol names detected (low confidence)
[[links]]
from = "frontend"
to = "shared-types"
type = "shared-types"
description = "Both repos reference: UserProfile, Session, AuthToken"
# Confidence: medium (3 shared symbols)

# Suggested features (review and add to your instance config)

[[features]]
name = "userprofile"
description = "Shared functionality between frontend and shared-types"
repos = ["frontend", "shared-types"]
entry_points = ["UserProfile", "Session", "AuthToken"]
```

Review the suggestions, edit as needed, and paste into your config.

## Viewing config

```sh
# List declared links
nestweaver list-links --config ./nestweaver-instance.toml

# List declared features
nestweaver list-features --config ./nestweaver-instance.toml
```

## Storage engine tuning (advanced)

Write-capable database opens use a hardened engine configuration. The defaults
are chosen for safety, not throughput, and **the thread bound is a correctness
setting, not a performance one**.

| Variable | Default | Effect |
| --- | --- | --- |
| `NESTWEAVER_LBUG_MAX_THREADS` | `1` | Engine thread-pool size (`0` = library auto). |
| `NESTWEAVER_LBUG_BUFFER_POOL_BYTES` | auto | Buffer pool size in bytes. A larger pool avoids eviction when the working set fits. |
| `NESTWEAVER_LBUG_AUTO_CHECKPOINT` | on | Set `0`/`false`/`off` to defer automatic checkpoints. |

> **Raising `NESTWEAVER_LBUG_MAX_THREADS` re-opens a known crash window.**
> The storage engine's optimistic page read has no reader pinning on native
> builds, so an eviction racing a concurrent read is an unguarded SIGSEGV. The
> race requires concurrency inside the engine; capping the pool at `1` is what
> removes it, and `1` is the only value that fully eliminates it. This crash is
> what began a reported incident in which a vault went from 752 notes to 1 —
> recovered only because the write-ahead log had not yet checkpointed.
>
> Read-only opens keep full parallelism and are unaffected. The measured cost
> of the cap on the write path was negligible (index throughput 55 s vs 58 s;
> bulk load is already single-threaded), so raise it only if you have measured
> a real query-latency cost on your own workload, and accept the crash risk
> knowingly.

Deferring auto-checkpoints reduces exposure to a separate upstream defect: the
string-corruption trigger needs several checkpoint-separated segments, so
deferring during a bulk load makes it less likely. Corruption is detected and
reported rather than silently returned in either case.

## Daemon, watcher, and MCP server coexistence

All write operations route through the daemon process, which holds the single
write connection to the database. LadybugDB (the storage engine) supports
single-writer, multiple-reader (SWMR) access — the daemon writes while MCP
servers and CLI commands read concurrently via read-only connections.

**How it works:**

1. The daemon auto-starts when any CLI write command or MCP server needs it.
   It holds the write lock for its lifetime and exits after 1 hour of idle.
2. The file-watcher (`nestweaver brain watch`) runs inside the daemon process,
   sharing the daemon's write connection for incremental updates. Watching does
   not require an instance config — without one, the daemon falls back to a
   built-in denylist of unsafe roots. `nestweaver watch --force` replaces an
   already-running watcher (e.g. one orphaned by a killed `watch` CLI) instead
   of failing; a direct (non-daemon) watcher is only used when no daemon is
   running, and if it can't take the DB write lock the error tells you to stop
   the daemon first.
3. MCP servers open the database read-only and route write operations
   (like `brain_add_source`) through the daemon's gRPC service.
4. CLI read commands open the database read-only. CLI write commands
   (index, brain add, materialize-projects, etc.) send RPCs to the daemon.
5. Multiple MCP servers, CLI commands, and IDE integrations can share the
   same database concurrently without lock contention.

**Recommended workflow:**

```sh
# The daemon starts automatically — no manual setup needed.
# Just use the CLI or MCP server; the daemon manages itself.
nestweaver mcp --db ./brain.lbug
```

No extra configuration is required — the coexistence is handled automatically.

## Full example

Here's a complete config for a multi-repo project with a web client,
mobile app, backend service, and IoT firmware:

```toml
instance_id = "acme-platform"

[snapshot_storage]
backend = "local"
path = "~/.local/share/nestweaver/acme/snapshots"

[workspace]
backend = "local"
path = "~/.local/share/nestweaver/acme/workspace"

[inference]
endpoint = "http://localhost:11434"
embedding_model = "nomic-embed-text"
summary_model = "qwen2.5-coder:7b"

[git]
credential_method = "gh"

# Repositories
[[repos]]
url = "https://github.com/acme/web-client"

[[repos]]
url = "https://github.com/acme/mobile-app"

[[repos]]
url = "https://github.com/acme/api-service"

[[repos]]
url = "https://github.com/acme/device-firmware"

# How repos communicate
[[links]]
from = "web-client"
to = "api-service"
type = "http-api"
description = "React web app calls REST API"
endpoints = ["/api/users", "/api/devices", "/api/sessions"]

[[links]]
from = "mobile-app"
to = "api-service"
type = "http-api"
description = "React Native app calls REST API"
endpoints = ["/api/users", "/api/devices", "/api/sessions"]

[[links]]
from = "mobile-app"
to = "device-firmware"
type = "ble"
description = "App connects to device via BLE"
identifiers = ["6E400001-B5A3-F393-E0A9-E50E24DCCA9E"]

[[links]]
from = "api-service"
to = "device-firmware"
type = "http-api"
description = "Backend pushes OTA updates to devices"
endpoints = ["/api/firmware/update"]

# Cross-cutting features
[[features]]
name = "device-onboarding"
description = "Device discovery, BLE pairing, cloud registration, initial config"
repos = ["mobile-app", "device-firmware", "api-service"]
entry_points = ["BLEScanner", "pairDevice", "registerDevice", "initConfig"]

[[features]]
name = "user-auth"
description = "Login, signup, OAuth, session management"
repos = ["web-client", "mobile-app", "api-service"]
entry_points = ["LoginForm", "useAuth", "authMiddleware", "refreshToken"]

[[features]]
name = "data-sync"
description = "Real-time data sync from device to cloud to clients"
repos = ["device-firmware", "api-service", "web-client", "mobile-app"]
entry_points = ["sendReading", "ingestData", "useRealtimeData", "DataStream"]

[[features]]
name = "ota-updates"
description = "Over-the-air firmware update pipeline"
repos = ["api-service", "device-firmware"]
entry_points = ["publishFirmware", "checkForUpdate", "applyUpdate"]
```

## Projects

Projects aggregate notes, symbols, and components into named units that span
repos and vaults. They enable unified retrieval via `project_context`.

```toml
[[projects]]
name = "device-onboarding"
description = "End-to-end device onboarding flow"
aliases = ["onboarding", "DO"]
vault_folder = "Projects/device-onboarding"
repos = ["mobile-app", "device-firmware", "api-service"]
features = ["device-onboarding"]
components = ["ble-scanner", "device-registry"]

[[projects]]
name = "platform"
description = "Top-level platform composite"
components = ["device-onboarding", "user-auth"]
```

### Project fields

| Field | Required | Description |
|-------|----------|-------------|
| `name` | yes | Project name (used for lookup and UID generation) |
| `description` | no | Human-readable description |
| `aliases` | no | Alternative names for this project (e.g., `["DO", "Device Onboarding"]`) |
| `vault_folder` | no | Vault folder whose notes belong to this project |
| `repos` | no | Repos whose symbols belong to this project |
| `features` | no | Feature bundles to include |
| `components` | no | Sub-projects (for composites) |
| `parent` | no | Parent project name |
| `tags` | no | Tags associated with this project |
| `wiki_sources` | no | External wiki content to ingest via MCP |
| `external_refs` | no | Links to external tools (Jira, Figma, etc.) |

### Implicit project detection

If your vault has a `Projects/<slug>/` folder containing a `<slug>.md` entry
note, NestWeaver detects it as an implicit project during vault indexing.

## Cross-Domain Configuration

Control how NestWeaver bridges notes to code symbols.

```toml
[cross_domain]
stoplist_extend = ["Platform", "Service", "Manager"]
min_symbol_name_length = 4
```

| Field | Default | Description |
|-------|---------|-------------|
| `stoplist_extend` | `[]` | Words to add to the built-in stoplist |
| `stoplist_replace` | `null` | If set, replaces the built-in stoplist entirely |
| `min_symbol_name_length` | `4` | Minimum symbol name length for bridging |

## MCP Servers (for wiki ingestion)

Declare external MCP servers that NestWeaver can call to fetch wiki content.

```toml
[[mcp_servers]]
name = "confluence"
command = "npx"
args = ["-y", "@anthropic/confluence-mcp"]

[mcp_servers.env]
CONFLUENCE_URL = "https://yoursite.atlassian.net"
CONFLUENCE_API_TOKEN = "your-token"
```

Projects reference MCP servers in their `wiki_sources`:

```toml
[[projects.wiki_sources]]
label = "Architecture Doc"
mcp_server = "confluence"
tool = "get_page_content"
args = { pageId = "12345" }
```

## Scheduled wiki refresh

`materialize-projects` only ingests wiki content when run explicitly. Wiki
pages edited by collaborators cause graph drift over time. To keep wiki
content fresh, set up a periodic refresh using your platform's scheduler.

### Option A: watcher flag (logs the interval, pair with cron/launchd)

The `--refresh-wiki-hours` flag on `watch` and `brain watch` records the
intended refresh cadence. Pair it with an external scheduler that runs
`materialize-projects` on the same interval:

```sh
nestweaver brain watch ~/notes --db ./brain.lbug \
  --refresh-wiki-hours 6 --config ./nestweaver-instance.toml
```

### Option B: cron (Linux)

```cron
# Re-fetch wiki sources every 6 hours
0 */6 * * * /usr/local/bin/nestweaver materialize-projects \
  --config /path/to/nestweaver-instance.toml \
  --db /path/to/main.lbug
```

### Option C: launchd (macOS)

Save as `~/Library/LaunchAgents/com.nestweaver.wiki-refresh.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.nestweaver.wiki-refresh</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/local/bin/nestweaver</string>
        <string>materialize-projects</string>
        <string>--config</string>
        <string>/path/to/nestweaver-instance.toml</string>
        <string>--db</string>
        <string>/path/to/main.lbug</string>
    </array>
    <key>StartInterval</key>
    <integer>21600</integer>
    <key>StandardOutPath</key>
    <string>/tmp/nestweaver-wiki-refresh.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/nestweaver-wiki-refresh.log</string>
</dict>
</plist>
```

Load it:

```sh
launchctl load ~/Library/LaunchAgents/com.nestweaver.wiki-refresh.plist
```

### Option D: systemd timer (Linux)

Create `/etc/systemd/user/nestweaver-wiki-refresh.service`:

```ini
[Unit]
Description=NestWeaver wiki source refresh

[Service]
Type=oneshot
ExecStart=/usr/local/bin/nestweaver materialize-projects \
  --config /path/to/nestweaver-instance.toml \
  --db /path/to/main.lbug
```

Create `/etc/systemd/user/nestweaver-wiki-refresh.timer`:

```ini
[Unit]
Description=Refresh NestWeaver wiki sources every 6 hours

[Timer]
OnBootSec=5min
OnUnitActiveSec=6h

[Install]
WantedBy=timers.target
```

Enable:

```sh
systemctl --user enable --now nestweaver-wiki-refresh.timer
```
