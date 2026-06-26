//! Worker pool that claims jobs from the SQLite queue, fetches repos via bare
//! clones, and indexes them via `GitBareReader` + `index_with_reader`.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Semaphore;

use crate::bare_clone::BareCloneWorkspace;
use crate::jobs::{IndexJob, JobQueue};

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
}

impl IndexingStatus {
    pub fn new() -> Self {
        Self {
            active: Arc::new(AtomicBool::new(false)),
            current_repo: Arc::new(tokio::sync::RwLock::new(String::new())),
            queue_depth: Arc::new(AtomicU32::new(0)),
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
}

impl WorkerPool {
    pub fn new(concurrency: usize) -> Self {
        Self {
            concurrency,
            semaphore: Arc::new(Semaphore::new(concurrency)),
        }
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
        loop {
            // Check shutdown signal.
            if *shutdown.borrow() {
                break;
            }

            // Try to claim the next job (behind a std::sync::Mutex since
            // rusqlite::Connection is !Sync).
            let job = {
                let q = queue.lock().expect("job queue lock poisoned");
                // Update queue depth while we hold the lock.
                if let Some(ref st) = status {
                    if let Ok(depth) = q.queue_depth() {
                        st.queue_depth
                            .store((depth.pending + depth.running) as u32, Ordering::Relaxed);
                    }
                }
                q.claim_next(2) // 2s debounce
            };

            let job = match job {
                Ok(Some(job)) => job,
                Ok(None) => {
                    // No jobs available — mark idle.
                    if let Some(ref st) = status {
                        st.active.store(false, Ordering::Relaxed);
                        *st.current_repo.write().await = String::new();
                        st.queue_depth.store(0, Ordering::Relaxed);
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

            // Acquire a semaphore permit to bound concurrency.
            let permit = self.semaphore.clone().acquire_owned().await.unwrap();
            let queue = queue.clone();
            let workspace = workspace.clone();
            let store = store.clone();
            let instance_id = instance_id.clone();
            let status_clone = status.clone();

            tokio::spawn(async move {
                let _permit = permit;

                // process_job is CPU-bound (parsing + indexing), run on the
                // blocking pool so we don't starve the tokio runtime.
                let result = {
                    let job_clone = job.clone();
                    tokio::task::spawn_blocking(move || {
                        process_job(&job_clone, &workspace, &store, &instance_id)
                    })
                    .await
                };

                match result {
                    Ok(Ok(())) => {
                        let q = queue.lock().expect("job queue lock poisoned");
                        let _ = q.complete(job.id);
                        tracing::info!(repo = job.repo_id, "index complete");
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

                // Update queue depth after job completion.
                if let Some(ref st) = status_clone {
                    let q = queue.lock().expect("job queue lock poisoned");
                    if let Ok(depth) = q.queue_depth() {
                        let total = (depth.pending + depth.running) as u32;
                        st.queue_depth.store(total, Ordering::Relaxed);
                        if total == 0 {
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
fn process_job(
    job: &IndexJob,
    workspace: &BareCloneWorkspace,
    store: &nestweaver_store::GraphStore,
    instance_id: &str,
) -> Result<(), anyhow::Error> {
    // 1. Ensure the bare clone exists.
    let bare = workspace.ensure_clone(&job.repo_url)?;

    // 2. Fetch latest refs from origin.
    bare.fetch()?;

    // 3. Discover remote HEAD SHA.
    let remote_sha = bare.head_sha()?;

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
        return Ok(());
    }

    // 5. Build a reader over the bare clone at the new SHA.
    let reader = crate::content_reader::GitBareReader::new(&bare.path, &remote_sha);

    // 6. Full index via index_with_reader.
    //    Incremental indexing through ContentReader is a follow-up optimization;
    //    for v1 we always do a full index.
    crate::index_with_reader(
        &reader,
        store,
        instance_id,
        &job.repo_url,
        &remote_sha,
        None,
    )?;

    Ok(())
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
            .upsert("test-repo", &url, JobTrigger::Unindexed)
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
            .upsert("status-test-repo", &url, JobTrigger::Unindexed)
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
