# Cross-Repo Links, Feature Bundles, and Config Bootstrapping — Design Spec

**Date:** 2026-05-22
**Status:** Implemented

---

## Overview

Extend NestWeaver with instance-level cross-repo linking, feature bundles,
and a suggest-links command that helps agents build the config automatically.

**Problem:** Repos that communicate via HTTP APIs, BLE, shared databases, or
other protocols have no structural edges in the graph. An agent working on a
feature that spans 3 repos has no way to discover the other repos are involved.

**Solution:** Three additions:
1. `[[links]]` in instance config — declares protocol-level relationships that can't be auto-detected
2. `[[features]]` in instance config — declares cross-cutting feature bundles
3. `suggest-links` command — analyzes indexed repos and proposes config using manifest deps and IDF-filtered name matching

---

## 1. Instance Config Extensions

### Link declarations

```toml
[[links]]
from = "mobile-app"
to = "api-service"
type = "http-api"
description = "React Native app calls backend REST API"
endpoints = ["/api/sessions", "/api/devices", "/api/events"]

[[links]]
from = "mobile-app"
to = "device-firmware"
type = "ble"
description = "App connects to IoT device via BLE UART"
identifiers = ["6E400001-B5A3-F393-E0A9-E50E24DCCA9E"]
```

#### LinkConfig struct

```rust
#[derive(Debug, Deserialize, Clone)]
pub struct LinkConfig {
    pub from: String,
    pub to: String,
    #[serde(rename = "type")]
    pub link_type: String,
    pub description: Option<String>,
    pub endpoints: Option<Vec<String>>,
    pub identifiers: Option<Vec<String>>,
    pub contract: Option<String>,
}
```

Supported link types (open enum, any string accepted):
- `http-api` — REST/HTTP communication
- `grpc` — gRPC/protobuf
- `graphql` — GraphQL
- `ble` — Bluetooth Low Energy
- `websocket` — WebSocket
- `shared-db` — shared database
- `event-bus` — message queue / pub-sub
- `shared-types` — shared type package
- Custom strings allowed

### Feature declarations

```toml
[[features]]
name = "device-pairing"
description = "IoT device discovery, BLE pairing, device registration"
repos = ["mobile-app", "device-firmware", "api-service"]
entry_points = ["BLEProvider", "connectToDevice", "flash_device"]

[[features]]
name = "session-sync"
description = "Recording sessions from device to cloud"
repos = ["mobile-app", "api-service"]
entry_points = ["useCurrentSession", "syncSessions", "syncDataToFirestore"]
```

#### FeatureConfig struct

```rust
#[derive(Debug, Deserialize, Clone)]
pub struct FeatureConfig {
    pub name: String,
    pub description: Option<String>,
    pub repos: Vec<String>,
    pub entry_points: Vec<String>,
}
```

### InstanceConfig changes

Add to existing struct:

```rust
pub struct InstanceConfig {
    // ... existing fields ...
    pub links: Option<Vec<LinkConfig>>,
    pub features: Option<Vec<FeatureConfig>>,
}
```

---

## 2. Repo name matching

Links and features reference repos by short name (e.g., "mobile-app").
The short name is derived from the repo URL: the last path segment with
`.git` stripped — the same logic as `repo_name_from_url()`.

When resolving a link, match the short name against indexed repos:
```
"mobile-app" matches "file:///Users/.../project/mobile-app"
"api-service" matches "file:///Users/.../project/api-service"
```

If ambiguous (multiple repos match), require a longer path prefix.

---

## 3. CLI Commands

### `nestweaver context --feature <name>`

1. Load instance config (from `--config <path>` flag or registry)
2. Look up feature by name
3. Resolve all `entry_points` as seeds (search each across all repos in the feature)
4. Run Personalized PageRank from those seeds
5. Output: seeds, connected symbols, links between the feature's repos, feature description

Output format adds a `feature` section:

```
Feature: device-pairing
  IoT device discovery, BLE pairing, device registration
  Repos: mobile-app, device-firmware, api-service

Links:
  mobile-app → device-firmware (ble): App connects to IoT device via BLE UART
  mobile-app → api-service (http-api): React Native app calls backend REST API

Seeds (3 resolved):
  BLEProvider  Function  src/context/ble-context.js:86
  connectToDevice  Function  src/context/ble-context.js:349
  flash_device  Function  upload.py:13

Connected (N symbols, ranked by relevance):
  ...
```

### `nestweaver list-links`

Print all declared links from the instance config:

```
mobile-app → api-service
  Type: http-api
  Description: React Native app calls backend REST API
  Endpoints: /api/sessions, /api/devices, /api/events

mobile-app → device-firmware
  Type: ble
  Description: App connects to IoT device via BLE UART
  Identifiers: 6E400001-B5A3-F393-E0A9-E50E24DCCA9E
```

### `nestweaver list-features`

Print all declared features:

```
device-pairing
  IoT device discovery, BLE pairing, device registration
  Repos: mobile-app, device-firmware, api-service
  Entry points: BLEProvider, connectToDevice, flash_device

session-sync
  Recording sessions from device to cloud
  Repos: mobile-app, api-service
  Entry points: useCurrentSession, syncSessions, syncDataToFirestore
```

