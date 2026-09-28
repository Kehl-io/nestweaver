//! The `config` and `instance` command families.
//!
//! Moved verbatim out of `src/main.rs` (nw-348).

use crate::*;

pub(crate) fn run_config(command: ConfigCommands) -> anyhow::Result<(i32, Option<String>)> {
    match command {
        ConfigCommands::Validate { path, json } => {
            match nestweaver_engine::InstanceConfig::from_file(&path) {
                Ok(config) => {
                    if json {
                        let result = serde_json::json!({
                            "valid": true,
                            "path": path.display().to_string(),
                            "instance_id": config.instance_id,
                            "repo_count": config.repos.len(),
                        });
                        println!("{}", serde_json::to_string(&result)?);
                    } else {
                        println!(
                            "Valid instance config: {} (instance_id: {}, repos: {})",
                            path.display(),
                            config.instance_id,
                            config.repos.len()
                        );
                    }
                    Ok((EXIT_SUCCESS, None))
                }
                Err(error) if json => {
                    let message = format!("validate instance config {}: {error:#}", path.display());
                    let result = serde_json::json!({
                        "valid": false,
                        "path": path.display().to_string(),
                        "error": message,
                    });
                    println!("{}", serde_json::to_string(&result)?);
                    Ok((EXIT_ERROR, None))
                }
                Err(error) => Err(error)
                    .with_context(|| format!("validate instance config {}", path.display())),
            }
        }
    }
}

/// nw-359 leg (3). Disclose that a daemon-only operation cannot honour a
/// GRANTED bypass, and name what that costs.
///
/// `instance merge` and `instance remove --purge-graph` exist only as daemon
/// RPCs: the server side runs migration journals, extension-metadata
/// preparation, search reconciliation and node-graph deletion finalisation
/// around the store call. There is no direct implementation and there must not
/// be one — a ~300-line CLI twin of that orchestration is the exact shape the
/// twin rule forbids, and it would drift on the first change to either side.
///
/// # Why this WARNS instead of refusing, which is a reversal
///
/// Refusing was the first shape of this fix, and the end-to-end remedy harness
/// disproved it in one run. `instance merge` is not merely a command a user may
/// choose to run — it is a remedy this product PRINTS, from the multi-instance
/// refusal, with the instance names substituted in.
/// `multi_instance_refusal_emits_a_runnable_consolidation_command` runs exactly
/// that printed string, under a granted bypass, and asserts that it works. A
/// refusal would have made a shipped remedy un-runnable, which is a fresh
/// instance of the class this lane exists to close (nw-334, nw-328). Trading
/// one defect for a worse one is not a fix.
///
/// # What is disclosed, and the limit of it
///
/// The command proceeds and may auto-start a daemon. What changes is that this
/// is no longer SILENT: the auto-started daemon holds the database write lease
/// for its idle timeout (default 3600s), and that is what blocks the follow-up
/// `index` which merge's own remedy (`merge_reindex_guidance`) prescribes. The
/// same interaction is already worked around BY HAND in
/// `tests/error_remedy_test.rs`, whose comment records the hour-long lease and
/// stops the daemon itself — independent confirmation of the harm, written by
/// someone who hit it. This message gives the operator what that test gave
/// itself.
///
/// It does NOT release the lease, and that limit is stated rather than implied.
/// The complete fix is to stop a daemon THIS command started, which needs a
/// "did I start it?" answer `ensure_daemon` does not return, and a reusable
/// stop path that exists today only as the body of the `daemon stop` arm.
/// Reimplementing that here would be the twin this doc comment just argued
/// against.
///
/// This does NOT fire on bare `NESTWEAVER_NO_DAEMON=1`. `resolve_use_daemon`
/// grants the bypass only on `NESTWEAVER_ALLOW_NO_DAEMON`, so a request that
/// policy refused still routes through the daemon, correctly — which is the
/// half the item had backwards.
pub(crate) fn warn_daemon_route_unavoidable(operation: &str, db_path: &Path, use_daemon: bool) {
    if use_daemon {
        return;
    }
    eprintln!(
        "Warning: `{operation}` runs entirely inside the daemon — migration \
         journal, extension metadata, search reconciliation — and has no direct \
         implementation, so the daemon bypass you granted cannot be honoured \
         here and a daemon may be started.\n  \
         That daemon holds the database write lease for its idle timeout, which \
         will block a following bypassed write against {0}. Release it with: \
         `nestweaver daemon --db {0} stop`",
        db_path.display()
    );
}

