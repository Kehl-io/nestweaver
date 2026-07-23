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

### Environment variables

All flags can be set via environment variables:

| Variable | Equivalent flag |
|----------|----------------|
| `NESTWEAVER_AUTH_TOKEN` | `--auth-token` |
| `NESTWEAVER_ADMIN_TOKEN` | `--admin-token` |
| `NESTWEAVER_WEBHOOK_SECRET` | `--webhook-secret` |
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

The MCP HTTP listener is always **gRPC port + 1** and inherits the `--bind` IP. So `--bind 0.0.0.0:9378` exposes MCP-over-HTTP (with `/webhook`, `/admin/api/*`, and `/metrics`) on `0.0.0.0:9379` — relevant when publishing ports from Docker.

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

# Start with TLS
nestweaver daemon --db ./brain.lbug run \
  --server \
  --bind 0.0.0.0:9378 \
  --tls-cert ./tls/server.pem \
  --tls-key ./tls/server-key.pem \
  --auth-token "$NESTWEAVER_AUTH_TOKEN"
```

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

Repos are declared in the instance config. The server clones them as blobless bare repos (~90% smaller than full clones) and indexes them automatically.

```toml
[[repos]]
url = "https://github.com/acme/api-service"

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
interval = time_since_last_commit / 2
```

Bounded between `min_poll` (default 45s) and `max_poll` (default 8h). Active repos are polled frequently; dormant repos back off.

```toml
[server.indexing]
min_poll = "45s"    # minimum polling interval
max_poll = "8h"     # maximum polling interval
workers = 8         # concurrent indexing workers
```

Per-repo overrides:

```toml
[[repos]]
url = "https://github.com/acme/monorepo"
poll = "30s"    # high-traffic repo: poll aggressively
```

### Three-layer reindexing

1. **Webhooks** — near-instant (push to indexed in <60s)
2. **Adaptive polling** — catches missed webhooks, backs off for dormant repos
3. **Periodic full re-index** — after 150 incremental updates, 7 days, or 0.25% random spot-check

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

For a hybrid merge, NestWeaver reports an exact union only when both local and
server responses are exact and complete. RRF can then deduplicate the complete
union and use its length. If either source is incomplete, the merged total is
`gte max(local total, server total, merged rows)`; source totals are never
summed because local and server data can overlap.

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

- **`fallback`** keeps the deadline tight (capped at ~250ms) so the local fast path is never blocked waiting on the server.
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
    "sources": ["local", "server"]
  }
}
```

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
# => repos: 200, notes: 5000, size: 1.2 GB, created: 2026-06-25T10:30:00Z
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
| `/metrics` | GET | Prometheus metrics (served by the daemon on the MCP HTTP port `:9379`; no admin token required) |

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
    stop_grace_period: 30s

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
# Check which repos are behind
nestweaver brain stale-check

# Force reindex a specific repo (use UID from GET /admin/api/repos)
REPO_UID=$(curl -s -H "Authorization: Bearer $ADMIN_TOKEN" \
  http://localhost:9379/admin/api/repos | jq -r '.[0].id')
curl -X POST -H "Authorization: Bearer $ADMIN_TOKEN" \
  "http://localhost:9379/admin/api/repos/${REPO_UID}/reindex"

# Check indexed repos
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
  http://localhost:9379/admin/api/repos | jq '.[].name'
```
