# Task 5e Report — Mandatory Index Commit Epilogue

## Outcome

Every full, incremental, server-reader, and fallback indexing graph mutation now
publishes a mandatory epilogue before it can return success or a later error:

1. invalidate live PageRank;
2. remove the persisted PageRank sidecar, or quarantine it under a non-authoritative
   `.stale` name when direct removal fails;
3. bump the live graph generation; and
4. persist the bumped generation before any PageRank compute/save can return.

PageRank compute/save failures are returned instead of logged and swallowed.
Finalization runs through all stages, aggregates its failures, and is attached to
the original graph-write error when both fail. The server full-index write gate
remains held through publication and PageRank refresh.

## Exhaustive indexing commit audit

The post-change sweep was:

```text
rg -n "commit_transaction\(|bulk_reindex_write\(|bulk_index_write\(|compute_pagerank\(|save_pagerank_cache\(|bump_graph_generation\(|save_graph_generation\(|full_index_fallback\(|delete_repo_all_data\(" crates/nestweaver-engine/src/index.rs crates/nestweaver-engine/src/worker.rs crates/nestweaver-daemon/src/server.rs src/main.rs
```

The indexing mutation paths and their later fallible work are:

| Path | Graph commit/mutation | Later fallible work now covered |
| --- | --- | --- |
| Local full/tiered/force | repo/re-identification writes plus `bulk_index_write` or transactional `bulk_reindex_write`; resolution, edge, contract, and SHA writes | mandatory publication in the core path; required primary `merge_save_filemeta`; PageRank compute/save |
| Server reader full | same core graph path | mandatory publication and PageRank compute/save while the caller's write gate is still held |
| Local incremental | explicit `commit_transaction` | mandatory publication followed by returned PageRank compute/save |
| Server incremental | explicit `commit_transaction` | mandatory publication followed by returned PageRank compute/save while the write gate remains live |
| Missing repo/non-git fallback | same full core path | fallback sidecars followed by returned PageRank compute/save |
| Non-ancestor fallback | forced `bulk_reindex_write` | old graph deletion and replacement are one transaction; mandatory publication and returned PageRank compute/save |

The old non-ancestor pre-delete was removed. A forced fallback now uses
`bulk_reindex_write`, so replacement deletion is not exposed outside the
replacement transaction. `full_index_fallback` now takes a request struct instead
of suppressing a too-many-arguments lint. Transactional deletion counts flow
through `IndexResult.symbols_deleted` into the fallback result.

The daemon/root test-only `unnecessary_get_then_check` findings were corrected
with `contains_key`; no lint suppressions were added.

## Deterministic failure coverage

An injected indexing epilogue IO boundary covers persisted PageRank removal,
generation persistence, PageRank computation, and PageRank persistence. The
regressions prove:

- a primary full-index filemeta save error occurs after the replacement graph is
  committed but still returns with live/persisted stale PageRank invalidated and
  a durable bumped generation;
- PageRank compute and save errors are returned after mandatory publication;
- a PageRank removal failure quarantines the stale sidecar and continues through
  generation publication;
- local incremental and non-ancestor fallback compute failures leave their
  committed graph changes visible while stale PageRank remains unavailable;
- server full-index compute failure publishes generation/invalidation before the
  write guard is dropped;
- existing full, delete-only, and empty-reader behavior remains covered by the
  engine suite.

TDD evidence:

- RED: the filemeta regression observed the replacement graph commit but failed
  because live stale PageRank remained authoritative. GREEN: 1 passed.
- RED: deterministic PageRank compute/save injections were swallowed. GREEN:
  both errors are typed and returned after publication.
- GREEN: `cargo test -p nestweaver-engine compute_failure_finalizes -- --nocapture`
  — 2 passed (incremental and non-ancestor fallback).
- GREEN: `cargo test -q -p nestweaver-engine server_full_compute_failure_finalizes_before_releasing_write_gate -- --nocapture`
  — 1 passed.

## Verification

- `cargo test -q -p nestweaver-engine --lib` — 810 passed, 4 ignored.
- `cargo test -q -p nestweaver-daemon` — 134 passed.
- `cargo test -q -p nestweaver-daemon server::startup_helper_tests -- --nocapture`
  — 70 passed.
- `cargo test -q --bin nestweaver daemon_index_phase_tests -- --nocapture`
  — 3 passed.
- `cargo clippy -p nestweaver-engine -p nestweaver-daemon -p nestweaver --all-targets -- -D warnings`
  — passed without allowances.
- `cargo fmt --all -- --check` — passed.
- `git diff --check` — passed.

The pre-existing `Cargo.lock` and generated web distribution changes were kept
out of this task's commit.
