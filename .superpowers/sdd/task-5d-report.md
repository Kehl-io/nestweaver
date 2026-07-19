# Task 5d report — durable deletion reconciliation errors

## Status

Implemented and verified. Required reconciliation failures are returned only
after every safe finalizer stage has been attempted; mutation errors remain the
primary error and include the aggregate reconciliation details.

## Error contract and caller coverage

- Added `DeletionReconciliationError` with ordered
  `DeletionReconciliationFailure { stage, repo_uid, message }` entries.
- The engine code-deletion finalizer aggregates repo sidecar, derived-state,
  generation, and persisted PageRank failures without short-circuiting.
- Typed repo and vault removal, web-admin repo removal, stale prune, instance
  merge, and instance purge now surface required repair failures consistently.
- Partial mutation errors preserve their original gRPC code/message or HTTP
  status/message and append the reconciliation aggregate.
- Search reconciliation is gated by before/after fingerprints of the exact
  Note/Heading/Section/Tag rows Tantivy indexes. Code/project-only mutations do
  not rebuild or false-fail search.
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
| Live PageRank invalidation | Required, infallible for every graph deletion | Prevents live stale graph-node scores; graph generation does not invalidate the independent PageRank cache. |
| Persisted PageRank removal | Required for every graph deletion | No reliable scope discriminator proves persisted ranks exclude vault/project nodes. Absence is success. |
| Tantivy rebuild | Required only when indexed Note/Heading/Section/Tag rows changed | Available writers rebuild; configured-but-unavailable/corrupt startup state surfaces `search-index`; disabled/read-only state is intentional. Code/project-only deletion is not applicable. |
| Parsed cache | Intentionally retained | It is content-hash keyed and repo independent. |
| Missing/corrupt fail-open input or absent removable sidecar | Best-effort / not applicable | It cannot preserve a known stale slice; consumers fall back to safe recomputation and deletion absence is the desired state. |

Manifest, embedding, and legacy retirement remain ordered internally: a failed
canonical write never removes the recovery copy. Independent later stages still
run. Vault-only finalization requires embedding persistence, generation and
PageRank invalidation, and search rebuild only when indexed rows changed.
Code-only manifest/cluster/PageRank stages still run, while Tantivy is not
applicable.

## Review 1 corrections

- Removed Tantivy from the code-only engine/web finalizer. Repo removal no
  longer fails on a reader-only writer or rewrites unrelated vault documents.
- Added authoritative before/after fingerprints of the exact Tantivy document
  projection. Unlike a count, these detect merge reparenting where indexed
  fields change but row totals do not, while ignoring graph-only metadata.
- Added explicit production startup states: `Disabled`, writer `Available`, and
  configured `Unavailable(reason)`. Reader fallback remains queryable while the
  missing writer is retained for a later indexed-row mutation error.
- Vault-only remove/prune/merge/purge now invalidate live PageRank and remove
  the persisted sidecar, including partial-error paths.
- Store embedding reconciliation now returns typed canonical-persistence and
  optional legacy-retirement results. Engine/daemon aggregates label legacy
  failure as `legacy-retirement` without string inspection.
- Purge and web combined-error regressions perform real committed graph
  deletion, force a real generation-sidecar failure, and assert the original
  mutation code/status/message remains primary.

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
and generation persistence in one run. It verifies ordered
aggregate stages while persisted/live PageRank invalidation still runs. Separate
tests cover typed manifest/embedding legacy retirement, typed repo/vault errors,
later vault search rebuild after embedding persistence failure, indexed-row
gating, startup writer unavailability, vault PageRank, and real purge/web
mutation-plus-durable-reconciliation failures.

Review 1 RED/GREEN evidence:

- RED: `cargo test -q -p nestweaver-store embedding_reconciliation_preserves_typed_legacy_retirement_failure --no-run` — exit 101, typed sub-stage API missing.
- RED: `cargo test -q -p nestweaver-daemon production_startup_preserves_configured_but_writer_unavailable_state --no-run` — exit 101, production startup helper/state seam missing.
- RED: first full daemon run after count-based gating — 6 failures; the merge reparent regression returned success because equal row counts hid changed indexed fields. Replacing counts with full indexed-row fingerprints fixed it.
- RED: `cargo test -q -p nestweaver-daemon indexed_search_fingerprint_ignores_non_tantivy_note_metadata` — the whole-row fingerprint changed for non-indexed Note metadata; exact Tantivy document projection fixed it.
- GREEN: store embedding 4/4; engine deletion finalizers 5/5; repo-sidecar integration 3/3; daemon 130/130.

Final validation:

- `cargo test -q -p nestweaver-web` — 99/99 across the library and integration binaries.
- `cargo clippy -q -p nestweaver-store -p nestweaver-engine -p nestweaver-daemon -p nestweaver-web --all-targets -- -D warnings -A clippy::too_many_arguments -A clippy::unnecessary_get_then_check` — passed.
- `cargo fmt --all --check` and `git diff --check` — passed.

## Preserved unrelated worktree changes

The pre-existing `Cargo.lock` modification and generated frontend `dist`
deletion/index change were not staged or modified for this task.
