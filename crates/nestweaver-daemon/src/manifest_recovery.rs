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

/// D3: how long ONE repository's manifest capture may take. Each repo gets
/// its own budget, started when its capture starts — after the write gate is
/// held, for the re-captures under it — so neither a long gate wait nor the
/// other repos' captures spend it.
pub(super) const REPO_CAPTURE_BUDGET: Duration = Duration::from_secs(60);

/// D3: how long ALL the captures made under the write gate may take together
/// (the re-capture and the verification capture of one reconciliation). It
/// starts once the gate is held, so the gate hold is bounded by this, not by
/// repos × [`REPO_CAPTURE_BUDGET`] × 2.
pub(super) const PASS_CAPTURE_BUDGET: Duration = Duration::from_secs(60);

/// D3: after this many consecutive passes in which the same repository ran
/// out of its OWN budget, that repository is refused with a contention reason
/// and the others are published, so one persistently slow repository cannot
/// block every manifest rebuild (nw-705's per-repo isolation).
pub(super) const CONSECUTIVE_OVERRUNS_BEFORE_REFUSAL: u32 = 3;

/// The time budgets of one capture pass.
#[derive(Debug, Clone)]
pub(super) struct CaptureBudget {
    pub(super) repo: Duration,
    pub(super) pass: Duration,
    /// Test seam: simulated slow I/O for a named repository.
    #[cfg(test)]
    pub(super) delays: HashMap<String, Duration>,
}

impl CaptureBudget {
    pub(super) fn new(repo: Duration, pass: Duration) -> Self {
        Self {
            repo,
            pass,
            #[cfg(test)]
            delays: HashMap::new(),
        }
    }
}

impl From<Duration> for CaptureBudget {
    fn from(repo: Duration) -> Self {
        Self::new(repo, PASS_CAPTURE_BUDGET)
    }
}

/// D3: a capture ran out of its budget. The whole snapshot is deferred and
/// retried; no repository is refused and no `index --repo` remedy is named,
/// because a re-index cannot cure contention. A repository that runs out of
/// its own budget on [`CONSECUTIVE_OVERRUNS_BEFORE_REFUSAL`] passes in a row
/// is refused instead (see [`contention_refusal_reason`]).
#[derive(Debug)]
pub(super) struct CaptureDeferred {
    repo: String,
    budget: Duration,
    pass: bool,
}

impl std::fmt::Display for CaptureDeferred {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.pass {
            write!(
                f,
                "deferred: contention: the manifest captures under the write gate exceeded \
                 their {:?} pass budget at {}; the snapshot is retried and no repository is \
                 refused",
                self.budget, self.repo
            )
        } else {
            write!(
                f,
                "deferred: contention: the manifest capture of {} exceeded its {:?} budget; \
                 the snapshot is retried and no repository is refused",
                self.repo, self.budget
            )
        }
    }
}

impl std::error::Error for CaptureDeferred {}

pub(super) fn is_contention_deferral(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<CaptureDeferred>())
}

/// D3: the reason recorded for a repository refused for persistent
/// contention. Deterministic, so captures before and under the gate agree.
pub(super) fn contention_refusal_reason(repo_budget: Duration) -> String {
    format!(
        "contention: the manifest capture exceeded its {repo_budget:?} budget on \
         {CONSECUTIVE_OVERRUNS_BEFORE_REFUSAL} consecutive passes"
    )
}

fn contention_failure(
    repo: &nestweaver_schema::Repo,
    repo_budget: Duration,
) -> manifest::ManifestRepoFailure {
    let mut failure =
        manifest::ManifestRepoFailure::new(repo, contention_refusal_reason(repo_budget));
    // A re-index cannot cure contention; the repository is probed again.
    failure.remedy = "none needed: the repository is probed again automatically and \
                      published once its capture fits the budget"
        .to_string();
    failure
}

fn overrun_streak(state: &DaemonState, repo: &str) -> u32 {
    let overruns = state
        .manifest_recovery
        .capture_overruns
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    overruns.get(repo).copied().unwrap_or(0)
}

fn record_overrun(state: &DaemonState, repo: &str) -> u32 {
    let mut overruns = state
        .manifest_recovery
        .capture_overruns
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let streak = overruns.entry(repo.to_string()).or_insert(0);
    *streak = streak.saturating_add(1);
    *streak
}

fn clear_overruns(state: &DaemonState, repo: &str) {
    state
        .manifest_recovery
        .capture_overruns
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(repo);
}