pub(crate) fn run_instance(command: InstanceCommands, use_daemon: bool) -> anyhow::Result<i32> {
    match command {
        InstanceCommands::Identity { db, json } => {
            let db_path = db.unwrap_or_else(default_db_path);
            require_existing_db(&db_path)?;
            let store = nestweaver_store::GraphStore::open_read_only_without_migration(&db_path)
                .map_err(|error| anyhow::anyhow!("open {}: {error}", db_path.display()))?;
            let identity = store
                .publication_identity()
                .map_err(|error| anyhow::anyhow!("read graph identity: {error}"))?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "database {} has no publication identity; reopen it writable with this NestWeaver version to initialize it",
                        db_path.display()
                    )
                })?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "db": db_path,
                        "brain_uuid": identity.brain_uuid,
                        "publication_uuid": identity.publication_uuid,
                    }))?
                );
            } else {
                println!("Database:         {}", db_path.display());
                println!("Brain UUID:       {}", identity.brain_uuid);
                println!("Publication UUID: {}", identity.publication_uuid);
            }
            Ok(EXIT_SUCCESS)
        }
        InstanceCommands::AdoptIdentity {
            config_path,
            db,
            dry_run,
            json,
        } => {
            let config = nestweaver_engine::InstanceConfig::from_file(&config_path)
                .with_context(|| format!("load config {}", config_path.display()))?;
            let db_path = db
                .or_else(|| config.db_path())
                .unwrap_or_else(default_db_path);
            require_existing_db(&db_path)?;
            let store = nestweaver_store::GraphStore::open_read_only_without_migration(&db_path)
                .map_err(|error| anyhow::anyhow!("open {}: {error}", db_path.display()))?;
            let identity = store
                .publication_identity()
                .map_err(|error| anyhow::anyhow!("read graph identity: {error}"))?
                .ok_or_else(|| anyhow::anyhow!("database has no publication identity"))?;
            let adoption = nestweaver_engine::adopt_expected_brain_uuid(
                &config_path,
                &identity.brain_uuid,
                dry_run,
            )?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "config": config_path,
                        "db": db_path,
                        "dry_run": dry_run,
                        "changed": adoption.changed,
                        "previous_expected_brain_uuid": adoption.previous,
                        "adopted_brain_uuid": adoption.adopted,
                        "publication_uuid": identity.publication_uuid,
                    }))?
                );
            } else if dry_run {
                println!(
                    "Would bind {} to brain {} from {}{}",
                    config_path.display(),
                    adoption.adopted,
                    db_path.display(),
                    if adoption.changed {
                        ""
                    } else {
                        " (already bound)"
                    }
                );
            } else if adoption.changed {
                println!(
                    "Bound {} to brain {} from {}.",
                    config_path.display(),
                    adoption.adopted,
                    db_path.display()
                );
            } else {
                println!(
                    "{} is already bound to brain {}.",
                    config_path.display(),
                    adoption.adopted
                );
            }
            Ok(EXIT_SUCCESS)
        }
        InstanceCommands::Register { config_path } => {
            let config = nestweaver_engine::InstanceConfig::from_file(Path::new(&config_path))?;
            // Store the canonical path so the registry entry is immune to
            // CWD differences between `register` and later lookups. The file
            // was just read successfully, so canonicalization cannot fail.
            let canonical = std::fs::canonicalize(&config_path)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or(config_path);
            let registry_path = default_registry_path();
            let mut registry =
                nestweaver_engine::registry::Registry::load_or_create(&registry_path)?;
            registry.register(&config.instance_id, &canonical)?;
            println!("Registered instance '{}'", config.instance_id);
            Ok(EXIT_SUCCESS)
        }
        InstanceCommands::List => {
            let registry =
                nestweaver_engine::registry::Registry::load_or_create(&default_registry_path())?;
            if registry.list().is_empty() {
                println!("No instances registered.");
            } else {
                for entry in registry.list() {
                    println!("  {} -> {}", entry.id, entry.config_path);
                }
            }
            Ok(EXIT_SUCCESS)
        }
        InstanceCommands::Remove {
            id,
            purge_graph,
            db,
        } => {
            // Validate the DB BEFORE mutating the registry — a typo'd
            // --db must fail without the instance already being removed.
            let db_path = if purge_graph {
                let db_path = db.unwrap_or_else(default_db_path);
                require_existing_db(&db_path)?;
                Some(db_path)
            } else {
                None
            };
            let mut registry =
                nestweaver_engine::registry::Registry::load_or_create(&default_registry_path())?;
            let registry_removed = match registry.remove(&id) {
                Ok(()) => true,
                Err(e) => {
                    // With --purge-graph we tolerate a missing registry
                    // entry so ghost instances (left by a misconfigured
                    // merge) can still be cleaned out of the graph.
                    if purge_graph {
                        eprintln!("Note: {e}; continuing with graph purge");
                        false
                    } else {
                        return Err(e);
                    }
                }
            };
            if registry_removed {
                println!("Removed instance '{id}' from registry");
            }
            if let Some(db_path) = db_path {
                // The same seam as `merge`: `purge_instance` is a server-side
                // streaming RPC with no direct twin. Checked here rather than
                // beside the `--purge-graph` parse because the registry removal
                // above must still happen without a daemon.
                warn_daemon_route_unavoidable(
                    "instance remove --purge-graph",
                    &db_path,
                    use_daemon,
                );
                let rt = tokio::runtime::Runtime::new()?;
                let mut client = rt
                    .block_on(nestweaver_client::DaemonClient::connect(&db_path, None))
                    .context("failed to connect to daemon")?;

                let mut stream = rt
                    .block_on(client.purge_instance(&id))
                    .context("purge_instance RPC failed")?;

                let mut had_error = false;
                rt.block_on(async {
                    while let Ok(Some(p)) = stream.message().await {
                        eprintln!("{}", p.message);
                        if p.phase == nestweaver_proto::Phase::Error as i32 {
                            had_error = true;
                        }
                    }
                });

                if had_error {
                    return Err(anyhow::anyhow!("purge_instance failed"));
                }
            }
            Ok(EXIT_SUCCESS)
        }
        InstanceCommands::Pull { id } => {
            let registry =
                nestweaver_engine::registry::Registry::load_or_create(&default_registry_path())?;
            let entry = registry
                .get(&id)
                .ok_or_else(|| anyhow::anyhow!("instance '{}' not registered", id))?;
            let config =
                nestweaver_engine::InstanceConfig::from_file(Path::new(&entry.config_path))?;
            let backend = nestweaver_storage::create_backend(
                &config.snapshot_storage.backend,
                config.snapshot_storage.path.as_deref(),
            )?;
            let dest = dirs::data_local_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("nestweaver")
                .join("snapshots")
                .join(&id);
            std::fs::create_dir_all(&dest)?;
            let meta = backend.pull_snapshot(&dest)?;
            nestweaver_engine::verify_snapshot(&dest).map_err(|e| {
                anyhow::anyhow!(
                    "pulled snapshot failed integrity check: {e}; \
                     the snapshot in storage may be corrupted"
                )
            })?;
            println!("Pulled snapshot v{} for '{}'", meta.version, id);
            Ok(EXIT_SUCCESS)
        }
        InstanceCommands::Merge { from, to, db } => {
            let db_path = db.unwrap_or_else(default_db_path);
            // Merging against a typo'd --db must fail db_not_found, not
            // autostart a daemon that creates an empty DB and false-greens
            // ("No rows found").
            require_existing_db(&db_path)?;
            warn_daemon_route_unavoidable("instance merge", &db_path, use_daemon);
            let rt = tokio::runtime::Runtime::new()?;
            let mut client = rt
                .block_on(nestweaver_client::DaemonClient::connect(&db_path, None))
                .context("failed to connect to daemon")?;

            let result = rt
                .block_on(client.merge_instance(&from, &to))
                .context("merge_instance RPC failed")?;

            if result.vaults_reparented + result.repos_reparented + result.projects_reparented == 0
            {
                println!("No rows found with instance_id '{from}'.");
            } else {
                println!(
                    "Merged '{from}' -> '{to}': {} vault(s), {} repo(s), {} project(s)",
                    result.vaults_reparented, result.repos_reparented, result.projects_reparented
                );
                if !result.discarded_vaults.is_empty() {
                    eprintln!(
                        "{}",
                        merge_discarded_vault_guidance(&result.discarded_vaults, &to)
                    );
                }
                if !result.repos_needing_reindex.is_empty() {
                    eprintln!("{}", merge_reindex_guidance(&result.repos_needing_reindex));
                }
            }
            Ok(EXIT_SUCCESS)
        }
        InstanceCommands::AbortMigration { db, force } => {
            // Offline recovery: operate on the sidecar journals directly (the
            // daemon is wedged and won't boot). nw-091 / Bug 3B.
            let db_path = db.unwrap_or_else(default_db_path);
            match abort_instance_migration_offline(&db_path, force)? {
                nestweaver_engine::AbortMigrationOutcome::NothingToAbort => {
                    println!("No pending instance-migration journal — nothing to abort.");
                }
                nestweaver_engine::AbortMigrationOutcome::AbortedPrepared => {
                    println!(
                        "Aborted a prepared instance-migration journal (no graph mutation had \
                         happened). The daemon can boot now."
                    );
                }
                nestweaver_engine::AbortMigrationOutcome::ForceDiscardedApplied => {
                    eprintln!(
                        "Force-discarded a graph-applied migration journal. The graph mutation \
                         itself remains — verify the merge result and reconcile if needed."
                    );
                }
                nestweaver_engine::AbortMigrationOutcome::ForceDiscardedUnknownPhase => {
                    eprintln!(
                        "Force-discarded an unreadable migration journal (phase unknown). The \
                         graph may or may not have been mutated — verify the merge result and \
                         reconcile if needed."
                    );
                }
            }
            Ok(EXIT_SUCCESS)
        }
    }
}

