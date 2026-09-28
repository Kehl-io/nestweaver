//! The `backup` and `snapshot` command families and their restore/quiesce guards.
//!
//! Moved verbatim out of `src/main.rs` (nw-348).

use crate::*;

pub(crate) fn run_backup(command: BackupCommands) -> anyhow::Result<i32> {
    match command {
        BackupCommands::Save {
            output,
            db,
            config,
            include_clones,
            // `--force` is obsolete: the daemon now backs up under its own write
            // lock (there is no client-side quiesce that can fail).
            force: _,
        } => {
            let db_path = resolve_db_with_config(db, config.as_deref())?;
            if !db_path.exists() {
                anyhow::bail!(
                    "database not found at {}; run 'nestweaver index' first",
                    db_path.display()
                );
            }
            let db_path = std::fs::canonicalize(&db_path).unwrap_or(db_path);

            let instance_id = nestweaver_daemon::lifecycle::instance_id_from_db_path(&db_path);

            let workspace_path = if include_clones {
                db_path.parent().map(|p| p.join("workspace"))
            } else {
                None
            };

            let config = nestweaver_engine::BackupConfig {
                db_path: db_path.clone(),
                output_path: output,
                include_clones,
                instance_id: instance_id.clone(),
                workspace_path,
            };

            let rt = tokio::runtime::Runtime::new()?;
            #[cfg(target_os = "macos")]
            let launchd_running = nestweaver_daemon::launchd::is_running(&instance_id);
            #[cfg(not(target_os = "macos"))]
            let launchd_running = false;
            let existing_daemon =
                rt.block_on(nestweaver_client::DaemonClient::connect_existing(&db_path));
            let daemon_running = launchd_running || existing_daemon.is_ok();

            if daemon_running {
                // The daemon owns the files and performs the whole backup
                // in-process (holding its own write lock), so a single RPC does
                // it — no client-side quiesce/copy, and it works even when the
                // client does not share the daemon's filesystem.
                eprintln!("Backing up via the running daemon...");
                let mut client = existing_daemon
                    .map_err(|e| anyhow::anyhow!("failed to connect to daemon: {e}"))?;
                let resp = rt
                    .block_on(async {
                        client
                            .inner_mut()
                            .backup(nestweaver_proto::BackupRequest {
                                output_path: config.output_path.to_string_lossy().into_owned(),
                                include_clones,
                            })
                            .await
                    })
                    .map_err(|e| anyhow::anyhow!("Backup RPC failed: {e}"))?
                    .into_inner();

                for warning in &resp.warnings {
                    eprintln!("Warning: {warning}");
                }
                eprintln!("Backup saved to {}", resp.output_path);
                eprintln!("  Instance:     {}", resp.instance_id);
                eprintln!("  Tier:         {}", resp.tier);
                eprintln!("  Version:      {}", resp.nestweaver_version);
                eprintln!("  Repos:        {}", resp.repo_count);
                eprintln!("  Symbols:      {}", resp.symbol_count);
                eprintln!("  DB size:      {}", format_bytes(resp.db_size_bytes));
                eprintln!("  Compressed:   {}", format_bytes(resp.total_compressed));
                return Ok(EXIT_SUCCESS);
            }

            eprintln!("Creating backup...");
            let result = nestweaver_engine::backup_save(&config)?;
            let m = &result.manifest;
            for warning in &m.warnings {
                eprintln!("Warning: {warning}");
            }

            eprintln!("Backup saved to {}", result.output_path.display());
            eprintln!("  Instance:     {}", m.instance_id);
            eprintln!("  Tier:         {}", m.tier);
            eprintln!("  Version:      {}", m.nestweaver_version);
            eprintln!("  Created:      {}", m.created_at);
            eprintln!("  DB size:      {}", format_bytes(m.sizes.db));
            eprintln!(
                "  Uncompressed: {}",
                format_bytes(m.sizes.total_uncompressed)
            );
            eprintln!(
                "  Write pause:  {}ms",
                result.write_pause_duration.as_millis()
            );
            eprintln!("  Total time:   {}", format_elapsed(result.duration));
            Ok(EXIT_SUCCESS)
        }
        BackupCommands::Inspect { path } => {
            let manifest = nestweaver_engine::backup_inspect(&path)?;
            for warning in &manifest.warnings {
                println!("Warning: {warning}");
            }
            println!("NestWeaver Snapshot -- {}", path.display());
            println!("  Instance:     {}", manifest.instance_id);
            println!("  Created:      {}", manifest.created_at);
            println!(
                "  Version:      {} (schema v{})",
                manifest.nestweaver_version, manifest.schema_version
            );
            println!(
                "  Tier:         {}{}",
                manifest.tier,
                if manifest.tier == "standard" {
                    " (no git clones)"
                } else {
                    ""
                }
            );
            println!("  Repos:        {}", manifest.repo_count);
            println!("  Symbols:      {}", manifest.symbol_count);
            println!(
                "  Uncompressed: {}",
                format_bytes(manifest.sizes.total_uncompressed)
            );
            println!(
                "  Compressed:   {}",
                format_bytes(manifest.sizes.total_compressed)
            );
            println!("  Checksums:    {} file(s)", manifest.checksums.len());
            Ok(EXIT_SUCCESS)
        }
        BackupCommands::List { dir } => {
            let items = nestweaver_engine::backup_list(&dir)?;
            if items.is_empty() {
                println!("No snapshots found in {}", dir.display());
                return Ok(EXIT_SUCCESS);
            }
            println!("Available snapshots in {}:", dir.display());
            for (path, m) in &items {
                let filename = path
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_default();
                println!(
                    "  {}  {}  {}  {} repos  v{}  {}",
                    m.created_at,
                    m.tier,
                    format_bytes(m.sizes.total_compressed),
                    m.repo_count,
                    m.nestweaver_version,
                    filename,
                );
            }
            Ok(EXIT_SUCCESS)
        }
        BackupCommands::Restore {
            path,
            data_dir,
            start,
        } => {
            eprintln!("Restoring backup from {}...", path.display());

            // Refuse if a daemon is live on the target: restore renames the live
            // dir aside and deletes it, so a running daemon would keep writing to
            // unlinked inodes and the restored state would silently diverge.
            //
            // A pure read, run FIRST so that this refusal — the one that can
            // name the daemon and its pid — mutates nothing whatsoever.
            ensure_no_live_daemon_for_restore(&data_dir)?;

            // THE authorization, and it is held for the whole restore: through
            // the rename-aside, the cutover, the recovery journal and the
            // cleanup. The pidfile above permits absent, empty and stale
            // states; none of those is evidence that nobody is writing, and
            // this is what supplies that evidence.
            let config = nestweaver_engine::RestoreConfig {
                snapshot_path: path,
                data_dir: data_dir.clone(),
            };

            let result = with_exclusive_restore_access(&data_dir, || {
                nestweaver_engine::backup_restore(&config)
            })?;
            let m = &result.manifest;
            for warning in &m.warnings {
                eprintln!("Warning: {warning}");
            }

            eprintln!("Backup restored to {}", data_dir.display());
            eprintln!("  Instance:     {}", m.instance_id);
            eprintln!("  Version:      {}", m.nestweaver_version);
            eprintln!("  Tier:         {}", m.tier);
            eprintln!("  Repos:        {}", m.repo_count);
            eprintln!("  Symbols:      {}", m.symbol_count);
            eprintln!("  Restored in:  {}", format_elapsed(result.duration));

            if let Some(preserved) = &result.preserved_copy {
                eprintln!();
                eprintln!(
                    "An earlier restore of this data directory was interrupted, and the copy of \
                     your pre-restore data it left behind could not be PROVEN redundant, so it \
                     was preserved rather than deleted:"
                );
                eprintln!("  {}", preserved.display());
                eprintln!(
                    "Check the restored data, then remove it: rm -rf {}",
                    preserved.display()
                );
            }

            if m.tier == "standard" {
                eprintln!();
                eprintln!(
                    "Standard-tier restore: git clones not included. \
                     Start the daemon to re-clone repos in the background."
                );
            }

            if start {
                eprintln!();
                eprintln!("Starting daemon with restored data...");
                let lbug = find_lbug_in_dir(&data_dir);
                if let Some(db) = lbug {
                    eprintln!("  Database: {}", db.display());
                    let exe =
                        std::env::current_exe().unwrap_or_else(|_| PathBuf::from("nestweaver"));
                    let db_str = db.display().to_string();
                    match std::process::Command::new(&exe)
                        .args(["daemon", "run", "--db", &db_str])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                    {
                        Ok(mut child) => {
                            // spawn() returns as soon as the fork succeeds — before
                            // the daemon initializes or binds. Give it a moment, then
                            // confirm it did not immediately exit (port/socket
                            // conflict, unreadable db, ...) before claiming success.
                            std::thread::sleep(std::time::Duration::from_millis(700));
                            match child.try_wait() {
                                Ok(Some(status)) => {
                                    eprintln!(
                                        "  Daemon exited immediately ({status}) — it is NOT running."
                                    );
                                    eprintln!(
                                        "  Run manually to see the error: nestweaver daemon run --db {}",
                                        db.display()
                                    );
                                }
                                Ok(None) => {
                                    eprintln!("  Daemon started (pid {})", child.id());
                                }
                                Err(e) => {
                                    eprintln!(
                                        "  Daemon spawned (pid {}) but its status could not be \
                                         confirmed: {e}",
                                        child.id()
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("  Failed to start daemon: {e}");
                            eprintln!(
                                "  Run manually: nestweaver daemon run --db {}",
                                db.display()
                            );
                        }
                    }
                } else {
                    eprintln!(
                        "  No .lbug file found in {}; launch manually.",
                        data_dir.display()
                    );
                }
            }

            Ok(EXIT_SUCCESS)
        }
    }
}

/// Find the first .lbug file in a directory (non-recursive).
pub(crate) fn find_lbug_in_dir(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .find_map(|entry| {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("lbug") && p.is_file() {
                Some(p)
            } else {
                None
            }
        })
}

/// Every incumbent database a restore of `data_dir` is about to rename aside,
/// copy over, or delete.
///
/// Both the live data directory AND a `<data>.restoring` left by an earlier
/// interrupted restore, because the restore reconciles that directory too —
/// and because a daemon whose data directory was already renamed aside still
/// holds its lease on the inode that now lives under `.restoring`. Looking
/// only at `data_dir` would create a brand-new, trivially-free lease file
/// beside a partial copy and conclude that nobody was writing.
pub(crate) fn restore_lease_targets(data_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut targets = Vec::new();

    for dir in [data_dir.to_path_buf(), data_dir.with_extension("restoring")] {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                anyhow::bail!(
                    "cannot enumerate incumbent databases in {} before destructive restore: {error}",
                    dir.display()
                )
            }
        };

        for entry in entries {
            let entry = entry.with_context(|| {
                format!(
                    "cannot enumerate every incumbent database in {} before destructive restore",
                    dir.display()
                )
            })?;
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("lbug") {
                continue;
            }

            let metadata = std::fs::metadata(&path).with_context(|| {
                format!(
                    "cannot inspect possible incumbent database {} before destructive restore",
                    path.display()
                )
            })?;
            if metadata.is_file() {
                targets.push(nestweaver_daemon::lifecycle::canonical_db_path(&path));
            }
        }
    }

    // Every restore takes leases in the same canonical order, so two recovery
    // attempts cannot deadlock by discovering directory entries differently.
    // Canonicalisation also collapses symlink aliases to one lock identity.
    targets.sort();
    targets.dedup();
    Ok(targets)
}

/// Hold the canonical database write lease over every database a restore is
/// about to destroy, for as long as the restore runs.
///
/// This is the authorization `backup restore` never had. Its only guard was
/// [`ensure_no_live_daemon_for_restore`], which reads a pidfile — advisory
/// runtime metadata whose flock is held on an INODE, so unlinking `daemon.pid`
/// silently defeats every path-based owner check — and which PERMITS absent,
/// empty and stale pidfiles. None of those states says anything about whether
/// a process is writing the database right now. Restore then renames the
/// directory aside and `remove_dir_all`s it, so a daemon with a missing
/// pidfile, a standalone watcher, or any direct writer kept operating on
/// unlinked inodes while the directory was replaced underneath it: split
/// brain, and the loss of the current data.
///
/// The lease is the same one `index`, `watch`, `embed`, `brain watch`, the
/// vault commands, the Tantivy rebuild and the daemon itself take. It combines
/// lbug-compatible database ownership with descriptor-scoped database and
/// sidecar locks, while restore additionally closes the stable namespaces for
/// both the live and rename-aside data directories before enumeration. The
/// kernel releases every claim on process exit, so there is no stale ownership
/// record to reap.
///
/// A probe answers "was anyone holding this a moment ago". Only a HELD lease
/// answers "is anyone holding this for as long as I am deleting their data",
/// which is the question a check-then-destroy cannot ask.
#[must_use = "the leases must be HELD for the whole restore — dropping them \
              immediately reduces this to a probe, and the window that reopens \
              is precisely the one in which the data directory is renamed aside \
              and unlinked"]
pub(crate) fn require_exclusive_restore_access(
    data_dir: &Path,
) -> anyhow::Result<RestoreWriteAuthority> {
    // Close both namespaces before the first enumeration. Every upgraded
    // database creator takes a shared claim on a stable lock file in its data
    // directory's parent before creating/opening a database; restore takes the
    // exclusive form for both the live directory and the `.restoring`
    // rename-aside directory it reconciles. Each file is keyed by the exact
    // data-directory name, so unrelated siblings remain independent.
    let mut namespaces = Vec::with_capacity(2);
    for namespace_dir in [data_dir.to_path_buf(), data_dir.with_extension("restoring")] {
        match nestweaver_daemon::lifecycle::acquire_db_namespace_lease(&namespace_dir) {
            Ok(lease) => namespaces.push(lease),
            Err(nestweaver_daemon::lifecycle::WriteLeaseError::Held) => anyhow::bail!(
                "cannot restore a backup over {}: a writer holds the database namespace write lease for {}; stop every writer and retry",
                data_dir.display(),
                namespace_dir.display()
            ),
            Err(nestweaver_daemon::lifecycle::WriteLeaseError::Unavailable(error)) => {
                anyhow::bail!(
                    "cannot prove exclusive ownership of the database namespace containing {}: {error}; refusing destructive restore",
                    namespace_dir.display()
                )
            }
        }
    }
    let targets = restore_lease_targets(data_dir)?;
    let leases = targets
        .iter()
        .map(|db| {
            let namespace = namespaces
                .iter()
                .find(|namespace| namespace.authorizes(db))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "no held restore namespace covers incumbent database {}",
                        db.display()
                    )
                })?;
            match nestweaver_daemon::lifecycle::acquire_db_write_lease_under_namespace(
                db,
                namespace,
            ) {
                Ok(lease) => Ok(lease),
                Err(nestweaver_daemon::lifecycle::WriteLeaseError::Held) => anyhow::bail!(
                    "cannot restore a backup over this data directory: another process holds the write lease for {}. Stop the holder first, then retry.",
                    db.display()
                ),
                Err(nestweaver_daemon::lifecycle::WriteLeaseError::Unavailable(error)) => {
                    anyhow::bail!(
                        "cannot take the write lease for {} before destructive restore: {error}",
                        db.display()
                    )
                }
            }
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    // Re-enumerate as an invariant check. Unlike the previous check-only
    // scheme, the exclusive namespace authority remains held after this read
    // and through cutover, so an upgraded late creator cannot enter after it.
    let observed = restore_lease_targets(data_dir)?;
    anyhow::ensure!(
        observed == targets,
        "the incumbent database set changed while restore write leases were being acquired — refusing destructive restore; stop every writer and retry"
    );

    Ok(RestoreWriteAuthority {
        _namespaces: namespaces,
        _leases: leases,
    })
}

