# Performance Optimization Roadmap: Items 5-7

**Date:** 2026-05-30
**Status:** Planning
**Context:** Items 1-4 are implemented on `feat/daemon-concurrent-access`. Items 5-7 require deeper investigation before implementation.

---

## Item 5: Materialized Project Context Sidecars

**Problem:** `project_context` re-queries repos/notes/symbols from the DB and runs PPR on every call. For frequently-accessed projects, this is redundant work.

**Proposed approach:** Pre-compute the top-N results for each project after indexing and store as a sidecar file (`<db>.project-cache/<project-name>.json`). On query, serve from the materialized view unless the graph generation has changed. Invalidate when `bump_and_persist_generation()` is called.

**Research backing:** MV4PG (arXiv 2411.18847) shows 2-28x workload speedup for property graph materialized views with template-based incremental maintenance. Write overhead is 30-70% per operation.

**Key design questions:**
1. What's the right granularity for cache keys? (project × intent × token_budget?)
2. How do we handle the `since` and `recency_weight` parameters that make each query unique?
3. Should the daemon proactively materialize on startup, or lazily on first query per project?
4. How does this interact with the existing F16 response cache? (generation-keyed + scope_digest)

**Investigation steps:**
- Benchmark `project_context` latency for different project sizes (small: 5 repos, medium: 15 repos, large: 50+ repos)
- Profile where time is spent: DB queries vs PPR vs token budgeting vs serialization
- Prototype a simple generation-keyed JSON sidecar and measure hit rate over a real work session
- Evaluate whether the F16 response cache already covers this use case (it may — check scope_digest coverage)

**Estimated effort:** 2-3 days implementation, 1 day testing
**Risk:** Low — additive cache layer, no architectural changes

---

## Item 6: Bounded Incremental PageRank

**Problem:** PageRank is recomputed over the full graph (all nodes + edges) even when only a few files changed. For a 50K-symbol graph with 20 iterations, this is O(20 × (50K + edges)) per index operation.

**Proposed approach:** When only a small fraction of nodes changed (<5%), run PPR from the changed nodes' 2-hop neighborhoods and merge the resulting scores with the cached full-graph PageRank. For larger changes, fall back to full recomputation.

**Research backing:** No confirmed claims from the deep research on incremental PageRank for code graphs specifically. The open question remains about Monte Carlo random walks and bookmark coloring algorithms. However, the Glean architecture (immutable stacking with O(fanout) bounds) provides a conceptual model.

**Key design questions:**
1. What's the right threshold for "small fraction"? (5% of nodes? 10 files?)
2. How do we track which nodes changed? (diff the filemeta sidecar before/after indexing?)
3. What's the error bound on the approximation? (for code navigation, ±10% rank accuracy is fine)
4. Should we use Monte Carlo random walks (proven for approximate PPR) instead of power iteration?
5. How does this interact with edge-type-weighted PageRank (CALLS 2x, IMPORTS 1x)?

**Investigation steps:**
- Benchmark full PageRank on real graphs: how long does it actually take? (may be fast enough that this doesn't matter)
- Implement change tracking: record which node UIDs were inserted/updated/deleted during indexing
- Prototype 2-hop neighborhood PPR: extract subgraph around changed nodes, run PPR, merge
- Measure approximation error vs full recomputation on real graphs
- Research Monte Carlo PPR algorithms (Fogaras et al. 2005, Lofgren et al. 2014) for applicability

**Estimated effort:** 3-5 days research + prototype, 2-3 days implementation
**Risk:** Medium — approximation quality needs validation, edge cases around graph disconnection

---

## Item 7: Glean-Style Immutable Database Stacking

**Problem:** Currently, incremental indexing does `delete_note_cascade()` + reinsert for changed files, and code indexing re-parses changed files and overwrites their symbols. For large repos, even the incremental path touches the DB heavily.

**Proposed approach:** Adopt Glean's immutable database stacking architecture:
- Each indexing operation creates a new thin "overlay" DB with only the changed facts
- The base DB is never mutated — stale units are hidden via an ownership/visibility layer
- Multiple DB versions coexist simultaneously
- Periodic merge/compaction combines overlays into a new base

**Research backing (high confidence, 3-0 verified):** Glean (Meta) achieves incremental code graph indexing through immutable stacking with unit-based ownership propagation. Each fact is mapped to an owning unit (file/module). Incremental updates exclude stale units from the base DB. Ownership sets propagated via A || B combination. Storage overhead ~7%. The bound is O(fanout) rather than O(repository).

**Key design questions:**
1. Can LadybugDB support multiple concurrent database files with a unified query layer? (likely not — would need a custom overlay mechanism)
2. How does the ownership model map to NestWeaver's graph structure? (Note/Symbol nodes → owned by file, edges → owned by source node's file)
3. How does stacking interact with Tantivy? (Tantivy is already segment-based — natural fit)
4. What's the compaction strategy? (merge after N overlays? on daemon idle? on explicit command?)
5. How does this interact with the daemon architecture? (daemon manages the overlay stack)

**Investigation steps:**
- Study Glean's implementation in detail (https://github.com/facebookincubator/Glean)
- Prototype an overlay mechanism: write changed facts to a second `.lbug` file, query both with a UNION-like layer
- Evaluate whether LadybugDB's query layer can be extended with virtual nodes/edges from an overlay
- If LadybugDB can't support overlays, evaluate alternative: use a lightweight in-memory store for the overlay and merge on compaction
- Benchmark the delete-cascade + reinsert path to establish the baseline cost

**Estimated effort:** 2-4 weeks research + design, 2-4 weeks implementation
**Risk:** High — fundamental architectural change, requires deep LadybugDB knowledge, may require upstream changes

---

## Priority and Sequencing

```
Item 5 (Materialized views)  ←  Start here: quickest win, lowest risk
     ↓
Item 6 (Incremental PageRank)  ←  Next: moderate complexity, validate need via benchmarks
     ↓
Item 7 (Glean-style stacking)  ←  Long-term: only if enterprise scale demands it
```

Item 5 can be prototyped in a day to validate the approach. Item 6 needs benchmarking first — if PageRank only takes 200ms on a 50K graph, the optimization isn't worth the complexity. Item 7 is a multi-week architectural initiative that should only be pursued if customers hit scaling limits with the current approach.
