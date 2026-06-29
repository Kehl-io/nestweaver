//! Worker pool that claims jobs from the SQLite queue, fetches repos via bare
//! clones, and indexes them via `GitBareReader` + `index_with_reader`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Semaphore;

use crate::bare_clone::BareCloneWorkspace;
use crate::circuit_breaker::RemoteCircuitBreakers;
use crate::config::RepoType;
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
/// queue, fetches the latest commits into a bare clone, and runs full indexing
/// via `index_with_reader`.
pub struct WorkerPool {
    concurrency: usize,
    semaphore: Arc<Semaphore>,
    /// Per-repo index strategy, keyed by [`canonical_repo_id`] of the repo URL.
    /// Repos absent from the map (or mapped to [`RepoType::Code`]) are indexed
    /// as code; entries mapped to [`RepoType::Vault`] are indexed as markdown.
    /// Populated from the instance config by the daemon; empty by default, so
    /// an unconfigured pool indexes everything as code (the prior behaviour).
    repo_types: Arc<HashMap<String, RepoType>>,
}

impl WorkerPool {
    pub fn new(concurrency: usize) -> Self {
        Self {
            concurrency,
            semaphore: Arc::new(Semaphore::new(concurrency)),
            repo_types: Arc::new(HashMap::new()),
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
        self.run_inner(
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

    /// Run the worker loop with an optional drained flag.
    ///
    /// When `drained` is `Some(flag)` and the flag is `true`, the worker
    /// sleeps instead of claiming new jobs. In-flight jobs are unaffected
    /// (they finish naturally).
    #[allow(clippy::too_many_arguments)]
    pub async fn run_with_drain(
        &self,
        queue: Arc<Mutex<JobQueue>>,
        workspace: Arc<BareCloneWorkspace>,
        store: Arc<nestweaver_store::GraphStore>,
        instance_id: String,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
        status: Option<IndexingStatus>,
        drained: Arc<AtomicBool>,
        write_mutex: Option<Arc<tokio::sync::Mutex<()>>>,
    ) {
        self.run_inner(
            queue,
            workspace,
            store,
            instance_id,
            &mut shutdown,
            status,
            Some(drained),
            write_mutex,
        )
        .await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_inner(
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
        loop {
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

            tokio::spawn(async move {
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
                            commit_prepared_job_with_write_gate(
                                &prepared,
                                &store,
                                &instance_id,
                                move || {
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
                            )
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
            None => bare.head_sha()?,
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
    let r_uid = nestweaver_schema::repo_uid(instance_id, &prepared.repo_url);
    let indexed_sha = store
        .lookup_repo(&r_uid)
        .ok()
        .flatten()
        .map(|r| r.indexed_sha)
        .unwrap_or_default();

    if prepared.remote_sha == indexed_sha {
        tracing::debug!(repo = prepared.repo_id, "already up to date");
        return Ok(());
    }

    // Build a reader over the bare clone at the new SHA.
    let reader =
        crate::content_reader::GitBareReader::new(&prepared.bare_path, &prepared.remote_sha);

    match prepared.repo_type {
        RepoType::Vault => {
            // Markdown-vault repo: index Note/Section/Heading nodes via the
            // markdown indexer. It performs all graph mutations in one pass,
            // so acquire the caller's write gate up front and hold it for the
            // duration (also performs the job-cancellation check).
            let _write_guard = acquire_write_guard()?;
            crate::index_markdown_with_reader(&reader, store, instance_id, &prepared.repo_url)?;
        }
        RepoType::Code => {
            // Full index via index_with_reader.
            //    Incremental indexing through ContentReader is a follow-up
            //    optimization; for v1 we always do a full index.
            crate::index_with_reader_and_write_gate(
                &reader,
                store,
                instance_id,
                &prepared.repo_url,
                &prepared.remote_sha,
                None,
                acquire_write_guard,
            )?;
        }
    }

    Ok(())
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
}
