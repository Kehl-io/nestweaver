# Performance Optimization Roadmap: Items 5-7

**Date:** 2026-05-30 (updated with research findings)
**Status:** Research complete, ready for implementation planning
**Context:** Items 1-4 are implemented on `feat/daemon-concurrent-access`.

---

## Benchmarks (measured on this machine)

| Operation | Graph size | Time |
|-----------|-----------|------|
| Full index (200 files, 1K symbols) | 200 files | 1.3s |
| Full PageRank (200 files, 1K symbols) | ~1K nodes | <286ms (including process startup) |
| Lazy PageRank first query | ~1K nodes | 286ms |
| Cached PageRank subsequent query | ~1K nodes | 295ms |

At NestWeaver's current scale (~50K symbols, ~200K edges), full PageRank likely takes 1-3 seconds. This makes incremental PageRank a moderate-priority optimization — warm-start alone would cut it to <500ms.

---

## Item 5: Materialized Project Context with Generation-Based Invalidation

### Problem
`project_context` re-queries repos/notes/symbols from the DB and runs Personalized PageRank on every call. For frequently-accessed projects in a daemon session, this is redundant when the graph hasn't changed.

### Research Findings

**MV4PG (arXiv 2411.18847):** Materialized views for property graphs achieve up to 97x per-query and 29x workload-wide speedup. The key technique: compress multi-hop variable-length path queries into pre-materialized single edges. Write overhead is 20-30% for small mutations. Biggest wins are on queries that traverse long paths (e.g., `[:CALLS*..]`).

**Neo4j pattern (Max De Marzi):** In-process LoadingCache (Guava-style) keyed on node ID with TTL-based expiration. `refreshAfterWrite` enables async background refresh — readers get stale-but-fast results while recomputation runs. No built-in materialized views in Neo4j.

**Generation-based invalidation (CasCache pattern):** Monotonic generation counter per cache namespace. Cached entries store the generation they were computed at. On read, compare entry generation to current — miss if stale. CAS fence variant rejects late writes that would overwrite fresher data.

**NestWeaver already has this infrastructure:** The F16 response cache uses `generation + scope_digest` keying with 24h TTL and LRU eviction. The graph generation is bumped after each index operation via `bump_and_persist_generation()`.

### Design

**Approach: Extend the existing F16 response cache to cover `project_context`.**

The F16 cache already handles generation-based invalidation for most tools. The reason `project_context` isn't cached is that it's not in the cacheable tool list (it depends on PPR which was previously considered too dynamic).

With lazy PageRank (item 3, already implemented), the PageRank cache is stable between index operations. This means `project_context` results are also stable between index operations — they can be safely cached.

**Changes:**
1. Add `project_context` to the cacheable tool set in `tools.rs` (the `is_cacheable_tool` function)
2. The scope_digest already captures the query parameters (project name, token_budget, intent, etc.), so different queries for the same project get different cache entries
3. The generation counter already invalidates on index — no new invalidation logic needed
4. For the daemon, the in-memory response cache is shared across all gRPC clients

**For PPR-heavy queries (`brain_context` with custom seeds):** These are already cacheable via F16. Verify they're in the cacheable set.

**Estimated effort:** 1 day (mostly testing)
**Risk:** Very low — extending an existing, proven cache system

---

## Item 6: Warm-Start and Incremental PageRank

### Problem
PageRank is recomputed from scratch (uniform initialization, 20 iterations) even when only a few files changed. At 50K nodes this takes 1-3 seconds — fast, but avoidable.

### Research Findings

**Warm-start power iteration:** Initialize from previous PageRank vector instead of uniform. When <1% of nodes change, convergence drops from ~20 iterations to 2-5. Trivial to implement — just persist the rank vector. ~4x speedup for typical incremental updates.

**Local forward push (Andersen/Chung/Lang 2006):** Starting from changed nodes, push residual probability mass forward along edges until residuals fall below epsilon. Work is proportional to affected nodes, not graph size. For 100 changed files in a 50K-node graph, touches ~1-5K nodes. Time complexity O(1/epsilon), independent of graph size. ~20-50x speedup for small changes.

**Monte Carlo random walks (Bahmani et al. 2010):** Store R random walks per node. On edge change, re-sample only walks that traverse the changed edge. Maintenance cost O(n ln m / epsilon²). Tested at Twitter scale. More complex than forward push but handles streaming updates naturally.

**Structural change detection:** PageRank depends only on link structure (edges), not node content. If an index operation only changes node metadata (content hashes, line counts) but no edges are added/removed, PageRank doesn't need recomputation at all.

### Design

**Phase A: Warm-start (trivial, do first)**

The PageRank sidecar (`<db>.pagerank.json`) already persists scores. Currently, `compute_pagerank` initializes from uniform. Change it to:
1. Load previous scores from the sidecar if available
2. Initialize the score vector from previous values for existing nodes, uniform for new nodes
3. Run power iteration — convergence in 2-5 iterations instead of 20

**Changes:**
- `ranking.rs:compute_pagerank` — accept an optional `warm_start: Option<&HashMap<String, f64>>` parameter
- `ensure_pagerank_loaded` — pass the loaded sidecar scores as warm start
- Track whether edges changed during indexing (add a `edges_changed: bool` flag to `IndexResult` / `MarkdownIndexResult`)
- If no edges changed, skip PageRank entirely

**Phase B: Forward push from dirty nodes (if benchmarks justify)**