#[must_use = "dropping this authority reopens the restore namespace"]
#[derive(Debug)]
pub(crate) struct RestoreWriteAuthority {
    // Drop database leases before the enclosing namespace authorities.
    _leases: Vec<nestweaver_daemon::lifecycle::DbWriteLease>,
    _namespaces: Vec<nestweaver_daemon::lifecycle::DbNamespaceLease>,
}

/// Run the destructive restore phase under exact database and namespace
/// authority, then release that authority before the caller performs any
/// post-restore work such as `--start` daemon launch.
pub(crate) fn with_exclusive_restore_access<T>(
    data_dir: &Path,
    restore: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let authority = require_exclusive_restore_access(data_dir)?;
    let result = restore();
    drop(authority);
    result
}

impl RestoreWriteAuthority {
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self._leases.len()
    }
}

/// Refuse a restore while a live daemon serves the target data directory.
///
/// **This is not the authorization.** [`require_exclusive_restore_access`] is;
/// this runs first only because it is a pure read that can name the daemon and
/// its pid, which is a better message than "another process holds the lease",
/// and because a refusal here mutates nothing at all — not even a lease file.
/// Every permissive answer below is now backed by the write lease, so an
/// absent, empty or stale pidfile can no longer authorize anything on its own.
///
/// Restore renames the live data dir aside and `remove_dir_all`s it. If a
/// daemon is actively serving that dir, it keeps writing to now-unlinked inodes
/// and the restored state silently diverges. Mirror the snapshot-build quiesce
/// guard (commit 9a1e6fa): derive the instance from the target's `.lbug`, probe
/// the pidfile for a live daemon, and refuse if one holds it.
///
/// Because restore is *destructive*, this **fails closed** — unlike the
/// non-destructive snapshot-build guard, an unreadable/garbage pidfile refuses
/// rather than permits. The precondition is "the daemon is provably stopped";
/// anything we cannot confirm is treated as still-running:
/// - no `.lbug` in `data_dir` (fresh target, nothing to serve) → **permit**
/// - pidfile absent → **permit**
/// - pidfile present but EMPTY → **permit** (see below)
/// - pidfile parses to a live pid → **refuse**
/// - pidfile parses to a dead/stale pid → **permit**
/// - pidfile present but unreadable / unparseable → **refuse**
///
/// Empty is "no PID is claimed here", which is the same fact as an absent
/// pidfile and must be permitted for the same reason. It is not merely
/// hypothetical: [`retract_failed_start_pidfile`] produces exactly this state
/// after a start that lost the database-lock race. Treating it as unparseable
/// refused the restore and told the operator to "remove the stale pidfile" —
/// pushing them toward the `rm daemon.pid` that causes the runtime-ownership
/// incident. Fail-closed on garbage is unchanged; empty is not garbage.
///
/// What that permission cost, before the write lease was required, is the
/// whole of this guard's former job: "no PID is claimed here" and "nobody is
/// writing" are different facts, and only the second one may authorize a
/// destructive restore.
pub(crate) fn ensure_no_live_daemon_for_restore(data_dir: &Path) -> anyhow::Result<()> {
    let Some(db) = find_lbug_in_dir(data_dir) else {
        return Ok(());
    };
    let instance_id = nestweaver_daemon::lifecycle::instance_id_from_db_path(&db);
    let pidfile = nestweaver_daemon::lifecycle::pidfile_path(&instance_id);

    match std::fs::read_to_string(&pidfile) {
        // No pidfile → nothing claims this data dir → permit.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        // Present but unreadable → cannot confirm the daemon is stopped, and
        // restore is destructive → fail closed.
        Err(e) => anyhow::bail!(
            "a pidfile exists at {} but could not be read to confirm the daemon is stopped \
             ({e}) — refusing a destructive restore. Stop the daemon \
             (`nestweaver daemon stop`) or remove the stale pidfile, then retry.",
            pidfile.display(),
        ),
        // Present but EMPTY → claims no PID at all, which is the same fact as
        // an absent pidfile → permit. `retract_failed_start_pidfile` leaves
        // exactly this after a failed start; refusing here would tell the
        // operator to remove the pidfile by hand, which is the action this
        // whole branch exists to stop provoking.
        Ok(contents) if contents.trim().is_empty() => Ok(()),
        Ok(contents) => match contents.trim().parse::<i32>() {
            // Present but garbage (non-numeric) → cannot confirm → fail closed.
            Err(_) => anyhow::bail!(
                "a pidfile exists at {} but could not be parsed to confirm the daemon is \
                 stopped — refusing a destructive restore. Stop the daemon \
                 (`nestweaver daemon stop`) or remove the stale pidfile, then retry.",
                pidfile.display(),
            ),
            // Live pid → daemon is running → refuse.
            Ok(pid) if nestweaver_client::autostart::is_process_alive(pid) => anyhow::bail!(
                "a daemon (pid {pid}) is running on the target data directory {} — restoring \
                 would rename its live files aside and delete them while it keeps writing to the \
                 unlinked inodes, silently diverging the restored state. Stop it with \
                 `nestweaver daemon stop` and retry.",
                data_dir.display(),
            ),
            // Dead/stale pid → daemon is gone → permit.
            Ok(_) => Ok(()),
        },
    }
}

