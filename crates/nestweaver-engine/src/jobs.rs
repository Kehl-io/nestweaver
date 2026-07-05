//! SQLite-backed job queue for server-side indexing.
//!
//! Jobs are keyed by `repo_id` (one pending/running job per repo at a time).
//! Workers discover HEAD themselves — no SHA in the payload.
//!
//! Priority ordering: unindexed(0) > webhook(1) > poll(2) > scheduled(3) > retry(4).

use rusqlite::{Connection, params};
use std::path::Path;

/// Schema for the persisted periodic-full reindex tracker. One row per repo:
/// `update_count` is the number of incremental updates since the last full
/// re-index, `last_full_unix` is the wall-clock time (Unix epoch seconds) of
/// that last full, or NULL if one has never been recorded.
const REINDEX_STATE_DDL: &str = "CREATE TABLE IF NOT EXISTS reindex_state (
    repo_id        TEXT PRIMARY KEY,
    update_count   INTEGER NOT NULL DEFAULT 0,
    last_full_unix INTEGER
);";

/// Default lease (visibility timeout) applied when a job is claimed, in
/// seconds. A `running` job whose `lease_expires_at` falls before "now" is
/// reclaimed by the continuous reaper ([`JobQueue::reap_expired_leases`]).
///
/// Chosen comfortably longer than a normal index but still bounded — see the
/// heartbeat-vs-longer-timeout tradeoff documented on `reap_expired_leases`.
pub const DEFAULT_LEASE_SECS: i64 = 1800;

/// Age (seconds since `started_at`) at which [`JobQueue::recover_stale`]
/// reclaims a `running` job. Deliberately tied to [`DEFAULT_LEASE_SECS`] so the
/// lease and the stale-threshold cannot drift apart: `recover_stale` is the
/// fallback for legacy `running` rows that predate the lease columns (NULL
/// lease, invisible to the reaper), and it should fire on the same horizon a
/// leased job would have expired on.
pub const STALE_RECOVERY_SECS: i64 = DEFAULT_LEASE_SECS;

/// What triggered this indexing job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobTrigger {
    Unindexed,
    Webhook,
    Poll,
    Scheduled,
    Retry,
}

impl JobTrigger {
    /// Numeric priority — lower is higher priority.
    pub fn priority(self) -> i32 {
        match self {
            Self::Unindexed => 0,
            Self::Webhook => 1,
            Self::Poll => 2,
            Self::Scheduled => 3,
            Self::Retry => 4,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Unindexed => "unindexed",
            Self::Webhook => "webhook",
            Self::Poll => "poll",
            Self::Scheduled => "scheduled",
            Self::Retry => "retry",
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "unindexed" => Self::Unindexed,
            "webhook" => Self::Webhook,
            "poll" => Self::Poll,
            "scheduled" => Self::Scheduled,
            "retry" => Self::Retry,
            _ => Self::Scheduled,
        }
    }
}

/// Current status of a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Pending,
    Running,
    Succeeded,
    DeadLetter,
    Cancelled,
}

impl JobStatus {
    fn from_str(s: &str) -> Self {
        match s {
            "pending" => Self::Pending,
            "running" => Self::Running,
            "succeeded" => Self::Succeeded,
            "dead_letter" => Self::DeadLetter,
            "cancelled" => Self::Cancelled,
            // "failed" in the DB maps to DeadLetter (the actual terminal failure state)
            "failed" => Self::DeadLetter,
            _ => Self::Pending,
        }
    }
}

/// A single indexing job.
#[derive(Debug, Clone)]
pub struct IndexJob {
    pub id: i64,
    pub repo_id: String,
    pub repo_url: String,
    pub trigger: JobTrigger,
    pub priority: i32,
    pub status: JobStatus,
    pub attempt: i32,
    pub max_attempts: i32,
    pub error_msg: Option<String>,
    pub branch: Option<String>,
    /// Per-claim fencing token stamped by [`JobQueue::claim_next`]. `complete`
    /// and `fail` are CAS-guarded on this value so a stale worker whose lease
    /// was reclaimed cannot mutate a job that now belongs to someone else.
    /// `None` for jobs that have never been claimed (or legacy migrated rows).
    pub claimed_by: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
}

/// Summary of queue depth by status.
#[derive(Debug, Clone, Default)]
pub struct QueueDepth {
    pub pending: i64,
    pub running: i64,
    pub succeeded: i64,
    pub dead_letter: i64,
}

/// Info about a currently running job, for admin API reporting.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RunningJobInfo {
    pub repo: String,
    pub started_at: Option<String>,
    pub duration_s: f64,
}

/// Completed job observation for metrics export.
#[derive(Debug, Clone)]
pub struct CompletedJobMetric {
    pub id: i64,
    pub status: JobStatus,
    pub duration_s: f64,
    /// Unix-seconds completion time. The metrics cursor advances on THIS, not on
    /// `id`: `index_jobs` has `UNIQUE(repo_id)` so a repo's row id is stable
    /// across every re-index, and an `id`-based cursor would count each repo at
    /// most once ever. `completed_at` is set to "now" on each completion, so it
    /// advances per completion.
    pub completed_at: i64,
}

/// SQLite-backed job queue. One instance per server process.
pub struct JobQueue {
    conn: Connection,
}

