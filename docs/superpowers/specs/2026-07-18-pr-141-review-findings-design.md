# PR 141 Review Findings Design

## Goal

Close the six correctness gaps found during review of PR 141 without rewriting
user-authored work already present on `fix/nw-bug-sweep`. Indexed code data is
derived and rebuildable; vault notes and other authored data must remain intact.

## Existing Work to Preserve

Two local commits already address four findings:

- `6e551693` validates daemon-backed index instance IDs and treats incomplete or
  failed daemon index streams as failures.
- `e885e4d5` removes stale sidecar slices during `prune_stale` and invalidates
  PageRank after code deletion.

Uncommitted PageRank/indexing hardening builds on the latter commit. Those edits
remain in scope. Unrelated `Cargo.lock` and generated frontend changes remain
untouched.

## Instance Merge

Repo File, Symbol, Service, edge, and derived graph rows are rebuildable index
data. During `old -> new` migration, each source Repo will therefore be handled
as follows:

1. Record its user-facing identifier and old repo UID for reporting and sidecar
   cleanup.
2. Cascade-delete the source repo's indexed children and derived nodes.
3. Delete the source Repo node.
4. Insert the target-instance Repo only when an equivalent target Repo does not
   already exist; an existing target Repo wins the collision.
5. Continue reporting the repo as needing re-index so the target graph is rebuilt
   from its working tree.

The store result will expose the deleted source UIDs to the daemon. After a
successful merge, the daemon removes their file-metadata and resolution-
dependency sidecar slices, invalidates graph and PageRank caches, removes the
persisted PageRank sidecar, and rebuilds Tantivy. Vault reparenting behavior is
unchanged.

This deliberately avoids rewriting embedded UIDs and every incident edge in
place. That alternative is more complex, collision-prone, and unnecessary for
derived code data.

## Purge and Deletion Cache Consistency

Every operation that removes code graph data must leave all query layers
consistent. `remove_repo`, repo-removing `prune_stale`, `merge_instance`, and
code-removing `purge_instance` will:

- bump graph generation when the graph changed;
- clear in-memory PageRank safely against concurrent lazy computation;
- remove the persisted PageRank sidecar; and
- rebuild Tantivy when applicable.

Deletion-only indexing will count removed files and recompute PageRank even when
no files were parsed. Recomputing an empty graph replaces prior scores with an
empty cache.

## Auto-Setup Completion Semantics

`run_auto_setup` remains best-effort from the indexing command's perspective:
indexing itself still succeeds if integration setup fails. Internally, however,
the setup function will aggregate failed detected tools and return an error if
any configuration attempt fails. Successful tools may remain configured.

`maybe_run_auto_setup` writes `.setup_done` only when all intended configuration
attempts succeed. A partial failure leaves the marker absent so the next eligible
index can retry. Detecting no tools is a successful no-op and may still write the
one-time marker.

## Validation and Stream Failures

The existing local fixes remain the intended design:

- validate the effective instance ID both before the CLI daemon/direct split and
  at the daemon RPC trust boundary;
- accept only a terminal `Done` phase as daemon-index success; and
- skip auto-setup and return a nonzero status for `Error`, empty, or truncated
  streams.

## Testing

Development follows red-green-refactor. New regression coverage will prove:

- merging deletes old-instance code rows and a subsequent target re-index cannot
  return both old and new symbols;
- repo collisions preserve the existing target Repo while removing source rows;
- merge sidecar cleanup removes only source repo slices;
- purge and merge invalidate PageRank/generation state;
- partial auto-setup failure is surfaced and does not create `.setup_done`;
- the existing prune, deletion-only indexing, RPC validation, and daemon-stream
  regressions remain green.

Final verification includes formatting, focused tests for each affected crate,
Clippy for affected packages, and an affected-test/blast-radius review. No commit,
push, or PR update is part of implementation unless separately requested.