Only implement if Phase A's warm-start still takes >500ms at enterprise scale (>100K nodes).
1. Track which node UIDs were inserted/updated/deleted during indexing
2. Set initial residuals only at dirty nodes
3. Push forward until convergence
4. Merge with cached scores for untouched nodes

**Phase C: Skip if no structural changes**

Add an `edges_changed` counter to the index result. If zero edges were added/removed (content-only change), skip PageRank entirely — the cached scores are still exact.

**Estimated effort:** Phase A: 1 day. Phase B: 3-5 days. Phase C: 0.5 days.
**Risk:** Phase A: None. Phase B: Medium (approximation quality needs validation). Phase C: None.

---

## Item 7: Glean-Style Immutable Database Stacking

### Problem
Incremental indexing does `delete_note_cascade()` + reinsert for changed files. Code indexing re-parses and overwrites symbols. At scale (>500 files changed, enterprise monorepos), the delete-cascade + reinsert cycle is expensive and holds the write lock.

### Research Findings

**Glean architecture (Meta, verified):** Each DB is immutable. Incremental updates create a new layer atop a base DB. The stack appears as one logical DB. Facts are hidden via unit-based ownership sets (not per-fact bitmaps — ownership sets are 10-100x fewer than facts). Ownership propagation: if fact F (owned by set A) references fact G (owned by set B), derived ownership is `A || B`. Storage uses Elias-Fano encoding (~2 bits per element), adding ~7% overhead. RocksDB as storage backend. Compaction is periodic full rebuild (no leveled compaction for graph layers).

**Key insight — ownership sets vs bitmaps:** Glean doesn't tag each fact individually. Facts produced by the same file share an ownership set, stored as an interval map (consecutive facts with the same owner = one entry). This is why the overhead is only 7% — not per-fact overhead.

**SCIP/LSIF comparison:** SCIP (Sourcegraph) uses human-readable symbol strings that enable per-file replacement, but Sourcegraph hasn't shipped true incremental stacking — they re-process per upload. Glean's approach is more mature.

**Adapting to LadybugDB (Kuzu-style):** Three approaches evaluated:
1. **Application-level overlay** — add `layer_id` + `visible` columns to every table. Query with `WHERE visible = true`. Tombstone rows hide old facts. **Downside:** pollutes every query with filter, schema changes required.
2. **Separate staging tables** — write incremental updates to staging tables, merge periodically. **Simplest practical approach** for LadybugDB.
3. **Ownership as sidecar** — store ownership sets outside the graph. Filter results at query time. **Most Glean-faithful** but adds query-time overhead.

**LSM analogy:** Glean stacking maps to size-tiered compaction. Layers accumulate; periodic full rebuild is the "major compaction." For NestWeaver, this means: accumulate small incremental updates in a lightweight structure, merge into the main DB when idle or when layer count exceeds a threshold.

### Design

**Phase A: Staged incremental writes (medium effort, high payoff)**

Instead of `delete_note_cascade()` + reinsert, write changed facts to an in-memory staging area, then apply them as a single batch diff:

1. During indexing, track the set of changed file paths
2. For each changed file, compute the new set of nodes/edges
3. Diff against the existing nodes/edges for that file (query by `file_path`)
4. Apply only the delta: insert new, delete removed, update changed
5. Skip files whose content hash hasn't changed (already done via filemeta cache)

**Key data structure:** A `ChangeBatch` that accumulates:
- `nodes_to_insert: Vec<Node>`
- `nodes_to_delete: Vec<String>` (UIDs)
- `edges_to_insert: Vec<Edge>`
- `edges_to_delete: Vec<(String, String)>` (source, target UIDs)

The daemon applies the batch in a single transaction.

**Phase B: File-level ownership tracking (longer term)**

Add a `owner_file: String` column to Symbol and Note node tables. On incremental update:
1. `DELETE FROM Symbol WHERE owner_file = $changed_file`
2. Insert new symbols for that file
3. No cascade needed — edges that reference deleted symbols are handled by the graph engine's referential integrity

This is the NestWeaver-adapted version of Glean's ownership model. Simpler than full stacking, but captures the key benefit: O(changed files) rather than O(cascade depth).

**Phase C: True immutable stacking (enterprise scale only)**

Only if Phases A-B are insufficient at >500K node scale:
1. Create overlay `.lbug` files for incremental updates
2. Query across base + overlay with a union resolver
3. Compact overlays into a new base on daemon idle
4. Requires either LadybugDB changes or a custom query adapter layer

**Estimated effort:** Phase A: 3-5 days. Phase B: 1-2 weeks. Phase C: 4-8 weeks.
**Risk:** Phase A: Low. Phase B: Medium. Phase C: High (may require upstream DB changes).

---

## Updated Priority and Sequencing

```
Item 5 (F16 cache extension)     ←  1 day, very low risk — do immediately
     ↓
Item 6A (Warm-start PageRank)    ←  1 day, zero risk — do immediately
     ↓
Item 6C (Skip if no edge changes) ←  0.5 days, zero risk — do immediately
     ↓
Item 7A (Staged batch diffs)     ←  3-5 days, low risk — next sprint
     ↓
Item 6B (Forward push)           ←  3-5 days, medium risk — only if benchmarks justify
     ↓
Item 7B (File-level ownership)   ←  1-2 weeks, medium risk — next quarter
     ↓
Item 7C (True stacking)          ←  4-8 weeks, high risk — only at enterprise scale
```

Items 5, 6A, and 6C are quick wins that should ship together — total ~2.5 days. Item 7A is the next meaningful architectural step. Everything beyond that is driven by measured customer need at scale.