pub(super) struct Snapshot {
    generation: u64,
    identity: nestweaver_store::PublicationIdentity,
    budget: CaptureBudget,
    inventory: String,
    debt: Option<Vec<u8>>,
    inputs: Vec<(String, ManifestInputs)>,
    /// nw-705: repositories this capture refused, each with its reason. The
    /// rebuild keeps the others current instead of failing as a whole.
    pub(super) failures: Vec<manifest::ManifestRepoFailure>,
}

pub(super) fn capture(
    state: &DaemonState,
    budget: impl Into<CaptureBudget>,
) -> anyhow::Result<Snapshot> {
    capture_within(state, budget.into(), None)
}

/// One capture pass. `pass_deadline` bounds the whole pass (the captures
/// under the write gate); each repository also has its own budget.
fn capture_within(
    state: &DaemonState,
    budget: CaptureBudget,
    pass_deadline: Option<Instant>,
) -> anyhow::Result<Snapshot> {
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
    let mut failures = Vec::new();
    let mut remaining = 32 * 1024 * 1024;
    for repo in &repos {
        // D3: a repository that ran out of its own budget on enough passes in
        // a row is refused without spending the gate on it again. The
        // refused-repo probe re-tries it off the gate and clears the streak.
        if overrun_streak(state, &repo.uid) >= CONSECUTIVE_OVERRUNS_BEFORE_REFUSAL {
            failures.push(contention_failure(repo, budget.repo));
            continue;
        }
        // nw-705: each repository is captured on its own. One that is refused
        // (a policy recorded under other settings, a missing root, a
        // malformed manifest) is recorded by name and the rest are still
        // rebuilt; it used to fail the capture for EVERY repo. A refused
        // repo's partial reads do not spend the shared budget.
        let mut bytes = remaining;
        let own_deadline = Instant::now() + budget.repo;
        let deadline = pass_deadline.map_or(own_deadline, |pass| pass.min(own_deadline));
        #[cfg(test)]
        if let Some(delay) = budget.delays.get(&repo.uid) {
            std::thread::sleep(*delay);
        }
        match capture_repo(state, config.as_ref(), limits, repo, &mut bytes, deadline) {
            Ok((policy, captured)) => {
                clear_overruns(state, &repo.uid);
                remaining = bytes;
                inventory.push(serde_json::json!({ "repo": repo, "policy": policy }));
                inputs.push((repo.uid.clone(), captured));
            }
            Err(error) if manifest::is_manifest_capture_deadline(&error) => {
                // The pass budget ran out: no single repository is to blame.
                if pass_deadline.is_some_and(|pass| Instant::now() >= pass && pass < own_deadline) {
                    return Err(CaptureDeferred {
                        repo: repo.uid.clone(),
                        budget: budget.pass,
                        pass: true,
                    }
                    .into());
                }
                let streak = record_overrun(state, &repo.uid);
                if streak >= CONSECUTIVE_OVERRUNS_BEFORE_REFUSAL {
                    let failure = contention_failure(repo, budget.repo);
                    tracing::warn!(
                        repo = %repo.uid,
                        reason = %failure.reason,
                        "manifest rebuild refused a persistently slow repository; the others \
                         are published"
                    );
                    failures.push(failure);
                    continue;
                }
                return Err(CaptureDeferred {
                    repo: repo.uid.clone(),
                    budget: budget.repo,
                    pass: false,
                }
                .into());
            }
            Err(error) => {
                let failure = manifest::ManifestRepoFailure::new(repo, format!("{error:#}"));
                tracing::warn!(
                    repo = %repo.uid,
                    reason = %failure.reason,
                    remedy = %failure.remedy,
                    "manifest rebuild refused a repository; the others stay current"
                );
                failures.push(failure);
            }
        }
    }
    manifest::ensure_manifest_generation(&state.store, generation)?;
    Ok(Snapshot {
        generation,
        identity,
        budget,
        inventory: serde_json::to_string(&inventory)?,
        debt: manifest::manifest_debt_revision(&state.db_path)?,
        inputs,
        failures,
    })
}

