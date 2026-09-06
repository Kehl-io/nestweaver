# Server Mode Guide

NestWeaver's server mode turns a single daemon into a centralized code intelligence server. It indexes repos for your team, serves queries over gRPC and MCP-over-HTTP, and gives every AI agent instant access to an org-wide code graph.

**Key benefits:**
- Eliminate cold starts — agents connect to a pre-built graph instead of indexing from scratch
- Cross-repo blast radius and impact analysis across the entire org
- One-command onboarding for new developers
- Webhook-driven re-indexing keeps the graph fresh within seconds of a push

---

## Starting the Server

### CLI flags

```bash
nestweaver daemon --db ./brain.lbug run \
  --config ./nestweaver-instance.toml \
  --server \
  --bind 0.0.0.0:9378 \
  --auth-token "$NESTWEAVER_AUTH_TOKEN" \
  --admin-token "$NESTWEAVER_ADMIN_TOKEN"
```

| Flag | Description |
|------|-------------|
| `--server` | Enable server mode (TCP listener, webhook endpoint, MCP-over-HTTP) |
| `--bind <addr>` | gRPC bind address (default: `127.0.0.1:9378`) |
| `--auth-token <token>` | Bearer token for query authentication |
| `--admin-token <token>` | Bearer token for admin API endpoints |
| `--tls-cert <path>` | PEM-encoded TLS certificate |
| `--tls-key <path>` | PEM-encoded TLS private key |
| `--webhook-secret <secret>` | HMAC secret for webhook signature verification |
| `--webhook-secret-old <secret>` | Previous secret, accepted during rotation |
| `--snapshot <dir>` | Boot as a **read-only snapshot replica**: materialize this snapshot directory into a private working copy and serve it read-only. Requires `--server`; write RPCs, background indexing and `/webhook` are disabled |
| `--acme-domain <domain>` | Auto-provision a publicly-trusted TLS cert at runtime via Let's Encrypt TLS-ALPN-01. Requires `--server` and the `acme` build feature. TLS-ALPN-01 validates on **port 443**, so bind such that `:443` reaches the daemon (e.g. `--bind 0.0.0.0:443`) |
| `--acme-email <email>` | ACME account contact — recommended, for expiry notices |
| `--acme-production` | Use the Let's Encrypt **production** directory. The default is **staging** (untrusted certs, high rate limits), so issuance can be debugged without a rate-limit ban. Pass this only once the staging flow works end to end |
| `--port-file <path>` | Write the actual bound port to this file — the way to read an ephemeral `--bind …:0` port programmatically |

