# Filesystem mutation authority

Namespace restore and publication mutation retain their existing lockfiles for
compatibility and additionally hold an independent registry anchor keyed by the
canonical data directory or publication root. The registry lives under the effective UID's account-database home at
`.local/state/nestweaver/authority`, with a private 0700 directory
and 0600 files. It is deliberately independent of `HOME`, `XDG_STATE_HOME`, `TMPDIR`, replaceable data
roots, and publication `LOCK` files. Each entry costs one inode. Since nw-811
an entry records the domain and canonical path it guards
(`nestweaver-authority v1\n<domain>\0<path>`); it never holds credentials,
PID authority, or graph data. Entries written by earlier releases are empty.

Every scratch, test, CI and QA database mints entries, so `nestweaver daemon gc`
prunes the registry (`--dry-run` reports without changing anything). An entry
is unlinked only while gc holds its exclusive lock and the pathname still
names the locked inode, so an entry held by any process, shared or exclusive,
is never displaced. An acquirer that opened an entry just before gc unlinked
it sees the replacement and retries onto a fresh entry. A self-describing
entry is removed when its path no longer exists. An empty legacy entry cannot
name its database, so it is removed only when it is older than 24 hours,
unheld, and matches no anchor of a live database named by a daemon boot log
or `--db` (data directory, publication root, slots and operation journals).
Older binaries without the retry fail closed ("registry entry was replaced")
if they lose that microsecond race; they never gain a second owner.
`daemon gc --db X` limits the prune to the records whose scope belongs to X
(its data directory, publication root, slots and operation journals) and
leaves every legacy entry alone; without `--db` the whole registry is swept. A
registry gc refuses to sweep (for example a root that is not a private owner
directory) is reported as a warning, and gc still exits 0 when its launch-agent
and directory sweeps succeeded.

Lock authority for a real database is always the account registry. Debug
builds honour an absolute `NESTWEAVER_AUTHORITY_DIR` registry override, and
only for a scope under the system temp directory (`std::env::temp_dir()` and
`/tmp`, canonical or not; on macOS that includes `/private/var/folders` and
`/private/tmp`). `.cargo/config.toml` sets it to `target/test-authority` for
every process cargo runs, so test suites, whose databases are tempdirs, no
longer write to the account registry. A database anywhere else uses the
account registry even under `cargo run`, so a `cargo run` CLI, the launchd
daemon and every worktree still exclude each other on it. Release builds
ignore the variable entirely, so a `cargo test --release` run (CI's macOS job)
still writes its tempdir records to the account registry; `daemon gc` reclaims
them, because their scope is gone. A debug `daemon gc` with the override set
sweeps the override registry; a release binary sweeps the account registry.

This is a cooperating-process protocol. It assumes the private registry and its ancestors remain
intact. Account-home resolution uses `getpwuid_r`, avoiding temporary-directory
cleaners and inconsistent launch environments. An absent account home fails
closed instead of falling back to a second registry. Arbitrary same-UID mutation of registry entries, changing their access
permissions, or direct writes to database bytes is outside the guarantee: an
owner can always bypass their own advisory locks. Registry symlinks, foreign
ownership, and permissive modes are refused. Descriptor/path identity is checked
after acquisition and when authorizing mutation, so detected substitution
revokes an incumbent guard. This is not a claim that a fresh process can detect
an arbitrary replacement of all trusted registry state. Excluding that threat
requires an authority outside the user's write permissions.

Within this boundary, replacing either restore namespace lockfile (live or
`.restoring`), replacing publication `LOCK`, or renaming and recreating the
publication root cannot admit another upgraded writer. Publication operations
also flock the root directory descriptor: the displaced root cannot acquire a
second owner under its new name. Exact-root authorization compares its pinned
inode, canonical path, LOCK inode and registry entry. Unrelated sibling roots
and namespaces remain independent. Existing lockfiles still exclude older
cooperating clients during normal operation; replacement resistance requires
upgraded clients. Descriptor locks are CLOEXEC with bounded fork-before-exec
retries, matching the existing protocol on Linux and macOS.

Fresh staged creation checks the held database descriptor's device/inode against
the current non-symlink path. A path plus a zero length is insufficient proof.
For resumable Planned creation, a private random seed inode is created and
synced in the operation journal directory. Its exact plan, canonical target,
device and inode are durably recorded before a hard link publishes the staged
target name. The surviving seed link prevents inode reuse while the plan is
recoverable. A retry validates that exact seed and target before binding writer
authority. Unrelated empty files, changed plans, symlinks and replacement inodes
are refused. A nonempty target must reopen without migration and carry the
exact planned identity before the worker advances to Graph. After Graph is durably checkpointed, the worker retires the seed and its
creation record before indexing. Graph-entry cleanup is idempotent across
interruption; completed journals cannot keep pruned database storage alive.
Deleting the seed link does not delete the publication's data link.

Checks at authority and mutation boundaries detect completed substitutions; they
are not an atomic filesystem sandbox against a same-UID actor racing every
individual LadybugDB path open. Such a guarantee would require descriptor-based
engine IO or a separately privileged broker. No product path intentionally
replaces registry entries or a held publication root.

Restore also holds exact database writer leases for the target and aside
inventories. Its local journal v4 binds a mandatory replacement policy, directory identity,
allowed paths and
immutable per-file metadata witnesses; it does not change the archive format.
Archive validation, owned operational-lock preparation and durable physical
marker binding precede the finalized destination receipt. Recovery checks that
receipt without reopening or rehashing a leased graph and never refreshes it to
accept changed files. Original-copy promotion requires complete original proof;
cleanup requires proof for every surviving original file. Uncertified target
or aside copies are preserved outside active slots with their paths disclosed,
or recovery refuses retaining the copies and journal. These checks remain
within the cooperating-process and same-UID boundary described above.

Live-layout recognition grants preservation, not recursive-delete authority.
Its original-file proof exists only inside the mandatory v4 preserve receipt;
legacy deletion fields stay absent. A version/tag loss cannot turn that proof
into legacy cleanup authority. The complete old live dataset moves through
held source/reservation directory descriptors with atomic no-replace semantics;
post-rename namespace and immutable original-file checks precede journal
retirement. Copied catalogs alone prove no ownership. All retained paths are
reported and remain outside active slots, consuming disk space until reviewed.
