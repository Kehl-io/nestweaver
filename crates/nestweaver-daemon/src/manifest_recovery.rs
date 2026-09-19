//! One daemon-owned reconciliation loop. Queries can wake it, never write.
use super::{
    ConnectionGuard, DaemonState, MutationWorkerOwnership, current_repo_eligibility_config,
};
use nestweaver_engine::content_reader::{ContentReader, FilesystemReader, GitBareReader};
use nestweaver_engine::manifest::{
    self, ManifestInputs, ManifestUnavailable, ManifestUnavailableReason,
};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, atomic::Ordering};
use std::time::{Duration, Instant};

pub(super) struct Snapshot {
    generation: u64,
    identity: nestweaver_store::PublicationIdentity,
    deadline: Instant,
    inventory: String,
    debt: Option<Vec<u8>>,
    inputs: Vec<(String, ManifestInputs)>,
}

pub(super) fn capture(state: &DaemonState, deadline: Instant) -> anyhow::Result<Snapshot> {
    let generation = state.store.graph_generation();
    let identity = state
        .store
        .publication_identity()?
        .ok_or_else(|| anyhow::anyhow!("graph publication identity is absent"))?;
    identity.validate()?;
    manifest::ensure_manifest_generation(&state.store, generation)?;
    let config = current_repo_eligibility_config(state)?;
    let limits = state
        .instance_cfg
        .as_ref()
        .map(|c| c.indexing.limits())
        .unwrap_or_default();
    let mut repos = state.store.list_repos(None)?;
    repos.sort_by(|a, b| a.uid.cmp(&b.uid));
    let mut inventory = Vec::new();
    let mut inputs = Vec::new();
    let mut remaining = 32 * 1024 * 1024;
    for repo in &repos {
        let root = repo.local_root().map(Path::new);
        let reader: Box<dyn ContentReader> = if let Some(root) = root {
            let unskip = config
                .as_ref()
                .map(|c| c.unskip_names_for(&repo.url, Some(root)))
                .unwrap_or(&[]);
            let excludes = config
                .as_ref()
                .map(|c| c.exclude_globs_for(&repo.url, Some(root)))
                .unwrap_or(&[]);
            Box::new(
                FilesystemReader::with_limits(root, limits)
                    .unskipping(unskip)
                    .excluding(excludes)?
                    .strict_enumeration(),
            )
        } else {
            anyhow::ensure!(
                state.server_mode,
                "{}: no recorded local source root",
                repo.uid
            );
            anyhow::ensure!(
                !repo.indexed_sha.is_empty(),
                "{}: no completed bare revision",
                repo.uid
            );
            let workspace = nestweaver_engine::bare_clone::BareCloneWorkspace {
                root: state
                    .db_path
                    .parent()
                    .unwrap_or(Path::new("."))
                    .join("workspace"),
            };
            let clones = workspace.list_clones()?;
            let mut matching = clones
                .into_iter()
                .filter(|clone| clone.url.trim_end_matches('/') == repo.url.trim_end_matches('/'));
            let clone = matching.next().ok_or_else(|| {
                anyhow::anyhow!("{}: recorded bare clone is unavailable", repo.uid)
            })?;
            anyhow::ensure!(
                matching.next().is_none(),
                "{}: multiple recorded bare clones",
                repo.uid
            );
            Box::new(
                GitBareReader::with_limits(&clone.path, &repo.indexed_sha, limits)
                    .local_objects_only(),
            )
        };
        let policy = state.store.get_repo_index_policy(&repo.uid)?;
        anyhow::ensure!(
            policy.as_deref() == Some(reader.eligibility_fingerprint().as_str()),
            "{}: recorded source eligibility is absent or differs from current configuration",
            repo.uid
        );
        inventory.push(serde_json::json!({ "repo": repo, "policy": policy }));
        let captured = manifest::capture_manifest_inputs(reader.as_ref(), &mut remaining, deadline)
            .map_err(|e| anyhow::anyhow!("{}: {e:#}", repo.uid))?;
        inputs.push((repo.uid.clone(), captured));
    }
    manifest::ensure_manifest_generation(&state.store, generation)?;
    Ok(Snapshot {
        generation,
        identity,
        deadline,
        inventory: serde_json::to_string(&inventory)?,
        debt: manifest::manifest_debt_revision(&state.db_path)?,
        inputs,
    })
}

