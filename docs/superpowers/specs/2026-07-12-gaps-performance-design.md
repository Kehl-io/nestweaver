# Gaps Endpoint Performance: Generation-Gated Lazy Materialization

## Problem

The `/api/v1/gaps` endpoint takes ~2.1s because it runs 2,116 individual `callers_of` graph queries (one per service) on every request. This scales linearly with service count and has no caching. At 100K services the endpoint would take ~100s.

## Goal

Sub-100ms cold reads, sub-1ms warm reads, zero staleness, no write amplification at index time. Must scale to 100K+ services.

## Architecture

Lazy materialization with generation-based cache invalidation. The gaps endpoint becomes a thin cache-check layer. Computation happens once per `graph_generation` bump, on first read after any index operation.

```
Request → read lock → generation match? → serve cached JSON
                           ↓ no
                      write lock → double-check → batch query → cache → serve
```

The `graph_generation` counter (already bumped on every index) is the sole staleness signal. One `u64` comparison per request.

## Components

### GapsCache (new: `crates/nestweaver-web/src/gaps_cache.rs`)

```rust
pub struct GapsCache {
    cached: RwLock<Option<CachedGaps>>,
}

struct CachedGaps {
    generation: u64,
    response: serde_json::Value,
}
```

- `RwLock` allows concurrent reads without blocking.
- Pre-serialized `serde_json::Value` avoids re-serialization on the hot path.
- Double-check after acquiring write lock prevents thundering herd.
- Failed computes leave the cache empty; next request retries.

Public API:

```rust
impl GapsCache {
    pub fn new() -> Self;
    pub fn get_or_compute(&self, store: &GraphStore) -> Result<serde_json::Value, ApiError>;
}
```

### Batch query (new method on `GraphStore`: `tested_service_uids`)

Replaces 2,116 individual `callers_of` calls with one graph query:

```cypher
MATCH (s:Symbol)-[:CALLS]->(t:Symbol)<-[:SERVICE_HAS_SYMBOL]-(svc:Service)
WHERE s.file_path CONTAINS 'test' OR s.file_path CONTAINS 'spec'
RETURN DISTINCT svc.uid
```

Returns the set of service UIDs that have at least one test caller. The untested set is `all_services - tested_services` (set difference in Rust).

Located in `crates/nestweaver-store/src/read.rs`.

### Endpoint handler (modified: `crates/nestweaver-web/src/routes/gaps.rs`)

Becomes a 3-line delegation:

```rust
pub async fn gaps(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let result = state.gaps_cache.get_or_compute(&state.store)?;
    Ok(Json(result).into_response())
}
```

All computation logic moves into `GapsCache::compute()`.

### AppState (modified: `crates/nestweaver-web/src/state.rs`)

Add `gaps_cache: GapsCache` field, initialized with `GapsCache::new()`.

## Computation logic (inside `GapsCache::compute`)

Moved from the current `gaps()` handler with two changes:

1. **Undocumented**: unchanged — `count_references_code_edges()` check, then per-repo symbol grouping only when count is zero. Already fast enough (only runs when no vault references exist).

2. **Untested**: replaced with batch path:
   ```rust
   let all_uids: HashSet<String> = services.iter().map(|s| s.uid.clone()).collect();
   let tested = store.tested_service_uids()?;
   let untested: Vec<String> = all_uids.difference(&tested).cloned().collect();
   ```

3. **Disconnected pairs**: unchanged (empty vec, future work).

## Cache invalidation

The cache compares its stored `generation` against `store.graph_generation()`. Any index operation (full index, re-index, incremental update) bumps the generation, causing the next read to recompute. No timers, no TTLs, no manual invalidation.

This matches the existing `bridge_scores` pattern on `AppState` and the SQLite schema-cookie pattern used industry-wide.

## Error handling

- Batch query failure: cache stays empty, next request retries. No poisoned state.
- RwLock poisoning (panic during compute): use `unwrap_or_else(|e| e.into_inner())` on read path.
- LadybugDB `CONTAINS` limitation: the `file_path CONTAINS 'test'` filter runs inside the graph engine. If KuzuDB doesn't support `CONTAINS` in this context, fall back to fetching all callers and filtering in Rust (still one query, not N).

## Files changed

| File | Change |
|------|--------|
| `crates/nestweaver-store/src/read.rs` | Add `tested_service_uids()` method |
| `crates/nestweaver-web/src/gaps_cache.rs` | New file: `GapsCache` struct |
| `crates/nestweaver-web/src/routes/gaps.rs` | Simplify to cache delegation |
| `crates/nestweaver-web/src/state.rs` | Add `gaps_cache` field |
| `crates/nestweaver-web/src/lib.rs` | Add `mod gaps_cache` |

## Not changed

- Indexing pipeline — no write-time precomputation
- Schema — no new node properties or edge types
- Frontend — response shape is identical
- Other endpoints — no side effects

## Testing

1. **Unit test** `tested_service_uids()` against the existing e2e test fixture.
2. **Integration test**: call `/api/v1/gaps` twice; assert second call is sub-1ms.
3. **Accuracy check**: `#[cfg(test)]` assertion comparing batch result against N+1 result during development.

## Expected performance

| Scenario | Before | After |
|----------|--------|-------|
| Cold (first after index) | 2.1s | ~50ms |
| Warm (cached) | 2.1s | <1ms |
| After re-index | 2.1s | ~50ms cold, <1ms warm |
| 100K services cold | ~100s | ~200ms |
| 100K services warm | ~100s | <1ms |

## Design rationale

**Why not index-time precomputation?** The read-to-write ratio is ~1:100 (gaps called ~10 times per session, indexing runs ~1,000 times). Precomputation wastes 99% of write-side work. Lazy materialization only pays when someone actually asks.

**Why not a simple batch query without caching?** A batch query alone gets cold reads to ~50ms, but the gaps button is clicked multiple times per session. Caching avoids redundant recomputation between index operations.

**Why `RwLock` not `OnceLock`?** `OnceLock` can't be invalidated. The cache must recompute when `graph_generation` changes.

**Why pre-serialize JSON?** The hot path should be: read lock → integer compare → clone → respond. No serialization, no allocation beyond the clone.