### `nestweaver suggest-links --db <path>`

Analyzes the indexed graph and proposes `[[links]]` and `[[features]]` config.

#### Detection signals:

1. **Manifest dependencies (high confidence)** — if repo A's manifest
   (`package.json`, `go.mod`, `Cargo.toml`, `pyproject.toml`, `requirements.txt`)
   declares a dependency on repo B's package name, that is a direct, authoritative
   link. Manifest data is stored in a JSON sidecar (`<db>.manifests.json`) written
   during `nestweaver index`.

2. **IDF-filtered shared symbol names (low confidence)** — symbol names that
   appear in multiple repos but pass noise, framework-pattern, and IDF filters.
   Requires at least two specific shared names to suggest a link. See section 5
   for the full algorithm.

#### Output:

Prints suggested TOML that the user can review and paste into their config.
Manifest-based links appear first:

```
# Suggested links (review and add to your instance config)

# Manifest dependency detected
[[links]]
from = "mobile-app"
to = "api-service"
type = "package-dependency"
description = "Depends on @myorg/api-service (from manifest)"
# Confidence: high

# Shared symbol names detected
[[links]]
from = "mobile-app"
to = "admin-dashboard"
type = "shared-types"
description = "Both repos reference: syncFirestoreToMongo, SessionModel, UserProfile"
# Confidence: medium (3 shared symbols)

# Suggested features (review and add to your instance config)

[[features]]
name = "syncfirerestoretomongo"
description = "Shared functionality between mobile-app and admin-dashboard"
repos = ["mobile-app", "admin-dashboard"]
entry_points = ["syncFirestoreToMongo", "SessionModel", "UserProfile"]
```

---

## 4. Implementation

### Files modified

| File | Change |
|---|---|
| `crates/nestweaver-engine/src/config.rs` | Added `LinkConfig`, `FeatureConfig` to `InstanceConfig` |
| `crates/nestweaver-engine/src/manifest.rs` | New file: manifest parsing (package.json, go.mod, Cargo.toml, pyproject.toml, requirements.txt) + sidecar load/save |
| `crates/nestweaver-engine/src/query.rs` | Added `build_feature_context()` |
| `crates/nestweaver-engine/src/suggest.rs` | New file: `suggest_links()` with manifest + IDF name signals |
| `src/main.rs` | Added `--feature` flag, `--config` flag, `ListLinks`, `ListFeatures`, `SuggestLinks` commands |

### No store/schema changes

Links and features are config-driven metadata. They don't create new node types
or edge types in the graph. The `context --feature` command resolves entry_points
to seeds and runs PPR — same as `context` with explicit seeds.

### Config loading

Add a `--config <path>` flag to commands that need link/feature data:
- `context --feature` needs config to find the feature definition
- `list-links` / `list-features` need config
- `suggest-links` only needs the DB (no config required — it generates the config)

If `--config` is not provided, check the registry for the instance config path.
If no registry entry, skip link/feature enrichment silently.

---

## 5. `suggest-links` Algorithm

Two signals are evaluated in order of reliability. Manifest links are emitted
first so the most reliable suggestions appear at the top.

### Signal 1 — Manifest dependencies (high confidence)

```
1. Load the manifest sidecar (<db>.manifests.json)
2. For each ordered pair of repos (A, B):
   a. If B has a package_name AND A's dependency list contains B's package_name
      (or ends with "/<package_name>" for Go-style scoped paths):
      → suggest a "package-dependency" link from A to B, confidence: high
```

Manifest data is populated by `nestweaver index` from `package.json`,
`go.mod`, `Cargo.toml`, `pyproject.toml`, or `requirements.txt`.

### Signal 2 — IDF-filtered shared symbol names (low confidence)

```
1. For each repo, load all symbol names into a set
2. Build a name→repos inverted index across all repos
3. Compute IDF threshold: min(30% of repo count, 3)
4. For each unordered pair of repos (A, B):
   a. Compute the intersection of their symbol name sets
   b. Filter out:
      - Noise names: get, set, main, init, config, error, … (50 entries)
      - Common framework patterns: ButtonProps, useContext, AuthProvider, … (suffix + exact list)
      - Names appearing in more than idf_max repos (IDF filter)
      - Short non-specific names (len ≤ 6 with no camelCase)
   c. Require at least one "high-specificity" symbol:
      - len >= 15, OR
      - contains 2+ uppercase letters or underscores (camelCase/snake_case)
   d. If |filtered_intersection| >= 2 AND has_specific:
      - confidence: high if >= 5 shared, medium if >= 3, low if >= 2
      → suggest a "shared-types" link
5. For any link with >= 3 shared symbols, also suggest a feature bundle
   with the longest shared symbol name as the feature name seed and the
   top 5 shared symbols as entry_points
```

---

## 6. Exit codes

Same as existing: 0 success, 1 error, 2 not found (feature/link not in config).