/// Quiesce guard for `snapshot build`. A snapshot is a raw copy of the graph
/// file; if a daemon is actively writing this DB the copy can be torn — and a
/// torn copy still passes verify/load — so refuse unless the DB is quiesced.
///
/// The guarded instance id is derived from the **DB path**, never from a
/// `--instance` flag: `snapshot build --instance <other>` would otherwise check
/// the wrong pidfile, miss the daemon actually writing this DB, and capture a
/// torn hot-copy. Fail CLOSED on a present-but-unreadable/garbage pidfile — we
/// cannot confirm the daemon is stopped, so refuse rather than risk a torn
/// snapshot. An EMPTY pidfile claims no PID and is treated like an absent one;
/// see [`ensure_no_live_daemon_for_restore`], which this mirrors, for why that
/// distinction matters after a failed `daemon start`.
pub(crate) fn ensure_no_live_daemon_for_snapshot_build(db_path: &Path) -> anyhow::Result<()> {
    let instance_id = nestweaver_daemon::lifecycle::instance_id_from_db_path(db_path);
    let pidfile = nestweaver_daemon::lifecycle::pidfile_path(&instance_id);

    let daemon_check: anyhow::Result<()> = match std::fs::read_to_string(&pidfile) {
        // No pidfile → nothing claims this DB → quiesced.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        // Present but unreadable → cannot confirm the daemon is stopped → fail closed.
        Err(e) => anyhow::bail!(
            "a pidfile exists at {} but could not be read to confirm the daemon is stopped \
             ({e}) — refusing to build a possibly-torn snapshot. Stop the daemon \
             (`nestweaver daemon stop`) or remove the stale pidfile, then retry.",
            pidfile.display(),
        ),
        // Present but EMPTY → no PID is claimed → quiesced, exactly as an
        // absent pidfile is. This is the state a failed `daemon start` leaves
        // behind via `retract_failed_start_pidfile`.
        Ok(contents) if contents.trim().is_empty() => Ok(()),
        Ok(contents) => match contents.trim().parse::<i32>() {
            // Present but garbage (non-numeric) → cannot confirm → fail closed.
            Err(_) => anyhow::bail!(
                "a pidfile exists at {} but could not be parsed to confirm the daemon is \
                 stopped — refusing to build a possibly-torn snapshot. Stop the daemon \
                 (`nestweaver daemon stop`) or remove the stale pidfile, then retry.",
                pidfile.display(),
            ),
            // Live pid → daemon is writing this DB → refuse.
            Ok(pid) if nestweaver_client::autostart::is_process_alive(pid) => anyhow::bail!(
                "a daemon (pid {pid}) is running on this database {} — a raw snapshot could \
                 capture a torn, inconsistent copy. Stop it with `nestweaver daemon stop` and \
                 retry, or use `nestweaver server backup` for a consistent in-process snapshot.",
                db_path.display(),
            ),
            // Dead/stale pid → daemon is gone → quiesced.
            Ok(_) => Ok(()),
        },
    };
    daemon_check?;

    // A pidfile is advisory runtime metadata and may be unlinked underneath a
    // live daemon. The database lock is the durable ownership proof that
    // survives that incident state. Rollback, prune, rebuild, and snapshot
    // creation all require a quiescent graph, so only a provably free lock is
    // safe; an indeterminate probe fails closed.
    ensure_database_write_lock_quiesced(
        db_path,
        nestweaver_daemon::lifecycle::db_write_lock(db_path),
    )?;

    // Standalone `code watch` and `brain watch` processes do not own a daemon
    // pidfile. They publish their PID in `<db>.lock`; apply the same fail-closed
    // quiescence check so snapshot build cannot race those writers either.
    let watcher_lock = nestweaver_engine::sidecar_path(db_path, ".lock");
    match std::fs::read_to_string(&watcher_lock) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => anyhow::bail!(
            "a watcher lock exists at {} but could not be read ({error}) — refusing to build a \
             possibly-torn snapshot. Stop the standalone watcher or remove its stale lock, then \
             retry.",
            watcher_lock.display(),
        ),
        Ok(contents) => match contents.trim().parse::<i32>() {
            Err(_) => anyhow::bail!(
                "a watcher lock exists at {} but could not be parsed — refusing to build a \
                 possibly-torn snapshot. Stop the standalone watcher or remove its stale lock, \
                 then retry.",
                watcher_lock.display(),
            ),
            Ok(pid) if nestweaver_client::autostart::is_process_alive(pid) => anyhow::bail!(
                "a standalone watcher (pid {pid}) is writing this database {} — stop it before \
                 building a snapshot.",
                db_path.display(),
            ),
            Ok(_) => Ok(()),
        },
    }
}