/// Abort recovery is an offline mutation of the same durable journal a live
/// daemon merge advances. Hold the canonical database write lease before even
/// inspecting that journal, and retain it through durable removal. `--force`
/// changes phase policy only; it never bypasses ownership.
pub(crate) fn abort_instance_migration_offline(
    db_path: &Path,
    force: bool,
) -> anyhow::Result<nestweaver_engine::AbortMigrationOutcome> {
    require_existing_db(db_path)?;
    let _lease = require_exclusive_store_access_with_remedy(
        db_path,
        "abort an instance-migration recovery journal",
        ExclusivityRemedy::StopTheHolder,
    )?;
    nestweaver_engine::abort_instance_extension_migration(db_path, force)
}

/// Guidance for vaults whose notes were discarded by a merge collision.
///
/// When two instances hold a vault at the same root, the one with fewer notes
/// loses and its notes are dropped. That was reported as a bare
/// `Note: <root> (N notes discarded)` with no remedy — a line that states data
/// loss and stops. Notes are re-derivable from the files on disk, so say how
/// (nw-112).
pub(crate) fn merge_discarded_vault_guidance(discarded: &[String], to_instance: &str) -> String {
    let mut guidance = String::from(
        "\nNOTE: a vault collision discarded notes from the losing vault.\n\
         Those notes are re-derivable from the files on disk — re-index each root \
         below under the target instance to restore them:\n",
    );
    for entry in discarded {
        guidance.push_str("  ");
        guidance.push_str(entry);
        guidance.push('\n');
    }
    guidance.push_str(&format!(
        "  nestweaver brain refresh <root> --instance {to_instance}"
    ));
    guidance
}

pub(crate) fn merge_reindex_guidance(repos: &[String]) -> String {
    let mut guidance = String::from(
        "\nNOTE: source repo graph rows were removed during merge.\n\
         Force re-index each repo listed below; this recreates them under the target instance:\n",
    );
    for repo in repos {
        guidance.push_str("  ");
        guidance.push_str(repo);
        guidance.push('\n');
    }
    guidance.push_str("  nestweaver index --repo <path> --force\n");
    guidance.push_str("  nestweaver materialize-projects --config <instance.toml>");
    guidance
}
