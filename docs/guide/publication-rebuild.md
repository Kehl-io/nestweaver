# Publication rebuild and recovery

The regex-v3 and embedding-pipeline-v2 release uses a new publication format.
Upgrading an existing brain requires one complete graph reindex and re-embed.
NestWeaver builds that replacement beside the incumbent database and does not
change `CURRENT` until every artifact validates.

## Upgrade

Stop the daemon and any external watchers, then run:

```sh
nestweaver daemon stop --config /path/to/instance.toml
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
nestweaver daemon stop --config /path/to/instance.toml
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
