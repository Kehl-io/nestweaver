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
