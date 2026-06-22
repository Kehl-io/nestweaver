# Instance Config Guide

An instance config tells NestWeaver which repos to index, how they relate
to each other, and what cross-cutting features span them.

## Quick Start

Create a file (e.g., `nestweaver-instance.toml`):

```toml
instance_id = "my-project"

[snapshot_storage]
backend = "local"
path = "~/.local/share/nestweaver/my-project/snapshots"

[workspace]
backend = "local"
path = "~/.local/share/nestweaver/my-project/workspace"

[inference]
endpoint = "http://localhost:11434"
embedding_model = "nomic-embed-text"
summary_model = "qwen2.5-coder:7b"

[git]
credential_method = "gh"

[[repos]]
url = "https://github.com/myorg/frontend"

[[repos]]
url = "https://github.com/myorg/backend"
```

Then:

```sh
# Index each repo
nestweaver index --repo ./frontend --db ./my-project.lbug
nestweaver index --repo ./backend --db ./my-project.lbug

# Query across both
nestweaver search "UserService" --db ./my-project.lbug
nestweaver context UserService --db ./my-project.lbug
```

## Config Reference

### Required sections

#### `instance_id`

A unique name for this instance. Used in UIDs and registry.

```toml
instance_id = "my-project"
```

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

#### `[inference]`

LLM endpoint for generating summaries and embeddings. Must be set
explicitly — there is no global default. This prevents routing one
instance's source to another instance's model.

```toml
[inference]
endpoint = "http://localhost:11434"   # Ollama, vLLM, or any OpenAI-compatible API
embedding_model = "nomic-embed-text"
summary_model = "qwen2.5-coder:7b"
```

#### `[embedding]`

Controls the local embedding model and hybrid retrieval weights. The embedding
layer enables natural language queries by finding semantically similar symbols,
notes, and headings as seeds for the graph walk.

```toml
[embedding]
model_id = "sentence-transformers/all-MiniLM-L6-v2"  # any BERT-compatible HuggingFace model
cache_dir = "~/.cache/nestweaver/models"

# Optional: use an external API instead of the local model (falls back to local on failure)
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
| `model_id` | `"sentence-transformers/all-MiniLM-L6-v2"` | HuggingFace model ID for local embeddings |
| `cache_dir` | `"~/.cache/nestweaver/models"` | Directory to cache downloaded model weights |
| `external_endpoint` | — | Optional external embedding API endpoint (falls back to local on failure) |
| `external_model` | — | Model name for the external endpoint |
| `weight_ppr` | `0.40` | Fusion weight for graph structure (Personalized PageRank) |
| `weight_bm25` | `0.25` | Fusion weight for BM25 text match |
| `weight_semantic` | `0.35` | Fusion weight for embedding similarity |
| `always_blend_semantic` | `true` | Add semantic matches to PPR seeds even when name resolution finds results |
| `semantic_seed_limit` | `5` | Top-k semantic hits injected as PPR seeds |
| `semantic_search_limit` | `200` | Top-k semantic hits fed into fusion scoring |

#### `[git]`

How to authenticate git operations.

```toml
[git]
credential_method = "gh"   # "gh" | "ssh" | "credential-helper"
```

#### `[[repos]]`

List of repositories to index. Each entry needs at minimum a URL.

```toml
[[repos]]
url = "https://github.com/myorg/frontend"

[[repos]]
url = "https://github.com/myorg/backend"
sparse = false        # optional: disable sparse checkout (default: true)
pin_sha = "abc123"    # optional: pin to a specific commit
```

### Optional sections

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

## Daemon, watcher, and MCP server coexistence

All write operations route through the daemon process, which holds the single
write connection to the database. LadybugDB (the storage engine) supports
single-writer, multiple-reader (SWMR) access — the daemon writes while MCP
servers and CLI commands read concurrently via read-only connections.

**How it works:**

1. The daemon auto-starts when any CLI write command or MCP server needs it.
   It holds the write lock for its lifetime and exits after 1 hour of idle.
2. The file-watcher (`nestweaver brain watch`) runs inside the daemon process,
   sharing the daemon's write connection for incremental updates.
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

`materialize-instance` only ingests wiki content when run explicitly. Wiki
pages edited by collaborators cause graph drift over time. To keep wiki
content fresh, set up a periodic refresh using your platform's scheduler.

### Option A: watcher flag (logs the interval, pair with cron/launchd)

The `--refresh-wiki-hours` flag on `watch` and `brain watch` records the
intended refresh cadence. Pair it with an external scheduler that runs
`materialize-instance` on the same interval:

```sh
nestweaver brain watch ~/notes --db ./brain.lbug \
  --refresh-wiki-hours 6 --config ./nestweaver-instance.toml
```

### Option B: cron (Linux)

```cron
# Re-fetch wiki sources every 6 hours
0 */6 * * * /usr/local/bin/nestweaver-materialize-instance \
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
        <string>/usr/local/bin/nestweaver-materialize-instance</string>
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
ExecStart=/usr/local/bin/nestweaver-materialize-instance \
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
