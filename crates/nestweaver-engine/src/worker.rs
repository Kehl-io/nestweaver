//! Worker pool that claims jobs from the SQLite queue, fetches repos via bare
//! clones, and indexes them via `GitBareReader`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Semaphore;

use crate::bare_clone::BareCloneWorkspace;
use crate::circuit_breaker::RemoteCircuitBreakers;
use crate::config::RepoType;
use crate::content_reader::ContentReader;
use crate::jobs::{IndexJob, JobQueue, canonical_repo_id};

#[derive(Debug)]
struct JobCancelled;

impl std::fmt::Display for JobCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("job cancelled")
    }
}

impl std::error::Error for JobCancelled {}

/// Shared indexing status that can be observed by other components (e.g. the
/// daemon's `brain_status` handler) to report whether indexing is in progress.
#[derive(Clone)]
pub struct IndexingStatus {
    /// Whether any worker is currently indexing.
    pub active: Arc<AtomicBool>,
    /// The repo currently being indexed (empty when idle).
    pub current_repo: Arc<tokio::sync::RwLock<String>>,
    /// Number of pending + running jobs.
    pub queue_depth: Arc<AtomicU32>,
    /// Number of spawned tasks that are still running. Used together with
    /// the SQLite queue depth to avoid prematurely reporting idle when
    /// tasks are in flight but no jobs are pending.
    pub in_flight: Arc<AtomicU32>,
}

