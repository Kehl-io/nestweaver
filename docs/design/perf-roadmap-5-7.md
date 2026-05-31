# Performance Optimization Roadmap: Items 5-7

**Date:** 2026-05-30 (validated against codebase and research)
**Status:** Research complete, validated, ready for implementation
**Context:** Items 1-4 are implemented on `feat/daemon-concurrent-access`.

---

## Benchmarks (measured on this machine)

| Operation | Graph size | Time |
|-----------|-----------|------|
| Full index (200 files, 1K symbols) | 200 files | 1.3s |
| Full PageRank (200 files, 1K symbols) | ~1K nodes | <286ms (including process startup) |
| Lazy PageRank first query | ~1K nodes | 286ms |
| Cached PageRank subsequent query | ~1K nodes | 295ms |

At NestWeaver's current scale (~50K symbols, ~200K edges), full PageRank likely takes 1-3 seconds.

---

## Item 5: Validate and Optimize Project Context Caching

### Problem
`project_context` runs PPR on every call. For frequently-accessed projects in a daemon session, this may be redundant when the graph hasn't changed.

### Codebase Validation (verified)

**`project_context` IS already in the cacheable tool set** (`CACHEABLE_TOOLS` in `tools.rs`). The F16 response cache should already be caching it with generation-keyed invalidation.

The F16 cache uses:
- **`key_hash`:** derived from tool name + normalized args (captures query parameters)
- **`scope_digest`:** XOR of file paths and content hashes (captures graph state, NOT query params)
- **`generation`:** monotonic counter bumped after each index operation
- **TTL:** 24 hours
- **Eviction:** LRU by `last_access` when over size limit

**This means Item 5 may already be working.** The investigation shifts from "add caching" to "verify caching works and diagnose why project_context still feels slow."

### Revised Design

1. **Verify F16 cache hits for project_context** — add instrumentation (or check existing `CACHE_HITS` / `CACHE_MISSES` counters) to confirm project_context queries hit the cache on repeated calls
2. **If cache misses:** investigate whether the scope_digest changes between calls even when the graph hasn't changed (possible if file mtimes are included in the digest)
3. **If cache hits but still slow:** the bottleneck is elsewhere (DB query, PPR computation, or serialization). Profile with `tracing` spans
4. **Daemon-specific optimization:** the daemon holds the GraphStore in memory, so the F16 cache is shared across all MCP clients. Verify the daemon's dispatch path goes through the F16 cache (check if `dispatch_json_tool` calls the cached `dispatch` or the uncached `dispatch_uncached`)

**Estimated effort:** 0.5-1 day investigation + fix
**Risk:** Very low

---

## Item 6: Warm-Start and Incremental PageRank

### Problem
PageRank is recomputed from scratch (uniform initialization, 20 iterations) even when only a few files changed.

### Research Findings (validated)

**Warm-start power iteration:** Initialize from previous PageRank vector instead of uniform. Convergence is faster when the graph has changed only slightly — the previous vector is already close to the new fixpoint. Kamvar et al. (Stanford) demonstrated ~50-300% speedup via Power Extrapolation on 80M-node web graphs. Specific iteration count reduction depends on the magnitude of the change. [Source: Kamvar et al., "Extrapolation Methods for Accelerating PageRank Computations"]

**Local forward push (Andersen/Chung/Lang 2006):** Push residual probability mass forward from seed nodes along edges until residuals fall below epsilon. Complexity is O(m/(α·ε)) where m = number of edges, α = teleport probability, ε = convergence threshold. This is NOT independent of graph size — it depends on m. However, in practice, for localized changes, it terminates much earlier because most residuals decay below epsilon before reaching distant nodes. [Source: arXiv 2403.05198 survey of PPR algorithms]

**Monte Carlo random walks (Bahmani et al. 2010):** Store R random walks per node. On edge change, re-sample only walks traversing the changed edge. Total maintenance cost O(nR·ln(m)/ε²) where R = walks per node. Requires random-order edge arrivals; adversarial orderings can be worse. [Source: arXiv 1204.5500]

**Structural change detection (verified against codebase):** PageRank depends only on link structure (edges, weights), not node content. The PPR algorithm in `ppr.rs` operates purely on `AdjacencyData` — no node attributes (word_count, content, etc.) are referenced. If an index operation adds/updates nodes but no edges change, PageRank scores are unchanged.

