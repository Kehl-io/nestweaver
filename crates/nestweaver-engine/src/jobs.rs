//! SQLite-backed job queue for server-side indexing.
//!
//! Jobs are keyed by `repo_id` (one pending/running job per repo at a time).
//! Workers discover HEAD themselves — no SHA in the payload.
//!
//! Priority ordering: unindexed(0) > webhook(1) > poll(2) > scheduled(3) > retry(4).

use rusqlite::{Connection, params};
use std::path::Path;

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
    Failed,
    DeadLetter,
}

impl JobStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::DeadLetter => "dead_letter",
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "pending" => Self::Pending,
            "running" => Self::Running,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "dead_letter" => Self::DeadLetter,
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
    pub failed: i64,
    pub dead_letter: i64,
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
                created_at   INTEGER NOT NULL DEFAULT (strftime('%s','now')),
                updated_at   INTEGER NOT NULL DEFAULT (strftime('%s','now')),
                started_at   INTEGER,
                completed_at INTEGER,
                UNIQUE(repo_id)
            );",
        )?;
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
                created_at   INTEGER NOT NULL DEFAULT (strftime('%s','now')),
                updated_at   INTEGER NOT NULL DEFAULT (strftime('%s','now')),
                started_at   INTEGER,
                completed_at INTEGER,
                UNIQUE(repo_id)
            );",
        )?;
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
    ) -> Result<(), rusqlite::Error> {
        let priority = trigger.priority();
        let trigger_str = trigger.as_str();
        // Single ON CONFLICT clause that fires on repo_id conflict and
        // conditionally updates based on current status:
        //
        // - pending/failed: upgrade priority if new one is higher, reset to pending.
        // - succeeded/dead_letter: reset to pending with new trigger, clear attempt
        //   counter so the job gets a fresh run.
        // - running: no-op — don't interrupt in-progress work.
        self.conn.execute(
            "INSERT INTO index_jobs (repo_id, repo_url, trigger, priority, status)
             VALUES (?1, ?2, ?3, ?4, 'pending')
             ON CONFLICT (repo_id) DO UPDATE SET
               priority   = CASE WHEN status IN ('pending', 'failed')
                                 THEN MIN(excluded.priority, priority)
                                 WHEN status IN ('succeeded', 'dead_letter')
                                 THEN excluded.priority
                                 ELSE priority END,
               trigger    = CASE WHEN status IN ('pending', 'failed') AND excluded.priority < priority
                                 THEN excluded.trigger
                                 WHEN status IN ('succeeded', 'dead_letter')
                                 THEN excluded.trigger
                                 ELSE trigger END,
               status     = CASE WHEN status IN ('pending', 'failed', 'succeeded', 'dead_letter')
                                 THEN 'pending' ELSE status END,
               attempt    = CASE WHEN status IN ('succeeded', 'dead_letter')
                                 THEN 0 ELSE attempt END,
               updated_at = CASE WHEN status IN ('pending', 'failed', 'succeeded', 'dead_letter')
                                 THEN strftime('%s','now') ELSE updated_at END",
            params![repo_id, repo_url, trigger_str, priority],
        )?;
        Ok(())
    }

    /// Claim the next pending job, respecting the debounce window.
    ///
    /// Uses `BEGIN IMMEDIATE` so busy_timeout applies on contention.
    /// Only jobs whose `updated_at + debounce_secs <= now` are eligible.
    pub fn claim_next(&self, debounce_secs: i64) -> Result<Option<IndexJob>, rusqlite::Error> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;

        let result = self.conn.query_row(
            "UPDATE index_jobs
             SET status     = 'running',
                 started_at = strftime('%s','now'),
                 attempt    = attempt + 1,
                 updated_at = strftime('%s','now')
             WHERE id = (
                 SELECT id FROM index_jobs
                 WHERE status = 'pending'
                   AND CAST(updated_at AS INTEGER) + ?1 <= CAST(strftime('%s','now') AS INTEGER)
                 ORDER BY priority ASC, created_at ASC
                 LIMIT 1
             )
             RETURNING id, repo_id, repo_url, trigger, priority, status,
                       attempt, max_attempts, error_msg,
                       created_at, updated_at, started_at, completed_at",
            params![debounce_secs],
            |row| row_to_job(row),
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
    pub fn complete(&self, job_id: i64) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "UPDATE index_jobs
             SET status       = 'succeeded',
                 completed_at = strftime('%s','now'),
                 updated_at   = strftime('%s','now')
             WHERE id = ?1",
            params![job_id],
        )?;
        Ok(())
    }

    /// Mark a job as failed.
    ///
    /// If `is_poison` is true or `attempt >= max_attempts`, the job moves
    /// directly to dead_letter. Otherwise it goes back to pending for retry
    /// with the `retry` trigger.
    pub fn fail(&self, job_id: i64, error: &str, is_poison: bool) -> Result<(), rusqlite::Error> {
        // Read current attempt/max_attempts to decide the outcome.
        let (attempt, max_attempts): (i32, i32) = self.conn.query_row(
            "SELECT attempt, max_attempts FROM index_jobs WHERE id = ?1",
            params![job_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;

        if is_poison || attempt >= max_attempts {
            self.conn.execute(
                "UPDATE index_jobs
                 SET status       = 'dead_letter',
                     error_msg    = ?2,
                     completed_at = strftime('%s','now'),
                     updated_at   = strftime('%s','now')
                 WHERE id = ?1",
                params![job_id, error],
            )?;
        } else {
            self.conn.execute(
                "UPDATE index_jobs
                 SET status     = 'pending',
                     trigger    = 'retry',
                     priority   = 4,
                     error_msg  = ?2,
                     updated_at = strftime('%s','now')
                 WHERE id = ?1",
                params![job_id, error],
            )?;
        }
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
                "failed" => depth.failed = count,
                "dead_letter" => depth.dead_letter = count,
                _ => {}
            }
        }
        Ok(depth)
    }

    /// Return all dead-lettered jobs.
    pub fn dead_letters(&self) -> Result<Vec<IndexJob>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT id, repo_id, repo_url, trigger, priority, status,
                    attempt, max_attempts, error_msg,
                    created_at, updated_at, started_at, completed_at
             FROM index_jobs
             WHERE status = 'dead_letter'
             ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], |row| row_to_job(row))?;
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
}