impl IndexingStatus {
    pub fn new() -> Self {
        Self {
            active: Arc::new(AtomicBool::new(false)),
            current_repo: Arc::new(tokio::sync::RwLock::new(String::new())),
            queue_depth: Arc::new(AtomicU32::new(0)),
            in_flight: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Create from existing Arc fields (e.g. shared with DaemonState).
    pub fn from_arcs(
        active: Arc<AtomicBool>,
        current_repo: Arc<tokio::sync::RwLock<String>>,
        queue_depth: Arc<AtomicU32>,
    ) -> Self {
        Self {
            active,
            current_repo,
            queue_depth,
            in_flight: Arc::new(AtomicU32::new(0)),
        }
    }
}

impl Default for IndexingStatus {
    fn default() -> Self {
        Self::new()
    }
}

/// Coordinates concurrent indexing workers. Each worker claims a job from the
/// queue, fetches the latest commits into a bare clone, and indexes changed
/// code repos incrementally unless a full refresh is required.
pub struct WorkerPool {
    concurrency: usize,
    semaphore: Arc<Semaphore>,
    /// Per-repo index strategy, keyed by [`canonical_repo_id`] of the repo URL.
    /// Repos absent from the map (or mapped to [`RepoType::Code`]) are indexed
    /// as code; entries mapped to [`RepoType::Vault`] are indexed as markdown.
    /// Populated from the instance config by the daemon; empty by default, so
    /// an unconfigured pool indexes everything as code (the prior behaviour).
    repo_types: Arc<HashMap<String, RepoType>>,
    /// Tracks successful incremental code updates so server mode can
    /// periodically force a full refresh and bound graph drift.
    reindex_tracker: Arc<Mutex<crate::scheduler::ReindexTracker>>,
}

impl WorkerPool {
    pub fn new(concurrency: usize) -> Self {
        Self {
            concurrency,
            semaphore: Arc::new(Semaphore::new(concurrency)),
            repo_types: Arc::new(HashMap::new()),
            reindex_tracker: Arc::new(Mutex::new(crate::scheduler::ReindexTracker::new())),
        }
    }

    /// Attach per-repo index strategies resolved from the instance config.
    ///
    /// The map is keyed by [`canonical_repo_id`] of each repo's URL. Only repos
    /// that should be indexed as something other than code need an entry, but
    /// supplying the full set is also fine.
    pub fn with_repo_types(mut self, repo_types: HashMap<String, RepoType>) -> Self {
        self.repo_types = Arc::new(repo_types);
        self
    }

    /// Number of concurrent workers this pool allows.
    pub fn concurrency(&self) -> usize {
        self.concurrency
    }

    /// Run the worker loop. Claims jobs from the queue, processes them with
    /// bounded concurrency via `tokio::spawn`. Exits when `shutdown` fires.
    ///
    /// The optional `status` is updated as jobs start/finish so callers (e.g.
    /// the daemon's `brain_status` RPC) can report indexing progress.
    pub async fn run(
        &self,
        queue: Arc<Mutex<JobQueue>>,
        workspace: Arc<BareCloneWorkspace>,
        store: Arc<nestweaver_store::GraphStore>,
        instance_id: String,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
        status: Option<IndexingStatus>,
    ) {
        self.run_with_drain(
            queue,
            workspace,
            store,
            instance_id,
            &mut shutdown,
            status,
            None,
            None,
        )
        .await;
    }

    /// Run the worker loop with an optional drained flag and write mutex.
    ///
    /// When `drained` is `Some(flag)` and the flag is `true`, the worker sleeps
    /// instead of claiming new jobs (in-flight jobs finish naturally). Each job's
    /// write phase acquires `write_mutex`, so an in-progress backup — which holds
    /// that lock while it copies — simply makes the worker WAIT for the lock.
    /// Writes are never skipped or dropped; the job proceeds once the lock frees.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_with_drain(
        &self,
        queue: Arc<Mutex<JobQueue>>,
        workspace: Arc<BareCloneWorkspace>,
        store: Arc<nestweaver_store::GraphStore>,
        instance_id: String,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
        status: Option<IndexingStatus>,
        drained: Option<Arc<AtomicBool>>,
        write_mutex: Option<Arc<tokio::sync::Mutex<()>>>,
    ) {
        let circuit_breakers = Arc::new(RemoteCircuitBreakers::new());
        let repo_types = self.repo_types.clone();

        // Rehydrate the reindex tracker from the persisted store so the
        // periodic-full update counter and 7-day backstop survive a daemon
        // restart. The in-memory tracker is only a cache; the DB is the
        // cross-restart source of truth.
        {
            let rows = {
                let q = queue.lock().expect("job queue lock poisoned");
                q.load_reindex_state()
            };
            match rows {
                Ok(rows) => {
                    let mut tracker = self
                        .reindex_tracker
                        .lock()
                        .expect("reindex tracker lock poisoned");
                    tracker.load_persisted(rows);
                }
                Err(e) => tracing::error!("load reindex state: {e}"),
            }
        }

        // Track in-flight per-job tasks so a shutdown can drain them instead of
        // abandoning an in-progress index write.
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            // Reap finished jobs so the set doesn't grow unbounded.
            while tasks.try_join_next().is_some() {}

            // Check shutdown signal.
            if *shutdown.borrow() {
                break;
            }

            // If drained, sleep instead of claiming new jobs.
            if let Some(ref flag) = drained
                && flag.load(Ordering::Relaxed)
            {
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                    _ = shutdown.changed() => {}
                }
                continue;
            }

            // Try to claim the next job (behind a std::sync::Mutex since
            // rusqlite::Connection is !Sync).
            let job = {
                let q = queue.lock().expect("job queue lock poisoned");
                // Update queue depth while we hold the lock.
                if let Some(ref st) = status
                    && let Ok(depth) = q.queue_depth()
                {
                    st.queue_depth
                        .store((depth.pending + depth.running) as u32, Ordering::Relaxed);
                }
                q.claim_next(2) // 2s debounce
            };

            let job = match job {
                Ok(Some(job)) => job,
                Ok(None) => {
                    // No pending jobs — only mark fully idle when no
                    // spawned tasks are still running.
                    if let Some(ref st) = status {
                        let flying = st.in_flight.load(Ordering::Relaxed);
                        if flying == 0 {
                            st.active.store(false, Ordering::Relaxed);
                            *st.current_repo.write().await = String::new();
                            st.queue_depth.store(0, Ordering::Relaxed);
                        }
                    }
                    // Wait briefly or until shutdown.
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                        _ = shutdown.changed() => {}
                    }
                    continue;
                }
                Err(e) => {
                    tracing::error!("claim job: {e}");
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                        _ = shutdown.changed() => {}
                    }
                    continue;
                }
            };

            // Signal that indexing is active.
            if let Some(ref st) = status {
                st.active.store(true, Ordering::Relaxed);
                *st.current_repo.write().await = job.repo_id.clone();
            }

            // Track in-flight tasks so queue depth reflects running work.
            if let Some(ref st) = status {
                st.in_flight.fetch_add(1, Ordering::Relaxed);
            }

            // Acquire a semaphore permit to bound concurrency.
            let permit = self.semaphore.clone().acquire_owned().await.unwrap();
            let queue = queue.clone();
            let workspace = workspace.clone();
            let store = store.clone();
            let instance_id = instance_id.clone();
            let status_clone = status.clone();
            let write_mutex = write_mutex.clone();
            let circuit_breakers = circuit_breakers.clone();
            let repo_types = repo_types.clone();
            let reindex_tracker = self.reindex_tracker.clone();

            tasks.spawn(async move {
                let _permit = permit;

                // process_job is CPU-bound (parsing + indexing), run on the
                // blocking pool so we don't starve the tokio runtime.
                let result = {
                    let job_clone = job.clone();
                    let queue_check = queue.clone();
                    // Resolve how this repo should be indexed (code vs vault).
                    let repo_type = repo_types
                        .get(&canonical_repo_id(&job_clone.repo_url))
                        .cloned()
                        .unwrap_or(RepoType::Code);
                    tokio::task::spawn_blocking(move || {
                        let prepared = prepare_job(
                            &job_clone,
                            &workspace,
                            &store,
                            &instance_id,
                            Some(&circuit_breakers),
                            repo_type,
                        )?;

                        // Check if the job was cancelled (admin repo removal)
                        // before indexing. Verifies both ID and repo_id to
                        // guard against SQLite ID reuse.
                        {
                            let q = queue_check.lock().expect("job queue lock");
                            if !q
                                .job_is_active(job_clone.id, &job_clone.repo_id)
                                .unwrap_or(false)
                            {
                                tracing::info!(
                                    repo = %job_clone.repo_id,
                                    "job cancelled (repo removed), skipping"
                                );
                                return Ok(());
                            }
                        }
                        if let Some(prepared) = prepared {
                            let queue_for_gate = queue_check.clone();
                            let job_for_gate = job_clone.clone();
                            let write_mutex_for_gate = write_mutex.clone();
                            let force_full_reindex = if prepared.repo_type == RepoType::Code {
                                let tracker = reindex_tracker
                                    .lock()
                                    .expect("reindex tracker lock poisoned");
                                should_force_full_reindex(
                                    Some(&tracker),
                                    &prepared.repo_id,
                                    current_file_count(&store, &instance_id, &prepared.repo_url),
                                    crate::scheduler::ReindexTracker::random_spot_check(),
                                )
                            } else {
                                false
                            };

                            let outcome = commit_prepared_job_with_reindex_decision(
                                &prepared,
                                &store,
                                &instance_id,
                                force_full_reindex,
                                move || {
                                    // Acquire the write lock. A backup in progress holds this lock
                                    // while it copies files, so this simply waits until the backup
                                    // finishes — the write is deferred by contention, never dropped.
                                    let _write_guard = write_mutex_for_gate
                                        .as_ref()
                                        .map(|m| m.clone().blocking_lock_owned());
                                    let q = queue_for_gate.lock().expect("job queue lock");
                                    if !q
                                        .job_is_active(job_for_gate.id, &job_for_gate.repo_id)
                                        .unwrap_or(false)
                                    {
                                        tracing::info!(
                                            repo = %job_for_gate.repo_id,
                                            "job cancelled (repo removed), skipping"
                                        );
                                        return Err(anyhow::Error::new(JobCancelled));
                                    }
                                    Ok(_write_guard)
                                },
                            )?;

                            if prepared.repo_type == RepoType::Code {
                                persist_reindex_outcome(
                                    &queue_check,
                                    &reindex_tracker,
                                    &prepared.repo_id,
                                    outcome,
                                );
                            }

                            Ok(())
                        } else {
                            Ok(())
                        }
                    })
                    .await
                };

                match result {
                    Ok(Ok(())) => {
                        let q = queue.lock().expect("job queue lock poisoned");
                        let _ = q.complete(job.id, &job.repo_id);
                        if let Ok(true) = q.requeue_if_stale(&job.repo_id) {
                            tracing::info!(repo = %job.repo_id, "re-queued: push arrived during indexing");
                        }
                        tracing::info!(repo = job.repo_id, "index complete");
                    }
                    Ok(Err(e)) if is_job_cancelled_error(&e) => {
                        let q = queue.lock().expect("job queue lock poisoned");
                        let _ = q.complete(job.id, &job.repo_id);
                        tracing::info!(repo = job.repo_id, "index cancelled");
                    }
                    Ok(Err(e)) => {
                        let is_poison = is_poison_error(&e);
                        let q = queue.lock().expect("job queue lock poisoned");
                        let _ = q.fail(job.id, &e.to_string(), is_poison);
                        tracing::error!(repo = job.repo_id, error = %e, "index failed");
                    }
                    Err(join_err) => {
                        // Task panicked or was cancelled.
                        let q = queue.lock().expect("job queue lock poisoned");
                        let _ = q.fail(job.id, &format!("task panic: {join_err}"), false);
                        tracing::error!(repo = job.repo_id, error = %join_err, "worker task panicked");
                    }
                }

                // Update queue depth and in-flight count after job completion.
                if let Some(ref st) = status_clone {
                    let remaining_in_flight = st.in_flight.fetch_sub(1, Ordering::Relaxed) - 1;
                    let q = queue.lock().expect("job queue lock poisoned");
                    if let Ok(depth) = q.queue_depth() {
                        let total = (depth.pending + depth.running) as u32;
                        st.queue_depth
                            .store(total.max(remaining_in_flight), Ordering::Relaxed);
                        if total == 0 && remaining_in_flight == 0 {
                            st.active.store(false, Ordering::Relaxed);
                            // Clear current_repo — requires async, so we
                            // spawn a minimal task.
                            let cr = st.current_repo.clone();
                            tokio::spawn(async move {
                                *cr.write().await = String::new();
                            });
                        }
                    }
                }
            });
        }

        // Shutdown signalled: drain in-flight jobs so no index write is
        // abandoned mid-flight. spawn_blocking work cannot be aborted, so each
        // remaining task is awaited to completion before we return.
        while tasks.join_next().await.is_some() {}
    }
}

