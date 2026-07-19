# Task 5d report — durable deletion reconciliation errors

## Status

Implemented and verified. Required reconciliation failures are returned only
after every safe finalizer stage has been attempted; mutation errors remain the
primary error and include the aggregate reconciliation details.

## Error contract and caller coverage

- Added `DeletionReconciliationError` with ordered
  `DeletionReconciliationFailure { stage, repo_uid, message }` entries.
- The engine finalizer aggregates repo sidecar, derived-state, generation,
  persisted PageRank, and supplied Tantivy failures without short-circuiting.
- Typed repo and vault removal, web-admin repo removal, stale prune, instance
  merge, and instance purge now surface required repair failures consistently.
- Partial mutation errors preserve their original gRPC code/message or HTTP
  status/message and append the reconciliation aggregate.
- Reader-only Tantivy handles are no longer silently skipped when supplied to a
  mutation finalizer: `WriterUnavailable` is a required search-stage failure.
  An absent optional index remains not applicable.
- Filemeta and resolution-dependency rewrites now use the existing flushed,
  sibling-temp-file atomic replacement helper.

## Stage classification

| Stage | Classification | Reason |
|---|---|---|
| Filemeta slice persistence | Required | A stale same-UID slice can classify every re-added file as unchanged and leave deleted symbols absent. |
| Resolution-dependency slice persistence | Required | Durable deleted-UID incremental state can influence a later same-UID resolution. Missing/corrupt input still deliberately fails open to a full resolution. |
| Manifest canonical persistence | Required | Repo-UID suggestions must agree with the authoritative Repo table. |
| Embedding canonical persistence | Required | Deleted Symbol/Note/Heading vectors must not return after restart. |
| Manifest/embedding legacy retirement | Required after canonical persistence | A fallback copy can resurrect deleted data; retirement is attempted only after the canonical replacement is durable and is reported as `legacy-retirement`. |
| Cluster sidecar removal | Required for code deletion | It contains Symbol UIDs without a generation guard. Vault-only paths do not apply because this output is Symbol-keyed. |
| Live graph-generation bump | Required, infallible | Invalidates live generation-keyed caches and always runs even if durable stages fail. |
| Generation sidecar persistence | Required | Other processes must observe the mutation generation. |
| Live PageRank invalidation | Required, infallible for code deletion | Prevents live stale graph-node scores and always runs. Existing vault-only PageRank preservation remains intentional. |
| Persisted PageRank removal | Required for code deletion | A restart must not reload scores for deleted graph rows. Absence is success. |
| Tantivy rebuild | Required when a search index is supplied/configured | Search documents must be rebuilt from the authoritative graph. No configured index is not applicable. |
| Parsed cache | Intentionally retained | It is content-hash keyed and repo independent. |
| Missing/corrupt fail-open input or absent removable sidecar | Best-effort / not applicable | It cannot preserve a known stale slice; consumers fall back to safe recomputation and deletion absence is the desired state. |

Manifest, embedding, and legacy retirement remain ordered internally: a failed
canonical write never removes the recovery copy. Independent later stages still
run. Vault-only finalization requires embedding persistence, generation
persistence, and configured search rebuild; code-only manifest/cluster/PageRank
stages remain not applicable there.

## TDD evidence

RED:

- `cargo test -p nestweaver-engine deletion_finalizer_aggregates_failures_and_runs_every_later_stage --no-run` — exit 101: the aggregate stage/error API and injected IO boundary did not exist.
- `cargo test -q -p nestweaver-daemon prune_surfaces_search_reconciliation_failure_after_other_finalizers --no-run` — exit 101: reconciliation callbacks returned `()` and could not surface Tantivy failure.
- `cargo test -q -p nestweaver-web admin_remove_repo_surfaces_tantivy_rebuild_failure` — exit 101: the helper returned `Ok(())` despite reader-only Tantivy failure.

GREEN:

- `cargo test -q -p nestweaver-engine --lib deletion_finalizer` — 4 passed.
- `cargo test -q -p nestweaver-engine --test remove_repo_sidecar` — 3 passed.
- `cargo test -q -p nestweaver-daemon reconciliation_failure` — 3 passed.
- `cargo test -q -p nestweaver-daemon typed_remove` — 2 passed.
- `cargo test -q -p nestweaver-daemon` — 125 passed before the final three focused regressions; all final focused regressions passed.
- `cargo test -q -p nestweaver-web` — 37 library and 31 integration tests passed before the final combined-error regression; the final focused regression also passed.

The representative engine injection fails a filemeta save, cluster removal,
generation persistence, and Tantivy rebuild in one run. It verifies ordered
aggregate stages while persisted/live PageRank invalidation still runs. Separate
tests cover legacy retirement, typed repo/vault errors, later vault search
rebuild after embedding persistence failure, prune/merge search failure, purge
mutation-plus-search failure, and web mutation-plus-Tantivy failure.

## Preserved unrelated worktree changes

The pre-existing `Cargo.lock` modification and generated frontend `dist`
deletion/index change were not staged or modified for this task.