pub(super) fn reconcile(state: &DaemonState, before: Snapshot) -> anyhow::Result<()> {
    // The daemon gate excludes graph writers. The store publication lease also
    // excludes snapshot/publication owners that hold a different outer gate.
    let lease = state
        .store
        .try_acquire_index_publication_lease()?
        .ok_or_else(|| anyhow::anyhow!("publication owner is active"))?;
    lease.ensure_clean_for_snapshot()?;
    if let Err(problem) = manifest::current_manifest_snapshot(&state.store, &state.db_path) {
        anyhow::ensure!(
            problem.retryable,
            "refusing to replace a non-recoverable artifact: {problem}"
        );
    }
    let after = capture(state, before.deadline)?;
    anyhow::ensure!(
        before.generation == after.generation
            && before.identity == after.identity
            && before.inventory == after.inventory
            && before.debt == after.debt
            && before.inputs == after.inputs,
        "manifest sources changed during derivation; retrying a fresh snapshot"
    );
    drop(before);
    let manifests: HashMap<_, _> = after
        .inputs
        .iter()
        .map(|(uid, inputs)| (uid.clone(), manifest::parse_manifest(inputs)))
        .collect();
    if after.debt.is_none() {
        manifest::mark_manifest_reconciliation_pending(
            &state.db_path,
            "complete manifest derivation",
        )?;
    }
    let publication_debt = manifest::manifest_debt_revision(&state.db_path)?;
    manifest::save_manifest_cache_for_db(&manifests, &state.store, &state.db_path)?;
    // Fail closed if an external editor changed bytes during the atomic save.
    // Leave durable debt; never let the new envelope hide this pending work.
    let verified = capture(state, after.deadline)?;
    if after.identity != verified.identity
        || after.generation != verified.generation
        || after.inventory != verified.inventory
        || after.inputs != verified.inputs
        || publication_debt != verified.debt
    {
        manifest::mark_manifest_reconciliation_pending(
            &state.db_path,
            "source changed during publication",
        )?;
        anyhow::bail!("manifest sources changed during publication");
    }
    nestweaver_store::durable_sidecar::remove_file_durable_if_exists(
        &manifest::manifest_debt_path(&state.db_path),
    )?;
    Ok(())
}

/// Three immediate attempts, then a cooldown that can recover from repaired
/// storage even when the source and graph have not changed.
#[derive(Default)]
struct RetryBudget {
    attempts: u32,
    resume_at: Option<Instant>,
}
impl RetryBudget {
    fn admit(&mut self, now: Instant) -> bool {
        if let Some(resume_at) = self.resume_at {
            if now < resume_at {
                return false;
            }
            self.attempts = 0;
            self.resume_at = None;
        }
        if self.attempts >= 3 {
            self.resume_at = Some(now + Duration::from_secs(30));
            return false;
        }
        self.attempts += 1;
        true
    }
}