/// Process a single indexing job: fetch, compare SHAs, index if needed.
#[allow(dead_code)]
fn process_job(
    job: &IndexJob,
    workspace: &BareCloneWorkspace,
    store: &nestweaver_store::GraphStore,
    instance_id: &str,
) -> Result<(), anyhow::Error> {
    let Some(prepared) = prepare_job(job, workspace, store, instance_id, None, RepoType::Code)?
    else {
        return Ok(());
    };
    commit_prepared_job(&prepared, store, instance_id)
}

#[derive(Debug)]
struct PreparedIndexJob {
    repo_id: String,
    repo_url: String,
    bare_path: std::path::PathBuf,
    remote_sha: String,
    /// How the repo's contents should be indexed (code vs markdown vault).
    repo_type: RepoType,
}

fn prepare_job(
    job: &IndexJob,
    workspace: &BareCloneWorkspace,
    store: &nestweaver_store::GraphStore,
    instance_id: &str,
    circuit_breakers: Option<&RemoteCircuitBreakers>,
    repo_type: RepoType,
) -> Result<Option<PreparedIndexJob>, anyhow::Error> {
    let fetch = || -> Result<_, anyhow::Error> {
        // 1. Ensure the bare clone exists.
        let bare = workspace.ensure_clone(&job.repo_url)?;

        // 2. Fetch latest refs from origin.
        bare.fetch_branch(job.branch.as_deref())?;

        // 3. Discover remote SHA — use the configured branch if set.
        let remote_sha = match &job.branch {
            Some(branch) => bare.sha_for_ref(&format!("refs/heads/{}", branch))?,
            None => bare
                .sha_for_ref("FETCH_HEAD")
                .or_else(|_| bare.head_sha())?,
        };

        Ok((bare.path.clone(), remote_sha))
    };

    let (bare_path, remote_sha) = if let Some(cb) = circuit_breakers {
        let host = RemoteCircuitBreakers::extract_host(&job.repo_url);
        cb.call(&host, fetch)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
    } else {
        fetch()?
    };

    // 4. Compare against the SHA we last indexed.
    let r_uid = nestweaver_schema::repo_uid(instance_id, &job.repo_url);
    let indexed_sha = store
        .lookup_repo(&r_uid)
        .ok()
        .flatten()
        .map(|r| r.indexed_sha)
        .unwrap_or_default();

    if remote_sha == indexed_sha {
        tracing::debug!(repo = job.repo_id, "already up to date");
        return Ok(None);
    }

    Ok(Some(PreparedIndexJob {
        repo_id: job.repo_id.clone(),
        repo_url: job.repo_url.clone(),
        bare_path,
        remote_sha,
        repo_type,
    }))
}

fn commit_prepared_job(
    prepared: &PreparedIndexJob,
    store: &nestweaver_store::GraphStore,
    instance_id: &str,
) -> Result<(), anyhow::Error> {
    commit_prepared_job_with_write_gate(prepared, store, instance_id, || Ok::<_, anyhow::Error>(()))
}

fn commit_prepared_job_with_write_gate<G, F>(
    prepared: &PreparedIndexJob,
    store: &nestweaver_store::GraphStore,
    instance_id: &str,
    acquire_write_guard: F,
) -> Result<(), anyhow::Error>
where
    F: FnOnce() -> Result<G, anyhow::Error>,
{
    commit_prepared_job_with_reindex_tracker(
        prepared,
        store,
        instance_id,
        None,
        acquire_write_guard,
    )
}