`--acme-*` is a third TLS option alongside self-signed (`server init-tls`) and
bring-your-own (`--tls-cert`/`--tls-key`); see [TLS Setup](#tls-setup).

### Environment variables

All flags can be set via environment variables:

| Variable | Equivalent flag |
|----------|----------------|
| `NESTWEAVER_AUTH_TOKEN` | `--auth-token` |
| `NESTWEAVER_ADMIN_TOKEN` | `--admin-token` |
| `NESTWEAVER_WEBHOOK_SECRET` | `--webhook-secret` |
| `NESTWEAVER_WEBHOOK_SECRET_OLD` | `--webhook-secret-old` |
| `NESTWEAVER_BIND` | `--bind` |

### Instance config (instance.toml)

Server settings can also be declared in the instance config. Note that `bind`, `auth_token`,
`admin_token`, and `webhook_secret` are **not** config-file keys — they come from CLI flags
(`--bind`, `--auth-token`, `--admin-token`, `--webhook-secret`) or their corresponding
environment variables (see table above).

```toml
[server.indexing]
workers = 8
min_poll = "45s"
max_poll = "8h"
```

### Embedding backend and readiness

The daemon uses `[embedding]` from the instance config. Local device selection
has three exact policies:

| `accelerator` | Behavior |
|---------------|----------|
| `auto` | Metal in a Metal-enabled build; CPU only when Metal is not compiled. A Metal failure is reported; `auto` does not retry on CPU. |
| `metal` | Requires compiled Metal support plus successful device creation and full model inference. It never selects CPU. |
| `cpu` | Selects CPU directly and never probes Metal. |

An `external_endpoint` is authoritative and never switches to a local model
after a failure. Daemon startup is cache-only: it does not contact Hugging Face
or download missing local model files. Populate the configured cache while the
daemon is stopped with
`nestweaver embed --db <path> --local --model-id <id> --cache-dir <path>`.

On macOS, `daemon start` and client autostart register a launchd agent that owns
the foreground daemon process. An explicit `daemon run --server` remains in the
invoking foreground. Neither path forks or self-daemonizes. A bound socket is
not embedding readiness. The daemon publishes `state = "ready"` only after the
selected backend completes a real inference probe with a non-empty, finite
vector of the expected dimension. Non-semantic requests remain available while
embedding is loading or failed.

Inspect both binary capability and per-daemon runtime state:

```sh
nestweaver diagnostics capabilities --json
nestweaver daemon --db <path> status
nestweaver brain status --db <path> --json
```

Runtime status includes `state`, `backend`, `requested_device`,
`selected_device`, `model_id`, `error`, `metal_compiled`, and `fallback_used`,
plus the five `pass_*` fields documented under "Embedding pass progress" below.
For a ready local backend, `selected_device` is `metal` or `cpu` and
`fallback_used` remains `false`. A ready external backend has an empty
`selected_device` because it has no local device.

`state` is one of `disabled`, `loading`, `ready`, `failed`, or `embedding`.

### Embedding pass progress

`state` reports `embedding` — not `ready` — while a daemon-route embedding
pass is in flight. It is a strictly narrower `ready`: the model is loaded and
usable. Treat `embedding` as ready for "can this daemon answer semantic
queries"; read the boolean `pass_active` when you want an unambiguous machine
signal rather than a string match.

`embedding` is computed at read time and substitutes for `ready` only. A pass
running while the underlying state is `loading` or `failed` still reports that
underlying state, so `pass_active` — not the state string — is the reliable
"is a pass running" signal.

While a pass runs, `embedding_status` also carries `pass_active`,
`pass_processed`, `pass_total`, `pass_started_at` (unix seconds), and
`pass_scope`. `pass_total` is the eligible-node count from the same preflight
`PlanEmbed` reports; it is `0` until that preflight finishes, which is
reported as "total not yet counted" rather than as a fabricated percentage.
Throughput and ETA are derived client-side from `pass_started_at`. All of
these are proto3 scalars, so a daemon older than 4.2 decodes as "no pass
running" instead of failing.

`nestweaver brain status` renders the same numbers as a `Progress:` line, and
`nestweaver embed` polls this status to print a live counter on the daemon
route.

### Write-path visibility

`brain status` reports two different queues, and they are not
interchangeable:

- `queue_depth` — pending + running entries in the server-side **index job
  queue**. This is what the admin API and the `nestweaver_index_queue_depth`
  Prometheus gauge have always meant.
- `write_queue_depth` — **write RPCs blocked on the daemon write lock**, not
  counting the one holding it. A long `embed` holds that lock, so a queued
  `index` shows up here and never in `queue_depth`.

`write_holder` names what currently holds the write lock and
`write_holder_seconds` how long it has held it. Every writer in the daemon
process stamps it: RPC names for gRPC writers (`embed`, `index_repo`,
`remove_repo`, ...), `worker_commit` for the worker pool — the web admin
API's repo removal goes through the same `remove_repo` RPC, not a separate
name. An empty `write_holder` while `write_queue_depth` is non-zero means a
daemon older than 4.2, not an unidentifiable writer.

A CLI command blocked on the write lock prints a periodic stderr line naming
the holder and how long it has held it. That line is a **message, not a
timeout**: long-running write RPCs remain uncapped, and nothing in this path
cancels or shortens them.

---

## Network Architecture

The server listens on two ports: gRPC (:9378) and MCP-over-HTTP (:9379). The web UI (default :3000, :9377 in the macOS .app) is a separate optional process (`nestweaver ui`). Webhook and admin API endpoints are mounted as routes on the MCP HTTP server (:9379), not on separate ports.

```
┌─────────────────────────────────────────────────────────┐
│                   NestWeaver Server                       │
│                                                           │
│  :9378  gRPC    Query API (TCP + TLS)                     │
│  :9379  HTTP    MCP-over-HTTP (AI agents)                 │
│                  ├─ /webhook      (GitHub/GitLab/Gitea push) │
│                  └─ /admin/api/*  (repo & queue mgmt)     │
│                                                           │
│  ┌─────────────────────────────────────────────────┐      │
│  │            Daemon Core                           │      │
│  │  Single-writer + Index job queue                 │      │
│  │  LadybugDB + Tantivy + Embeddings                │      │
│  └─────────────────────────────────────────────────┘      │
└─────────────────────────────────────────────────────────┘
     ▲ gRPC         ▲ gRPC          ▲ MCP-over-HTTP
     │               │               │
  Dev A (local)   Dev B (local)   AI Agent (Claude/Cursor)
```

`daemon run --server` starts the gRPC (:9378) and MCP HTTP (:9379) listeners. The web UI (default :3000, :9377 in the macOS .app) is started separately via `nestweaver ui` and is not part of the server container default.

The MCP HTTP listener inherits the `--bind` IP and is **gRPC port + 1** for fixed binds. So `--bind 0.0.0.0:9378` exposes MCP-over-HTTP (with `/webhook`, `/admin/api/*`, and `/metrics`) on `0.0.0.0:9379` — relevant when publishing ports from Docker. Exception: with an ephemeral `--bind 127.0.0.1:0` the gRPC port is OS-assigned at runtime, so the MCP listener binds its own ephemeral port (`:0`) instead of gRPC + 1; read both actual ports from the daemon's port file or startup log.

The MCP endpoint is **`POST /mcp`** on the HTTP port.

NestWeaver's registry holds **42** tools. The number is derivable, not typed:
`all_tool_schemas_undecorated()` in `crates/nestweaver-mcp/src/tools.rs` is the
registry, and `tools::tool_doc_tests::all_tools_have_doc_categories` asserts the
documented table covers exactly `tool_list(false)["tools"].len()`. Read it back
with a `tools/list` call rather than trusting this paragraph.

| Transport | Tools advertised |
|---|---|
| Daemon-backed stdio, daemon proxy, hybrid, MCP-over-HTTP | 42 |
| Direct read-only mode | 36 — the registry minus the six `MUTATING_TOOLS` |
| `--lite` | 6 — `brain_context`, `brain_search`, `brain_impact`, `brain_status`, `brain_guide`, `detect_changes` |

**Validation** *is* identical on every transport: all of them enforce the
`--tools`/`--lite` allowlists, and tool schemas reject unknown argument names and
out-of-range numeric values (e.g. `token_budget` outside 1–16000, `depth` outside
1–15) instead of silently ignoring them. **Tool exposure is not** — direct
read-only mode drops the six mutating tools from both `tools/list` and dispatch.

Every tool schema carries MCP `annotations` — `readOnlyHint`,
`destructiveHint`, `idempotentHint`, `openWorldHint` — derived from the same
`MUTATING_TOOLS` table rather than hand-written, so a client can distinguish
`prune_stale` from `brain_status` on the wire without a local table.

| Port | Protocol | Auth | Purpose |
|------|----------|------|---------|
| 9378 | gRPC | Bearer token (TLS recommended) | Primary query API for CLI clients and local daemons |
| 9379 | HTTP | Bearer token / HMAC | MCP-over-HTTP for AI agents, plus `/webhook` (HMAC), `/admin/api/*` (admin token), and `/metrics` (Prometheus) |

---

## Authentication

### Query authentication (bearer token)

All gRPC and MCP-over-HTTP requests require a bearer token when `auth_token` is set:

```bash
# gRPC (automatic when using `nestweaver connect`)
nestweaver connect grpcs://nestweaver.internal:9378 --token "$NESTWEAVER_AUTH_TOKEN"

# MCP-over-HTTP (set in AI tool config)
# Authorization: Bearer <token>
```

The token is a shared secret — all team members use the same token. This is intentional: NestWeaver assumes everyone in the org has code read access. Fine-grained ACLs are an enterprise feature.

`--auth-token` and `--admin-token` (and their `NESTWEAVER_AUTH_TOKEN` / `NESTWEAVER_ADMIN_TOKEN` equivalents) must be **at least 32 bytes**. The daemon refuses to start if a supplied token is shorter — short tokens are trivially brute-forceable. Generate one with `openssl rand -hex 32` (or `head -c 32 /dev/urandom | base64`).

### Admin authentication

Admin API endpoints require a separate `admin_token`. This token grants access to:

- Repo management (add, remove, force reindex)
- Job queue management (drain, resume, clear dead-letter)
- Backup operations
- Server configuration
- **Six MCP tools.** A query token may only invoke read-only tools. The six
  entries of `MUTATING_TOOLS` (`crates/nestweaver-mcp/src/http.rs` — the single
  canonical list, which both the HTTP gate and the daemon's gRPC gate consult)
  require the admin token: `brain_add_source`, `brain_remove_source`,
  `brain_memory_consolidate`, `set_extension`, `prune_stale`,
  `compact_embeddings`. If an agent gets a 403 from `prune_stale` over
  MCP-over-HTTP while every other tool works, this is why.

```bash
curl -H "Authorization: Bearer $NESTWEAVER_ADMIN_TOKEN" \
  http://nestweaver.internal:9379/admin/api/repos
```

---

## TLS Setup

### Quick setup with init-tls

```bash
# Generate a self-signed certificate (development/internal use)
nestweaver server init-tls --output-dir ./tls

# Custom validity (1-36500 days; default 365)
nestweaver server init-tls --output-dir ./tls --validity-days 90

# Start with TLS
nestweaver daemon --db ./brain.lbug run \
  --server \
  --bind 0.0.0.0:9378 \
  --tls-cert ./tls/server.pem \
  --tls-key ./tls/server-key.pem \
  --auth-token "$NESTWEAVER_AUTH_TOKEN"
```

#### Re-running is key rotation, not initialization

`init-tls` **refuses** to touch a directory that already holds any of `ca.pem`,
`ca-key.pem`, `server.pem`, `server-key.pem`, `client.pem` or `client-key.pem`.
It exits **64** (`EX_USAGE`) and prints the exact invocation that would perform
the replacement. Nothing on disk changes.

Through 8.x it printed a warning and overwrote anyway: the CA private key was
gone, `client.pem` was left behind signed by the CA that no longer existed, and
the command exited 0 over a directory whose client certificate failed with
`unable to get local issuer certificate`.

```bash
# Rotate the CA and everything under it, in one staged install.
nestweaver server init-tls --output-dir ./tls --san localhost --client --force
```

`--force`:

- replaces the **whole** bundle. Any managed file the new bundle does not
  provide is retired with the CA that signed it — dropping `--client` removes
  `client.pem` and `client-key.pem` rather than leaving them unverifiable. The
  refusal says so before you run it;
- stages the complete new bundle (final modes, fsynced) before touching
  anything, then installs by `rename` only. Files are retired leaf-first and
  installed root-first, so at no instant does the directory hold a leaf
  certificate signed by a CA other than the `ca.pem` beside it;
- keeps the replaced bundle in `<output-dir>/.nestweaver-tls.backup/` (mode
  0700), so a rotation you did not mean to perform is recoverable. Exactly one
  generation is kept;
- takes an exclusive lock on `<output-dir>/.nestweaver-tls.lock`. A second
  concurrent `init-tls` stands down with an error rather than interleaving its
  writes into a split bundle;
- replaces a symlinked member with a regular file instead of writing through
  it.

An install interrupted part way through (a kill, a crash, a full disk) leaves a
`.nestweaver-tls.journal`; the next `init-tls` rolls the directory back to the
bundle that preceded it and says so on stderr.

### Manual certificate setup

Provide any PEM-encoded certificate and key:

```bash
nestweaver daemon --db ./brain.lbug run \
  --server \
  --bind 0.0.0.0:9378 \
  --tls-cert /etc/nestweaver/tls/server.pem \
  --tls-key /etc/nestweaver/tls/server-key.pem
```

When TLS is enabled:
- The gRPC listener (`:9378`) requires TLS — plain TCP connections are refused
- Clients connect with `grpcs://` (not `grpc://`)
- The UDS (Unix domain socket) listener remains unencrypted (it's process-isolated)

---

## Repo Configuration

Repos are declared in the instance config. The server clones them as blobless bare repos (~90% smaller than full clones) and indexes them automatically. Each `[[repos]]` entry requires `url`; `name` is an optional display alias, and `type` is optional (`code` is the default, `vault` indexes Markdown).

```toml
[[repos]]
url = "https://github.com/acme/api-service"
name = "api"             # optional display alias

[[repos]]
url = "https://github.com/acme/web-client"
branch = "main"          # optional: track a specific branch (default: default branch)
poll = "2m"              # optional: override adaptive polling interval

[[repos]]
url = "https://github.com/acme/shared-vault"
type = "vault"           # index as markdown vault, not code
```

### Vault repos

A repo declared with `type = "vault"` is indexed as a **markdown vault** rather than source code: the server parses every `.md` file into `Note` / `Section` / `Heading` nodes (plus a `Vault` node and `Tag` links) instead of code symbols. This is the same model `nestweaver` uses for an Obsidian-style knowledge vault, applied to a cloned repo. Use it for design-doc repos, runbooks, ADR archives, or any markdown knowledge base you want queryable alongside code.

Once indexed, vault notes are queryable from any connected client just like local notes:

- `brain_search` / `note_get` find and read notes by title or keyword
- `brain_context` seeds from a note title to pull related notes and code
- `backlinks` finds what links to a note

In server mode these tools route to the server (merge or fallback), so a developer with no local copy of the vault still gets its notes in results, tagged `"server"` in `_meta.sources`.

### Git credentials

Configure how the server authenticates to Git remotes:

```toml
[git]
credential_method = "gh"    # use `gh auth token` (GitHub CLI)
# credential_method = "env" # use GIT_ASKPASS or GH_TOKEN env var
# credential_method = "ssh" # use SSH keys
```

---

## Webhook Setup

Webhooks provide near-instant re-indexing when code is pushed. The server also runs adaptive polling as a fallback (GitHub webhooks have zero retries).

### GitHub

1. In your repo or org settings, go to **Webhooks > Add webhook**
2. Set the payload URL to `http://nestweaver.internal:9379/webhook`
3. Set content type to `application/json`
4. Set the secret to your `NESTWEAVER_WEBHOOK_SECRET`
5. Select "Just the push event"

### GitLab

1. Go to **Settings > Webhooks**
2. Set URL to `http://nestweaver.internal:9379/webhook`
3. Set the secret token
4. Check "Push events"

### Gitea

1. In your repository settings, go to **Webhooks > Add Webhook > Gitea**
2. Set the target URL to `http://nestweaver.internal:9379/webhook`
3. Set the secret to your `NESTWEAVER_WEBHOOK_SECRET`
4. Trigger on **Push events**

Gitea signs the payload with its own `X-Gitea-Signature` header, carrying a raw hex HMAC-SHA256 of the body — without the `sha256=` prefix that GitHub's `X-Hub-Signature-256` uses. The server accepts a request that bears *either* a valid GitHub *or* a valid Gitea signature (it tries both), using the same `--webhook-secret` / `--webhook-secret-old` rotation pair. The repo URL is read from `repository.clone_url`, just like GitHub.

### Webhook configuration

The webhook secret is set via CLI flag or environment variable (not the config file):

```bash
--webhook-secret "$NESTWEAVER_WEBHOOK_SECRET"
# or: export NESTWEAVER_WEBHOOK_SECRET=...
```

### Dual-secret rotation

NestWeaver supports two webhook secrets simultaneously for zero-downtime rotation:

1. Add `--webhook-secret-old "$CURRENT_SECRET"` (or set `NESTWEAVER_WEBHOOK_SECRET_OLD`)
2. Change `--webhook-secret` (or `NESTWEAVER_WEBHOOK_SECRET`) to the new secret
3. Restart the daemon, then update the secret on GitHub/GitLab/Gitea
4. Once all webhooks use the new secret, remove `--webhook-secret-old`

During rotation, the server checks the new secret first, falls back to the old secret, and logs a deprecation warning when the old secret matches.

```bash
# CLI flags
--webhook-secret "$NEW_SECRET" --webhook-secret-old "$OLD_SECRET"
```

---

## Adaptive Polling

The server automatically polls repos for changes using `git ls-remote`. The polling interval adapts based on repo activity:

```
base     = time_since_last_commit / 2
interval = clamp(base, floor, max_poll)
actual   = uniform in [interval / 2, interval * 1.5)   # jitter, anti-thundering-herd
```

Active repos are polled frequently; dormant repos back off. Source of truth:
`compute_interval` and `jittered` in `crates/nestweaver-engine/src/scheduler.rs`.

**The floor depends on webhook health**, which the earlier `min_poll` wording did
not say:

| Webhook state | Floor |
|---|---|
| healthy | **300s** (`WEBHOOK_HEALTHY_FLOOR`) — polling is only a safety net when pushes already arrive |
| unhealthy / not configured | `min_poll` (default 45s) |

So configuring `min_poll = "45s"` does **not** give you 45-second polling while
webhooks are working; you will see ~5 minutes as the floor, jittered.

```toml
[server.indexing]
min_poll = "45s"    # floor only when webhooks are unhealthy
max_poll = "8h"     # maximum polling interval
workers = 8         # concurrent indexing workers
```

Per-repo overrides:

```toml
[[repos]]
url = "https://github.com/acme/monorepo"
poll = "30s"        # fixed interval — bypasses the adaptive formula and both floors
```

`poll` also accepts `"never"` and `"manual"`, which disable scheduled polling for
that repo entirely (`PollOverride::Never | Manual`). Use them for a repo driven
solely by webhooks or by `POST /admin/api/repos/{id}/reindex`.

### Three-layer reindexing

1. **Webhooks** — near-instant (push to indexed in <60s)
2. **Adaptive polling** — catches missed webhooks, backs off for dormant repos
3. **Periodic full re-index** — whichever comes first:
   - `max(150, file_count * 0.5%)` incremental updates. The threshold is
     **proportional**, not a flat 150: a 100k-file monorepo needs 500, not 150
     (`ReindexTracker`, `crates/nestweaver-engine/src/scheduler.rs`).
   - a 7-day time backstop, stored as wall-clock so it survives a daemon restart
   - a 0.25% random spot-check per poll cycle

None of these detect a **resolver-generation** bump. A full re-index triggered by
any of the three does bring a repo up to the current generation, but nothing
schedules one because the generation changed — after upgrading NestWeaver, force
one: `POST /admin/api/repos/{id}/reindex`, or `nestweaver index --repo <path>
--force`.

---

## Client Connection

### Connect to a server

**Recommended — device flow (one command, no token to copy):**

```bash
# gh-style browser onboarding
nestweaver connect grpcs://nestweaver.internal:9378 --device
```

This runs the OAuth 2.0 Device Authorization Grant (RFC 8628): the client prints a short user code, opens your browser to the verification page, and waits while an admin approves the request. On approval the issued token is written to your client config automatically — no token needs to be shared out-of-band. `--device` is implied when you omit `--token`, so `nestweaver connect <url>` alone triggers the same flow.

**Alternative — explicit token:**

```bash
# One-time setup: register a server with a pre-shared bearer token
nestweaver connect grpcs://nestweaver.internal:9378 --token "$NESTWEAVER_AUTH_TOKEN"
```

**Verify either way:**

```bash
nestweaver brain status
# => server_mode: true, repo_count: 200, indexing_active: false
```

### Configuration hierarchy

The client discovers upstream servers from (highest priority first):

1. `NESTWEAVER_UPSTREAM` and `NESTWEAVER_TOKEN` environment variables
2. `.nestweaver/server.toml` in the current repo (checked into source)
3. `~/.config/nestweaver/upstreams.toml` (user config, written by `nestweaver connect`)
4. `instance.toml` `[[upstream]]` section

### Repo-level config (.nestweaver/server.toml)

Check this file into your repo so every developer auto-connects:

```toml
[upstream]
url = "grpcs://nestweaver.internal:9378"
token = "${NESTWEAVER_TOKEN}"
mode = "fallback"
```

### User-level config (~/.config/nestweaver/upstreams.toml)

`nestweaver connect` writes personal upstreams here:

```toml
[[upstream]]
name = "team-server"
url = "grpcs://nestweaver.internal:9378"
token = "..."
mode = "fallback"
```

### Instance config (upstream section)

```toml
[[upstream]]
name = "team-server"
url = "grpcs://nestweaver.internal:9378"
token = "${NESTWEAVER_TOKEN}"
mode = "fallback"
timeout = "1s"                # optional: upstream request ceiling (default 1s)
ca_cert = "/path/to/ca.crt"   # optional: for self-signed certificates
```

When connecting to a server using self-signed TLS certificates (e.g., generated
by `nestweaver server init-tls`), set `ca_cert` to the path of the CA certificate
PEM file. This tells the client to trust the server's certificate even though it
is not signed by a public CA.

---

## Routing Modes

When a local daemon is connected to an upstream server, each tool first gets a tool-specific routing category. The upstream mode can override merge/local-first/server-preferred categories, but local-only, two-tier, and combined tools keep their tool-specific behavior.

| Mode | Behavior | Best for |
|------|----------|----------|
| `fallback` | Prefer local-first routing where overridable; query the server when local results are missing, stale, or below the tool threshold | Default. Best for developers who work on a subset of repos |
| `merge` | Prefer parallel local + server routing where overridable; merge results with RRF (k=60) | Cross-repo search and analysis |
| `primary` | Prefer server-first routing where overridable; local remains fallback/overlay where supported | CI environments, thin clients |

```toml
[[upstream]]
name = "team-server"
url = "grpcs://nestweaver.internal:9378"
token = "${NESTWEAVER_TOKEN}"
mode = "fallback"    # fallback | merge | primary
```

### Per-tool routing

Different tools have different optimal routing:

| Tool category | Routing | Reason |
|--------------|---------|--------|
| `brain_search`, `brain_context` | merge | Combine local and org-wide results |
| `read_symbols`, `investigate` | local-first | Prefer exact local source spans, then server fallback |
| `blast_radius`, `brain_impact` | two-tier | Show local impact + org-wide impact separately |
| `flow_trace` | continuation | Start local, continue across repos on server |
| `hub_nodes`, `clusters` | server-preferred | Structural analysis needs the full graph |
| `brain_status`, `stale_check`, `brain_doc_stats` | combined | Preserve status/metadata shape while including both sources |
| `detect_changes`, memory/admin tools | local-only | These depend on local working-tree or personal state |

### `brain_search` count semantics

Every `brain_search` JSON response reports the display-independent match count
alongside the returned rows:

- `total_matches` counts distinct logical note/tag and symbol entities, not raw
  heading or section hits. A note and a symbol with the same title remain two
  entities.
- `total_matches_relation: "eq"` means `total_matches` is exact;
  `"gte"` means it is a safe lower bound because bounded counting or an
  incomplete source prevented an exact count.
- `returned_matches` is the number of rows in `results` after the requested
  display limit and any hybrid deduplication.
- `truncated` is true whenever the total is a lower bound or fewer rows were
  returned than the exact total.
- Every row, including `response_format: "concise"`, carries its canonical
  domain-qualified `uid`. Hybrid deduplication uses that identity rather than
  presentation fields such as title or location.
- `semantic_applied` is always `false` and `degraded_components` always empty.
  `brain_search` is keyword/BM25-only and never requests a semantic leg, so
  neither can it be degraded. Both are reported rather than omitted so a
  caller checking them can distinguish "no semantic leg" from "field not
  implemented on this path". The direct gRPC daemon response, both MCP paths,
  and the hybrid merge below all agree on this. In a merge, `semantic_applied`
  is the AND across contributing tiers (only claimed when every tier applied
  it) and `degraded_components` is their deduplicated union (a component
  degraded in either tier is degraded in the merged answer). A tier that omits
  `semantic_applied` counts as `false`, and if NO contributing tier reports
  either field the merge omits both rather than inventing them — so on a merged
  response, absence means "no tier reported", not "`false`".
  `degraded_components` has exactly one value in the current vocabulary,
  `"semantic"`. The structured (`connected`-schema) merge used by
  `brain_context` / `project_context` applies the same rules. Those tools do
  have a semantic leg, so a merge there can mix a semantically ranked tier with
  a lexical one; the AND then reports `false`, which is correct for the merged
  row set but does not say which tier was which.

For a hybrid merge, NestWeaver reports an exact union only when both local and
server responses have valid, internally consistent exact-count metadata and
every returned row has a unique canonical identity within its source. RRF can
then deduplicate the complete logical union by UID. If either source is
incomplete or any row is unkeyed, the merged total is a conservative lower
bound derived from trustworthy source totals and proven distinct UIDs, never
from the raw merged-row length. Source totals are not summed because local and
server data can overlap.

### Staleness detection

The client compares local `indexed_sha` against server `RepoStates` using `git ls-remote`. When local state is behind:
- `brain_status` shows `stale_repos` in the response
- Results include `_meta.sources` indicating which data sources contributed
- Fallback mode automatically routes stale repos to the server

### Upstream timeout

Each `[[upstream]]` entry takes an optional `timeout` key (default `1s`) that sets the **ceiling** for a single upstream request:

```toml
[[upstream]]
url = "grpcs://nestweaver.internal:9378"
timeout = "1s"    # ceiling; the live deadline is adaptive (see below)
```

The live per-query deadline is **adaptive and mode-aware**, not a fixed value. The client scales off a rolling EWMA of observed upstream latencies and clamps the result per routing mode:

- **`fallback`** keeps the deadline tight — capped at **200ms** (`FALLBACK_MODE_CAP` in `crates/nestweaver-federation/src/health.rs`, pinned by `effective_timeout_fallback_capped_at_200ms`) — so the local fast path is never blocked waiting on the server.
- **`merge`** and **`primary`** allow up to the configured `timeout` ceiling (default 1s) — the richer org-wide answer is the whole point of those modes.

On a cold start (no latency samples yet) the mode ceiling is used directly. Raising `timeout` only affects `merge`/`primary`; the fallback cap is fixed.

---

## Impact Analysis

### Pre-push impact (local development)

Check what your uncommitted changes might break across the org:

```bash
nestweaver pre-push-impact --local-changes
```

This sends symbol-level diffs to the server, which queries the org-wide graph and returns:

```
local_impact:
  - billing/webhook.rs::handleRefund (3 callers in this repo)

org_wide_impact:
  - notification-service: RefundNotifier depends on handleRefund
  - admin-dashboard: RefundTable renders handleRefund output
  - billing-worker: RefundProcessor calls handleRefund

compatibility: BREAKING (function signature changed)
```

### CI integration (diff-based)

```bash
# In CI: analyze a PR diff against the server
nestweaver pre-push-impact --diff origin/main..HEAD --format json

# Post a formatted comment to the PR
nestweaver pre-push-impact --diff origin/main..HEAD --format json \
  | nestweaver format-comment --input - \
  | gh pr comment --body-file -
```

### Two-tier blast radius

When connected to a server, `blast_radius` returns a two-tier envelope. Each
tier is an **object**, not an array: `local_impact` is the full local blast
radius result (with its own `changed_symbols`, `affected_symbols`, `coverage`,
etc.), and `org_wide_impact` wraps the server's result under `results` alongside
the `source_server` it came from. Provenance is carried in `_meta.sources`.

```json
{
  "tier": "two_tier",
  "local_impact": {
    "changed_symbols": [...],
    "affected_symbols": [...],
    "risk": "medium",
    "coverage": { "...": "..." }
  },
  "org_wide_impact": {
    "source_server": "server",
    "results": {
      "changed_symbols": [...],
      "affected_symbols": [...]
    }
  },
  "_meta": {
    "scope": "hybrid",
    "sources": ["local", "server"],
    "stale_repos": []
  }
}
```

`provenance()` always emits all three legs — see
`crates/nestweaver-schema/src/provenance.rs`. The `scope` vocabulary is `local`
(one local source), the single source's own name, or `hybrid` when more than one
contributed. The example above is the **CLI/hybrid** route's vocabulary; on the
daemon's own `/mcp` path `add_provenance_metadata` stamps `scope: "federated"`
with `sources: ["daemon", "<upstream>"]` when an upstream is healthy, and
`scope: "single-node"` with `sources: ["daemon"]` when not.

`_meta` also carries a `limits` leg when a safeguard clamped the request
(`add_limit_metadata` in `crates/nestweaver-mcp/src/http.rs`).

> ### Wire change in 9.0.0 — read this if you speak MCP-over-HTTP directly
>
> `_meta` used to be stamped on the **outer `tools/call` envelope** under
> `nestweaver.io/`-prefixed keys: `nestweaver.io/sources`, `nestweaver.io/scope`,
> `nestweaver.io/stale_repos`. It now lives on the **payload**, under the
> unprefixed key `_meta`, with the shape above. The prefixed envelope keys are
> gone.
>
> Clients that go through an MCP SDK are unaffected, since they read the tool
> result. A client that reached into the envelope for `nestweaver.io/sources`
> now reads `null` and will silently treat every federated answer as
> unattributed. Update to `payload._meta`.

When no healthy upstream is configured, the response degrades to
`tier: "local_only"` (the local result plus a `tier` marker, with no
`org_wide_impact`). When an upstream is configured but the org-wide query fails
or times out, `tier` stays `"two_tier"` and `org_wide_impact` becomes
`{ "source_server": ..., "status": "unavailable", "note": ... }`.

---

## PageRank Cold-Start

Impact, repo-map, and the UI overview rank symbols by PageRank ("CodeRank").
NestWeaver keeps that computation off the critical path so ranks are served
immediately in normal operation.

> **This section is about *latency*, not *correctness*.** Everything below
> describes how fast ranks are served, and assumes the underlying edges are the
> ones the current resolver would write. **9.0.0 bumps `RESOLVER_GENERATION`
> from 3 to 4** (`crates/nestweaver-engine/src/resolver_generation.rs`), so a
> graph indexed by an earlier release serves ranks *quickly* and *wrongly*: they
> are computed over edges written before `.h` files were dispatched to the C++
> grammar, before C/C++ `MEMBER_OF` edges existed at all, and before C++
> `#include` resolved to `IMPORTS`. Re-index every repo — `nestweaver index
> --repo <path> --force` — before trusting any ranking on this server.
>
> **`stale-check` detects this as of 9.0.0.** A generation-stale repo reports
> `status: "outdated_resolver"` with `resolver_stale: true` and
> `needs_reindex: true`, and the command exits 2. Through 8.x it did not: the
> ladder was `missing`/`incomplete`/SHA-behind-HEAD only and never read
> `<db>.resolver_generation.json`, so a generation-3 graph reported `ok` and
> exited 0. `hub_nodes`, `bridge_nodes`, `repo_map`, `ranking rank` and
> `get_summary` at hub level also disclose it, via the `rankings_stale` boolean
> and a top-level `stale_repos` array (distinct from `_meta.stale_repos`, which
> is federation staleness). `clusters`, `blast_radius`, `generate-guide`,
> PPR-backed context and the web UI still disclose nothing.

- **Computed at index time.** A full re-index (`index --force`) and every
  incremental update compute PageRank and persist it to `<db>.pagerank.json`.
  A normally-indexed DB therefore serves ranks straight from the sidecar with no
  compute on the first query.
- **Pre-warmed on server/UI startup (nw-029 T7).** When the daemon starts
  serving the UI it warms the rank cache up front, so the first impact request
  never pays a compute.
- **Lazy single-flight fallback.** A DB with no sidecar — an older DB, or one
  whose very first `index` fell back to a full pass without writing the sidecar
  — computes ranks once, on the first rank-consuming query. The compute is
  single-flight: concurrent first requests block on and share the one
  computation rather than each recomputing.
- **The impact UI degrades gracefully.** While ranks are computing it shows a
  loading state with a 30s timeout toast, and auto-refreshes when ranks land via
  the `pagerank:recomputed` SSE event.

**Measured cost (release build).** On a 4-repo, ~32.8k-symbol DB, the cold
lazy compute completed in ~0.19s of total wall time (process start + store open
+ compute), and a warm query served from the sidecar in ~0.08s. PageRank is
cheap; index time is dominated by parsing, not ranking. Note the bug report that
prompted this work measured ~7 minutes for the first impact query — that was a
**debug** build, which is roughly two orders of magnitude slower than release
and not representative of deployed behavior.

---

## Backup and Restore

### Create a backup

```bash
# Save a snapshot of the current database (written to the exact path you pass)
nestweaver backup save ./brain-backup.nwsnap.zst

# Output: ./brain-backup.nwsnap.zst
```

The backup process:
1. Quiesces writes (blocks new write transactions)
2. Runs SQLite CHECKPOINT
3. Copies the database + Tantivy index
4. Compresses into a `.nwsnap.zst` archive
5. Resumes writes

### Restore from backup

```bash
# Restore a snapshot
nestweaver backup restore /backups/brain-2026-06-25T10-30-00.nwsnap.zst \
  --data-dir ./brain.lbug

# All queries work immediately after restore
# Bare clones are re-fetched in the background
```

### Inspect a backup

```bash
nestweaver backup inspect /backups/brain-2026-06-25T10-30-00.nwsnap.zst
# => Repos: 200, Symbols: 480000, Uncompressed: 1.2 GB, Compressed: 210 MB,
#    Created: 2026-06-25T10:30:00Z
# (there is no separate notes count — symbol_count covers code; a vault's
# Note/Section/Heading nodes are not broken out in the manifest)
```

### Automatic backups

Backup configuration is planned for a future release. Use `nestweaver backup save` (CLI) for manual backups.

---

## Admin API

The admin API is mounted on the MCP HTTP server (`:9379`) under `/admin/api/` and requires the admin token.

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/admin/api/repos` | GET | List all indexed repos with status |
| `/admin/api/repos` | POST | Add a new repo to the index |
| `/admin/api/repos/{id}` | DELETE | Remove a repo from the index |
| `/admin/api/repos/{id}/reindex` | POST | Force reindex of a specific repo |
| `/admin/api/reload` | POST | Reload server configuration |
| `/admin/api/queue` | GET | View the indexing job queue |
| `/admin/api/drain` | POST | Pause job processing |
| `/admin/api/resume` | POST | Resume job processing |
| `/admin/api/drain/status` | GET | Check drain state |
| `/admin/api/dead-letter` | GET | View failed jobs |
| `/admin/api/dead-letter/{id}/retry` | POST | Retry a failed job |
| `/admin/api/dead-letter/{id}` | DELETE | Dismiss a failed job |
| `/admin/api/status` | GET | Server status — what `nestweaver server status` reads |
| `/admin/api/metrics` | GET | Prometheus metrics, admin-token-gated (the same series as the unauthenticated `/metrics`, so a single scrape target works on either port) |

`GET /admin` issues a permanent redirect to `/admin/api/status`.

The OAuth 2.0 Device Authorization Grant (RFC 8628) flow is mounted at `/auth`
on the same listener. Route these three if NestWeaver is behind a reverse proxy:

| Endpoint | Method | Auth | Purpose |
|----------|--------|------|---------|
| `/auth/device` | POST | none (per-IP rate limited) | Start the device flow; returns a user code |
| `/auth/token` | POST | none (per-IP rate limited) | Poll for the issued token |
| `/auth/device/approve` | POST | admin token | Operator approves a pending device |
| `/metrics` | GET | Prometheus metrics (served by the daemon on the MCP HTTP port `:9379`; requires a valid bearer token when `auth_token` is configured — the query token or the admin token both work, it is not admin-only; open only on unauthenticated loopback dev binds) |

```bash
# List repos
curl -H "Authorization: Bearer $NESTWEAVER_ADMIN_TOKEN" \
  http://localhost:9379/admin/api/repos

# Force reindex (use the repo UID from GET /admin/api/repos)
curl -X POST -H "Authorization: Bearer $NESTWEAVER_ADMIN_TOKEN" \
  http://localhost:9379/admin/api/repos/myinstance%3A%3Ahttps%3A%2F%2Fgithub.com%2Facme%2Fapi-service/reindex

# Drain the queue (pause indexing)
curl -X POST -H "Authorization: Bearer $NESTWEAVER_ADMIN_TOKEN" \
  http://localhost:9379/admin/api/drain
```

### Checking server status

`nestweaver server status` queries a running server's `GET /admin/api/status` endpoint and prints a concise, human-readable summary — no curl/jq needed:

```bash
nestweaver server status --url http://nestweaver.internal:9379 --token "$NESTWEAVER_ADMIN_TOKEN"
```

- `--url` is the admin/MCP HTTP base URL (the gRPC port + 1, e.g. `:9379`).
- `--token` is the admin bearer token; it defaults to the `NESTWEAVER_ADMIN_TOKEN` environment variable, so you can omit the flag when that is set.

The output reports the instance ID, version, mode (`server`/`daemon`, plus `(drained)` when paused), repos indexed, total symbols, queue depth, indexing state (`active`/`idle`), and active read/write counts.

---

## Docker Deployment

### docker-compose.yml

A non-loopback bind (`0.0.0.0`) is rejected at startup without TLS
(`validate_bind_security`), so the compose file provisions self-signed certs via
a one-shot `init-tls` service before the server starts. For production, swap this
for your own PKI or terminate TLS at a reverse proxy and bind the daemon to
`127.0.0.1`.

```yaml
services:
  # One-shot: generate self-signed TLS certs into the shared volume. Idempotent —
  # only generates when absent, so restarts reuse the same cert.
  init-tls:
    build: .
    entrypoint: ["/bin/sh", "-c"]
    command:
      - |
        if [ ! -f /data/nestweaver/tls/server.pem ]; then
          nestweaver server init-tls \
            --output-dir /data/nestweaver/tls \
            --san localhost --san nestweaver --san 127.0.0.1
        fi
    volumes:
      - nestweaver-data:/data/nestweaver

  nestweaver:
    build: .
    # image: ghcr.io/kehl-io/nestweaver:latest  # when published
    depends_on:
      init-tls:
        condition: service_completed_successfully
    ports:
      - "9378:9378"   # gRPC
      - "9379:9379"   # MCP-over-HTTP + webhook + admin API
    volumes:
      - ./instance.toml:/etc/nestweaver/instance.toml:ro
      - nestweaver-data:/data/nestweaver
    environment:
      - NESTWEAVER_AUTH_TOKEN=${NESTWEAVER_AUTH_TOKEN}
      - NESTWEAVER_ADMIN_TOKEN=${NESTWEAVER_ADMIN_TOKEN}
      - NESTWEAVER_WEBHOOK_SECRET=${NESTWEAVER_WEBHOOK_SECRET}
    command:
      [
        "daemon", "run",
        "--server",
        "--bind", "0.0.0.0:9378",
        "--tls-cert", "/data/nestweaver/tls/server.pem",
        "--tls-key", "/data/nestweaver/tls/server-key.pem",
        "--db", "/data/nestweaver/brain.lbug",
        "--config", "/etc/nestweaver/instance.toml"
      ]
    restart: unless-stopped
    # Must exceed the drain ceiling (NESTWEAVER_DRAIN_TIMEOUT_SECS, default
    # 660s) or `docker compose stop` SIGKILLs the daemon mid-write, and should
    # exceed `daemon stop`'s own ceiling + 30s window so the kill never lands
    # while the CLI is still reporting. Docker enforces this deadline; the
    # daemon cannot extend it. See docs/guide/daemon-shutdown.md.
    stop_grace_period: 720s

volumes:
  nestweaver-data:
```

```bash
# Start
docker compose up -d

# Check logs
docker compose logs -f nestweaver

# Backup
docker compose exec nestweaver nestweaver backup save /data/backups/brain-backup.nwsnap.zst
```

### Resource requirements

| Scale | Repos | CPU | RAM | Disk |
|-------|-------|-----|-----|------|
| Small | 1-20 | 2 cores | 2 GB | 10 GB |
| Medium | 20-100 | 4 cores | 8 GB | 50 GB |
| Large | 100-500 | 8 cores | 16 GB | 200 GB |

Disk is primarily blobless bare clones (~90% smaller than full clones) plus the LadybugDB graph and Tantivy index.

---

## Troubleshooting

### Server won't start

```bash
# Check if the port is already in use
lsof -i :9378

# Check daemon logs
cat ~/.local/state/nestweaver/*/daemon.log

# Start with verbose logging
RUST_LOG=nestweaver_daemon=debug nestweaver daemon --db ./brain.lbug run --server
```

> In server mode a **malformed `--config`** is fatal: the daemon prints the TOML
> parse error to stderr and exits rather than silently starting with no repos
> and no webhook secret. A *missing* config file is non-fatal (built-in defaults).
> If the server exits immediately after "failed to parse --config", fix the TOML.

### Client can't connect

```bash
# Test gRPC connectivity
grpcurl -plaintext nestweaver.internal:9378 list

# Check auth token
nestweaver connect grpcs://nestweaver.internal:9378 --token "$TOKEN"
nestweaver brain status   # should show server_mode: true

# If using TLS, verify the certificate
openssl s_client -connect nestweaver.internal:9378 </dev/null
```

### Webhooks not triggering reindex

```bash
# Check webhook delivery in GitHub (Settings > Webhooks > Recent Deliveries)
# Verify the HMAC secret matches
# Check the webhook endpoint is reachable
curl -v http://nestweaver.internal:9379/webhook

# Check the admin queue for failed jobs
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
  http://localhost:9379/admin/api/dead-letter
```

### Embedding or semantic retrieval unavailable

First inspect `nestweaver diagnostics capabilities --json` and
`nestweaver daemon --db <path> status`.

| Observation | Meaning | Corrective action |
|-------------|---------|-------------------|
| `metal_compiled = false` | This binary does not contain the Metal backend. With `auto`, CPU is selected; explicit `metal` fails. | Install a Metal-enabled macOS release archive or rebuild with `cargo install --locked --path . --features metal`. |
| `selected_device = ""` | Expected for an external backend. For a local backend it means state is not ready. | Check `backend`, `state`, and `error`; do not infer a device until local state is `ready`. |
| Error reports a missing model cache artifact | The cache-only daemon found no required model file in the configured cache. | Stop the daemon, run `nestweaver embed --db <path> --local --model-id <id> --cache-dir <path>` with the same model/cache, then restart with `--config <path>` and recheck status. |
| External endpoint readiness or request failure | The configured external backend is unavailable; no local model is attempted. | Restore the endpoint/credentials. To switch local, remove the external config, stop the daemon, direct-local re-embed with `--force`, then restart with the same `--config`. |
| Response contains `"semantic"` in `degraded_components` | Semantic retrieval was requested but the model was not ready, inference failed, or the DB has no embeddings. | Inspect embedding status and populate/fix the model or embeddings. Graph, PPR, and BM25 results remain available; `semantic_applied` is `false`. |

For an explicit external-to-local switch:

```sh
CONFIG=/absolute/path/to/nestweaver-instance.toml
DB=/absolute/path/to/brain.lbug
MODEL=sentence-transformers/all-MiniLM-L6-v2
CACHE="$HOME/.cache/nestweaver/models"
# First remove external_endpoint/external_model from "$CONFIG".
nestweaver daemon --db "$DB" stop
nestweaver embed --db "$DB" --local --model-id "$MODEL" --cache-dir "$CACHE" --force
nestweaver daemon --db "$DB" start --config "$CONFIG"
nestweaver brain status --db "$DB" --json
```

### High query latency

```bash
# Check Prometheus metrics
# The daemon serves /metrics on the MCP HTTP port (gRPC port + 1):
curl http://localhost:9379/metrics | grep nestweaver_query
# The same metrics are also exposed on :9377 when the web UI is running.

# Expected p95 latencies (LAN):
#   brain_search:    <50ms
#   brain_context:   <50ms
#   blast_radius:    <2s
#   flow_trace:      <2s

# If latency is high:
# 1. Check if indexing is active (steals CPU)
nestweaver brain status   # indexing_active field
# 2. Check worker count vs CPU cores
# 3. Check if Tantivy index needs rebuilding
nestweaver brain reindex-search
```

### Stale index

```bash
# Check which repos are behind HEAD.
# Exit: 0 = all fresh · 2 = at least one needs a re-index (behind HEAD,
# incomplete, missing, OR built by an older resolver generation) · 1 = the check
# itself failed · 64 = bad usage. Gate on 2, never on 1 — those demand opposite
# responses.
nestweaver brain stale-check

# As of 9.0.0 stale-check also reads <db>.resolver_generation.json: a graph
# built by an older NestWeaver reports status "outdated_resolver" and exits 2.
# To name just that subset (the remedy needs --force; plain `index` is
# incremental and a no-op on a repo already at HEAD):
nestweaver stale-check --json | jq '{any_needs_reindex, resolver_stale_repos}'

# Force reindex a specific repo (use UID from GET /admin/api/repos)
REPO_UID=$(curl -s -H "Authorization: Bearer $ADMIN_TOKEN" \
  http://localhost:9379/admin/api/repos | jq -r '.[0].id')
curl -X POST -H "Authorization: Bearer $ADMIN_TOKEN" \
  "http://localhost:9379/admin/api/repos/${REPO_UID}/reindex"

# Check indexed repos
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
  http://localhost:9379/admin/api/repos | jq '.[].name'
```
