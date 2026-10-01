# Publication rebuild and recovery

The regex-v3 and embedding-pipeline-v2 release uses a new publication format.
Upgrading an existing brain requires one complete graph reindex and re-embed.
NestWeaver builds that replacement beside the incumbent database and does not
change `CURRENT` until every artifact validates.

## Storage-engine upgrade (LadybugDB 0.21): rebuild required

This release updates LadybugDB to 0.21 (0.21.1), which changes how text keys are
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

1. Stop the daemon and run `nestweaver backup save <file>`. This works with
   either version: this release reads the old database read-only (no
   checkpoint, no marker) and seals the archive as pre-upgrade, so restoring
   it later warns that it must be rebuilt.
2. Run the command the error prints (below). The
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

`publication rollback` cannot select a predecessor built by an older storage
engine: this version refuses to open it. To roll back across the upgrade,
reinstall the previous NestWeaver version and restore the backup you took
before rebuilding. If a database built by an older engine was left mid-way
through a checkpoint (a `<db>.wal.checkpoint` or `<db>.shadow` beside it),
open it once with the previous NestWeaver version to finish that checkpoint,
then back it up and rebuild.

The same release changes these crash-recovery behaviours:

* A stale `<db>.wal.checkpoint` from another database is now renamed to
  `<db>.wal.checkpoint.stale-<epoch>`, never deleted.
* A frozen `<db>.wal.checkpoint` whose pages were already applied before a
  crash (its `<db>.shadow` is gone or empty) is reported, by the daemon's
  preflight as well as by any open, with the single safe move,
  `mv <db>.wal.checkpoint <db>.wal.checkpoint.applied`. Leave `<db>.wal` in
  place: it holds later commits. Keep the `.applied` file: after a power loss
  (not just a crash) it may still hold committed records. Nothing is moved
  automatically.
* A recovery that cannot finish an interrupted checkpoint keeps the frozen
  log and retries on the next open. It is reported as a resource problem only
  when the engine's cause is a full buffer pool or disk (free space, or raise
  `NESTWEAVER_LBUG_BUFFER_POOL_BYTES`); any other cause is shown as the engine
  wrote it. Either way, do not move log files aside.
* A write whose post-commit checkpoint the engine defers is reported as
  committed and is never retried. The daemon reopens the store by itself to
  finish the checkpoint (`brain_status` shows `store_reopen_pending` until it
  does). If reads keep the store busy for about a minute, it briefly holds off
  new reads so the reopen can happen.
* A daemon whose selected publication was left mid-checkpoint now boots and
  finishes the checkpoint itself; resolving `CURRENT` no longer needs a
  read-only open that such a slot refuses.

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
cross-repository call links (inferred once, after every source is indexed),
projects, note→code links, BM25, per-scope regex shards, embeddings, and
ranking metadata, then revalidates the inputs before the atomic switch. Note→code
links are built with the same reconciler the daemon uses (`[Graph] linking notes
to code`), and every fully indexed vault is recorded as derived by the current
Markdown link rules, so the first daemon start after the switch serves at once:
it owes no code-link debt and re-derives no vault. Validation refuses a staged
graph whose links are owed or were built by other link rules. Interaction history is copied
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

### Resuming after the sources changed

A rebuild of a large brain takes long enough (about an hour for tens of
repositories) that a source often changes before it finishes, and final
validation then refuses the cutover. Resume the same operation: it compares the
current inputs with the ones the operation recorded and re-indexes only the
repositories and vaults whose content or commit changed into the staged slot.
What it recomputes:

- **Per changed source:** the source's graph, and the embeddings of its nodes
  whose content changed (symbols by content hash; notes by content hash; a
  note's headings whenever the note changed at all, since a heading is embedded
  with its note's title). Unchanged sources keep their staged graph and vectors.
- **Over the whole graph, every time:** cross-repository call links (inferred
  after every source is indexed, so they do not depend on indexing order),
  project membership, note→code links, BM25, and ranking. Regex shards are
  refreshed for the scopes whose content moved.

It then validates again. The staged graph (symbols, notes, and symbol,
cross-repository, and note→code edges) matches what a fresh rebuild of the
changed sources produces. Cross-repository links are inferred from each
file's parse at the content the graph indexed: a parse missing from the parse
cache is re-parsed from the working tree, and a file that no longer has that
content stops the rebuild before any link is replaced, so a lost cache never
silently drops links. A scoped resume reports:

```text
Resume: 1 input(s) changed since the build recorded them (repository file:///src/app); re-indexing only those, then rebuilding derived state and validating.
[Graph] re-indexing changed repository file:///src/app
[Embeddings] embedding changed nodes
```

Some changes cannot be scoped to a source: the instance configuration changed,
a repository or vault was added, removed, moved, or renamed, or the staged slot
was already sealed. Resume then says why, discards the operation, and starts a
full rebuild under a new operation:

```text
Resume cannot re-index only what changed: the instance configuration changed. Discarding operation <uuid> and starting a full rebuild.
```

A changed binary version, publication format, or database identity is still
refused; discard the operation and start a new one.

### Pause writers to the sources during a rebuild

Every change to a source between the start of a rebuild and its validation
costs a scoped re-index on resume, and a change to which sources exist costs a
full restart. Before a long rebuild, pause what writes to the indexed sources:
stop the daemon and any external watchers (as above), and hold off on commits,
checkouts, `git pull`, branch switches, and editor or sync tools (Obsidian
Sync, iCloud, Dropbox) writing into indexed repositories and vaults until the
rebuild reports `Publication <uuid> is Activated`. Do not add or remove repositories or
vaults, or edit the instance configuration, while it runs.

Graph progress is checkpointed after each repository and vault. A retry resumes
from the first unfinished source; a source whose captured content digest no
longer matches is re-indexed as described above. Final source revalidation enumerates every input
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
