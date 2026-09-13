# Filesystem mutation authority

Namespace restore and publication mutation retain their existing lockfiles for
compatibility and additionally hold an independent registry anchor keyed by the
canonical data directory or publication root. The registry lives under the effective UID's account-database home at
`.local/state/nestweaver/authority`, with a private 0700 directory
and 0600 files. It is deliberately independent of `HOME`, `XDG_STATE_HOME`, `TMPDIR`, replaceable data
roots, and publication `LOCK` files. Entries must not be pruned while any
NestWeaver processes may be running. An empty entry costs one inode; entries
contain no paths, credentials, PID authority, or graph data.

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
exact planned identity before the worker advances to Graph. The seed lives with
the journal until the journal is discarded; deleting its link does not delete
the publication's data link.

Checks at authority and mutation boundaries detect completed substitutions; they
are not an atomic filesystem sandbox against a same-UID actor racing every
individual LadybugDB path open. Such a guarantee would require descriptor-based
engine IO or a separately privileged broker. No product path intentionally
replaces registry entries or a held publication root.
