# Gaps Endpoint Performance Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the N+1 `callers_of` fan-out in the gaps endpoint with a single batch query and generation-gated cache, bringing response time from 2.1s to <1ms (warm) / ~50ms (cold).

**Architecture:** Add `tested_service_uids()` batch query to `GraphStore`, wrap gaps computation in a `GapsCache` that lazily computes on first read after each `graph_generation` bump, pre-serializes the JSON response, and serves from cache on subsequent reads.

**Tech Stack:** Rust, LadybugDB/KuzuDB (openCypher), axum, serde_json, `std::sync::RwLock`

**Spec:** `docs/superpowers/specs/2026-07-12-gaps-performance-design.md`

---

### Task 1: Add `tested_service_uids()` batch query to GraphStore

**Files:**
- Modify: `crates/nestweaver-store/src/read.rs` (append new method)
- Test: `crates/nestweaver-web/tests/api_test.rs`

- [ ] **Step 1: Write the failing test**

Add to `crates/nestweaver-web/tests/api_test.rs`, after the existing `gaps_returns_report_structure` test:

```rust
#[tokio::test]
async fn tested_service_uids_returns_services_with_test_callers() {
    let store = GraphStore::in_memory().unwrap();

    // Create a repo
    let repo = Repo {
        uid: "repo:test".to_string(),
        url: "https://example.com/test.git".to_string(),
        indexed_sha: "abc123".to_string(),
        staleness_commits_behind: 0,
        instance_id: String::new(),
        name: None,
        root_path: None,
    };
    store.insert_repo(&repo).unwrap();

    // Create a service
    let svc = Service {
        uid: "svc:test:routes".to_string(),
        name: "src/routes".to_string(),
        repo_uid: "repo:test".to_string(),
        summary: None,
        summary_hash: None,
        embedding: None,
    };
    store.insert_service(&svc).unwrap();

    // Create a symbol in the service
    let handler = Symbol {
        uid: "sym:test:handler".to_string(),
        name: "handler".to_string(),
        kind: SymbolKind::Function,
        repo_uid: "repo:test".to_string(),
        file_path: "src/routes/handler.ts".to_string(),
        start_line: 1,
        end_line: 10,
        signature: "function handler()".to_string(),
        summary: None,
        content_hash: "h1".to_string(),
        embedding: None,
        pagerank_score: None,
        is_entry_point: false,
        entry_point_kind: None,
        visibility: Visibility::Inferred,
        type_info: None,
        framework_hint: None,
        canonical_id: None,
    };
    store.insert_symbol(&handler).unwrap();
    store.insert_service_symbol_edge(&svc.uid, &handler.uid).unwrap();

    // Create a test symbol that calls the handler
    let test_sym = Symbol {
        uid: "sym:test:test_handler".to_string(),
        name: "test_handler".to_string(),
        kind: SymbolKind::Function,
        repo_uid: "repo:test".to_string(),
        file_path: "tests/routes/handler.test.ts".to_string(),
        start_line: 1,
        end_line: 5,
        signature: "function test_handler()".to_string(),
        summary: None,
        content_hash: "h2".to_string(),
        embedding: None,
        pagerank_score: None,
        is_entry_point: false,
        entry_point_kind: None,
        visibility: Visibility::Inferred,
        type_info: None,
        framework_hint: None,
        canonical_id: None,
    };
    store.insert_symbol(&test_sym).unwrap();
    store.insert_edge(&ResolvedEdge {
        source_uid: test_sym.uid.clone(),
        target_uid: handler.uid.clone(),
        edge_type: EdgeType::Calls,
        confidence: 1.0,
        link_type: None,
        evidence: vec![],
    }).unwrap();

    // Create an untested service
    let svc2 = Service {
        uid: "svc:test:models".to_string(),
        name: "src/models".to_string(),
        repo_uid: "repo:test".to_string(),
        summary: None,
        summary_hash: None,
        embedding: None,
    };
    store.insert_service(&svc2).unwrap();

    let model_sym = Symbol {
        uid: "sym:test:model".to_string(),
        name: "Model".to_string(),
        kind: SymbolKind::Class,
        repo_uid: "repo:test".to_string(),
        file_path: "src/models/model.ts".to_string(),
        start_line: 1,
        end_line: 20,
        signature: "class Model".to_string(),
        summary: None,
        content_hash: "h3".to_string(),
        embedding: None,
        pagerank_score: None,
        is_entry_point: false,
        entry_point_kind: None,
        visibility: Visibility::Inferred,
        type_info: None,
        framework_hint: None,
        canonical_id: None,
    };
    store.insert_symbol(&model_sym).unwrap();
    store.insert_service_symbol_edge(&svc2.uid, &model_sym.uid).unwrap();

    // tested_service_uids should return only the tested service
    let tested = store.tested_service_uids().unwrap();
    assert!(tested.contains("svc:test:routes"), "routes service has a test caller");
    assert!(!tested.contains("svc:test:models"), "models service has no test caller");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd crates/nestweaver-web && cargo test tested_service_uids -- --nocapture 2>&1 | tail -10`
