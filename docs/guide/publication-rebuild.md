# Publication rebuild and recovery

The regex-v3 and embedding-pipeline-v2 release uses a new publication format.
Upgrading an existing brain requires one complete graph reindex and re-embed.
NestWeaver builds that replacement beside the incumbent database and does not
change `CURRENT` until every artifact validates.

## Storage-engine upgrade (LadybugDB 0.21): rebuild required

This release updates LadybugDB to 0.21.0, which changes how text keys are
hashed. The engine's on-disk format marker did not change, so the engine
itself would open an older database and then silently miss lookups on
non-ASCII keys. NestWeaver therefore records the engine format itself (a
`storage.string_hash` row in the database and a `<db>.engine-format` file
beside it) and refuses, before the engine touches the file, any database that
lacks it:

```text
Error: nestweaver::db_rebuild_required

  × Database must be rebuilt for this version of NestWeaver: /path/to/brain.lbug
  help: /path/to/brain.lbug was built by a storage engine older than LadybugDB 0.21 ...
          nestweaver publication rebuild --config /path/to/instance.toml
```

The exit code is 1. The refused database is never opened and never changed,
and no daemon is started against it (both autostart and `daemon start` check
first, so a supervisor cannot crash-loop on it). MCP clients receive the same
code and command in the JSON-RPC error.

To upgrade:

1. Before installing, with the version you have now: stop the daemon and run
   `nestweaver backup save <file>`.
2. Install this release and run the command the error prints (below). The
   rebuild reads the old database read-only through a narrow exemption that
   scans rather than using primary-key lookups, writes the new graph into a
   fresh publication slot, and switches `CURRENT` only after it validates.
3. The old database stays where it is, unchanged. To roll back, reinstall the
   previous version and restore your backup. Do not open a rebuilt database
   with an older NestWeaver.

Without an instance config there is nothing to rebuild from, so the error
names a fresh `nestweaver index --repo <path> --db <new path>` instead; it
never names the refused file as a write target.

Snapshots follow the same fence: this release writes snapshot format 4 and
refuses any older snapshot with a message naming the rebuild. `backup restore`
of an archive written before the upgrade still restores it unchanged, and
warns that the restored database must be rebuilt before use.

The same release changes three crash-recovery behaviours:

* A stale `<db>.wal.checkpoint` from another database is now renamed to
  `<db>.wal.checkpoint.stale-<epoch>`, never deleted.
* A frozen `<db>.wal.checkpoint` whose pages were already applied before a
  crash (its `<db>.shadow` is gone) is reported with the single safe move,
  `mv <db>.wal.checkpoint <db>.wal.checkpoint.applied`. Leave `<db>.wal` in
  place: it holds later commits. Nothing is moved automatically.
* A recovery that runs out of disk space or buffer pool while finishing an
  interrupted checkpoint is reported as a resource problem, not corruption.
  Free space (or raise `NESTWEAVER_LBUG_BUFFER_POOL_BYTES`) and open again; do
  not move log files aside. A write whose post-commit checkpoint is deferred is
  reported as committed and is never retried; restart the daemon so recovery
  finishes the checkpoint.

## Upgrade

Stop the daemon and any external watchers, then run:

> `daemon stop` takes `--db`, not `--config`: the daemon subtree has no
> config-based database resolution, so `--config` is rejected outright and a
> bare `daemon stop` fails with "No database path provided". Its siblings
> `daemon start` and `publication rebuild` DO take `--config`, which is what
> makes this easy to miss.

```sh
nestweaver daemon --db /path/to/brain.lbug stop
nestweaver publication rebuild --config /path/to/instance.toml
nestweaver daemon start --config /path/to/instance.toml
```

The rebuild captures the exact repository and vault inputs, rebuilds the graph,
projects, BM25, per-scope regex shards, embeddings, and ranking metadata, then
revalidates the inputs before the atomic switch. Interaction history is copied
only for stable graph UIDs that still exist; the sealed preservation receipt
reports captured, imported, and deliberately pruned counts and checksums.

`--no-embed` is intentionally incompatible with `publication rebuild` and is
rejected before an operation or slot is created. A publication is a complete,
validated release unit; use an ordinary non-publication index when an
embedding-free development graph is required.

The command prints its operation UUID immediately. A failure or interruption
leaves the incumbent selected. Inspect and resume the same staging work with:

```sh
nestweaver publication status --db /path/to/brain.lbug
nestweaver publication status --db /path/to/brain.lbug --operation <uuid> --json
nestweaver publication rebuild --config /path/to/instance.toml --operation <uuid>
```

Resume is refused if source content, configuration, binary version, publication
format, or database identity changed. Stabilize the named input and start a new
operation instead of overriding that refusal.

Graph progress is checkpointed after each repository and vault. A retry resumes
from the first unfinished source, but only when the checkpoint's captured
content digest still matches. Final source revalidation enumerates every input
again and uses strong filesystem change tokens to avoid rereading unchanged
files on supported systems; ambiguous or changed metadata always falls back to
content hashing. Bundle size and BLAKE3 validation stream through a fixed-size
buffer, so multi-gigabyte graph artifacts do not require multi-gigabyte heap
allocations.

## Cancellation and cleanup

Cancellation is cooperative at safe batch boundaries. Read the latest revision
from `publication status`, then request it with:

```sh
nestweaver publication cancel <uuid> --revision <revision> --db /path/to/brain.lbug
```

A cancelled or retryably failed operation can be resumed. If it is no longer
needed, discard it using its latest revision:

```sh
nestweaver publication discard <uuid> --revision <revision> --db /path/to/brain.lbug
```

The unfiltered status response reports valid operations and invalid journals
independently, so one incompatible or corrupt `state.json` cannot hide healthy
operations. An invalid journal has no trustworthy target-slot identity; discard
it explicitly with:

```sh
nestweaver publication discard <uuid> --invalid --db /path/to/brain.lbug
```

Normal discard never removes the selected publication. Invalid-journal discard
removes only the operation directory and preserves every publication slot for a
later retention pass.

## Rollback

The predecessor remains retained after activation. If post-cutover validation
finds a problem, stop the daemon and switch back without rebuilding:

```sh
nestweaver daemon --db /path/to/brain.lbug stop
nestweaver publication rollback --config /path/to/instance.toml
nestweaver daemon start --config /path/to/instance.toml
```

Rollback is intentionally one step. A second rollback is refused instead of
switching back to the abandoned publication; a later successful activation
establishes a new one-step predecessor. Keep the predecessor until the new
release has passed normal workload verification and a fresh backup has been
taken.

Rollback proves the currently selected graph is quiescent before changing
`CURRENT`; an idle predecessor alone is not sufficient. Selector changes and
destructive slot pruning also share a publication-root filesystem lock, so
separate processes cannot select and reclaim the same slot concurrently.

## Failure behavior

- Missing, stale, corrupt, incompatible, or foreign regex shards widen only the
  affected scope to a graph scan; they cannot silently remove matches.
- A regex candidate query that reaches its safety cap is treated as saturated
  and widens that scope to a graph scan; the cap can never truncate matches.
- Retiring a regex scope unlinks only its selector. Immutable generation files
  remain available to existing readers until a separate retention pass removes
  them.
- A sidecar write failure leaves the graph commit valid and its coalesced
  outbox work retryable.
- A failed source revalidation, seal, pointer switch, or startup smoke leaves or
  restores the incumbent selection and records an actionable operation error.
- Never delete the base database, publication root, or retained predecessor to
  retry an upgrade. Use resume, discard, or rollback.
