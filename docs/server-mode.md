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
| `--bind <addr>` | gRPC bind address (default: `0.0.0.0:9378`) |
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

### Instance config (nestweaver-instance.toml)

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

The server listens on three ports. Webhook and admin API endpoints are mounted as routes on the MCP HTTP server (:9379), not on separate ports.

```
┌─────────────────────────────────────────────────────────┐
│                   NestWeaver Server                       │
│                                                           │
│  :9378  gRPC    Query API (TCP + TLS)                     │
│  :9379  HTTP    MCP-over-HTTP (AI agents)                 │
│                  ├─ /webhook      (GitHub/GitLab push)    │
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

`daemon run --server` starts the gRPC (:9378) and MCP HTTP (:9379) listeners. The web UI (:9377) is started separately via `nestweaver ui` and is not part of the server container default.

| Port | Protocol | Auth | Purpose |
|------|----------|------|---------|
| 9378 | gRPC | Bearer token (TLS recommended) | Primary query API for CLI clients and local daemons |
| 9379 | HTTP | Bearer token / HMAC | MCP-over-HTTP for AI agents, plus `/webhook` (HMAC) and `/admin/api/*` (admin token) |

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
3. Restart the daemon, then update the secret on GitHub/GitLab
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

Bounded between `poll_min` (default 45s) and `poll_max` (default 8h). Active repos are polled frequently; dormant repos back off.

```toml
[server.indexing]
poll_min = "45s"    # minimum polling interval
poll_max = "8h"     # maximum polling interval
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

```bash
# One-time setup: register a server
nestweaver connect grpcs://nestweaver.internal:9378 --token "$NESTWEAVER_AUTH_TOKEN"

# Verify the connection
nestweaver brain status
# => server_mode: true, repo_count: 200, indexing_active: false
```

### Configuration hierarchy

The client discovers upstream servers from (highest priority first):

1. `NESTWEAVER_UPSTREAM` and `NESTWEAVER_TOKEN` environment variables
2. `.nestweaver/server.toml` in the current repo (checked into source)
3. `~/.nestweaver/server.toml` (user config)
4. `nestweaver-instance.toml` `[[upstream]]` section

### Repo-level config (.nestweaver/server.toml)

Check this file into your repo so every developer auto-connects:

```toml
[upstream]
url = "grpcs://nestweaver.internal:9378"
token = "${NESTWEAVER_TOKEN}"
mode = "fallback"
```

### Instance config (upstream section)

```toml
[[upstream]]
name = "team-server"
url = "grpcs://nestweaver.internal:9378"
token = "${NESTWEAVER_TOKEN}"
mode = "fallback"
ca_cert = "/path/to/ca.crt"   # optional: for self-signed certificates
```

When connecting to a server using self-signed TLS certificates (e.g., generated
by `nestweaver server init-tls`), set `ca_cert` to the path of the CA certificate
PEM file. This tells the client to trust the server's certificate even though it
is not signed by a public CA.

---

## Routing Modes

When a local daemon is connected to an upstream server, queries are routed based on the configured mode:

| Mode | Behavior | Best for |
|------|----------|----------|
| `fallback` | Query local first; if the repo isn't indexed locally, fall back to server | Default. Best for developers who work on a subset of repos |
| `merge` | Query both local and server; merge results with RRF (k=60) | Cross-repo search and analysis |
| `primary` | Query server first; local is fallback | CI environments, thin clients |

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
| `blast_radius`, `brain_impact` | two-tier | Show local impact + org-wide impact separately |
| `flow_trace` | continuation | Start local, continue across repos on server |
| `hub_nodes`, `clusters` | server-preferred | Structural analysis needs the full graph |

### Staleness detection

The client compares local `indexed_sha` against server `RepoStates` using `git ls-remote`. When local state is behind:
- `brain_status` shows `stale_repos` in the response
- Results include `_meta.sources` indicating which data sources contributed
- Fallback mode automatically routes stale repos to the server

---

## Impact Analysis

### Pre-push impact (local development)

Check what your uncommitted changes might break across the org:

```bash
nestweaver impact --local-changes
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
nestweaver pre-push-impact origin/main...HEAD --format json

# Post a formatted comment to the PR
nestweaver pre-push-impact --diff origin/main..HEAD --format json | nestweaver format-comment --input - | gh pr comment --body-file -
```

### Two-tier blast radius

When connected to a server, `blast_radius` returns two-tier results:

```json
{
  "local_impact": [...],
  "org_wide_impact": [...],
  "_meta": {
    "sources": ["local", "team-server"]
  }
}
```

---

## Backup and Restore

### Create a backup

```bash
# Save a snapshot of the current database
nestweaver backup save --destination /backups

# Output: /backups/brain-2026-06-25T10-30-00.nwsnap.zst
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
  --db ./brain.lbug

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
| `/metrics` | GET | Prometheus metrics (served on the web UI port when running `nestweaver ui`) |

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

---

## Docker Deployment

### docker-compose.yml

```yaml
version: "3.8"

services:
  nestweaver:
    image: ghcr.io/kehl-io/nestweaver:latest
    ports:
      - "9378:9378"   # gRPC
      - "9379:9379"   # MCP-over-HTTP + webhook + admin API
    volumes:
      - nestweaver-data:/data
      - ./nestweaver-instance.toml:/etc/nestweaver/instance.toml:ro
    environment:
      NESTWEAVER_AUTH_TOKEN: "${NESTWEAVER_AUTH_TOKEN}"
      NESTWEAVER_ADMIN_TOKEN: "${NESTWEAVER_ADMIN_TOKEN}"
      NESTWEAVER_WEBHOOK_SECRET: "${NESTWEAVER_WEBHOOK_SECRET}"
      GH_TOKEN: "${GH_TOKEN}"
    command: >
      daemon --db /data/brain.lbug
      --config /etc/nestweaver/instance.toml
      run --server --bind 0.0.0.0:9378

volumes:
  nestweaver-data:
```

```bash
# Start
docker compose up -d

# Check logs
docker compose logs -f nestweaver

# Backup
docker compose exec nestweaver nestweaver backup save --destination /data/backups
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
cat ~/.local/share/nestweaver/*/logs/daemon.log

# Start with verbose logging
RUST_LOG=nestweaver_daemon=debug nestweaver daemon --db ./brain.lbug run --server
```

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
curl http://localhost:9379/metrics | grep nestweaver_query

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