Expected: FAIL — `tested_service_uids` method does not exist.

- [ ] **Step 3: Write the implementation**

Add to `crates/nestweaver-store/src/read.rs`, after the `callers_of` method (~line 497):

```rust
    /// Returns the set of service UIDs that have at least one caller whose
    /// `file_path` contains "test" or "spec" (case-insensitive). Used by the
    /// gaps endpoint to compute the untested-services set in a single query
    /// instead of N individual `callers_of` calls.
    pub fn tested_service_uids(&self) -> Result<HashSet<String>, StoreError> {
        let conn = self.conn()?;
        let q = "MATCH (caller:Symbol)-[:CALLS]->(callee:Symbol)\
                 <-[:SERVICE_HAS_SYMBOL]-(svc:Service) \
                 RETURN svc.uid, caller.file_path";
        let result = conn
            .query(q)
            .map_err(|e| StoreError::Query(e.to_string()))?;

        let mut tested = HashSet::new();
        for row in result {
            let svc_uid = extract_string(&row, 0)?;
            let file_path = extract_string(&row, 1)?;
            let lc = file_path.to_lowercase();
            if lc.contains("test") || lc.contains("spec") {
                tested.insert(svc_uid);
            }
        }
        Ok(tested)
    }
```

Add `use std::collections::HashSet;` at the top of `read.rs` if not already present.

- [ ] **Step 4: Run test to verify it passes**

Run: `cd crates/nestweaver-web && cargo test tested_service_uids -- --nocapture 2>&1 | tail -10`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/nestweaver-store/src/read.rs crates/nestweaver-web/tests/api_test.rs
git commit -m "feat(store): add tested_service_uids batch query for gaps endpoint"
```

---

### Task 2: Create `GapsCache` module

**Files:**
- Create: `crates/nestweaver-web/src/gaps_cache.rs`
- Modify: `crates/nestweaver-web/src/lib.rs` (add `pub mod gaps_cache;`)

- [ ] **Step 1: Create the GapsCache module**

Create `crates/nestweaver-web/src/gaps_cache.rs`:

```rust
use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

use nestweaver_store::GraphStore;
use serde_json::json;

use crate::error::ApiError;

struct CachedGaps {
    generation: u64,
    response: serde_json::Value,
}

pub struct GapsCache {
    cached: RwLock<Option<CachedGaps>>,
}

impl GapsCache {
    pub fn new() -> Self {
        Self {
            cached: RwLock::new(None),
        }
    }

    pub fn get_or_compute(&self, store: &GraphStore) -> Result<serde_json::Value, ApiError> {
        let current_gen = store.graph_generation();

        // Fast path: read lock, check generation
        {
            let guard = self.cached.read().unwrap_or_else(|e| e.into_inner());
            if let Some(cached) = guard.as_ref() {
                if cached.generation == current_gen {
                    return Ok(cached.response.clone());
                }
            }
        }

        // Slow path: write lock, double-check, compute
        let mut guard = self.cached.write().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = guard.as_ref() {
            if cached.generation == current_gen {
                return Ok(cached.response.clone());
            }
        }

        let response = Self::compute(store)?;
        *guard = Some(CachedGaps {
            generation: current_gen,
            response: response.clone(),
        });
        Ok(response)
    }

    fn compute(store: &GraphStore) -> Result<serde_json::Value, ApiError> {
        // --- undocumented ---
        let refs_count = store.count_references_code_edges()?;
        let undocumented = if refs_count == 0 {
            let repos = nestweaver_engine::list_repos(store, None)?;
            let mut module_counts: HashMap<String, usize> = HashMap::new();

            for repo in &repos {
                if let Ok(symbols) = store.lookup_symbols_by_repo(&repo.uid) {
                    for sym in &symbols {
                        let module = sym
                            .file_path
                            .split('/')
                            .next()
                            .unwrap_or(&sym.file_path)
                            .to_string();
                        *module_counts.entry(module).or_insert(0) += 1;
                    }
                }
            }

            let mut modules: Vec<serde_json::Value> = module_counts
                .into_iter()
                .map(|(module, count)| {
                    json!({
                        "module": module,
                        "symbol_count": count,
                    })
                })
                .collect();
            modules.sort_by(|a, b| {
                a["module"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b["module"].as_str().unwrap_or(""))
            });
            modules
        } else {
            Vec::new()
        };

        // --- untested ---
        let services = nestweaver_engine::list_services(store, None)?;
        let all_uids: HashSet<String> = services.iter().map(|s| s.uid.clone()).collect();
        let tested = store.tested_service_uids()?;
        let mut untested: Vec<String> = all_uids.difference(&tested).cloned().collect();
        untested.sort();

        // --- disconnected_pairs ---
        let disconnected_pairs: Vec<serde_json::Value> = Vec::new();

        Ok(json!({
            "undocumented": undocumented,
            "untested": untested,
            "disconnected_pairs": disconnected_pairs,
        }))
    }
}
```

- [ ] **Step 2: Register the module**

Add to `crates/nestweaver-web/src/lib.rs`, after the existing module declarations:

```rust
pub mod gaps_cache;
```

- [ ] **Step 3: Verify it compiles**

Run: `cd crates/nestweaver-web && cargo check 2>&1 | tail -5`
Expected: no errors

- [ ] **Step 4: Commit**

```bash
git add crates/nestweaver-web/src/gaps_cache.rs crates/nestweaver-web/src/lib.rs
git commit -m "feat(web): add GapsCache with generation-gated lazy materialization"
```

---

### Task 3: Wire GapsCache into AppState and endpoint

**Files:**
- Modify: `crates/nestweaver-web/src/state.rs` (add `gaps_cache` field)
- Modify: `crates/nestweaver-web/src/routes/gaps.rs` (simplify handler)

- [ ] **Step 1: Add `gaps_cache` to AppState**

In `crates/nestweaver-web/src/state.rs`:

Add import at top:
```rust
use crate::gaps_cache::GapsCache;
```

Add field to `AppState` struct (after `bridge_scores`):
```rust
    pub gaps_cache: GapsCache,
