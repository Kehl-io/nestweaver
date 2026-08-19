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

Discard never removes the selected publication.

## Rollback

The predecessor remains retained after activation. If post-cutover validation
finds a problem, stop the daemon and switch back without rebuilding:

```sh
nestweaver daemon stop --config /path/to/instance.toml
nestweaver publication rollback --config /path/to/instance.toml
nestweaver daemon start --config /path/to/instance.toml
```

Rollback is intentionally one step. Keep the predecessor until the new release
has passed normal workload verification and a fresh backup has been taken.

## Failure behavior

- Missing, stale, corrupt, incompatible, or foreign regex shards widen only the
  affected scope to a graph scan; they cannot silently remove matches.
- A sidecar write failure leaves the graph commit valid and its coalesced
  outbox work retryable.
- A failed source revalidation, seal, pointer switch, or startup smoke leaves or
  restores the incumbent selection and records an actionable operation error.
- Never delete the base database, publication root, or retained predecessor to
  retry an upgrade. Use resume, discard, or rollback.