impl JobQueue {
    /// Open (or create) the job queue database at `path`.
    pub fn open(path: &Path) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;
             PRAGMA synchronous=NORMAL;",
        )?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS index_jobs (
                id           INTEGER PRIMARY KEY,
                repo_id      TEXT NOT NULL,
                repo_url     TEXT NOT NULL,
                trigger      TEXT NOT NULL DEFAULT 'scheduled',
                priority     INTEGER NOT NULL DEFAULT 3,
                status       TEXT NOT NULL DEFAULT 'pending',
                attempt      INTEGER NOT NULL DEFAULT 0,
                max_attempts INTEGER NOT NULL DEFAULT 4,
                error_msg    TEXT,
                branch       TEXT,
                requeue_needed INTEGER NOT NULL DEFAULT 0,
                claimed_by   TEXT,
                lease_expires_at INTEGER,
                created_at   INTEGER NOT NULL DEFAULT (strftime('%s','now')),
                updated_at   INTEGER NOT NULL DEFAULT (strftime('%s','now')),
                started_at   INTEGER,
                completed_at INTEGER,
                UNIQUE(repo_id)
            );",
        )?;
        // Migrations: add columns to existing databases.
        let _ = conn.execute_batch("ALTER TABLE index_jobs ADD COLUMN branch TEXT;");
        let _ = conn.execute_batch(
            "ALTER TABLE index_jobs ADD COLUMN requeue_needed INTEGER NOT NULL DEFAULT 0;",
        );
        // T4.1/T4.2: per-claim fencing token + lease visibility timeout. Both
        // nullable so migrating an existing queue never loses in-flight rows —
        // legacy `running` rows simply have NULL lease/owner and are handled by
        // the `recover_stale` fallback (run at startup AND on every reaper tick)
        // until re-claimed with a lease.
        let _ = conn.execute_batch("ALTER TABLE index_jobs ADD COLUMN claimed_by TEXT;");
        let _ = conn.execute_batch("ALTER TABLE index_jobs ADD COLUMN lease_expires_at INTEGER;");
        // Persisted periodic-full reindex state (one row per repo). Lives in
        // the same DB as the job queue so it shares the daemon's lifecycle and
        // survives restarts — the in-memory `ReindexTracker` is only a cache.
        conn.execute_batch(REINDEX_STATE_DDL)?;
        Ok(Self { conn })
    }

    /// Open an in-memory job queue (for tests).
    #[cfg(test)]
    fn open_in_memory() -> Result<Self, rusqlite::Error> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;
             PRAGMA synchronous=NORMAL;",
        )?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS index_jobs (
                id           INTEGER PRIMARY KEY,
                repo_id      TEXT NOT NULL,
                repo_url     TEXT NOT NULL,
                trigger      TEXT NOT NULL DEFAULT 'scheduled',
                priority     INTEGER NOT NULL DEFAULT 3,
                status       TEXT NOT NULL DEFAULT 'pending',
                attempt      INTEGER NOT NULL DEFAULT 0,
                max_attempts INTEGER NOT NULL DEFAULT 4,
                error_msg    TEXT,
                branch       TEXT,
                requeue_needed INTEGER NOT NULL DEFAULT 0,
                claimed_by   TEXT,
                lease_expires_at INTEGER,
                created_at   INTEGER NOT NULL DEFAULT (strftime('%s','now')),
                updated_at   INTEGER NOT NULL DEFAULT (strftime('%s','now')),
                started_at   INTEGER,
                completed_at INTEGER,
                UNIQUE(repo_id)
            );",
        )?;
        conn.execute_batch(REINDEX_STATE_DDL)?;
        Ok(Self { conn })
    }

    /// Insert or update a job for `repo_id`.
    ///
    /// If a pending or failed job already exists for this repo, the priority is
    /// upgraded (lower = higher priority) and trigger is updated only when the
    /// new priority is strictly higher.
    ///
    /// Running jobs are not disturbed — the upsert is a no-op for running
    /// jobs so we don't interrupt an in-progress index. Succeeded and
    /// dead_letter jobs are reset to pending so new webhooks can re-trigger
    /// indexing.
    pub fn upsert(
        &self,
        repo_id: &str,
        repo_url: &str,
        trigger: JobTrigger,
        branch: Option<&str>,
    ) -> Result<(), rusqlite::Error> {
        let priority = trigger.priority();
        let trigger_str = trigger.as_str();
        // Single ON CONFLICT clause that fires on repo_id conflict and
        // conditionally updates based on current status:
        //
        // - pending/failed: upgrade priority if new one is higher, reset to pending.
        // - succeeded/dead_letter/cancelled: reset to pending with new trigger,
        //   clear attempt counter so the job gets a fresh run.
        // - running: no-op — don't interrupt in-progress work.
        //
        // Branch is always updated when a new value is provided (Some), regardless
        // of status, so the worker always uses the most recently configured branch.
        self.conn.execute(
            "INSERT INTO index_jobs (repo_id, repo_url, trigger, priority, status, branch)
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5)
             ON CONFLICT (repo_id) DO UPDATE SET
               priority   = CASE WHEN status IN ('pending', 'failed')
                                 THEN MIN(excluded.priority, priority)
                                 WHEN status IN ('succeeded', 'dead_letter', 'cancelled')
                                 THEN excluded.priority
                                 WHEN status = 'running'
                                 THEN MIN(excluded.priority, priority)
                                 ELSE priority END,
               trigger    = CASE WHEN status IN ('pending', 'failed') AND excluded.priority < priority
                                 THEN excluded.trigger
                                 WHEN status IN ('succeeded', 'dead_letter', 'cancelled')
                                 THEN excluded.trigger
                                 ELSE trigger END,
               status     = CASE WHEN status IN ('pending', 'failed', 'succeeded', 'dead_letter', 'cancelled')
                                 THEN 'pending' ELSE status END,
               attempt    = CASE WHEN status IN ('succeeded', 'dead_letter', 'cancelled')
                                 THEN 0 ELSE attempt END,
               branch     = CASE WHEN excluded.branch IS NOT NULL
                                 THEN excluded.branch ELSE branch END,
               requeue_needed = CASE WHEN status = 'running' THEN 1
                                     ELSE requeue_needed END,
               updated_at = strftime('%s','now')",
            params![repo_id, repo_url, trigger_str, priority, branch],
        )?;
        Ok(())
    }

    /// Claim the next pending job, respecting the debounce window.
    ///
    /// Uses `BEGIN IMMEDIATE` so busy_timeout applies on contention.
    /// Only jobs whose `updated_at + debounce_secs <= now` are eligible.
    pub fn claim_next(&self, debounce_secs: i64) -> Result<Option<IndexJob>, rusqlite::Error> {
        self.claim_next_with_lease(debounce_secs, DEFAULT_LEASE_SECS)
    }

    /// Claim the next pending job, stamping a lease that expires `lease_secs`
    /// from now. On claim we also mint a fresh random fencing token into
    /// `claimed_by`; the worker echoes it back to `complete`/`fail` so a stale
    /// worker cannot mutate a job that has since been reclaimed and re-issued.
    pub fn claim_next_with_lease(
        &self,
        debounce_secs: i64,
        lease_secs: i64,
    ) -> Result<Option<IndexJob>, rusqlite::Error> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;

        let result = self.conn.query_row(
            "UPDATE index_jobs
             SET status     = 'running',
                 started_at = strftime('%s','now'),
                 attempt    = attempt + 1,
                 requeue_needed = 0,
                 claimed_by = lower(hex(randomblob(16))),
                 lease_expires_at = CAST(strftime('%s','now') AS INTEGER) + ?2,
                 updated_at = strftime('%s','now')
             WHERE id = (
                 SELECT id FROM index_jobs
                 WHERE status = 'pending'
                   AND CAST(updated_at AS INTEGER) + ?1 <= CAST(strftime('%s','now') AS INTEGER)
                 ORDER BY priority ASC, created_at ASC
                 LIMIT 1
             )
             RETURNING id, repo_id, repo_url, trigger, priority, status,
                       attempt, max_attempts, error_msg, branch,
                       created_at, updated_at, started_at, completed_at, claimed_by",
            params![debounce_secs, lease_secs],
            row_to_job,
        );

        match result {
            Ok(job) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(Some(job))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(None)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Mark a job as succeeded.
    ///
    /// Does NOT update `updated_at` — that column is only set by external
    /// events (upsert). This ensures `requeue_if_stale` correctly detects
    /// "an event arrived while running" without false positives.
    pub fn complete(
        &self,
        job_id: i64,
        repo_id: &str,
        claimed_by: Option<&str>,
    ) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "UPDATE index_jobs
             SET status       = 'succeeded',
                 completed_at = strftime('%s','now')
             WHERE id = ?1 AND repo_id = ?2 AND status = 'running'
               AND claimed_by IS ?3",
            params![job_id, repo_id, claimed_by],
        )?;
        Ok(())
    }

    /// Re-queue a completed repo if an upsert arrived while it was running.
    /// Call this after complete() to catch pushes that arrived mid-index.
    pub fn requeue_if_stale(&self, repo_id: &str) -> Result<bool, rusqlite::Error> {
        // If an upsert arrived while the job was running, requeue_needed
        // was set to 1. Reset to pending and clear the flag.
        let changed = self.conn.execute(
            "UPDATE index_jobs SET status = 'pending', attempt = 0,
                    requeue_needed = 0,
                    updated_at = strftime('%s','now')
             WHERE repo_id = ?1 AND status = 'succeeded'
               AND requeue_needed = 1",
            params![repo_id],
        )?;
        Ok(changed > 0)
    }

    /// Mark all jobs for a repo as cancelled. Unlike DELETE, this preserves
    /// the row so that SQLite cannot reuse the ID for a new job — preventing
    /// an already-claimed worker from mistaking a new job for the old one.
    pub fn cancel_repo(&self, repo_id: &str) -> Result<usize, rusqlite::Error> {
        let changed = self.conn.execute(
            "UPDATE index_jobs SET status = 'cancelled',
                    updated_at = strftime('%s','now')
             WHERE repo_id = ?1 AND status != 'cancelled'",
            params![repo_id],
        )?;
        Ok(changed)
    }

    /// Check if a specific job is still valid (not cancelled, and still
    /// belongs to the expected repo). The caller must pass the repo_id from
    /// the originally-claimed job to guard against ID reuse.
    pub fn job_is_active(&self, job_id: i64, repo_id: &str) -> Result<bool, rusqlite::Error> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM index_jobs
             WHERE id = ?1 AND repo_id = ?2 AND status = 'running'",
            params![job_id, repo_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Mark a job as failed.
    ///
    /// If `is_poison` is true or `attempt >= max_attempts`, the job moves
    /// directly to dead_letter. Otherwise it goes back to pending for retry
    /// with the `retry` trigger.
    pub fn fail(
        &self,
        job_id: i64,
        claimed_by: Option<&str>,
        error: &str,
        is_poison: bool,
    ) -> Result<(), rusqlite::Error> {
        // Read current attempt/max_attempts to decide the outcome.
        let (attempt, max_attempts): (i32, i32) = self.conn.query_row(
            "SELECT attempt, max_attempts FROM index_jobs WHERE id = ?1",
            params![job_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;

        // Both terminal transitions are CAS-guarded on the running status and
        // the fencing token (#11/#12): a cancelled/reclaimed/already-finished
        // job matches zero rows, so `fail` becomes a no-op instead of
        // resurrecting the row (e.g. flipping `cancelled` -> `pending`).
        if is_poison || attempt >= max_attempts {
            self.conn.execute(
                "UPDATE index_jobs
                 SET status       = 'dead_letter',
                     error_msg    = ?2,
                     completed_at = strftime('%s','now'),
                     updated_at   = strftime('%s','now')
                 WHERE id = ?1 AND status = 'running' AND claimed_by IS ?3",
                params![job_id, error, claimed_by],
            )?;
        } else {
            self.conn.execute(
                "UPDATE index_jobs
                 SET status     = 'pending',
                     trigger    = 'retry',
                     priority   = 4,
                     error_msg  = ?2,
                     updated_at = strftime('%s','now')
                 WHERE id = ?1 AND status = 'running' AND claimed_by IS ?3",
                params![job_id, error, claimed_by],
            )?;
        }
        Ok(())
    }

    /// Return a `running` job to `pending` WITHOUT counting the attempt, for the
    /// case where the work never actually ran — specifically when the per-host
    /// circuit breaker rejected the fetch because the remote host is down.
    ///
    /// `claim_next` bumped `attempt` on the claim, so this decrements it to keep
    /// the net effect zero: a transient host outage must NOT burn a repo's retry
    /// budget and dead-letter it (a single 60s github.com blip would otherwise
    /// dead-letter every repo on that host). `created_at` is bumped so the
    /// deferred job rotates to the back of its priority tier rather than being
    /// immediately re-claimed ahead of everything else. CAS-guarded on the
    /// running status + fencing token, like `fail`, so a cancelled/reclaimed job
    /// is left untouched.
    pub fn defer(&self, job_id: i64, claimed_by: Option<&str>) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "UPDATE index_jobs
             SET status           = 'pending',
                 attempt          = MAX(0, attempt - 1),
                 claimed_by       = NULL,
                 lease_expires_at = NULL,
                 started_at       = NULL,
                 created_at       = strftime('%s','now'),
                 updated_at       = strftime('%s','now')
             WHERE id = ?1 AND status = 'running' AND claimed_by IS ?2",
            params![job_id, claimed_by],
        )?;
        Ok(())
    }

    /// Recover stale running jobs (crash recovery).
    ///
    /// Any job with `status='running'` and `started_at` older than
    /// `timeout_secs` is reset to `pending` with its `attempt` preserved.
    /// Returns the number of recovered jobs.
    pub fn recover_stale(&self, timeout_secs: i64) -> Result<usize, rusqlite::Error> {
        let count = self.conn.execute(
            "UPDATE index_jobs
             SET status     = 'pending',
                 started_at = NULL,
                 updated_at = strftime('%s','now')
             WHERE status = 'running'
               AND CAST(started_at AS INTEGER) + ?1 <= CAST(strftime('%s','now') AS INTEGER)",
            params![timeout_secs],
        )?;
        Ok(count)
    }

    /// Reclaim EVERY `running` job unconditionally. Call this ONLY once at daemon
    /// startup: under the single-writer model no worker is alive yet, so any
    /// `running` row is necessarily orphaned by the crash that stopped the previous
    /// daemon. The threshold-based [`Self::recover_stale`] (and the lease reaper)
    /// deliberately spare young `running` rows because mid-run those belong to a
    /// live worker — but at startup that reasoning is inverted, so a job crashed
    /// seconds in would otherwise sit `running` for up to the ~30-min lease before
    /// reclaim. Resets to `pending` (attempt count preserved, so the reaper's
    /// dead-letter cap still bounds a repeatedly-crashing job).
    pub fn recover_all_running_at_startup(&self) -> Result<usize, rusqlite::Error> {
        let count = self.conn.execute(
            "UPDATE index_jobs
             SET status     = 'pending',
                 started_at = NULL,
                 updated_at = strftime('%s','now')
             WHERE status = 'running'",
            [],
        )?;
        Ok(count)
    }

    /// Continuous lease reaper (T4.2). Reclaims any `running` job whose lease
    /// (`lease_expires_at`, stamped at claim) has elapsed as of `now` (Unix
    /// epoch seconds). Unlike [`Self::recover_stale`] — which runs once at
    /// startup against a fixed 30-min `started_at` threshold and therefore
    /// misses a crash seconds into a job — this is meant to run on a periodic
    /// tick, so a job orphaned by a crash is reclaimed within one reaper
    /// interval regardless of how young it is.
    ///
    /// Retries are bounded: a job that has already burned its attempts
    /// (`attempt >= max_attempts`) is dead-lettered instead of returned to
    /// pending, so a repeatedly-crashing worker cannot loop forever.
    ///
    /// `now` is passed in explicitly so the reaper is unit-testable without
    /// real sleeps. Only rows with a non-NULL `lease_expires_at` are eligible;
    /// legacy migrated `running` rows (NULL lease) are left to `recover_stale`.
    ///
    /// Heartbeat vs. longer base timeout: the worker's index runs inside an
    /// un-cancellable `spawn_blocking` with no per-job timeout and no natural
    /// progress point to extend the lease from, so rather than thread a
    /// heartbeat through the index loop we set the base lease
    /// ([`DEFAULT_LEASE_SECS`]) comfortably longer than a normal index but still
    /// bounded, and rely on this reaper.
    ///
    /// The failure mode when the base lease is set too short (relevant to
    /// anyone tuning [`DEFAULT_LEASE_SECS`]): a genuinely long (> lease) index
    /// is still running when its lease expires, so the reaper flips the row to
    /// `pending` and a second worker claims and indexes the SAME repo
    /// concurrently. The SHA short-circuit does NOT save us here — the first
    /// run has not committed its new `indexed_sha` yet, so both runs see the
    /// old SHA and both do full work. What keeps state consistent is NOT
    /// idempotency but (a) the per-claim fencing token — when the original slow
    /// worker finally finishes, its `complete`/`fail` no-op because its token no
    /// longer matches the reclaimed row — and (b) the write mutex, which
    /// serialises the two indexers' write phases so neither tears the graph.
    /// The real cost is therefore fully duplicated indexing work (wasted CPU
    /// and git I/O), not corrupted state. Size the lease so this is rare.
    pub fn reap_expired_leases(&self, now: i64) -> Result<usize, rusqlite::Error> {
        let count = self.conn.execute(
            "UPDATE index_jobs
             SET status       = CASE WHEN attempt >= max_attempts
                                     THEN 'dead_letter' ELSE 'pending' END,
                 error_msg    = CASE WHEN attempt >= max_attempts
                                     THEN 'lease expired: max attempts exceeded'
                                     ELSE error_msg END,
                 completed_at = CASE WHEN attempt >= max_attempts
                                     THEN strftime('%s','now') ELSE completed_at END,
                 -- Keep started_at for a dead-letter (preserves duration
                 -- metrics); clear it for a pending re-claim.
                 started_at   = CASE WHEN attempt >= max_attempts
                                     THEN started_at ELSE NULL END,
                 claimed_by       = NULL,
                 lease_expires_at = NULL,
                 updated_at   = strftime('%s','now')
             WHERE status = 'running'
               AND lease_expires_at IS NOT NULL
               AND CAST(lease_expires_at AS INTEGER) < ?1",
            params![now],
        )?;
        Ok(count)
    }

    /// Return current queue depth by status.
    pub fn queue_depth(&self) -> Result<QueueDepth, rusqlite::Error> {
        let mut depth = QueueDepth::default();
        let mut stmt = self
            .conn
            .prepare("SELECT status, COUNT(*) FROM index_jobs GROUP BY status")?;
        let rows = stmt.query_map([], |row| {
            let status: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            Ok((status, count))
        })?;
        for row in rows {
            let (status, count) = row?;
            match status.as_str() {
                "pending" => depth.pending = count,
                "running" => depth.running = count,
                "succeeded" => depth.succeeded = count,
                "dead_letter" => depth.dead_letter = count,
                _ => {}
            }
        }
        Ok(depth)
    }

    /// Return info about currently running jobs.
    pub fn running_jobs(&self) -> Result<Vec<RunningJobInfo>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT repo_id, started_at,
                    CAST(strftime('%s','now') AS REAL) - CAST(started_at AS REAL) AS dur
             FROM index_jobs WHERE status = 'running'",
        )?;
        let rows = stmt.query_map([], |row| {
            let started_epoch: Option<i64> = row.get(1)?;
            let duration: f64 = row.get::<_, Option<f64>>(2)?.unwrap_or(0.0);
            Ok(RunningJobInfo {
                repo: row.get(0)?,
                started_at: started_epoch.map(|e| format!("{e}")),
                duration_s: duration,
            })
        })?;
        rows.collect()
    }

    /// Return jobs that completed strictly after `since_completed_at` (Unix
    /// seconds), with elapsed duration where available.
    ///
    /// The cursor is `completed_at`, not `id`: `index_jobs` has `UNIQUE(repo_id)`,
    /// so a repo's row `id` never changes across re-indexes and an `id > last`
    /// cursor would count each repo at most once ever (the counters would flatline
    /// while the server kept re-indexing). `completed_at` is stamped "now" on each
    /// completion, so it advances per completion. Residual: a completion whose
    /// `completed_at` equals the previous batch's max and that commits after this
    /// read is skipped — a small boundary undercount at second granularity, vs.
    /// the total undercount an `id` cursor produced.
    pub fn completed_job_metrics_after(
        &self,
        since_completed_at: i64,
    ) -> Result<Vec<CompletedJobMetric>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT id, status, completed_at,
                    MAX(0.0, CAST(completed_at AS REAL) - CAST(started_at AS REAL)) AS dur
             FROM index_jobs
             WHERE completed_at > ?1
               AND completed_at IS NOT NULL
               AND started_at IS NOT NULL
             ORDER BY completed_at ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![since_completed_at], |row| {
            let status: String = row.get(1)?;
            Ok(CompletedJobMetric {
                id: row.get(0)?,
                status: JobStatus::from_str(&status),
                completed_at: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                duration_s: row.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
            })
        })?;
        rows.collect()
    }

    /// Return all dead-lettered jobs.
    pub fn dead_letters(&self) -> Result<Vec<IndexJob>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT id, repo_id, repo_url, trigger, priority, status,
                    attempt, max_attempts, error_msg, branch,
                    created_at, updated_at, started_at, completed_at, claimed_by
             FROM index_jobs
             WHERE status = 'dead_letter'
             ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], row_to_job)?;
        rows.collect()
    }

    /// Reset a dead-lettered job back to pending so it can be retried.
    ///
    /// Also called when a new event (webhook/poll) arrives for a repo that
    /// was previously dead-lettered — the `upsert` handles the
    /// `pending`/`failed` case, but `dead_letter` needs this explicit reset.
    pub fn reset_dead_letter(&self, repo_id: &str) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "UPDATE index_jobs
             SET status     = 'pending',
                 attempt    = 0,
                 error_msg  = NULL,
                 started_at = NULL,
                 completed_at = NULL,
                 updated_at = strftime('%s','now')
             WHERE repo_id = ?1 AND status = 'dead_letter'",
            params![repo_id],
        )?;
        Ok(())
    }

    /// Reset a dead-lettered job back to pending by its primary key (id).
    ///
    /// Unlike `reset_dead_letter` which matches by `repo_id`, this method
    /// uses the integer primary key — matching the `id` field returned by
    /// `dead_letters()` and surfaced through the admin API listing.
    pub fn reset_dead_letter_by_id(&self, job_id: i64) -> Result<bool, rusqlite::Error> {
        let updated = self.conn.execute(
            "UPDATE index_jobs
             SET status     = 'pending',
                 attempt    = 0,
                 error_msg  = NULL,
                 started_at = NULL,
                 completed_at = NULL,
                 updated_at = strftime('%s','now')
             WHERE id = ?1 AND status = 'dead_letter'",
            params![job_id],
        )?;
        Ok(updated > 0)
    }

    /// Permanently delete a dead-lettered job by its primary key.
    pub fn dismiss_dead_letter(&self, job_id: i64) -> Result<bool, rusqlite::Error> {
        let deleted = self.conn.execute(
            "DELETE FROM index_jobs WHERE id = ?1 AND status = 'dead_letter'",
            params![job_id],
        )?;
        Ok(deleted > 0)
    }

    /// Load all persisted periodic-full reindex state. Each tuple is
    /// `(repo_id, update_count, last_full_unix)`. Used to rehydrate the
    /// in-memory `ReindexTracker` at daemon startup so the update counter and
    /// 7-day backstop survive restarts.
    pub fn load_reindex_state(&self) -> Result<Vec<(String, u32, Option<i64>)>, rusqlite::Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT repo_id, update_count, last_full_unix FROM reindex_state")?;
        let rows = stmt.query_map([], |row| {
            let count: i64 = row.get(1)?;
            Ok((
                row.get::<_, String>(0)?,
                count.max(0) as u32,
                row.get::<_, Option<i64>>(2)?,
            ))
        })?;
        rows.collect()
    }

    /// Write through the reindex tracker's state for a single repo. Called on
    /// every tracker mutation (incremental bump or full-reindex reset) so the
    /// persisted store stays the cross-restart source of truth.
    pub fn upsert_reindex_state(
        &self,
        repo_id: &str,
        update_count: u32,
        last_full_unix: Option<i64>,
    ) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO reindex_state (repo_id, update_count, last_full_unix)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (repo_id) DO UPDATE SET
               update_count   = excluded.update_count,
               last_full_unix = excluded.last_full_unix",
            params![repo_id, update_count as i64, last_full_unix],
        )?;
        Ok(())
    }
}