```

Add initialization in all three constructors (`new`, `new_with_store`, `new_with_arc_tantivy`):
```rust
    gaps_cache: GapsCache::new(),
```

- [ ] **Step 2: Simplify the gaps handler**

Replace the entire contents of `crates/nestweaver-web/src/routes/gaps.rs` with:

```rust
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};

use crate::error::ApiError;
use crate::state::AppState;

pub async fn gaps(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let result = state.gaps_cache.get_or_compute(&state.store)?;
    Ok(Json(result).into_response())
}
```

- [ ] **Step 3: Verify compilation**

Run: `cd crates/nestweaver-web && cargo check 2>&1 | tail -5`
Expected: no errors

- [ ] **Step 4: Run existing gaps test**

Run: `cd crates/nestweaver-web && cargo test gaps_returns_report_structure -- --nocapture 2>&1 | tail -10`
Expected: PASS — response shape is identical.

- [ ] **Step 5: Commit**

```bash
git add crates/nestweaver-web/src/state.rs crates/nestweaver-web/src/routes/gaps.rs
git commit -m "feat(web): wire GapsCache into AppState and gaps endpoint"
```

---

### Task 4: Add cache behavior test

**Files:**
- Modify: `crates/nestweaver-web/tests/api_test.rs`

- [ ] **Step 1: Write cache hit test**

Add to `crates/nestweaver-web/tests/api_test.rs`:

```rust
#[tokio::test]
async fn gaps_second_call_returns_cached_result() {
    let app = make_app();

    // First call: cold
    let (status1, json1) = get_json(&app, "/api/v1/gaps").await;
    assert_eq!(status1, StatusCode::OK);

    // Second call: should return identical result (cached)
    let (status2, json2) = get_json(&app, "/api/v1/gaps").await;
    assert_eq!(status2, StatusCode::OK);
    assert_eq!(json1, json2, "cached response should be identical");
}
```

- [ ] **Step 2: Run test**

Run: `cd crates/nestweaver-web && cargo test gaps_second_call -- --nocapture 2>&1 | tail -10`
Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add crates/nestweaver-web/tests/api_test.rs
git commit -m "test(web): verify gaps cache returns identical results on second call"
```

---

### Task 5: Run full test suite and clippy

**Files:** None (validation only)

- [ ] **Step 1: Run clippy**

Run: `cargo clippy --all-targets -- -D warnings 2>&1 | tail -10`
Expected: no warnings

- [ ] **Step 2: Run rustfmt check**

Run: `cargo fmt --check 2>&1 | tail -5`
Expected: no output (already formatted)

- [ ] **Step 3: Run full test suite**

Run: `cd crates/nestweaver-web && cargo test 2>&1 | tail -15`
Expected: all tests pass, including `gaps_returns_report_structure`, `tested_service_uids_returns_services_with_test_callers`, `gaps_second_call_returns_cached_result`

- [ ] **Step 4: Manual perf verification**

Start the UI server and time the gaps endpoint:

```bash
nestweaver ui --db ~/.local/share/nestweaver/kory-brain/brain.lbug --port 3000 --no-open &
sleep 3

echo "Cold call:"
time curl -s http://localhost:3000/api/v1/gaps | python3 -c "import sys,json; d=json.load(sys.stdin); print(f'undocumented: {len(d[\"undocumented\"])}, untested: {len(d[\"untested\"])}')"

echo "Warm call:"
time curl -s http://localhost:3000/api/v1/gaps > /dev/null

pkill -f 'nestweaver ui'
```

Expected: cold <200ms, warm <5ms (vs current 2.1s)

- [ ] **Step 5: Final commit if any formatting changes needed**

```bash
cargo fmt
git add -A
git commit -m "style: rustfmt"  # only if there are changes
```
