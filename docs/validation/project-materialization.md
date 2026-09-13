# Project materialization verification

The ignored `write::tests::project_materialization_paired_139509_benchmark` test
compares two algorithms on the same runner and pinned synthetic fixture. Its
legacy loop is copied from `batch_insert_project_symbol_edges` and the companion
note/component helpers at `ad0619fb0e50e65253b52ea74d99f2969f65fbf2`, immediately
before COPY migration `598d0c1787fdcbfff1738ff72f1357926fcd2f23`.

The reference obtains a connection and prepares once per project, then executes
one auto-committing MATCH+CREATE per symbol UID. The candidate uses the current
`replace_materialized_projects` entry point, including its graph-difference
planning, atomic COPY, and commit. Both use the current pinned LadybugDB build;
this measures the algorithm change, not all differences between historical
binaries. No raw mutation API was added to production for the benchmark.

Synthetic fixture v1 contains 11 projects, 12,683 symbols, 139,509 project-symbol
relationships, 91 project-note relationships and 5 component relationships.
Memberships enumerate project then symbol, stopping at exactly 139,509 edges.
Files group 28 symbols; every source UID and hash is deterministic. Both runs
seed an independent disk-backed database before timing and verify exact counts
and note/component endpoint pairs afterwards. This is a reproducible equivalent
of the historical edge count, not the unavailable historical production graph.

On an otherwise idle runner, compile once with workspace feature unification:

```sh
cargo test --workspace --release --lib --no-run
```

Run the produced `nestweaver_store-*` test executable twice with this exact
filter and `--ignored --exact --nocapture --test-threads=1`:

```
write::tests::project_materialization_paired_139509_benchmark
```

Set `NW_PROJECT_BENCH_REVERSE=1` for the second run to reverse candidate/reference
order. Retain raw stdout/stderr, wall time and peak RSS, CPU model, kernel,
commit, memory, filesystem and load observations. A compiled binary can be run
directly to keep compiler activity outside the measurement window.

The test emits `PROJECT_MATERIALIZATION_RUN` for each path and
`PROJECT_MATERIALIZATION_PAIRED` with their measured ratio. It fails if COPY is
less than 10 times faster or its exclusive write phase exceeds 120 seconds.
Legacy execute time necessarily includes implicit per-statement commits;
claiming a separately measured commit time for that historical API would be
misleading.

The independent `project_materialization_benchmark` example exercises the same
candidate fixture with tracing enabled. Store logs identify snapshot/difference
planning, each relationship COPY (including CSV serialization), and transaction
commit. Its JSON additionally records fixture planning, apply, held-write-lease
and total durations. This is the detailed candidate timing companion to the
paired comparison, not a substitute for its matched reference.

Correctness gates are the quoted-endpoint test with 256 plain sample rows then
44 comma/quote-bearing endpoints, and a disk-backed four-stage failure matrix
(note/symbol COPY and component/parent transactional CREATE). Each failure reopens the complete prior
project topology and verifies all four relationship types.

Project-to-Project topology uses one prepared transactional CREATE per edge. The
0.19.1 native engine left component adjacency storage unreadable after a later
parent COPY failure in the disk-backed fault matrix (including a component self
edge). The matrix retains that late failure and exact reopened graph checks;
only the small topology path avoids COPY. High-volume note/symbol memberships
still use COPY. Missing topology endpoints return an error rather than silently
matching zero rows. The paired benchmark measures this complete production path.