/// Normalize a repo identifier for job-queue keying.
///
/// Strips a trailing `.git` suffix and any trailing slashes so that
/// `https://github.com/org/repo.git`, `https://github.com/org/repo/`,
/// and `https://github.com/org/repo` all map to the same key.
pub fn canonical_repo_id(url: &str) -> String {
    let mut s = url.to_string();
    if s.ends_with(".git") {
        s.truncate(s.len() - 4);
    }
    while s.ends_with('/') {
        s.pop();
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
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
        started_at: row.get(11)?,
        completed_at: row.get(12)?,
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
        )
        .unwrap();
        q.upsert("repo-1", "https://github.com/org/repo-1", JobTrigger::Poll)
            .unwrap();

        let depth = q.queue_depth().unwrap();
        assert_eq!(depth.pending, 1, "should coalesce into one job");
    }

    #[test]
    fn upsert_keeps_higher_priority() {
        let q = queue();
        // Insert with lower priority (poll = 2)
        q.upsert("repo-1", "https://github.com/org/repo-1", JobTrigger::Poll)
            .unwrap();
        // Upsert with higher priority (webhook = 1)
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
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
        )
        .unwrap();
        // Upsert with lower priority (scheduled = 3)
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Scheduled,
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
        )
        .unwrap();
        q.upsert(
            "repo-high",
            "https://github.com/org/high",
            JobTrigger::Unindexed,
        )
        .unwrap();
        q.upsert(
            "repo-mid",
            "https://github.com/org/mid",
            JobTrigger::Webhook,
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
        )
        .unwrap();
        let job = q.claim_next(0).unwrap().unwrap();

        q.complete(job.id).unwrap();

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
        )
        .unwrap();

        // Claim and fail — attempt becomes 1, max_attempts is 4, so it should retry
        let job = q.claim_next(0).unwrap().unwrap();
        assert_eq!(job.attempt, 1);
        q.fail(job.id, "network error", false).unwrap();

        let depth = q.queue_depth().unwrap();
        assert_eq!(depth.pending, 1, "should be back to pending for retry");
        assert_eq!(depth.dead_letter, 0);

        // Claim again — attempt 2
        let job = q.claim_next(0).unwrap().unwrap();
        assert_eq!(job.attempt, 2);
        assert_eq!(job.trigger, JobTrigger::Retry);
        q.fail(job.id, "network error", false).unwrap();

        // Claim again — attempt 3
        let job = q.claim_next(0).unwrap().unwrap();
        assert_eq!(job.attempt, 3);
        q.fail(job.id, "network error", false).unwrap();

        // Claim again — attempt 4, should dead-letter now
        let job = q.claim_next(0).unwrap().unwrap();
        assert_eq!(job.attempt, 4);
        q.fail(job.id, "final failure", false).unwrap();

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
        )
        .unwrap();
        let job = q.claim_next(0).unwrap().unwrap();

        q.fail(job.id, "auth failure 401", true).unwrap();

        let depth = q.queue_depth().unwrap();
        assert_eq!(
            depth.dead_letter, 1,
            "poison should dead-letter immediately"
        );
        assert_eq!(depth.pending, 0);
    }

    #[test]
    fn recover_stale_resets_old_running() {
        let q = queue();
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
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
    fn new_event_resets_dead_letter() {
        let q = queue();
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
        )
        .unwrap();
        let job = q.claim_next(0).unwrap().unwrap();
        q.fail(job.id, "poison", true).unwrap();

        let depth = q.queue_depth().unwrap();
        assert_eq!(depth.dead_letter, 1);

        // New event for the same repo — reset from dead_letter
        q.reset_dead_letter("repo-1").unwrap();
        // Now upsert should work since the status is pending
        q.upsert(
            "repo-1",
            "https://github.com/org/repo-1",
            JobTrigger::Webhook,
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
        )
        .unwrap();
        q.upsert("repo-2", "https://github.com/org/repo-2", JobTrigger::Poll)
            .unwrap();

        // Dead-letter repo-1
        let job = q.claim_next(0).unwrap().unwrap();
        q.fail(job.id, "poison", true).unwrap();

        let dead = q.dead_letters().unwrap();
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].repo_id, "repo-1");
        assert_eq!(dead[0].error_msg.as_deref(), Some("poison"));
    }

    #[test]
    fn queue_depth_counts_all_statuses() {
        let q = queue();
        // Create 3 jobs in different states
        q.upsert("repo-a", "https://github.com/org/a", JobTrigger::Webhook)
            .unwrap();
        q.upsert("repo-b", "https://github.com/org/b", JobTrigger::Poll)
            .unwrap();
        q.upsert("repo-c", "https://github.com/org/c", JobTrigger::Scheduled)
            .unwrap();

        // Claim and complete repo-a
        let job_a = q.claim_next(0).unwrap().unwrap();
        q.complete(job_a.id).unwrap();

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
        )
        .unwrap();

        let job = q.claim_next(0).unwrap().unwrap();
        assert_eq!(job.attempt, 1, "first claim should set attempt to 1");
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
    }

    #[test]
    fn canonical_id_coalesces_same_repo() {
        let q = queue();
        let url = "https://github.com/org/repo";
        let id1 = canonical_repo_id(url);
        let id2 = canonical_repo_id(&format!("{}.git", url));

        q.upsert(&id1, url, JobTrigger::Webhook).unwrap();
        q.upsert(&id2, url, JobTrigger::Poll).unwrap();

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
        )
        .unwrap();

        let depth = q.queue_depth().unwrap();
        assert_eq!(depth.running, 1, "job should still be running");
        assert_eq!(depth.pending, 0, "should not create a new pending job");
    }
}
