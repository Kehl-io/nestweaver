# v11.0.1 to v12.0.0 upgrade and recovery matrix

This is the supported Linux upgrade path exercised for the resolver-generation
migration. v12.0.0 is the first supported target in this matrix; older historical
fixtures are not implicitly supported.

| Starting state | Action | Expected result |
| --- | --- | --- |
| v11.0.1 indexed database, clean shutdown | Start v12.0.0 | Database opens and reports resolver-generation staleness until reindexed. |
| Same database | Force reindex with v12.0.0 | Reindex completes; stale check clears; symbol/dead-code queries return the fixture results. |
| Migrated database | Backup and inspect with v12.0.0 | Snapshot is readable and reports its contents and archive sizes separately. |
| Migrated database | Try to open it with v11.0.1 | **Not yet validated against the released v11 binary.** Until tested, rollback means restoring the separate pre-upgrade snapshot. |
| Pre-upgrade v11 snapshot | Restore with v12.0.0 | Restore is unchanged and explicitly requires a v12 reindex before serving. |
| Interrupted v12 reindex | Restart v12.0.0 | **Not yet validated.** Must be exercised before claiming crash/interruption recovery. |
| macOS v11.0.1 artifact | Upgrade and reindex with v12.0.0 | **Not yet validated.** Linux results do not establish macOS artifact compatibility. |

## Manual Linux smoke procedure

1. Obtain the published v11.0.1 Linux archive and its checksum file. Verify the
   checksum before extracting or running it.
2. Create a disposable database using v11.0.1 and index a small source fixture.
   Record a symbol query and a dead-code query as the baseline.
3. Stop v11 cleanly and copy the database and all database sidecars as the
   rollback snapshot. Keep this copy outside the active data directory.
4. Open the copied v11 database with v12.0.0. Confirm stale-check reports the
   required reindex, force-reindex, then confirm stale-check clears and repeat
   both fixture queries.
5. Save and inspect a v12 snapshot. Restore it to a separate directory and
   repeat the stale-check/reindex/query sequence.
6. Retain the pre-upgrade snapshot for rollback. Reopen that snapshot with v11
   only; do not try to roll back by opening the migrated database with v11.

The migrated-database-with-v11 refusal check is not part of this completed
procedure: it remains unvalidated against the released v11 binary, as recorded
in the matrix above. Interrupted reindex recovery and macOS artifact behavior
are also unvalidated.

The checked run for this backlog item used verified v11.0.1 and v12.0.0 Linux
x86_64 artifacts. It created and indexed a v11 fixture, upgraded/reindexed it
with v12, confirmed stale-check cleared and dead-code returned results, and
saved/restored a snapshot. It did not exercise reopening the migrated database
with v11, interrupted reindex recovery, or macOS artifact behavior; those remain
explicitly unclaimed.