/// Capture one repository's manifest inputs, or say why it cannot be.
fn capture_repo(
    state: &DaemonState,
    config: Option<&Arc<nestweaver_engine::InstanceConfig>>,
    limits: nestweaver_engine::index_limits::IndexLimits,
    repo: &nestweaver_schema::Repo,
    remaining: &mut usize,
    deadline: Instant,
) -> anyhow::Result<(Option<String>, ManifestInputs)> {
    let root = repo.local_root().map(Path::new);
    let reader: Box<dyn ContentReader> = if let Some(root) = root {
        let unskip = config
            .map(|c| c.unskip_names_for(&repo.url, Some(root)))
            .unwrap_or(&[]);
        let excludes = config
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
        let clone = matching
            .next()
            .ok_or_else(|| anyhow::anyhow!("{}: recorded bare clone is unavailable", repo.uid))?;
        anyhow::ensure!(
            matching.next().is_none(),
            "{}: multiple recorded bare clones",
            repo.uid
        );
        Box::new(
            GitBareReader::with_limits(&clone.path, &repo.indexed_sha, limits).local_objects_only(),
        )
    };
    let policy = state.store.get_repo_index_policy(&repo.uid)?;
    // nw-680: a policy recorded under a SUPERSEDED fingerprint version, with
    // the same configured parameters, is accepted. Demanding the current
    // version meant that after nw-652's v2 bump any one repo not yet
    // re-indexed failed this capture for EVERY repo, so the sidecar stayed at
    // its old generation and the debt was never paid. The trade-off: such a
    // repo's graph may still lack a non-build `target/` (what v2 restores on
    // its next index), and a manifest inside one is captured here anyway,
    // which can add an entry root, never remove one. A policy that differs in
    // its configured parameters still refuses (nw-705: this repo only).
    let accepted = policy.as_deref().is_some_and(|recorded| {
        recorded == reader.eligibility_fingerprint()
            || reader
                .superseded_eligibility_fingerprints()
                .iter()
                .any(|old| old == recorded)
    });
    anyhow::ensure!(
        accepted,
        "{}: recorded source eligibility is absent or differs from current configuration; \
         re-index it with `nestweaver index --repo <path>`",
        repo.uid
    );
    let captured = manifest::capture_manifest_inputs(reader.as_ref(), remaining, deadline)
        .map_err(|e| e.context(repo.uid.clone()))?;
    Ok((policy, captured))
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
    // D3: one budget for every capture made while the gate is held.
    let pass_deadline = Instant::now() + before.budget.pass;
    let after = capture_within(state, before.budget.clone(), Some(pass_deadline))?;
    anyhow::ensure!(
        before.generation == after.generation
            && before.identity == after.identity
            && before.inventory == after.inventory
            && before.debt == after.debt
            && before.inputs == after.inputs
            && before.failures == after.failures,
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
    // nw-705: the refused repositories travel in the same envelope as the
    // manifests (review M4), so no reader can take a partial snapshot as
    // complete coverage.
    manifest::save_manifest_snapshot_for_db(
        &manifest::ManifestSnapshot {
            repos: manifests,
            failures: after.failures.clone(),
        },
        &state.store,
        &state.db_path,
    )?;
    // Fail closed if an external editor changed bytes during the atomic save.
    // Leave durable debt; never let the new envelope hide this pending work.
    let verified = capture_within(state, after.budget.clone(), Some(pass_deadline))?;
    if after.identity != verified.identity
        || after.generation != verified.generation
        || after.inventory != verified.inventory
        || after.inputs != verified.inputs
        || after.failures != verified.failures
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

/// Review M5: first and longest wait between probes of refused repos.
const PARTIAL_PROBE_BACKOFF_MIN: Duration = Duration::from_secs(30);
const PARTIAL_PROBE_BACKOFF_MAX: Duration = Duration::from_secs(30 * 60);

fn next_partial_backoff(current: Duration) -> Duration {
    current.saturating_mul(2).min(PARTIAL_PROBE_BACKOFF_MAX)
}

/// Review M5: probe only the refused repositories. True when one is gone,
/// now captures, or is refused for a different reason — a full rebuild then
/// publishes the change. Each probe gets its own budget and deadline, so
/// the healthy repos are not re-listed.
pub(super) fn refused_repos_changed(
    state: &DaemonState,
    failures: &[manifest::ManifestRepoFailure],
) -> bool {
    let Ok(repos) = state.store.list_repos(None) else {
        return true;
    };
    let Ok(config) = current_repo_eligibility_config(state) else {
        return true;
    };
    let limits = state
        .instance_cfg
        .as_ref()
        .map(|c| c.indexing.limits())
        .unwrap_or_default();
    failures.iter().any(|failure| {
        let Some(repo) = repos.iter().find(|r| r.uid == failure.repo_uid) else {
            return true;
        };
        let mut budget = 32 * 1024 * 1024;
        let deadline = Instant::now() + REPO_CAPTURE_BUDGET;
        match capture_repo(state, config.as_ref(), limits, repo, &mut budget, deadline) {
            Ok(_) => {
                // D3: a repository refused for contention is captured again
                // by the full rebuild this triggers.
                clear_overruns(state, &repo.uid);
                true
            }
            // D3: still too slow; a contention refusal stands.
            Err(error)
                if manifest::is_manifest_capture_deadline(&error)
                    && failure.reason.starts_with("contention: ") =>
            {
                false
            }
            Err(error) => format!("{error:#}") != failure.reason,
        }
    })
}

/// D3: report a capture deferred by contention as `deferred`, with the
/// contention message, rather than as refused repositories.
fn publish_contention(
    runtime: &manifest::ManifestRecoveryRuntime,
    state: &DaemonState,
    attempts: u32,
    error: &anyhow::Error,
) {
    tracing::info!(error = %format_args!("{error:#}"), "manifest reconciliation deferred");
    runtime.publish(
        "deferred",
        attempts,
        Some(30),
        Some(ManifestUnavailable::new(
            ManifestUnavailableReason::PendingSourceChange,
            state.store.graph_generation(),
            format!("{error:#}"),
        )),
    );
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
    // nw-705: the capture key a partial rebuild last settled, and when the
    // refused repositories were last probed again.
    let mut settled_key = String::new();
    let mut last_partial_probe: Option<Instant> = None;
    // Review M5: how long until the refused repos are probed again. Doubles
    // on each probe that finds nothing changed; reset whenever any other
    // path runs (a re-index marks debt, which takes that path).
    let mut partial_backoff = PARTIAL_PROBE_BACKOFF_MIN;
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
                let failures = manifest::load_manifest_failures(&state.db_path);
                if failures.is_empty() {
                    runtime.publish("ready", 0, None, None);
                    retry = RetryBudget::default();
                    last_key.clear();
                    settled_key.clear();
                    last_partial_probe = None;
                    partial_backoff = PARTIAL_PROBE_BACKOFF_MIN;
                    delay = 2;
                    continue;
                }
                // nw-705: the other repositories are current; keep naming the
                // refused ones. Review M5: re-probe ONLY those repos, with
                // exponential backoff, and rebuild only when one changed, so
                // a repo that stays broken costs neither a full listing every
                // 30 s nor a flapping deadline.
                let partial = ManifestUnavailable::new(
                    ManifestUnavailableReason::IncompleteCoverage,
                    state.store.graph_generation(),
                    manifest::describe_manifest_failures(&failures),
                );
                runtime.publish(
                    "partial",
                    0,
                    Some(partial_backoff.as_secs()),
                    Some(partial.clone()),
                );
                delay = 2;
                if last_partial_probe.is_some_and(|at| at.elapsed() < partial_backoff) {
                    continue;
                }
                last_partial_probe = Some(Instant::now());
                let probe_state = Arc::clone(&state);
                let changed = tokio::task::spawn_blocking(move || {
                    refused_repos_changed(&probe_state, &failures)
                })
                .await
                .unwrap_or(true);
                if !changed {
                    partial_backoff = next_partial_backoff(partial_backoff);
                    continue;
                }
                partial_backoff = PARTIAL_PROBE_BACKOFF_MIN;
                partial
            }
            Ok(Err(error)) => {
                partial_backoff = PARTIAL_PROBE_BACKOFF_MIN;
                last_partial_probe = None;
                error
            }
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
        let snapshot =
            tokio::task::spawn_blocking(move || capture(&capture_state, REPO_CAPTURE_BUDGET)).await;
        let snapshot = match snapshot {
            Ok(Ok(snapshot)) => snapshot,
            Ok(Err(error)) if is_contention_deferral(&error) => {
                publish_contention(&runtime, &state, retry.attempts, &error);
                delay = 30;
                continue;
            }
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
            "{}:{}:{:?}:{:?}:{:?}",
            snapshot.generation,
            snapshot.inventory,
            snapshot.debt,
            source_digests,
            snapshot.failures
        );
        if key == settled_key {
            // Nothing changed since the partial rebuild that recorded these
            // refusals: no rewrite, no write lease.
            continue;
        }
        if key != last_key {
            retry = RetryBudget::default();
            last_key = key.clone();
        }
        let partial = !snapshot.failures.is_empty();
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
                if partial {
                    settled_key = key;
                    last_partial_probe = Some(Instant::now());
                } else {
                    runtime.publish("ready", retry.attempts, None, None);
                }
                retry = RetryBudget::default();
                last_key.clear();
                delay = 2;
            }
            Ok(Err(error)) if is_contention_deferral(&error) => {
                // D3: contention is not a failed attempt. It does not spend
                // the retry budget or end in "blocked".
                retry = RetryBudget::default();
                publish_contention(&runtime, &state, 0, &error);
                delay = 30;
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
    fn partial_probe_backoff_doubles_to_thirty_minutes() {
        let mut backoff = PARTIAL_PROBE_BACKOFF_MIN;
        let mut seen = vec![backoff.as_secs()];
        for _ in 0..8 {
            backoff = next_partial_backoff(backoff);
            seen.push(backoff.as_secs());
        }
        assert_eq!(seen, [30, 60, 120, 240, 480, 960, 1800, 1800, 1800]);
    }

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
