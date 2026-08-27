# Instance ID Migration Runbook

This runbook consolidates an **existing** database that was indexed before the
instance_id fix and therefore holds more than one `instance_id` convention.
Fresh databases indexed by a daemon started with `--config` already write one
consistent instance_id everywhere — you only need this if `list-repos`,
`list-projects`, and `brain list` disagree about the instance name.

## Why this exists

NestWeaver's runtime identity (the Unix domain socket, pidfile, launchd label,
and replica directory) is keyed off a short hash of the database's absolute
path. That hash exists because a socket path has a hard 104-byte `sun_path`
limit, so a full path can't be used as the socket name. Before the fix, that
db-path hash leaked out of the runtime layer and into the **graph data**: repos
were tagged with `instance_id = "<db-path-hash>"` (e.g. `c37ccf01`).

Meanwhile, two other subsystems used different conventions for the same
database:

- **Vaults** defaulted to the literal `instance_id = "default"` (`brain add` /
  `brain refresh` never threaded the config's instance_id through).
- **Projects** used the logical `instance_id` from `instance.toml` (e.g.
  `kory-brain`), which is what everything *should* have used.

The result was three conventions in one database. Because retrieval scopes by
instance_id, passing `--instance <logical-name>` matched the projects but was
rejected for symbols and vaults, since those rows were tagged with the hash or
with `default`.

Daemons started with `--config` now write the logical `instance_id` everywhere
(repos, symbols, vaults, and projects). This runbook fixes databases that were
indexed **before** that change.

## How to check your DB

Inspect each subsystem's instance tag. A mixed database looks like this:

```sh
# Repos — the "Instance:" line is the db-path hash on an old DB
nestweaver list-repos
#   repo:...:...
#     URL:      /Users/you/dev/workspaces/acme/acme-server
#     SHA:      a1b2c3d
#     Instance: c37ccf01          <- db-path hash (should be the logical name)

# Projects — already logical
nestweaver list-projects
#   acme
#     UID:      proj:kory-brain:...
#     Instance: kory-brain        <- logical name (correct)

# Vaults — instance is the middle segment of the vault UID (vlt:<instance>:<hash>)
nestweaver brain list
#   brain
#     UID:   vlt:default:9f8e7d…  <- "default" (should be the logical name)
#     Path:  /Users/you/brain
```

When the `Instance:` values (and the `vlt:<instance>:` segment) don't all match,
the database is mixed and this procedure applies. If they already agree, you're
done — nothing to migrate.

Add `--json` to any of these for machine-readable output, and `--db <path>` /
`--config <instance.toml>` to point at a non-default database.

## The consolidation procedure

Pick the **logical name** — the `instance_id` from your `instance.toml` (the
value already shown by `list-projects`, e.g. `kory-brain`) — and fold every
stray convention into it.

```sh
# 1. Merge each stray convention into the logical name.
#    `instance merge` rewrites the vault, project, and repo rows.
nestweaver instance merge --from c37ccf01 --to kory-brain   # the repo hash
nestweaver instance merge --from default  --to kory-brain   # the vault default

# 2. Re-index every repo the merge listed.
#    Merge removes each source repo's graph rows. Merge prints the exact repos
#    that need a forced re-index; re-index recreates their File/Symbol rows
#    under the target instance. Run one command per listed repo.
nestweaver index --repo /Users/you/dev/workspaces/acme/acme-server --force

# 3. Refresh the vault and re-materialize projects under the logical name.
nestweaver brain refresh /Users/you/brain --config ./instance.toml
nestweaver materialize-projects --config ./instance.toml

# 4. Verify one convention everywhere.
nestweaver list-repos
nestweaver list-projects
nestweaver brain list
```

`nestweaver instance merge` prints what it moved and, when it moved repos, the
follow-up you must run:

```
Merged 'c37ccf01' -> 'kory-brain': 0 vault(s), 2 repo(s), 0 project(s)

NOTE: source repo graph rows were removed during merge.
Force re-index each repo listed below; this recreates them under the target instance:
  /Users/you/dev/workspaces/acme/acme-server
  /Users/you/dev/workspaces/acme/acme-client
  nestweaver index --repo <path> --force
  nestweaver materialize-projects --config <instance.toml>
```

Run `nestweaver index --repo <path> --force` for **each** listed repo (step 2)
before you verify. If merge reports `No rows found with instance_id '<from>'`,
that convention wasn't present — skip it and move on.

`brain refresh` now honors `--config`: the vault is re-tagged with the config's
`instance_id` (the resolution order is `--instance` flag, then the config's
`instance_id`, then `"default"`). That's what moves the vault off `default`.

After the procedure, all three listings should show the same instance name and
the vault UID's `vlt:<instance>:` segment should match it.

## What stays hash-named (and why that's correct)

The **runtime** identity is intentionally still keyed off the db-path hash and
must not be "fixed":

- the Unix domain socket
- the pidfile
- the launchd label (macOS)
- the replica / runtime directory

These live under the runtime directory named for the hash because the socket
path is bounded by the 104-byte `sun_path` limit and must stay short and stable
per database file. Seeing hash-named sockets or launchd labels after migration
is **expected**, not drift — do not rename them or run `instance merge` against
them. Only the **graph data** (repos, symbols, vaults, projects) is consolidated
to the logical name.

## Going forward

- A daemon started with `--config <instance.toml>` writes the logical
  `instance_id` to every new row automatically — no manual step needed.
- `nestweaver index --instance <name>` overrides the instance for a single
  index run, if you ever need to place a repo under a specific name.
- `brain add`, `brain watch`, and `brain refresh` all resolve the instance the
  same way (`--instance` flag, then the config's `instance_id`, then
  `"default"`), so a `--config`-driven workflow stays consistent on its own.
- **A command that names NO instance adopts the one the database already
  holds**, rather than falling back to `"default"` and writing a second copy
  (nw-246). This is what makes an upgrade safe: a database indexed under
  7.0.0's db-path hash keeps that identity when you re-index without a config,
  instead of forking into two repo rows for one path — with two UIDs per
  symbol, `stale-check` permanently reporting a repo you just indexed as stale,
  and `prune-stale` unable to clean it because nothing is actually orphaned.
  The database records its instance on first write and reports it in
  `list-repos`.
- Naming an instance explicitly is still honoured, and a database may hold
  several. That is a supported configuration — a daemon restarted under a
  different `--config` writes new rows under the new instance, and
  `nestweaver instance merge` consolidates them when you want one. What is
  refused is the *ambiguous* case: an index with no `--instance` and no
  `--config` against a database that already holds more than one instance,
  where there is no safe default to pick.
- `brain refresh` additionally resolves from an **existing registration** for
  that vault root, and prefers it over the `"default"` fallback (nw-098). A
  refresh with no `--instance`/`--config` therefore refreshes the vault that is
  already there instead of registering a second one for the same root. An
  explicit instance that disagrees with the registration is refused, naming
  both, rather than creating the duplicate — a split root makes note counts the
  SUM of both vaults and `brain search` return duplicate rows.

Once a database is consolidated and you always start the daemon with `--config`,
it stays single-convention and you won't need this runbook again.

## Wedged migration journal

The daemon records instance migrations in a journal so an interrupted run can
be reconciled on the next boot. If the daemon refuses to boot because of a
wedged journal, clear it while the daemon is stopped:

```sh
nestweaver instance abort-migration --db ./brain.lbug
```

A `Prepared` journal (no graph mutation happened) is removed cleanly. A
`graph-applied` journal is refused unless `--force`, because the graph was
already mutated — restarting the daemon is preferred (boot self-heals a
re-runnable merge). `--force` discards the journal anyway, including an
unreadable/corrupt one whose phase is unknown; after a forced discard,
reconcile manually (re-run `instance merge` / re-index as needed).