pub(crate) fn ensure_database_write_lock_quiesced(
    db_path: &Path,
    observed: nestweaver_daemon::lifecycle::DbWriteLock,
) -> anyhow::Result<()> {
    match observed {
        nestweaver_daemon::lifecycle::DbWriteLock::Free => {}
        nestweaver_daemon::lifecycle::DbWriteLock::Held { pid } => {
            let owner = pid
                .map(|value| format!(" by pid {value}"))
                .unwrap_or_default();
            anyhow::bail!(
                "the database write lock for {} is held{owner} — refusing an operation that \
                 requires a quiescent graph. Stop the daemon or writer and retry.",
                db_path.display(),
            );
        }
        nestweaver_daemon::lifecycle::DbWriteLock::Unknown => anyhow::bail!(
            "the database write-lock state for {} could not be determined — refusing an \
             operation that requires a quiescent graph. Stop the daemon or writer, verify the \
             database is readable, and retry.",
            db_path.display(),
        ),
    }
    Ok(())
}

pub(crate) fn run_snapshot(command: SnapshotCommands, _use_daemon: bool) -> anyhow::Result<i32> {
    match command {
        SnapshotCommands::Build {
            instance,
            db,
            config,
            output,
        } => {
            // Resolve DB path: --db > --config > env/default
            let db_path = resolve_db_with_config(db, config.as_deref())?;

            if !db_path.exists() {
                anyhow::bail!(
                    "database not found at {}; run 'nestweaver index' first",
                    db_path.display()
                );
            }

            // Quiesce guard FIRST — before touching the store — and derived from
            // the DB path (not `--instance`) so a mismatched `--instance` can't
            // bypass detection of a live daemon and yield a torn hot-copy. A
            // consistent snapshot while the daemon runs is `nestweaver server
            // backup` (copies under the daemon's write lock).
            ensure_no_live_daemon_for_snapshot_build(&db_path)?;

            // Load instance config if provided
            let cfg = load_instance_config_opt(config.as_deref());

            // nw-053: default the recorded instance to how repos are ACTUALLY
            // stored now. Post-nw-019 the daemon stamps repos under the config's
            // LOGICAL `instance_id` (and the no-daemon CLI under config/"default"),
            // NOT the db-path hash. So resolve: `--instance` flag > config's
            // `instance_id` > db-path hash. The hash fallback survives ONLY for a
            // no-config DB, where the logical name is unknown and the hash is the
            // best legacy guess. (This id is recorded in the snapshot stamp and used
            // as the default output-dir name; the snapshot's repo set is read via
            // `list_repos(.., None)` below, so content is instance-agnostic.)
            let instance_id = instance
                .filter(|f| !f.is_empty())
                .or_else(|| cfg.as_ref().map(|c| c.instance_id.clone()))
                .unwrap_or_else(|| {
                    nestweaver_daemon::lifecycle::instance_id_from_db_path(&db_path)
                });
            // nw-052b residual: a `--instance` flag here bypasses the CLI
            // `resolve_instance_id` validator, so reject a colon/whitespace
            // instance before it lands in the stamp label and the
            // `snapshot-<instance>` output-dir name. Config-derived ids are
            // already validated at config-load; the hash fallback is always valid.
            nestweaver_engine::validate_instance_id(&instance_id)?;

            // Fetch repos by reading the store directly. The quiesce guard above
            // guarantees no daemon is writing this DB, so a raw read is safe —
            // and we must NOT autospawn a RW daemon here (that would itself trip
            // the quiesce guard on retry). Passing `false` skips the daemon and
            // takes the read-only store path.
            let repos: Vec<nestweaver_schema::Repo> = {
                let mut args = serde_json::json!({});
                args["instance"] = serde_json::json!(&instance_id);
                if let Some(value) =
                    try_hybrid_json_rpc(false, &db_path, config.as_deref(), "list_repos", args)?
                {
                    serde_json::from_value(unwrap_hybrid_payload(value))
                        .context("failed to deserialize repos from daemon response")?
                } else {
                    // No daemon: read directly from the store. The CLI `index` command
                    // stores repos with instance_id = "default", not the hash-based id
                    // used by the daemon, so pass None to return all repos.
                    let store = GraphStore::open_read_only(&db_path)
                        .map_err(|e| anyhow::anyhow!("failed to open database: {e}"))?;
                    nestweaver_engine::list_repos(&store, None)?
                }
            };

            // Fetch embedding dimension via daemon RPC (preferred) or direct store (fallback).
            let embedding_dim: u32 = {
                let args = serde_json::json!({});
                if let Some(value) = try_hybrid_json_rpc(
                    false,
                    &db_path,
                    config.as_deref(),
                    "embedding_dimension",
                    args,
                )? {
                    serde_json::from_value(value).unwrap_or(0)
                } else {
                    let store = GraphStore::open_read_only(&db_path)
                        .map_err(|e| anyhow::anyhow!("failed to open database: {e}"))?;
                    store.embedding_dimension().unwrap_or(0)
                }
            };

            // Schema hashes — shared with the replica compat gate (see
            // nestweaver_engine::schema_hashes) so build and load agree exactly.
            let (core_hash, ext_hash, effective_hash) =
                nestweaver_engine::schema_hashes(cfg.as_ref());

            // Embedding info — use [embedding].model_id (local sentence-transformer),
            // not [inference].embedding_model (remote Ollama model name).
            let embedding_model_id = cfg
                .as_ref()
                .map(|c| c.embedding.model_id.clone())
                .unwrap_or_else(|| "unknown".to_string());

            // Timestamp (RFC3339 UTC)
            let built_at = {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                let secs = now.as_secs();
                // Format as RFC3339 UTC
                let days = secs / 86400;
                let time_secs = secs % 86400;
                let hours = time_secs / 3600;
                let minutes = (time_secs % 3600) / 60;
                let seconds = time_secs % 60;

                // Convert days since epoch to y/m/d
                // Algorithm from http://howardhinnant.github.io/date_algorithms.html
                let z = days as i64 + 719468;
                let era = if z >= 0 { z } else { z - 146096 } / 146097;
                let doe = (z - era * 146097) as u64;
                let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
                let y = yoe as i64 + era * 400;
                let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
                let mp = (5 * doy + 2) / 153;
                let d = doy - (153 * mp + 2) / 5 + 1;
                let m = if mp < 10 { mp + 3 } else { mp - 9 };
                let y = if m <= 2 { y + 1 } else { y };
                format!("{y:04}-{m:02}-{d:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
            };

            let repo_stamps: Vec<nestweaver_engine::RepoStamp> = repos
                .iter()
                .map(|r| nestweaver_engine::RepoStamp {
                    url: r.url.clone(),
                    indexed_sha: r.indexed_sha.clone(),
                    commits_behind_head: r.staleness_commits_behind,
                })
                .collect();

            let stamp = nestweaver_engine::Stamp {
                format_version: nestweaver_engine::SNAPSHOT_FORMAT_VERSION,
                capabilities: vec![nestweaver_engine::SNAPSHOT_CAPABILITY_EMBEDDINGS.to_string()],
                instance_id: instance_id.clone(),
                // `build_snapshot_from_store` replaces these placeholders
                // with the database-owned values captured under the
                // publication lease. CLI/config identity is not authoritative.
                brain_uuid: String::new(),
                publication_uuid: String::new(),
                engine_version: env!("CARGO_PKG_VERSION").to_string(),
                min_compatible_engine: nestweaver_engine::MIN_SNAPSHOT_READER_VERSION.to_string(),
                schema_hash_core: core_hash,
                schema_hash_extensions: ext_hash,
                schema_hash_effective: effective_hash,
                embedding_model_id,
                embedding_dimension: embedding_dim,
                embedding_count: 0,
                built_at,
                repos: repo_stamps,
            };

            let manifest = nestweaver_engine::Manifest {
                repos: repos
                    .iter()
                    .map(|r| nestweaver_engine::ManifestRepo {
                        url: r.url.clone(),
                        indexed_sha: r.indexed_sha.clone(),
                        files_skipped: Vec::new(),
                    })
                    .collect(),
            };

            let output_dir = output.unwrap_or_else(|| {
                db_path
                    .parent()
                    .unwrap_or_else(|| std::path::Path::new("."))
                    .join(format!("snapshot-{instance_id}"))
            });

            let stamp =
                nestweaver_engine::build_snapshot(&output_dir, &stamp, &manifest, &db_path)?;

            println!("Snapshot built successfully in {}", output_dir.display());
            println!("  Instance: {}", stamp.instance_id);
            println!("  Engine: {}", stamp.engine_version);
            println!("  Schema: {}", stamp.schema_hash_effective);
            println!("  Repos: {}", stamp.repos.len());
            println!("  Embeddings: {}", stamp.embedding_count);
            Ok(EXIT_SUCCESS)
        }
        SnapshotCommands::Verify { path } => {
            match nestweaver_engine::verify_snapshot(Path::new(&path)) {
                Ok(stamp) => {
                    println!("Snapshot verified OK");
                    println!("  Instance: {}", stamp.instance_id);
                    println!("  Engine: {}", stamp.engine_version);
                    println!("  Schema: {}", stamp.schema_hash_effective);
                    println!("  Embedding model: {}", stamp.embedding_model_id);
                    println!("  Embeddings: {}", stamp.embedding_count);
                    println!("  Built: {}", stamp.built_at);
                    println!("  Repos: {}", stamp.repos.len());
                    Ok(EXIT_SUCCESS)
                }
                Err(e) => {
                    eprintln!("Snapshot verification failed: {e}");
                    Ok(EXIT_ERROR)
                }
            }
        }
        SnapshotCommands::Push {
            instance,
            config,
            snapshot_dir,
            backend,
            backend_path,
        } => {
            // Resolve snapshot directory and backend from args/config/instance registry.
            let (snap_dir, backend_name, b_path) = if let Some(inst_id) = instance {
                // Load from registry → instance config
                let registry = nestweaver_engine::registry::Registry::load_or_create(
                    &default_registry_path(),
                )?;
                let entry = registry
                    .get(&inst_id)
                    .ok_or_else(|| anyhow::anyhow!("instance '{}' not registered", inst_id))?;
                let cfg =
                    nestweaver_engine::InstanceConfig::from_file(Path::new(&entry.config_path))?;
                let dir = snapshot_dir.unwrap_or_else(|| {
                    dirs::data_local_dir()
                        .unwrap_or_else(|| PathBuf::from("."))
                        .join("nestweaver")
                        .join(&inst_id)
                        .join("snapshot")
                });
                let be_name = backend.unwrap_or(cfg.snapshot_storage.backend);
                let be_path = backend_path.or(cfg.snapshot_storage.path);
                (dir, be_name, be_path)
            } else if let Some(ref cfg_path) = config {
                // Load from explicit config file
                let cfg = nestweaver_engine::InstanceConfig::from_file(cfg_path)?;
                let dir = snapshot_dir.unwrap_or_else(|| {
                    dirs::data_local_dir()
                        .unwrap_or_else(|| PathBuf::from("."))
                        .join("nestweaver")
                        .join(&cfg.instance_id)
                        .join("snapshot")
                });
                let be_name = backend.unwrap_or(cfg.snapshot_storage.backend);
                let be_path = backend_path.or(cfg.snapshot_storage.path);
                (dir, be_name, be_path)
            } else if let (Some(b), Some(dir)) = (backend, snapshot_dir) {
                // Direct flags: --backend + --snapshot-dir
                (dir, b, backend_path)
            } else {
                anyhow::bail!("provide --instance, --config, or both --backend and --snapshot-dir");
            };

            // Verify integrity first
            let stamp = nestweaver_engine::verify_snapshot(&snap_dir)
                .map_err(|e| anyhow::anyhow!("snapshot integrity check failed: {e}"))?;

            // Build meta from stamp
            let meta = nestweaver_storage::SnapshotMeta {
                version: stamp.engine_version.clone(),
                instance_id: stamp.instance_id.clone(),
            };

            // Create backend and push
            let storage = nestweaver_storage::create_backend(&backend_name, b_path.as_deref())?;
            storage.push_snapshot(&snap_dir, &meta)?;

            println!(
                "Snapshot pushed: instance='{}' version='{}'",
                meta.instance_id, meta.version
            );
            Ok(EXIT_SUCCESS)
        }
    }
}