/// Normalize a repo identifier for job-queue keying.
///
/// Strips a trailing `.git` suffix and any trailing slashes so that
/// `https://github.com/org/repo.git`, `https://github.com/org/repo/`,
/// and `https://github.com/org/repo` all map to the same key.
pub fn canonical_repo_id(url: &str) -> String {
    let mut s = url.to_string();
    while s.ends_with('/') {
        s.pop();
    }
    if s.ends_with(".git") {
        s.truncate(s.len() - 4);
    }
    s
}

/// Map a rusqlite row to an `IndexJob`.
fn row_to_job(row: &rusqlite::Row) -> Result<IndexJob, rusqlite::Error> {
    Ok(IndexJob {
        id: row.get(0)?,
        repo_id: row.get(1)?,
        repo_url: row.get(2)?,
        trigger: JobTrigger::from_str(row.get::<_, String>(3)?.as_str()),
        priority: row.get(4)?,
        status: JobStatus::from_str(row.get::<_, String>(5)?.as_str()),
        attempt: row.get(6)?,
        max_attempts: row.get(7)?,
        error_msg: row.get(8)?,
        branch: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
        started_at: row.get(12)?,
        completed_at: row.get(13)?,
        claimed_by: row.get(14)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue() -> JobQueue {
        JobQueue::open_in_memory().expect("open in-memory queue")
    }

    #[test]
    fn upsert_creates_job() {
        let q = queue();
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
            None,
        )
        .unwrap();

        let depth = q.queue_depth().unwrap();
        assert_eq!(depth.pending, 1);
    }

    #[test]
    fn upsert_coalesces_same_repo() {
        let q = queue();
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
            None,
        )
        .unwrap();
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Poll,
            None,
        )
        .unwrap();

        let depth = q.queue_depth().unwrap();
        assert_eq!(depth.pending, 1, "should coalesce into one job");
    }

    #[test]
    fn upsert_keeps_higher_priority() {
        let q = queue();
        // Insert with lower priority (poll = 2)
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Poll,
            None,
        )
        .unwrap();
        // Upsert with higher priority (webhook = 1)
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
            None,
        )
        .unwrap();

        let job = q.claim_next(0).unwrap().expect("should have a job");
        assert_eq!(job.priority, 1, "priority should be upgraded to webhook(1)");
        assert_eq!(job.trigger, JobTrigger::Webhook);
    }

    #[test]
    fn upsert_does_not_downgrade_priority() {
        let q = queue();
        // Insert with high priority (webhook = 1)
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
            None,
        )
        .unwrap();
        // Upsert with lower priority (scheduled = 3)
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Scheduled,
            None,
        )
        .unwrap();

        let job = q.claim_next(0).unwrap().expect("should have a job");
        assert_eq!(job.priority, 1, "priority should stay at webhook(1)");
        assert_eq!(job.trigger, JobTrigger::Webhook);
    }

    #[test]
    fn claim_next_returns_highest_priority() {
        let q = queue();
        q.upsert(
            "repo-low",
            "https://github.com/org/low",
            JobTrigger::Scheduled,
            None,
        )
        .unwrap();
        q.upsert(
            "repo-high",
            "https://github.com/org/high",
            JobTrigger::Unindexed,
            None,
        )
        .unwrap();
        q.upsert(
            "repo-mid",
            "https://github.com/org/mid",
            JobTrigger::Webhook,
            None,
        )
        .unwrap();

        let job = q.claim_next(0).unwrap().expect("should have a job");
        assert_eq!(job.repo_id, "repo-high");
        assert_eq!(job.priority, 0);
    }

    #[test]
    fn claim_next_returns_none_when_empty() {
        let q = queue();
        let job = q.claim_next(0).unwrap();
        assert!(job.is_none());
    }

    #[test]
    fn claim_next_respects_debounce() {
        let q = queue();
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
            None,
        )
        .unwrap();

        // With a very large debounce, the just-inserted job shouldn't be claimable
        let job = q.claim_next(999_999).unwrap();
        assert!(job.is_none(), "debounce should prevent claiming");

        // With zero debounce, it should be claimable
        let job = q.claim_next(0).unwrap();
        assert!(job.is_some(), "zero debounce should allow claiming");
    }

    #[test]
    fn complete_sets_succeeded() {
        let q = queue();
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
            None,
        )
        .unwrap();
        let job = q.claim_next(0).unwrap().unwrap();

        q.complete(job.id, &job.repo_id, job.claimed_by.as_deref())
            .unwrap();

        let depth = q.queue_depth().unwrap();
        assert_eq!(depth.succeeded, 1);
        assert_eq!(depth.running, 0);
    }

    #[test]
    fn fail_retries_up_to_max() {
        let q = queue();
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
            None,
        )
        .unwrap();

        // Claim and fail — attempt becomes 1, max_attempts is 4, so it should retry
        let job = q.claim_next(0).unwrap().unwrap();
        assert_eq!(job.attempt, 1);
        q.fail(job.id, job.claimed_by.as_deref(), "network error", false)
            .unwrap();

        let depth = q.queue_depth().unwrap();
        assert_eq!(depth.pending, 1, "should be back to pending for retry");
        assert_eq!(depth.dead_letter, 0);

        // Claim again — attempt 2
        let job = q.claim_next(0).unwrap().unwrap();
        assert_eq!(job.attempt, 2);
        assert_eq!(job.trigger, JobTrigger::Retry);
        q.fail(job.id, job.claimed_by.as_deref(), "network error", false)
            .unwrap();

        // Claim again — attempt 3
        let job = q.claim_next(0).unwrap().unwrap();
        assert_eq!(job.attempt, 3);
        q.fail(job.id, job.claimed_by.as_deref(), "network error", false)
            .unwrap();

        // Claim again — attempt 4, should dead-letter now
        let job = q.claim_next(0).unwrap().unwrap();
        assert_eq!(job.attempt, 4);
        q.fail(job.id, job.claimed_by.as_deref(), "final failure", false)
            .unwrap();

        let depth = q.queue_depth().unwrap();
        assert_eq!(
            depth.dead_letter, 1,
            "should be dead-lettered after max attempts"
        );
        assert_eq!(depth.pending, 0);
    }

    #[test]
    fn fail_poison_goes_to_dead_letter() {
        let q = queue();
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
            None,
        )
        .unwrap();
        let job = q.claim_next(0).unwrap().unwrap();

        q.fail(job.id, job.claimed_by.as_deref(), "auth failure 401", true)
            .unwrap();

        let depth = q.queue_depth().unwrap();
        assert_eq!(
            depth.dead_letter, 1,
            "poison should dead-letter immediately"
        );
        assert_eq!(depth.pending, 0);
    }

    /// Read the lease_expires_at stamped on a job by claim_next.
    #[cfg(test)]
    fn lease_of(q: &JobQueue, job_id: i64) -> i64 {
        q.conn
            .query_row(
                "SELECT lease_expires_at FROM index_jobs WHERE id = ?1",
                params![job_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn crashed_job_under_stale_threshold_is_reclaimed() {
        let q = queue();
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
            None,
        )
        .unwrap();
        // Claim stamps a lease. This simulates a worker that grabbed the job
        // moments ago (well under the 30-min recover_stale threshold) and then
        // the daemon crashed.
        let job = q.claim_next(0).unwrap().unwrap();
        assert_eq!(job.status, JobStatus::Running);
        let lease = lease_of(&q, job.id);

        // A reaper tick BEFORE the lease expires must NOT reclaim the job —
        // this proves the mechanism is lease-based, not a fixed 30-min path.
        let reclaimed = q.reap_expired_leases(lease - 1).unwrap();
        assert_eq!(
            reclaimed, 0,
            "a job with a still-valid lease is not reclaimed"
        );
        assert_eq!(q.queue_depth().unwrap().running, 1);

        // A reaper tick AFTER the lease expires reclaims it to pending, even
        // though started_at is far younger than recover_stale's threshold.
        let reclaimed = q.reap_expired_leases(lease + 1).unwrap();
        assert_eq!(reclaimed, 1, "an expired-lease job must be reclaimed");
        let depth = q.queue_depth().unwrap();
        assert_eq!(depth.pending, 1, "reclaimed job returns to pending");
        assert_eq!(depth.running, 0);

        // It is re-claimable and the fencing token is freshly minted.
        let reclaimed_job = q.claim_next(0).unwrap().unwrap();
        assert_ne!(
            reclaimed_job.claimed_by, job.claimed_by,
            "re-claim must mint a new fencing token, invalidating the crashed worker"
        );
    }

    #[test]
    fn legacy_null_lease_running_row_is_reclaimed_by_recover_stale() {
        // A row left `running` by a pre-migration daemon has a NULL lease, so
        // the reaper's `lease_expires_at IS NOT NULL` filter skips it. The
        // periodic `recover_stale(STALE_RECOVERY_SECS)` tick is what rescues it
        // — closing finding #12 for pre-migration in-flight rows too.
        let q = queue();
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
            None,
        )
        .unwrap();
        let job = q.claim_next(0).unwrap().unwrap();

        // Simulate a legacy row: clear the lease and backdate started_at past
        // the recovery threshold.
        q.conn
            .execute(
                "UPDATE index_jobs
                 SET lease_expires_at = NULL,
                     started_at = CAST(strftime('%s','now') AS INTEGER) - ?2
                 WHERE id = ?1",
                params![job.id, STALE_RECOVERY_SECS + 60],
            )
            .unwrap();

        // The reaper cannot see it (NULL lease), even far in the future.
        let reaped = q
            .reap_expired_leases(i64::MAX / 2)
            .expect("reap should succeed");
        assert_eq!(
            reaped, 0,
            "NULL-lease legacy row is invisible to the reaper"
        );
        assert_eq!(q.queue_depth().unwrap().running, 1, "still stuck running");

        // The periodic recover_stale path reclaims it.
        let recovered = q.recover_stale(STALE_RECOVERY_SECS).unwrap();
        assert_eq!(recovered, 1, "recover_stale must rescue the legacy row");
        assert_eq!(q.queue_depth().unwrap().pending, 1);
        assert_eq!(q.queue_depth().unwrap().running, 0);
    }

    #[test]
    fn reaper_dead_letters_after_max_attempts() {
        let q = queue();
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
            None,
        )
        .unwrap();
        // Exhaust attempts: claim increments attempt each time; reap returns it
        // to pending until attempt reaches max_attempts, then dead-letters it.
        let mut last = q.claim_next(0).unwrap().unwrap();
        loop {
            let lease = lease_of(&q, last.id);
            q.reap_expired_leases(lease + 1).unwrap();
            let depth = q.queue_depth().unwrap();
            if depth.dead_letter == 1 {
                break;
            }
            assert_eq!(depth.pending, 1, "still retrying while attempts remain");
            last = q.claim_next(0).unwrap().unwrap();
            assert!(
                last.attempt <= last.max_attempts,
                "attempt must not exceed max before dead-lettering"
            );
        }
        assert_eq!(q.queue_depth().unwrap().dead_letter, 1);
    }

    #[test]
    fn recover_stale_resets_old_running() {
        let q = queue();
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
            None,
        )
        .unwrap();
        let job = q.claim_next(0).unwrap().unwrap();
        assert_eq!(job.status, JobStatus::Running);

        // Backdate started_at to simulate a stale job
        q.conn
            .execute(
                "UPDATE index_jobs SET started_at = started_at - 3600 WHERE id = ?1",
                params![job.id],
            )
            .unwrap();

        let recovered = q.recover_stale(1800).unwrap(); // 30 min timeout
        assert_eq!(recovered, 1);

        let depth = q.queue_depth().unwrap();
        assert_eq!(depth.pending, 1, "stale running should be reset to pending");
        assert_eq!(depth.running, 0);

        // The attempt count should be preserved
        let reclaimed = q.claim_next(0).unwrap().unwrap();
        assert_eq!(
            reclaimed.attempt, 2,
            "attempt preserved from prior run + new claim"
        );
    }

    #[test]
    fn recover_all_running_at_startup_reclaims_young_jobs() {
        let q = queue();
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
            None,
        )
        .unwrap();
        let job = q.claim_next(0).unwrap().unwrap();
        assert_eq!(job.status, JobStatus::Running);

        // A FRESH running job (crashed seconds ago) — recover_stale's 30-min
        // threshold would skip it, leaving it stuck until the lease expires.
        assert_eq!(
            q.recover_stale(1800).unwrap(),
            0,
            "threshold spares a young job"
        );
        assert_eq!(q.queue_depth().unwrap().running, 1);

        // Startup reclaim takes it immediately (no live worker at startup).
        assert_eq!(q.recover_all_running_at_startup().unwrap(), 1);
        assert_eq!(q.queue_depth().unwrap().pending, 1);
        assert_eq!(q.queue_depth().unwrap().running, 0);
    }

    #[test]
    fn new_event_resets_dead_letter() {
        let q = queue();
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
            None,
        )
        .unwrap();
        let job = q.claim_next(0).unwrap().unwrap();
        q.fail(job.id, job.claimed_by.as_deref(), "poison", true)
            .unwrap();

        let depth = q.queue_depth().unwrap();
        assert_eq!(depth.dead_letter, 1);

        // New event for the same repo — reset from dead_letter
        q.reset_dead_letter("repo-1").unwrap();
        // Now upsert should work since the status is pending
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
            None,
        )
        .unwrap();

        let depth = q.queue_depth().unwrap();
        assert_eq!(depth.pending, 1, "should be pending again after reset");
        assert_eq!(depth.dead_letter, 0);
    }

    #[test]
    fn dead_letters_returns_only_dead_lettered() {
        let q = queue();
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
            None,
        )
        .unwrap();
        q.upsert(
            "repo-2",
            "https://github.com/org/repo-2",
            JobTrigger::Poll,
            None,
        )
        .unwrap();

        // Dead-letter repo-1
        let job = q.claim_next(0).unwrap().unwrap();
        q.fail(job.id, job.claimed_by.as_deref(), "poison", true)
            .unwrap();

        let dead = q.dead_letters().unwrap();
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].repo_id, "repo-1");
        assert_eq!(dead[0].error_msg.as_deref(), Some("poison"));
    }

    #[test]
    fn queue_depth_counts_all_statuses() {
        let q = queue();
        // Create 3 jobs in different states
        q.upsert(
            "repo-a",
            "https://github.com/org/a",
            JobTrigger::Webhook,
            None,
        )
        .unwrap();
        q.upsert("repo-b", "https://github.com/org/b", JobTrigger::Poll, None)
            .unwrap();
        q.upsert(
            "repo-c",
            "https://github.com/org/c",
            JobTrigger::Scheduled,
            None,
        )
        .unwrap();

        // Claim and complete repo-a
        let job_a = q.claim_next(0).unwrap().unwrap();
        q.complete(job_a.id, &job_a.repo_id, job_a.claimed_by.as_deref())
            .unwrap();

        // Claim repo-b (leave running)
        let _job_b = q.claim_next(0).unwrap().unwrap();

        // repo-c stays pending

        let depth = q.queue_depth().unwrap();
        assert_eq!(depth.succeeded, 1);
        assert_eq!(depth.running, 1);
        assert_eq!(depth.pending, 1);
    }

    #[test]
    fn claim_increments_attempt() {
        let q = queue();
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
            None,
        )
        .unwrap();

        let job = q.claim_next(0).unwrap().unwrap();
        assert_eq!(job.attempt, 1, "first claim should set attempt to 1");
    }

    #[test]
    fn defer_does_not_burn_the_retry_budget() {
        // A circuit-open deferral must be net-zero on `attempt` so a transient
        // host outage can't dead-letter a repo after a few claim/defer cycles.
        let q = queue();
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
            None,
        )
        .unwrap();

        // Simulate several claim -> circuit-open -> defer cycles.
        for _ in 0..10 {
            let job = q.claim_next(0).unwrap().unwrap();
            assert_eq!(job.attempt, 1, "claim bumps attempt to 1");
            q.defer(job.id, job.claimed_by.as_deref()).unwrap();
        }

        // After 10 deferrals the job is still pending and has NOT accrued attempts
        // toward its max (which fail() would have, dead-lettering it long ago).
        let job = q.claim_next(0).unwrap().unwrap();
        assert_eq!(
            job.attempt, 1,
            "attempt must stay at 1 across deferrals (claim +1, defer -1)"
        );
        assert!(
            job.attempt < 4,
            "deferrals must not reach max_attempts / dead-letter"
        );
    }

    #[test]
    fn requeue_if_stale_does_not_trigger_on_normal_completion() {
        let q = queue();
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
            None,
        )
        .unwrap();
        let job = q.claim_next(0).unwrap().unwrap();
        q.complete(job.id, &job.repo_id, job.claimed_by.as_deref())
            .unwrap();
        // No external event happened — should NOT requeue
        assert!(
            !q.requeue_if_stale("repo-1").unwrap(),
            "should not requeue when no external event arrived during indexing"
        );
    }

    #[test]
    fn cancel_repo_marks_cancelled_and_blocks_active_check() {
        let q = queue();
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
            None,
        )
        .unwrap();
        let job = q.claim_next(0).unwrap().unwrap();
        assert!(q.job_is_active(job.id, "repo-1").unwrap());

        // Admin removes the repo — cancels the job
        q.cancel_repo("repo-1").unwrap();

        // The claimed worker should see the job as inactive
        assert!(
            !q.job_is_active(job.id, "repo-1").unwrap(),
            "cancelled job should not be considered active"
        );

        // complete() on a cancelled job should be a no-op (status != 'running')
        q.complete(job.id, "repo-1", job.claimed_by.as_deref())
            .unwrap();
        // Job should still be cancelled, not succeeded
        let depth = q.queue_depth().unwrap();
        assert_eq!(depth.succeeded, 0);
    }

    #[test]
    fn job_is_active_rejects_wrong_repo_id() {
        let q = queue();
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
            None,
        )
        .unwrap();
        let job = q.claim_next(0).unwrap().unwrap();

        // Correct repo_id → active
        assert!(q.job_is_active(job.id, "repo-1").unwrap());
        // Wrong repo_id (simulates ID reuse) → not active
        assert!(!q.job_is_active(job.id, "repo-2").unwrap());
    }

    #[test]
    fn canonical_repo_id_normalizes() {
        assert_eq!(
            canonical_repo_id("https://github.com/org/repo.git"),
            "https://github.com/org/repo"
        );
        assert_eq!(
            canonical_repo_id("https://github.com/org/repo/"),
            "https://github.com/org/repo"
        );
        assert_eq!(
            canonical_repo_id("https://github.com/org/repo"),
            "https://github.com/org/repo"
        );
        // repo.git/ must match repo.git (slash-then-git edge case)
        assert_eq!(
            canonical_repo_id("https://github.com/org/repo.git/"),
            "https://github.com/org/repo"
        );
    }

    #[test]
    fn canonical_id_coalesces_same_repo() {
        let q = queue();
        let url = "https://github.com/org/repo";
        let id1 = canonical_repo_id(url);
        let id2 = canonical_repo_id(&format!("{}.git", url));

        q.upsert(&id1, url, JobTrigger::Webhook, None).unwrap();
        q.upsert(&id2, url, JobTrigger::Poll, None).unwrap();

        let depth = q.queue_depth().unwrap();
        assert_eq!(depth.pending, 1, "same repo should coalesce");
    }

    #[test]
    fn upsert_ignores_running_job() {
        let q = queue();
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Scheduled,
            None,
        )
        .unwrap();
        let job = q.claim_next(0).unwrap().unwrap();
        assert_eq!(job.status, JobStatus::Running);

        // The job is now running. Upsert for the same repo should succeed
        // silently without changing the running job's state.
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
            None,
        )
        .unwrap();

        let depth = q.queue_depth().unwrap();
        assert_eq!(depth.running, 1, "job should still be running");
        assert_eq!(depth.pending, 0, "should not create a new pending job");
    }

    #[test]
    fn upsert_stores_branch() {
        let q = queue();
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
            Some("develop"),
        )
        .unwrap();

        let job = q.claim_next(0).unwrap().unwrap();
        assert_eq!(job.branch.as_deref(), Some("develop"));
    }

    #[test]
    fn upsert_updates_branch_on_conflict() {
        let q = queue();
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
            None,
        )
        .unwrap();

        // Second upsert with a branch should update the stored branch.
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Poll,
            Some("release/v2"),
        )
        .unwrap();

        let job = q.claim_next(0).unwrap().unwrap();
        assert_eq!(
            job.branch.as_deref(),
            Some("release/v2"),
            "branch should be updated on upsert"
        );
    }

    #[test]
    fn upsert_preserves_branch_when_new_is_none() {
        let q = queue();
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
            Some("develop"),
        )
        .unwrap();

        // Upsert with None branch should keep the existing branch.
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Poll,
            None,
        )
        .unwrap();

        let job = q.claim_next(0).unwrap().unwrap();
        assert_eq!(
            job.branch.as_deref(),
            Some("develop"),
            "branch should be preserved when new upsert has None"
        );
    }

    #[test]
    fn fail_does_not_resurrect_cancelled_job() {
        let q = queue();
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
            None,
        )
        .unwrap();
        // Claim (status -> running), then admin removes the repo -> cancelled.
        let job = q.claim_next(0).unwrap().unwrap();
        q.cancel_repo("repo-1").unwrap();

        // An in-flight worker errors after cancellation and calls fail().
        // Without a status guard this flips cancelled -> pending (retry),
        // resurrecting a job for a removed repo. With the guard it is a no-op.
        q.fail(job.id, job.claimed_by.as_deref(), "network error", false)
            .unwrap();

        let depth = q.queue_depth().unwrap();
        assert_eq!(
            depth.pending, 0,
            "fail() must NOT resurrect a cancelled job into pending"
        );
        // The row should remain cancelled (not pending, not dead_letter).
        let status: String = q
            .conn
            .query_row(
                "SELECT status FROM index_jobs WHERE id = ?1",
                params![job.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "cancelled", "cancelled job must stay cancelled");
    }

    #[test]
    fn fail_by_wrong_owner_is_noop() {
        let q = queue();
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
            None,
        )
        .unwrap();
        let job = q.claim_next(0).unwrap().unwrap();
        assert!(job.claimed_by.is_some(), "claim must stamp a fencing token");

        // A stale worker whose lease was reclaimed holds an old token. fail()
        // with the wrong token must not touch the row.
        q.fail(job.id, Some("not-the-real-token"), "stale error", false)
            .unwrap();

        let depth = q.queue_depth().unwrap();
        assert_eq!(
            depth.running, 1,
            "fail() by a non-owner must be a no-op; job stays running"
        );
        assert_eq!(depth.pending, 0);
        assert_eq!(depth.dead_letter, 0);

        // The rightful owner can still fail it.
        q.fail(job.id, job.claimed_by.as_deref(), "real error", false)
            .unwrap();
        let depth = q.queue_depth().unwrap();
        assert_eq!(depth.pending, 1, "the real owner's fail() must take effect");
    }

    #[test]
    fn claimed_job_without_branch_has_none() {
        let q = queue();
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
            None,
        )
        .unwrap();

        let job = q.claim_next(0).unwrap().unwrap();
        assert!(job.branch.is_none(), "branch should be None when not set");
    }
}
