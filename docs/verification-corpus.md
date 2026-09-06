# The verification corpus (nw-413)

## Why this exists

Five open backlog items are blocked on the same missing artefact, and none of
them owned building it — so it could never be scheduled:

| Item | What it needs the corpus for |
| --- | --- |
| nw-291 | dead-code re-measurement at per-language scale |
| nw-308 | hub/bridge name-collision rate across many repos |
| nw-322 | `investigate --scope project:` latency on a large multi-repo graph |
| nw-351 | C++ entry-point discovery, re-measured |
| nw-358 | `stale_repos` ordering across routes, needs tens of repos |

Every one of those measurements had previously been taken against Kory's
personal brain DB. That DB is not reproducible by anyone else, so no result
taken on it could be independently checked or re-run after a fix.

## Why a fixture is not enough

This is the most-repeated finding in the backlog and it is the reason the
corpus is not optional:

- **nw-352** — 821 of 822 real C++ `class` definitions were not extracted (0.1%).
  **No fixture could see it**, because `testdata/cpp/simple.cpp`'s
  `class SensorManager` extracts fine. Found only against a real tree.
- **nw-338** (read-only schema migrations) and **nw-291**'s High-tier
  miscalibration were both found only against a disposable copy of the real
  1 GB graph.

Fixture-green is necessary, not sufficient.

## Usage

```sh
scripts/build-verification-corpus.sh            # ~20 repos, 9 languages
scripts/build-verification-corpus.sh --small    # 5 repos, for iterating
scripts/build-verification-corpus.sh --teardown # delete everything
```

Environment:

- `CORPUS_DIR` — where clones and the `.lbug` live (default `/tmp/nw-corpus`)
- `NW` — the binary to index with (default `cargo run --release --`)

## Design decisions worth not re-litigating

**Every repo is pinned to a ref, resolved and printed as a SHA at clone time.**
An unpinned corpus is not a corpus: a measurement taken against "whatever
`main` was that day" cannot be compared to the next one. That is exactly how
nw-291's re-measurement became uncomparable to its own filing. Most entries
are pinned to a release tag rather than a raw SHA — weaker pinning, since a
tag can move — and one entry (`TypeScript-Node-Starter`, which publishes no
tags) floats on `master` and is flagged as such in the script's output. The
script resolves and prints the concrete SHA each ref checked out to, so a
measurement can still record exactly what it ran against.

**A repo that fails to clone or index is reported, never silently skipped.** A
corpus that quietly indexed 12 of 20 repos would make every measurement taken
on it wrong in a direction nobody could see — which is the same silent-partial
-success class as nw-387 and nw-394.

**Teardown stops the daemon before deleting the DB.** Otherwise the next run
inherits a daemon pointing at a path that no longer exists (nw-377's wedge).

**Repos are chosen for size, not just language spread.** The corpus has to
exceed the internal caps under test or it cannot prove they disclose
themselves: `DEFAULT_RETRIEVAL_BREADTH` 30, `HUB_COUNT` 30,
`MAX_CLUSTER_SUMMARIES` 50, `bound_identifiers`'s `MAX_IDENTIFIER_COUNT` 1000.

## Definition of done

nw-413 is closed when **one of the five dependent items has been re-measured
with this corpus and closed or updated on the result** — not when the script
exists. A fixture nobody has used is the same dead end as no fixture.