fn commit_prepared_job_with_reindex_tracker<G, F>(
    prepared: &PreparedIndexJob,
    store: &nestweaver_store::GraphStore,
    instance_id: &str,
    mut reindex_tracker: Option<&mut crate::scheduler::ReindexTracker>,
    acquire_write_guard: F,
) -> Result<(), anyhow::Error>
where
    F: FnOnce() -> Result<G, anyhow::Error>,
{
    let force_full_reindex = should_force_full_reindex(
        reindex_tracker.as_deref(),
        &prepared.repo_id,
        current_file_count(store, instance_id, &prepared.repo_url),
        false,
    );

    let outcome = commit_prepared_job_with_reindex_decision(
        prepared,
        store,
        instance_id,
        force_full_reindex,
        acquire_write_guard,
    )?;

    if let Some(tracker) = reindex_tracker.as_mut() {
        record_reindex_outcome(tracker, &prepared.repo_id, outcome);
    }

    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReindexOutcome {
    Skipped,
    Full,
    Incremental,
}

fn current_file_count(
    store: &nestweaver_store::GraphStore,
    instance_id: &str,
    repo_url: &str,
) -> u64 {
    let r_uid = nestweaver_schema::repo_uid(instance_id, repo_url);
    store
        .list_files_by_repo(&r_uid)
        .map(|files| files.len() as u64)
        .unwrap_or(0)
}

fn should_force_full_reindex(
    reindex_tracker: Option<&crate::scheduler::ReindexTracker>,
    repo_id: &str,
    file_count: u64,
    spot_check: bool,
) -> bool {
    reindex_tracker
        .is_some_and(|tracker| tracker.needs_full_reindex(repo_id, file_count) || spot_check)
}

fn record_reindex_outcome(
    tracker: &mut crate::scheduler::ReindexTracker,
    repo_id: &str,
    outcome: ReindexOutcome,
) {
    match outcome {
        ReindexOutcome::Incremental => tracker.record_incremental(repo_id),
        ReindexOutcome::Full => tracker.reset(repo_id),
        ReindexOutcome::Skipped => {}
    }
}

/// Record a reindex outcome in the in-memory tracker and write the updated
/// counter/backstop through to the persisted queue so it survives a restart.
///
/// A `Skipped` outcome leaves the tracker count and `last_full` unchanged
/// (`record_reindex_outcome` is a no-op for it), so persisting would just
/// re-write identical state on every poll of an unchanged code repo. Gate the
/// whole block on a non-`Skipped` outcome to avoid that redundant DB upsert.
fn persist_reindex_outcome(
    queue: &Mutex<JobQueue>,
    reindex_tracker: &Mutex<crate::scheduler::ReindexTracker>,
    repo_id: &str,
    outcome: ReindexOutcome,
) {
    if outcome == ReindexOutcome::Skipped {
        return;
    }
    let (count, last_full) = {
        let mut tracker = reindex_tracker
            .lock()
            .expect("reindex tracker lock poisoned");
        record_reindex_outcome(&mut tracker, repo_id, outcome);
        (tracker.count(repo_id), tracker.last_full_unix(repo_id))
    };
    let q = queue.lock().expect("job queue lock");
    if let Err(e) = q.upsert_reindex_state(repo_id, count, last_full) {
        tracing::error!(repo = %repo_id, "persist reindex state: {e}");
    }
}

fn commit_prepared_job_with_reindex_decision<G, F>(
    prepared: &PreparedIndexJob,
    store: &nestweaver_store::GraphStore,
    instance_id: &str,
    force_full_reindex: bool,
    acquire_write_guard: F,
) -> Result<ReindexOutcome, anyhow::Error>
where
    F: FnOnce() -> Result<G, anyhow::Error>,
{
    let r_uid = nestweaver_schema::repo_uid(instance_id, &prepared.repo_url);
    let existing_repo = store.lookup_repo(&r_uid).ok().flatten();
    let indexed_sha = existing_repo
        .as_ref()
        .map(|r| r.indexed_sha.as_str())
        .unwrap_or("");

    if prepared.remote_sha == indexed_sha {
        tracing::debug!(repo = prepared.repo_id, "already up to date");
        return Ok(ReindexOutcome::Skipped);
    }

    // Build a reader over the bare clone at the new SHA.
    let reader =
        crate::content_reader::GitBareReader::new(&prepared.bare_path, &prepared.remote_sha);

    match prepared.repo_type {
        RepoType::Vault => {
            // Markdown-vault repo: index Note/Section/Heading nodes via the
            // markdown indexer, then record the indexed SHA on the Repo node
            // (nw-003) so an unchanged vault is skipped on the next poll. The
            // scan + parse passes run off the write gate; the gate is acquired
            // only for the database-write phase (nw-006) — mirroring the code
            // path — and the closure also performs the job-cancellation check.
            crate::index_markdown_with_reader_and_write_gate(
                &reader,
                store,
                instance_id,
                &prepared.repo_url,
                &prepared.remote_sha,
                acquire_write_guard,
            )?;

            // Discover cross-domain (Note↔Symbol) edges now that the vault's
            // notes are indexed. The GitBareReader is passed via vault_readers
            // so note bodies can be read from the bare clone — without this,
            // std::fs::read_to_string would fail silently (no working tree)
            // and zero Note-to-Symbol edges would be built.
            let vault_uid = {
                let root_str = reader.root().to_string_lossy();
                nestweaver_schema::vault_uid(instance_id, &root_str)
            };
            let mut vault_readers = crate::cross_domain::VaultReaders::new();
            vault_readers.insert(
                vault_uid,
                &reader as &dyn crate::content_reader::ContentReader,
            );
            if let Err(e) =
                crate::cross_domain::discover_cross_domain_links_with_readers(store, &vault_readers)
            {
                tracing::warn!(
                    repo = %prepared.repo_url,
                    "cross-domain discovery after vault index failed: {e}"
                );
            }

            Ok(ReindexOutcome::Full)
        }
        RepoType::Code => {
            let can_incremental = !indexed_sha.is_empty()
                && !force_full_reindex
                && crate::git_diff::is_ancestor(
                    &prepared.bare_path,
                    indexed_sha,
                    &prepared.remote_sha,
                );

            if can_incremental {
                let result = crate::index::incremental_index_with_reader_and_write_gate(
                    &reader,
                    &prepared.bare_path,
                    store,
                    instance_id,
                    &prepared.repo_url,
                    &prepared.remote_sha,
                    acquire_write_guard,
                )?;
                if result.fell_back_to_full {
                    Ok(ReindexOutcome::Full)
                } else {
                    Ok(ReindexOutcome::Incremental)
                }
            } else {
                // Server full path: the bare reader passes no filemeta cache,
                // so the core indexer's bulk-delete currently fires for us. That
                // is an implicit invariant, not a guarantee — make pruning of
                // removed files explicit here so a force-push that drops files
                // can never leave stale File/Symbol or derived nodes behind.
                // Purge the repo's files/symbols and derived nodes before the
                // full re-index. Do NOT use `delete_repo_all_data`: it drops the
                // Repo node, and the full path passes `name=None`, which would
                // discard a display-name override on re-insert.
                if !indexed_sha.is_empty() {
                    let _ = store.bulk_delete_repo_files_and_symbols(&r_uid);
                    let _ = store.clear_repo_derived_nodes(&r_uid);
                }
                crate::index_with_reader_and_write_gate(
                    &reader,
                    store,
                    instance_id,
                    &prepared.repo_url,
                    &prepared.remote_sha,
                    None,
                    acquire_write_guard,
                )?;
                Ok(ReindexOutcome::Full)
            }
        }
    }
}

fn is_job_cancelled_error(e: &anyhow::Error) -> bool {
    e.downcast_ref::<JobCancelled>().is_some()
}

/// Heuristic: is this an error that will never succeed on retry?
fn is_poison_error(e: &anyhow::Error) -> bool {
    let msg = e.to_string().to_lowercase();
    msg.contains("401")
        || msg.contains("403")
        || msg.contains("not found")
        || msg.contains("authentication")
        || msg.contains("permission denied")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::JobTrigger;
    use std::process::Command;
    use tempfile::TempDir;

    /// Helper: create a source git repo with a single commit.
    fn create_source_repo(dir: &std::path::Path, files: &[(&str, &str)]) {
        std::fs::create_dir_all(dir).unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir)
            .output()
            .unwrap();
        for (path, content) in files {
            let full = dir.join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&full, content).unwrap();
        }
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir)
            .output()
            .unwrap();
    }

    fn commit_file(repo: &std::path::Path, path: &str, content: &str, message: &str) {
        let full = repo.join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&full, content).unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(repo)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", message])
            .current_dir(repo)
            .output()
            .unwrap();
    }

    fn make_code_job(id: i64, repo_id: &str, url: &str) -> IndexJob {
        IndexJob {
            id,
            repo_id: repo_id.to_string(),
            repo_url: url.to_string(),
            trigger: JobTrigger::Unindexed,
            priority: 0,
            status: crate::jobs::JobStatus::Running,
            attempt: 1,
            max_attempts: 4,
            error_msg: None,
            branch: None,
            created_at: 0,
            updated_at: 0,
            started_at: Some(0),
            completed_at: None,
        }
    }

    #[test]
    fn is_poison_detects_auth_errors() {
        let e = anyhow::anyhow!("remote: HTTP 401 Unauthorized");
        assert!(is_poison_error(&e));

        let e = anyhow::anyhow!("Permission denied (publickey)");
        assert!(is_poison_error(&e));

        let e = anyhow::anyhow!("repository not found");
        assert!(is_poison_error(&e));
    }

    #[test]
    fn is_poison_allows_transient_errors() {
        let e = anyhow::anyhow!("connection reset by peer");
        assert!(!is_poison_error(&e));

        let e = anyhow::anyhow!("timeout after 30s");
        assert!(!is_poison_error(&e));
    }

    #[test]
    fn reindex_decision_includes_random_spot_check() {
        let tracker = crate::scheduler::ReindexTracker::new();

        assert!(
            should_force_full_reindex(Some(&tracker), "repo-a", 10_000, true),
            "random spot checks must force a full server-mode reindex"
        );
        assert!(
            !should_force_full_reindex(Some(&tracker), "repo-a", 10_000, false),
            "fresh repos below count/time thresholds should stay incremental"
        );
        assert!(
            !should_force_full_reindex(None, "repo-a", 10_000, true),
            "spot checks only apply when server-mode tracking is enabled"
        );
    }

    #[test]
    fn process_job_indexes_local_bare_repo() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source");
        create_source_repo(
            &src,
            &[
                ("src/main.rs", "fn main() { println!(\"hello\"); }"),
                ("src/lib.rs", "pub fn greet() -> &'static str { \"hi\" }"),
            ],
        );
        let url = format!("file://{}", src.display());

        // Set up workspace + store + job.
        let ws = BareCloneWorkspace::new(&tmp.path().join("workspace")).unwrap();
        let store = nestweaver_store::GraphStore::in_memory().unwrap();
        let instance_id = "test-instance";

        let job = IndexJob {
            id: 1,
            repo_id: "test-repo".to_string(),
            repo_url: url.clone(),
            trigger: JobTrigger::Unindexed,
            priority: 0,
            status: crate::jobs::JobStatus::Running,
            attempt: 1,
            max_attempts: 4,
            error_msg: None,
            branch: None,
            created_at: 0,
            updated_at: 0,
            started_at: Some(0),
            completed_at: None,
        };

        // Process the job.
        process_job(&job, &ws, &store, instance_id).unwrap();

        // Verify: repo node was created in the store.
        let r_uid = nestweaver_schema::repo_uid(instance_id, &url);
        let repo = store.lookup_repo(&r_uid).unwrap();
        assert!(repo.is_some(), "repo should exist in store after indexing");

        // Verify: some symbols were indexed.
        let count = store.count_symbols().unwrap();
        assert!(count > 0, "should have indexed symbols from the repo");
        assert!(
            store.graph_generation() > 0,
            "server-mode reader indexing should invalidate generation-keyed caches"
        );
    }

    #[test]
    fn process_job_skips_when_already_indexed() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source");
        create_source_repo(&src, &[("a.txt", "hello")]);
        let url = format!("file://{}", src.display());

        let ws = BareCloneWorkspace::new(&tmp.path().join("workspace")).unwrap();
        let store = nestweaver_store::GraphStore::in_memory().unwrap();
        let instance_id = "test-instance";

        let job = IndexJob {
            id: 1,
            repo_id: "test-repo".to_string(),
            repo_url: url.clone(),
            trigger: JobTrigger::Webhook,
            priority: 1,
            status: crate::jobs::JobStatus::Running,
            attempt: 1,
            max_attempts: 4,
            error_msg: None,
            branch: None,
            created_at: 0,
            updated_at: 0,
            started_at: Some(0),
            completed_at: None,
        };

        // First index.
        process_job(&job, &ws, &store, instance_id).unwrap();

        // Second index should be a no-op (same SHA).
        process_job(&job, &ws, &store, instance_id).unwrap();
        // No assertion needed beyond "it didn't error" — the second call
        // should detect identical SHAs and return early.
    }

    #[test]
    fn code_repo_uses_incremental_index_after_initial_full_index() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source");
        create_source_repo(&src, &[("src/lib.rs", "pub fn one() -> i32 { 1 }\n")]);
        let url = format!("file://{}", src.display());

        let ws = BareCloneWorkspace::new(&tmp.path().join("workspace")).unwrap();
        let store = nestweaver_store::GraphStore::in_memory().unwrap();
        let instance_id = "test-instance";
        let mut tracker = crate::scheduler::ReindexTracker::new();

        let job = IndexJob {
            id: 1,
            repo_id: "incremental-repo".to_string(),
            repo_url: url.clone(),
            trigger: JobTrigger::Unindexed,
            priority: 0,
            status: crate::jobs::JobStatus::Running,
            attempt: 1,
            max_attempts: 4,
            error_msg: None,
            branch: None,
            created_at: 0,
            updated_at: 0,
            started_at: Some(0),
            completed_at: None,
        };

        let first = prepare_job(&job, &ws, &store, instance_id, None, RepoType::Code)
            .unwrap()
            .expect("initial code index should be prepared");
        commit_prepared_job_with_reindex_tracker(
            &first,
            &store,
            instance_id,
            Some(&mut tracker),
            || Ok::<_, anyhow::Error>(()),
        )
        .unwrap();
        assert_eq!(
            tracker.count(&job.repo_id),
            0,
            "initial code indexing must be a full index"
        );

        commit_file(
            &src,
            "src/added.rs",
            "pub fn two() -> i32 { 2 }\n",
            "add file",
        );

        let second = prepare_job(&job, &ws, &store, instance_id, None, RepoType::Code)
            .unwrap()
            .expect("updated code repo should be prepared");
        commit_prepared_job_with_reindex_tracker(
            &second,
            &store,
            instance_id,
            Some(&mut tracker),
            || Ok::<_, anyhow::Error>(()),
        )
        .unwrap();

        let r_uid = nestweaver_schema::repo_uid(instance_id, &url);
        let repo = store
            .lookup_repo(&r_uid)
            .unwrap()
            .expect("repo should exist after incremental index");
        assert_eq!(repo.indexed_sha, second.remote_sha);
        assert_eq!(
            tracker.count(&job.repo_id),
            1,
            "server-mode code updates should use the incremental path"
        );
    }

    /// nw-008: after an incremental re-index that shifts a changed file's
    /// exported symbol UID, the cross-file edges that *dependents* own
    /// (destroyed by the per-file `DETACH DELETE`) must be rebuilt so impact
    /// analysis still reaches the 1-hop and 2-hop reverse-dependents.
    ///
    /// Chain: a.ts → b.ts → c.ts (and d.ts → a.ts, 3 hops from c). Changing
    /// c.ts's exported symbol shifts its UID, which destroyed `b → c` until the
    /// next full re-index; this asserts it comes back on the incremental path.
    #[test]
    fn incremental_reresolves_two_hop_dependents() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source");
        create_source_repo(
            &src,
            &[
                ("c.ts", "export function cFunc(): number { return 1; }\n"),
                (
                    "b.ts",
                    "import { cFunc } from './c';\nexport function bFunc(): number { return cFunc() + 1; }\n",
                ),
                (
                    "a.ts",
                    "import { bFunc } from './b';\nexport function aFunc(): number { return bFunc() + 1; }\n",
                ),
                (
                    "d.ts",
                    "import { aFunc } from './a';\nexport function dFunc(): number { return aFunc() + 1; }\n",
                ),
            ],
        );
        let url = format!("file://{}", src.display());

        let ws = BareCloneWorkspace::new(&tmp.path().join("workspace")).unwrap();
        let store = nestweaver_store::GraphStore::in_memory().unwrap();
        let instance_id = "test-instance";
        let mut tracker = crate::scheduler::ReindexTracker::new();
        let job = make_code_job(1, "two-hop-repo", &url);

        // Initial full index.
        let first = prepare_job(&job, &ws, &store, instance_id, None, RepoType::Code)
            .unwrap()
            .expect("initial code index should be prepared");
        commit_prepared_job_with_reindex_tracker(
            &first,
            &store,
            instance_id,
            Some(&mut tracker),
            || Ok::<_, anyhow::Error>(()),
        )
        .unwrap();
        assert_eq!(tracker.count(&job.repo_id), 0, "initial index must be full");

        // Change c.ts's exported symbol so its start_line — and thus its
        // symbol UID — shifts. The name stays `cFunc` so b.ts still resolves.
        commit_file(
            &src,
            "c.ts",
            "// touched: shift cFunc's line so its symbol UID changes\nexport function cFunc(): number { return 2; }\n",
            "change c",
        );

        let second = prepare_job(&job, &ws, &store, instance_id, None, RepoType::Code)
            .unwrap()
            .expect("updated code repo should be prepared");
        commit_prepared_job_with_reindex_tracker(
            &second,
            &store,
            instance_id,
            Some(&mut tracker),
            || Ok::<_, anyhow::Error>(()),
        )
        .unwrap();
        assert_eq!(
            tracker.count(&job.repo_id),
            1,
            "the update must take the incremental path"
        );

        // Resolve c.ts's exported symbol at its NEW UID and walk impact.
        let new_c = store
            .find_symbol_by_name_and_file("cFunc", "c.ts")
            .unwrap()
            .expect("cFunc should exist in c.ts after incremental re-index");

        let impact = store.impact(&new_c.uid, 3, 0.0).unwrap();
        let impacted_files: std::collections::HashSet<&str> =
            impact.iter().map(|n| n.file_path.as_str()).collect();

        assert!(
            impacted_files.contains("b.ts"),
            "1-hop dependent b.ts must reach the re-resolved c.ts; impact files: {impacted_files:?}"
        );
        assert!(
            impacted_files.contains("a.ts"),
            "2-hop dependent a.ts must reach c.ts via the restored edge; impact files: {impacted_files:?}"
        );
        // d.ts (3-hop) is beyond the re-resolution cap — not a required result,
        // so we deliberately make no assertion about it.
    }

    /// nw-008 hub cap: when a changed file has more reverse-dependents than the
    /// `MAX_AFFECTED_FILES` cap, the transitive re-resolution pass is skipped
    /// (bounded work) but the incremental index still completes and advances the
    /// stored SHA. A periodic full re-index is the backstop for hubs.
    #[test]
    fn incremental_hub_cap_skips_reresolution_but_advances_sha() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source");

        // hub.ts is imported+called by 205 files (> MAX_AFFECTED_FILES = 200).
        const IMPORTERS: usize = 205;
        let mut files: Vec<(String, String)> = Vec::new();
        files.push((
            "hub.ts".to_string(),
            "export function hubFunc(): number { return 1; }\n".to_string(),
        ));
        for i in 0..IMPORTERS {
            files.push((
                format!("dep{i}.ts"),
                format!(
                    "import {{ hubFunc }} from './hub';\nexport function dep{i}(): number {{ return hubFunc() + {i}; }}\n"
                ),
            ));
        }
        let file_refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(p, c)| (p.as_str(), c.as_str()))
            .collect();
        create_source_repo(&src, &file_refs);
        let url = format!("file://{}", src.display());

        let ws = BareCloneWorkspace::new(&tmp.path().join("workspace")).unwrap();
        let store = nestweaver_store::GraphStore::in_memory().unwrap();
        let instance_id = "test-instance";
        let mut tracker = crate::scheduler::ReindexTracker::new();
        let job = make_code_job(1, "hub-repo", &url);

        let first = prepare_job(&job, &ws, &store, instance_id, None, RepoType::Code)
            .unwrap()
            .expect("initial code index should be prepared");
        commit_prepared_job_with_reindex_tracker(
            &first,
            &store,
            instance_id,
            Some(&mut tracker),
            || Ok::<_, anyhow::Error>(()),
        )
        .unwrap();

        // Change the hub: every importer is a reverse-dependent, blowing the cap.
        commit_file(
            &src,
            "hub.ts",
            "// touched\nexport function hubFunc(): number { return 2; }\n",
            "change hub",
        );

        let second = prepare_job(&job, &ws, &store, instance_id, None, RepoType::Code)
            .unwrap()
            .expect("updated hub repo should be prepared");
        commit_prepared_job_with_reindex_tracker(
            &second,
            &store,
            instance_id,
            Some(&mut tracker),
            || Ok::<_, anyhow::Error>(()),
        )
        .unwrap();

        // The incremental pass completed and the SHA advanced despite the cap.
        let r_uid = nestweaver_schema::repo_uid(instance_id, &url);
        let repo = store
            .lookup_repo(&r_uid)
            .unwrap()
            .expect("repo should exist after incremental index");
        assert_eq!(
            repo.indexed_sha, second.remote_sha,
            "incremental index must advance the stored SHA even when the hub cap skips re-resolution"
        );
        assert_eq!(
            tracker.count(&job.repo_id),
            1,
            "the hub update must take the incremental path"
        );
    }

    #[test]
    fn code_repo_full_refresh_resets_reindex_tracker() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source");
        create_source_repo(&src, &[("src/lib.rs", "pub fn one() -> i32 { 1 }\n")]);
        let url = format!("file://{}", src.display());

        let ws = BareCloneWorkspace::new(&tmp.path().join("workspace")).unwrap();
        let store = nestweaver_store::GraphStore::in_memory().unwrap();
        let instance_id = "test-instance";
        let mut tracker = crate::scheduler::ReindexTracker::new();

        let job = IndexJob {
            id: 1,
            repo_id: "refresh-repo".to_string(),
            repo_url: url.clone(),
            trigger: JobTrigger::Unindexed,
            priority: 0,
            status: crate::jobs::JobStatus::Running,
            attempt: 1,
            max_attempts: 4,
            error_msg: None,
            branch: None,
            created_at: 0,
            updated_at: 0,
            started_at: Some(0),
            completed_at: None,
        };

        let first = prepare_job(&job, &ws, &store, instance_id, None, RepoType::Code)
            .unwrap()
            .expect("initial code index should be prepared");
        commit_prepared_job_with_reindex_tracker(
            &first,
            &store,
            instance_id,
            Some(&mut tracker),
            || Ok::<_, anyhow::Error>(()),
        )
        .unwrap();

        for _ in 0..150 {
            tracker.record_incremental(&job.repo_id);
        }
        assert!(tracker.needs_full_reindex(&job.repo_id, 1));

        commit_file(
            &src,
            "src/lib.rs",
            "pub fn one() -> i32 { 11 }\n",
            "modify file",
        );

        let second = prepare_job(&job, &ws, &store, instance_id, None, RepoType::Code)
            .unwrap()
            .expect("updated code repo should be prepared");
        commit_prepared_job_with_reindex_tracker(
            &second,
            &store,
            instance_id,
            Some(&mut tracker),
            || Ok::<_, anyhow::Error>(()),
        )
        .unwrap();

        let r_uid = nestweaver_schema::repo_uid(instance_id, &url);
        let repo = store
            .lookup_repo(&r_uid)
            .unwrap()
            .expect("repo should exist after full refresh");
        assert_eq!(repo.indexed_sha, second.remote_sha);
        assert_eq!(
            tracker.count(&job.repo_id),
            0,
            "periodic full refresh should reset incremental update count"
        );
    }

    /// Helper: rewrite history (force-push) by removing a file and amending the
    /// commit, producing a SHA that is NOT a descendant of the previous one.
    fn force_remove_file(repo: &std::path::Path, path: &str) {
        Command::new("git")
            .args(["rm", path])
            .current_dir(repo)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "--amend", "--no-edit"])
            .current_dir(repo)
            .output()
            .unwrap();
    }

    #[test]
    fn code_repo_full_refresh_prunes_force_pushed_removed_file() {
        // nw-009 Fix #2 regression: a force-push that drops a file takes the
        // server full re-index path (non-ancestor SHA → not incremental). The
        // removed file's File/Symbol nodes must be pruned, not left behind.
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source");
        create_source_repo(
            &src,
            &[
                ("src/lib.rs", "pub fn one() -> i32 { 1 }\n"),
                ("src/helper.rs", "pub fn help() -> i32 { 2 }\n"),
            ],
        );
        let url = format!("file://{}", src.display());

        let ws = BareCloneWorkspace::new(&tmp.path().join("workspace")).unwrap();
        let store = nestweaver_store::GraphStore::in_memory().unwrap();
        let instance_id = "test-instance";
        let mut tracker = crate::scheduler::ReindexTracker::new();

        let job = IndexJob {
            id: 1,
            repo_id: "prune-repo".to_string(),
            repo_url: url.clone(),
            trigger: JobTrigger::Unindexed,
            priority: 0,
            status: crate::jobs::JobStatus::Running,
            attempt: 1,
            max_attempts: 4,
            error_msg: None,
            branch: None,
            created_at: 0,
            updated_at: 0,
            started_at: Some(0),
            completed_at: None,
        };

        // Initial full index — both files present.
        let first = prepare_job(&job, &ws, &store, instance_id, None, RepoType::Code)
            .unwrap()
            .expect("initial code index should be prepared");
        commit_prepared_job_with_reindex_tracker(
            &first,
            &store,
            instance_id,
            Some(&mut tracker),
            || Ok::<_, anyhow::Error>(()),
        )
        .unwrap();

        let r_uid = nestweaver_schema::repo_uid(instance_id, &url);
        let files = store.list_files_by_repo(&r_uid).unwrap();
        assert!(
            files.iter().any(|(_, p)| p == "src/helper.rs"),
            "helper.rs should be indexed before the force-push, got {files:?}"
        );

        // Force-push: rewrite history removing helper.rs. The new SHA is not a
        // descendant of the indexed SHA, so the full re-index path runs.
        force_remove_file(&src, "src/helper.rs");

        let second = prepare_job(&job, &ws, &store, instance_id, None, RepoType::Code)
            .unwrap()
            .expect("force-pushed repo should be prepared");
        assert_ne!(
            first.remote_sha, second.remote_sha,
            "force-push should produce a divergent SHA"
        );
        commit_prepared_job_with_reindex_tracker(
            &second,
            &store,
            instance_id,
            Some(&mut tracker),
            || Ok::<_, anyhow::Error>(()),
        )
        .unwrap();

        // The Repo node survives (Fix #2 keeps it, never delete_repo_all_data).
        let repo = store
            .lookup_repo(&r_uid)
            .unwrap()
            .expect("Repo node must survive the force-push prune");
        assert_eq!(repo.indexed_sha, second.remote_sha);

        let files = store.list_files_by_repo(&r_uid).unwrap();
        let paths: Vec<&str> = files.iter().map(|(_, p)| p.as_str()).collect();
        assert!(
            !paths.contains(&"src/helper.rs"),
            "force-pushed-away helper.rs File node should be pruned, got {paths:?}"
        );
        assert!(
            paths.contains(&"src/lib.rs"),
            "still-present lib.rs File node must remain, got {paths:?}"
        );
        assert!(
            store.symbols_in_file("src/helper.rs").unwrap().is_empty(),
            "force-pushed-away helper.rs symbols should be pruned"
        );
    }

    #[test]
    fn vault_repo_records_indexed_sha_and_second_poll_is_skipped() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source");
        create_source_repo(
            &src,
            &[
                ("README.md", "# Readme\n\nProject overview.\n"),
                ("docs/guide.md", "# Guide\n\n## Setup\n\ninstall steps\n"),
            ],
        );
        let url = format!("file://{}", src.display());

        let ws = BareCloneWorkspace::new(&tmp.path().join("workspace")).unwrap();
        let store = nestweaver_store::GraphStore::in_memory().unwrap();
        let instance_id = "test-instance";

        let job = IndexJob {
            id: 1,
            repo_id: "vault-repo".to_string(),
            repo_url: url.clone(),
            trigger: JobTrigger::Unindexed,
            priority: 0,
            status: crate::jobs::JobStatus::Running,
            attempt: 1,
            max_attempts: 4,
            error_msg: None,
            branch: None,
            created_at: 0,
            updated_at: 0,
            started_at: Some(0),
            completed_at: None,
        };

        // First poll: nothing indexed yet, so the job is prepared and committed
        // through the vault path.
        let prepared = prepare_job(&job, &ws, &store, instance_id, None, RepoType::Vault)
            .unwrap()
            .expect("first vault index should be prepared");
        let remote_sha = prepared.remote_sha.clone();
        commit_prepared_job(&prepared, &store, instance_id).unwrap();

        // The vault was actually indexed.
        assert!(
            store.count_notes().unwrap() >= 2,
            "vault index should have produced notes"
        );

        // nw-003: the Repo node now carries the indexed SHA.
        let r_uid = nestweaver_schema::repo_uid(instance_id, &url);
        let repo = store
            .lookup_repo(&r_uid)
            .unwrap()
            .expect("vault index must upsert a Repo node");
        assert_eq!(
            repo.indexed_sha, remote_sha,
            "vault repo must persist the indexed SHA so unchanged vaults are skipped"
        );

        // Second poll at the same SHA: the up-to-date short-circuit fires and
        // prepare_job returns None instead of re-indexing.
        let second = prepare_job(&job, &ws, &store, instance_id, None, RepoType::Vault).unwrap();
        assert!(
            second.is_none(),
            "an unchanged vault must be skipped on the next poll"
        );
    }

    #[test]
    fn prepare_job_respects_open_circuit_breaker() {
        let tmp = TempDir::new().unwrap();
        let ws = BareCloneWorkspace::new(&tmp.path().join("workspace")).unwrap();
        let store = nestweaver_store::GraphStore::in_memory().unwrap();
        let cb = RemoteCircuitBreakers::new();
        let host = RemoteCircuitBreakers::extract_host("https://example.com/org/repo.git");
        for _ in 0..5 {
            cb.record_failure(&host);
        }

        let job = IndexJob {
            id: 1,
            repo_id: "blocked-repo".to_string(),
            repo_url: "https://example.com/org/repo.git".to_string(),
            trigger: JobTrigger::Webhook,
            priority: 1,
            status: crate::jobs::JobStatus::Running,
            attempt: 1,
            max_attempts: 4,
            error_msg: None,
            branch: None,
            created_at: 0,
            updated_at: 0,
            started_at: Some(0),
            completed_at: None,
        };

        let err = prepare_job(
            &job,
            &ws,
            &store,
            "test-instance",
            Some(&cb),
            RepoType::Code,
        )
        .unwrap_err();

        assert!(err.to_string().contains("circuit breaker open"));
    }

    #[tokio::test]
    async fn worker_pool_processes_job() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source");
        create_source_repo(
            &src,
            &[("lib.rs", "pub fn add(a: i32, b: i32) -> i32 { a + b }")],
        );
        let url = format!("file://{}", src.display());

        // Set up components.
        let queue = JobQueue::open(&tmp.path().join("jobs.db")).unwrap();
        queue
            .upsert("test-repo", &url, JobTrigger::Unindexed, None)
            .unwrap();
        let queue = Arc::new(Mutex::new(queue));

        let workspace = Arc::new(BareCloneWorkspace::new(&tmp.path().join("workspace")).unwrap());
        let store = Arc::new(nestweaver_store::GraphStore::in_memory().unwrap());

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let pool = WorkerPool::new(2);

        let q = queue.clone();
        let s = store.clone();

        // Run the worker loop in a background task.
        let handle = tokio::spawn(async move {
            pool.run(q, workspace, s, "test".to_string(), shutdown_rx, None)
                .await;
        });

        // Wait for the job to be processed (poll queue depth).
        for _ in 0..60 {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            let depth = {
                let q = queue.lock().unwrap();
                q.queue_depth().unwrap()
            };
            if depth.succeeded >= 1 {
                break;
            }
        }

        // Signal shutdown and wait for the worker to exit.
        shutdown_tx.send(true).unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;

        // Verify the job completed.
        let depth = {
            let q = queue.lock().unwrap();
            q.queue_depth().unwrap()
        };
        assert_eq!(depth.succeeded, 1, "job should have succeeded");
        assert_eq!(depth.pending, 0);

        // Verify symbols were indexed.
        let count = store.count_symbols().unwrap();
        assert!(
            count > 0,
            "worker should have indexed the repo and created symbols"
        );
    }

    #[tokio::test]
    async fn worker_pool_updates_indexing_status() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source");
        create_source_repo(
            &src,
            &[("lib.rs", "pub fn mul(a: i32, b: i32) -> i32 { a * b }")],
        );
        let url = format!("file://{}", src.display());

        let queue = JobQueue::open(&tmp.path().join("jobs.db")).unwrap();
        queue
            .upsert("status-test-repo", &url, JobTrigger::Unindexed, None)
            .unwrap();
        let queue = Arc::new(Mutex::new(queue));

        let workspace = Arc::new(BareCloneWorkspace::new(&tmp.path().join("workspace")).unwrap());
        let store = Arc::new(nestweaver_store::GraphStore::in_memory().unwrap());

        let status = IndexingStatus::new();
        let status_check = status.clone();

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let pool = WorkerPool::new(1);

        let q = queue.clone();
        let s = store.clone();

        let handle = tokio::spawn(async move {
            pool.run(
                q,
                workspace,
                s,
                "test".to_string(),
                shutdown_rx,
                Some(status),
            )
            .await;
        });

        // Wait for the job to complete.
        for _ in 0..60 {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            let depth = {
                let q = queue.lock().unwrap();
                q.queue_depth().unwrap()
            };
            if depth.succeeded >= 1 {
                break;
            }
        }

        // After completion, the status should reflect idle state.
        // Give a moment for the status update to propagate.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert!(
            !status_check.active.load(Ordering::Relaxed),
            "indexing should not be active after job completes"
        );
        assert_eq!(
            status_check.queue_depth.load(Ordering::Relaxed),
            0,
            "queue depth should be 0 after job completes"
        );

        shutdown_tx.send(true).unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    }

    /// Persist tracker state to a file-backed queue, then rehydrate a fresh
    /// tracker from a *new* `JobQueue` on the same DB (simulating a daemon
    #[test]
    fn skipped_reindex_outcome_issues_no_upsert() {
        // nw-009 Part B: an unchanged code repo yields a Skipped outcome, which
        // leaves the tracker unchanged. Persisting it would re-upsert identical
        // state on every poll, so the write-through must be gated out entirely.
        let tmp = TempDir::new().unwrap();
        let queue = Mutex::new(JobQueue::open(&tmp.path().join("jobs.db")).unwrap());
        let tracker = Mutex::new(crate::scheduler::ReindexTracker::new());

        // Skipped: no tracker change, no DB upsert.
        persist_reindex_outcome(&queue, &tracker, "repo-a", ReindexOutcome::Skipped);
        assert!(
            queue
                .lock()
                .unwrap()
                .load_reindex_state()
                .unwrap()
                .is_empty(),
            "a Skipped outcome must not write any reindex_state row"
        );

        // A non-Skipped outcome still persists (gate is outcome-specific only).
        persist_reindex_outcome(&queue, &tracker, "repo-a", ReindexOutcome::Full);
        assert_eq!(
            queue.lock().unwrap().load_reindex_state().unwrap().len(),
            1,
            "a Full outcome must write through to the persisted state"
        );
    }

    /// restart) and assert the counter + last_full survive.
    #[test]
    fn reindex_state_survives_restart() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("jobs.db");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        // First "process": write through a near-threshold count + a last_full.
        {
            let queue = JobQueue::open(&db).unwrap();
            queue
                .upsert_reindex_state("repo-a", 150, Some(now - 3600))
                .unwrap();
            // A repo that has only accumulated incremental updates.
            queue.upsert_reindex_state("repo-b", 7, None).unwrap();
            // A repo whose last full was 8 days ago (time backstop).
            queue
                .upsert_reindex_state("repo-c", 0, Some(now - 8 * 24 * 3600))
                .unwrap();
        }

        // "Restart": new connection on the same DB, rehydrate the tracker.
        let queue = JobQueue::open(&db).unwrap();
        let tracker =
            crate::scheduler::ReindexTracker::from_persisted(queue.load_reindex_state().unwrap());

        // Counts and timestamps are restored.
        assert_eq!(tracker.count("repo-a"), 150);
        assert_eq!(tracker.last_full_unix("repo-a"), Some(now - 3600));
        assert_eq!(tracker.count("repo-b"), 7);
        assert_eq!(tracker.last_full_unix("repo-b"), None);

        // A restored at-threshold count still triggers a full re-index.
        assert!(
            tracker.needs_full_reindex("repo-a", 100),
            "restored at-threshold count should force a full"
        );

        // The 7-day wall-clock backstop fires from a persisted old last_full,
        // even though repo-c's update count is zero.
        assert!(
            tracker.needs_full_reindex("repo-c", 100),
            "persisted old last_full should fire the time backstop after restart"
        );
    }
}