### Codebase Validation (verified)

- `compute_pagerank` initializes `scores = vec![1.0/n; n]` (uniform) — confirmed at `ranking.rs:361-362`
- PageRank sidecar is `HashMap<String, f64>` as JSON — confirmed at `ranking.rs:689-709`
- `IndexResult` has `edges_count` field — confirmed. `MarkdownIndexResult` does not have `edges_count` (has `wikilinks_resolved` instead, which represents edges)
- Adding `warm_start: Option<&HashMap<String, f64>>` to `compute_pagerank` requires a signature change but won't break existing callers (add as last parameter with default)

### Design

**Phase A: Warm-start (trivial, do first)**

1. Add `warm_start: Option<&HashMap<String, f64>>` parameter to `compute_pagerank`
2. If provided, initialize scores from the warm-start map for known node UIDs, uniform (`1.0/n`) for new nodes
3. `ensure_pagerank_loaded` already has the sidecar loaded — pass it as warm start
4. Run power iteration as normal — it will converge faster from a closer starting point

**Phase B: Skip if no structural changes**

Track whether edges changed during indexing:
1. `IndexResult` already has `edges_count` — if zero, no new edges were created. But this doesn't track edge deletions
2. Add an `edges_deleted: usize` counter to the delete path
3. If `edges_count == 0 && edges_deleted == 0`, skip PageRank entirely — cached scores are exact
4. For markdown: `wikilinks_resolved` serves as the edge count. If zero and no notes were deleted, skip

**Phase C: Forward push from dirty nodes (only if benchmarks justify at >100K nodes)**

1. Track which node UIDs were inserted/updated/deleted during indexing
2. Set initial residuals only at dirty nodes
3. Push forward using Andersen/Chung/Lang algorithm until convergence
4. Complexity: O(m/(α·ε)) worst case, but terminates early for localized changes

**Estimated effort:** Phase A: 1 day. Phase B: 1 day. Phase C: 3-5 days.
**Risk:** Phase A: None. Phase B: None. Phase C: Medium (approximation quality needs validation).

---

## Item 7: Staged Incremental Writes and File-Level Ownership

### Problem
Incremental indexing uses `delete_note_cascade()` (deletes note + all headings/sections/edges via DETACH DELETE) followed by full reinsert. For code, `delete_symbols_in_file()` deletes all symbols for a file path then reinserts. At scale, this is expensive — especially the cascade deletes which execute multiple per-UID queries.

### Research Findings (validated)

**Glean stacking (Meta, confirmed):** Immutable DB layers with unit-based ownership. Facts hidden via ownership sets (UsetId), not per-fact bitmaps. Ownership propagation: `A || B` for referenced facts. Storage overhead ~7% using Elias-Fano encoding (~2 bits overhead per element above information-theoretic minimum; total bits depend on universe-to-element ratio). Backend: RocksDB (with LMDB alternative, 30-40% faster). Compaction: periodic full rebuild. [Sources: glean.software/blog/incremental/, engineering.fb.com/2024/12/19/developer-tools/glean-open-source-code-indexing/]

**SCIP (Sourcegraph, confirmed):** Human-readable symbol strings replace opaque numeric IDs, enabling per-file incremental updates without global renumbering. [Source: sourcegraph.com/blog/announcing-scip]

**LadybugDB constraints (verified against codebase):** LadybugDB does NOT support parameterized compound WHERE clauses — queries use string interpolation with escaping (see `write.rs:1620-1627`). An `owner_file` column approach would need escaped string interpolation, not parameterized queries. The DB does support `DETACH DELETE` which removes a node and all its edges atomically.

### Codebase Validation (verified)

- `delete_note_cascade` (`write.rs:1449-1485`): 4-step cascade — delete sections (DETACH), delete headings (DETACH), delete note (DETACH), delete unresolved wikilinks. Each step is a separate query per UID.
- `delete_symbols_in_file` (`write.rs:1610-1647`): Collects all Symbol UIDs for a (repo_uid, file_path) pair, then DETACH DELETE each.
- Both Symbol and Note have `file_path: String` fields — confirmed in schema and DB tables.
- `begin_transaction` / `commit_transaction` exist — confirmed at `db.rs:307-322`.

### Design

**Phase A: Batch diff writes (medium effort, high payoff)**

Instead of delete-cascade + full reinsert per file, compute the diff and apply only changes:

1. Before indexing a file, query its current nodes: `MATCH (s:Symbol) WHERE s.file_path = $escaped_path RETURN s.uid, s.name, s.content_hash`
2. After parsing, compare new symbols to existing by content_hash
3. Only insert truly new symbols, delete truly removed ones, skip unchanged
4. Apply all changes in a single transaction via `begin_transaction` / `commit_transaction`

For notes, the same pattern: query existing headings/sections by `note_uid`, diff against newly parsed, apply delta.

**Expected impact:** For a file where 1 of 10 functions changed, this does 1 delete + 1 insert instead of 10 deletes + 10 inserts.

**Phase B: File-level bulk delete (simpler alternative to Phase A)**

Add a `DELETE WHERE file_path` pattern for symbols:
```cypher
MATCH (s:Symbol) WHERE s.file_path = '<escaped_path>' AND s.repo_uid = '<escaped_repo>' DETACH DELETE s
```

This replaces the current pattern of collecting UIDs then deleting one-by-one. Fewer round trips to the DB, same result. Note: requires string interpolation with escaping per LadybugDB constraints.

For notes, the existing `delete_note_cascade` is already UID-based (one note at a time), so the improvement is more modest.

**Phase C: Glean-style ownership tracking (longer term)**

Store an `owner_unit: String` on every node (file path for code, relative path for notes). On incremental update:
1. Mark the owner_unit as "dirty"
2. New facts get the new unit's ownership
3. Old facts with the dirty unit are hidden (not deleted)
4. Periodic compaction merges hidden facts into a clean base

This requires either LadybugDB schema changes (adding `owner_unit` + `visible` columns) or an out-of-DB sidecar for the ownership map. The sidecar approach (interval map like Glean) avoids schema changes but adds query-time filtering cost.

**Phase D: True immutable stacking (enterprise scale only)**

Only if Phases A-C are insufficient at >500K nodes. Requires overlay `.lbug` files with union query resolution.

**Estimated effort:** Phase A: 3-5 days. Phase B: 1-2 days. Phase C: 2-3 weeks. Phase D: 4-8 weeks.
**Risk:** Phase A: Low. Phase B: Very low. Phase C: Medium. Phase D: High.

---

## Updated Priority and Sequencing

```
Item 5  (Verify & fix F16 caching)   ←  0.5-1 day, verify what's already working
     ↓
Item 6A (Warm-start PageRank)        ←  1 day, zero risk
     ↓
Item 6B (Skip if no edge changes)    ←  1 day, zero risk
     ↓
Item 7B (File-level bulk delete)     ←  1-2 days, very low risk
     ↓
Item 7A (Batch diff writes)          ←  3-5 days, low risk
     ↓
Item 6C (Forward push)              ←  3-5 days, medium risk — only if benchmarks justify
     ↓
Item 7C (Ownership tracking)        ←  2-3 weeks, medium risk — next quarter
     ↓
Item 7D (True stacking)             ←  4-8 weeks, high risk — only at enterprise scale
```

Quick wins (5 + 6A + 6B + 7B): ~4 days total. Next sprint (7A): 3-5 days.

### Validation Status

| Claim | Status |
|-------|--------|
| project_context not cacheable | **INCORRECT** — already cacheable |
| scope_digest captures query params | **INCORRECT** — captures file metadata; query params in key_hash |
| compute_pagerank initializes uniform | Confirmed |
| PageRank sidecar is HashMap JSON | Confirmed |
| IndexResult has edges_count | Confirmed |
| PageRank depends only on edges | Confirmed |
| delete_note_cascade is expensive | Confirmed |
| Symbol/Note have file_path | Confirmed |
| begin/commit_transaction exists | Confirmed |
| LadybugDB lacks parameterized WHERE | Confirmed |
| Warm-start "2-5 iterations" | **UNVERIFIED** — removed specific claim |
| Forward push O(1/ε) graph-independent | **INCORRECT** — O(m/(αε)), depends on edges |
| "1-5K nodes touched" estimate | **UNVERIFIED** — removed |
| Glean ~7% overhead | Confirmed |
| Elias-Fano ~2 bits per element | **CORRECTED** — ~2 bits overhead, not total |
| Glean uses RocksDB | Confirmed |
| MV4PG 97x per-query | **CORRECTED** — "nearly 100x" per paper |
