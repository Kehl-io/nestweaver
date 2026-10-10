# Watcher trigram regression evidence

## Delete and query behavior

On v12.0.0, a scratch vault configured with `with_trigrams = true` was watched,
then a note containing a unique search term was deleted. The note disappeared
from exact regex results and no stale posting returned it. During reconciliation
the ready/dirty counters were observed to fluctuate; dirty scopes correctly use
the scan fallback. This is safe, but it can produce a temporary stale-index
notice. The CLI notice now says that dirty scopes were scanned safely and only
recommends a forced rebuild when staleness persists; it no longer tells users
to enable an option that is already enabled.

## Rebuild process recovery

The v12.0.0 writer ownership fix is already present in `main` (PR #455): only
the reconciler and explicit requests refresh trigram postings under the daemon
write gate. Ten consecutive serial full rebuilds completed successfully on a
scratch fixture. The original kill-during-rebuild failure was not reproduced,
and the prior incident did not preserve a crash fixture or exact kill boundary.
This run is evidence against a repeatable failure, not proof of recovery from
an arbitrary process kill; no new independent writer defect was found to fix.

The repository retains ownership and outbox-drain regression coverage in
`tests/trigram_reconciler_ownership_test.rs` and the daemon reconciler tests.