pub(super) async fn run(state: Arc<DaemonState>) {
    let runtime = Arc::clone(&state.manifest_recovery);
    if state.read_only {
        runtime.publish("upstream_owned", 0, None, None);
        return;
    }
    let mut shutdown = state.shutdown_tx.subscribe();
    let mut last_key = String::new();
    let mut retry = RetryBudget::default();
    let mut delay = 0;
    loop {
        if state.shutdown_started.load(Ordering::SeqCst) || *shutdown.borrow() {
            break;
        }
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = tokio::time::sleep(Duration::from_secs(delay)) => {},
            _ = runtime.wake.notified(), if retry.attempts == 0 && delay <= 2 => {},
        }
        if state.shutdown_started.load(Ordering::SeqCst) {
            break;
        }
        let inspect_state = Arc::clone(&state);
        let health = tokio::task::spawn_blocking(move || {
            manifest::current_manifest_snapshot(&inspect_state.store, &inspect_state.db_path)
        })
        .await;
        let problem = match health {
            Ok(Ok(_)) => {
                runtime.publish("ready", 0, None, None);
                retry = RetryBudget::default();
                last_key.clear();
                delay = 2;
                continue;
            }
            Ok(Err(error)) => error,
            Err(error) => {
                tracing::error!(%error, "manifest inspection worker failed");
                delay = 4;
                continue;
            }
        };
        if !problem.retryable || problem.reason == ManifestUnavailableReason::PublicationInProgress
        {
            runtime.publish(
                if problem.retryable {
                    "deferred"
                } else {
                    "blocked"
                },
                retry.attempts,
                problem.retryable.then_some(2),
                Some(problem),
            );
            delay = 2;
            continue;
        }
        let capture_state = Arc::clone(&state);
        let snapshot = tokio::task::spawn_blocking(move || {
            capture(&capture_state, Instant::now() + Duration::from_secs(60))
        })
        .await;
        let snapshot = match snapshot {
            Ok(Ok(snapshot)) => snapshot,
            other => {
                let message = match other {
                    Ok(Err(e)) => format!("{e:#}"),
                    Err(e) => e.to_string(),
                    _ => unreachable!(),
                };
                runtime.publish(
                    "blocked",
                    retry.attempts,
                    None,
                    Some(ManifestUnavailable::new(
                        ManifestUnavailableReason::SourceUnavailable,
                        state.store.graph_generation(),
                        message,
                    )),
                );
                // Source probes rearm after files/configuration are repaired,
                // without requiring another request or manual reindex.
                delay = 30;
                continue;
            }
        };
        let source_digests: Vec<_> = snapshot
            .inputs
            .iter()
            .map(|(uid, inputs)| (uid, inputs.digest()))
            .collect();
        let key = format!(
            "{}:{}:{:?}:{:?}",
            snapshot.generation, snapshot.inventory, snapshot.debt, source_digests
        );
        if key != last_key {
            retry = RetryBudget::default();
            last_key = key;
        }
        if !retry.admit(Instant::now()) {
            runtime.publish("deferred", retry.attempts, Some(30), Some(problem));
            delay = 30;
            continue;
        }
        runtime.publish("running", retry.attempts, None, Some(problem));
        let guard = match ConnectionGuard::write(&state) {
            Ok(guard) => guard,
            Err(_) => break,
        };
        let lease = state.write_gate.lock("manifest_reconciliation").await;
        let repair_state = Arc::clone(&state);
        let result = tokio::task::spawn_blocking(move || {
            let _ownership = MutationWorkerOwnership {
                _write_lease: lease,
                _connection_guard: guard,
            };
            reconcile(&repair_state, snapshot)
        })
        .await;
        match result {
            Ok(Ok(())) => {
                runtime.publish("ready", retry.attempts, None, None);
                retry = RetryBudget::default();
                last_key.clear();
                delay = 2;
            }
            other => {
                let message = match other {
                    Ok(Err(e)) => format!("{e:#}"),
                    Err(e) => e.to_string(),
                    _ => unreachable!(),
                };
                delay = 1 << (retry.attempts - 1);
                runtime.publish(
                    if retry.attempts < 3 {
                        "retry_scheduled"
                    } else {
                        "blocked"
                    },
                    retry.attempts,
                    (retry.attempts < 3).then_some(delay),
                    Some(ManifestUnavailable::new(
                        ManifestUnavailableReason::PendingSourceChange,
                        state.store.graph_generation(),
                        message,
                    )),
                );
            }
        }
    }
    runtime.publish("stopped", retry.attempts, None, None);
}

#[cfg(test)]
mod retry_tests {
    use super::*;

    #[test]
    fn exhausted_budget_rearms_after_cooldown_without_source_change() {
        let now = Instant::now();
        let mut retry = RetryBudget::default();
        for expected in 1..=3 {
            assert!(retry.admit(now));
            assert_eq!(retry.attempts, expected);
        }
        assert!(!retry.admit(now));
        assert!(!retry.admit(now + Duration::from_secs(29)));
        assert!(retry.admit(now + Duration::from_secs(30)));
        assert_eq!(retry.attempts, 1);
        assert!(retry.admit(now + Duration::from_secs(31)));
        assert!(retry.admit(now + Duration::from_secs(33)));
        assert!(!retry.admit(now + Duration::from_secs(37)));
    }
}
