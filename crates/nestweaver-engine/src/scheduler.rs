//! Adaptive polling scheduler for server-mode repo monitoring.
//!
//! Poll interval adapts based on commit recency:
//! `clamp(time_since_last_commit / 2, 45s, 8h)`.
//!
//! When webhooks are healthy, the floor extends to 5 minutes.
//! Jitter prevents thundering herd on multi-repo instances.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Minimum default poll interval (45 seconds).
const DEFAULT_MIN_POLL: Duration = Duration::from_secs(45);
/// Maximum default poll interval (8 hours).
const DEFAULT_MAX_POLL: Duration = Duration::from_secs(8 * 3600);
/// Floor when webhooks are healthy (5 minutes).
const WEBHOOK_HEALTHY_FLOOR: Duration = Duration::from_secs(300);

/// Schedules adaptive polling for a set of repos based on commit recency.
pub struct PollScheduler {
    repos: Vec<RepoSchedule>,
    min_poll: Duration,
    max_poll: Duration,
    webhook_healthy: bool,
}

struct RepoSchedule {
    repo_id: String,
    repo_url: String,
    branch: Option<String>,
    poll_override: Option<PollOverride>,
    last_commit_time: Option<Instant>,
    next_poll_at: Instant,
}

/// Command sent to the live scheduler via an mpsc channel.
#[derive(Debug)]
pub enum SchedulerCommand {
    AddRepo {
        repo_id: String,
        repo_url: String,
        poll_override: Option<PollOverride>,
        branch: Option<String>,
    },
    RemoveRepo {
        repo_id: String,
    },
    ReloadConfig {
        repos: Vec<(String, String, Option<PollOverride>, Option<String>)>,
        min_poll: Option<std::time::Duration>,
        max_poll: Option<std::time::Duration>,
    },
}

/// Per-repo override for poll behavior.
#[derive(Debug)]
pub enum PollOverride {
    /// Always poll at this fixed interval.
    Fixed(Duration),
    /// Never poll this repo.
    Never,
    /// Only poll on manual trigger.
    Manual,
}

impl PollScheduler {
    /// Create a new scheduler with the given min/max poll bounds.
    pub fn new(min_poll: Duration, max_poll: Duration) -> Self {
        Self {
            repos: Vec::new(),
            min_poll,
            max_poll,
            webhook_healthy: false,
        }
    }

    /// Create a scheduler with default bounds (45s min, 8h max).
    pub fn with_defaults() -> Self {
        Self::new(DEFAULT_MIN_POLL, DEFAULT_MAX_POLL)
    }

    /// Set whether webhooks are healthy (extends floor to 5 min).
    pub fn set_webhook_healthy(&mut self, healthy: bool) {
        self.webhook_healthy = healthy;
    }

    /// Register a repo for polling.
    pub fn add_repo(
        &mut self,
        repo_id: String,
        repo_url: String,
        poll_override: Option<PollOverride>,
        branch: Option<String>,
    ) {
        self.repos.push(RepoSchedule {
            repo_id,
            repo_url,
            branch,
            poll_override,
            last_commit_time: None,
            next_poll_at: Instant::now(),
        });
    }

    /// Compute the next poll interval for a repo using the adaptive formula.
    ///
    /// - Fixed override: always returns the fixed duration.
    /// - Never/Manual: returns `Duration::MAX` (effectively never).
    /// - Adaptive: `clamp(time_since_last_commit / 2, floor, max_poll)`.
    ///   The floor is `min_poll` (45s) normally, or 5 minutes when webhooks
    ///   are healthy.
    #[allow(dead_code, private_interfaces)]
    pub(crate) fn next_interval(&self, repo: &RepoSchedule) -> Duration {
        compute_interval(repo, self.min_poll, self.max_poll, self.webhook_healthy)
    }

    /// Return repos that are due for polling now, advancing their
    /// `next_poll_at` to the next interval.
    pub fn due_repos(&mut self) -> Vec<(String, String, Option<String>)> {
        let now = Instant::now();
        let min_poll = self.min_poll;
        let max_poll = self.max_poll;
        let webhook_healthy = self.webhook_healthy;

        self.repos
            .iter_mut()
            .filter(|r| r.next_poll_at <= now)
            .filter_map(|r| {
                let interval = compute_interval(r, min_poll, max_poll, webhook_healthy);
                // Skip repos configured to never poll.
                if interval == Duration::MAX {
                    return None;
                }
                r.next_poll_at = now + jittered(interval);
                Some((r.repo_id.clone(), r.repo_url.clone(), r.branch.clone()))
            })
            .collect()
    }

    /// Update last commit time for a repo (called after checking ls-remote).
    pub fn update_commit_time(&mut self, repo_id: &str, time: Instant) {
        if let Some(repo) = self.repos.iter_mut().find(|r| r.repo_id == repo_id) {
            repo.last_commit_time = Some(time);
        }
    }

    /// Remove a repo from the scheduler.
    pub fn remove_repo(&mut self, repo_id: &str) {
        self.repos.retain(|r| r.repo_id != repo_id);
    }

    /// Number of tracked repos.
    pub fn repo_count(&self) -> usize {
        self.repos.len()
    }
}

/// Compute the adaptive interval for a repo given scheduler config.
/// Extracted as a free function to avoid borrow conflicts in `due_repos`.
fn compute_interval(
    repo: &RepoSchedule,
    min_poll: Duration,
    max_poll: Duration,
    webhook_healthy: bool,
) -> Duration {
    match &repo.poll_override {
        Some(PollOverride::Fixed(d)) => *d,
        Some(PollOverride::Never | PollOverride::Manual) => Duration::MAX,
        None => {
            let since_last = repo
                .last_commit_time
                .map(|t| t.elapsed())
                .unwrap_or(max_poll);
            let base = since_last.max(Duration::ZERO) / 2;
            let floor = if webhook_healthy {
                WEBHOOK_HEALTHY_FLOOR
            } else {
                min_poll
            };
            base.clamp(floor, max_poll)
        }
    }
}

/// Add jitter to an interval to prevent thundering herd.
/// Returns a value in `[interval/2, interval * 1.5)`.
fn jittered(interval: Duration) -> Duration {
    if interval >= Duration::MAX / 2 {
        return interval;
    }
    let millis = interval.as_millis() as u64;
    if millis == 0 {
        return interval;
    }
    let half = millis / 2;
    let jitter = rand::random_range(0..millis.max(1));
    Duration::from_millis(half + jitter)
}

/// Tracks incremental update counts per repo to decide when a full re-index
/// is needed.
///
/// Three triggers for full re-index:
/// 1. Proportional delta threshold: `max(150, file_count * 0.5%)`
/// 2. 0.25% random spot-check per poll cycle
/// 3. Time backstop (7 days) - handled externally by the scheduler loop
pub struct ReindexTracker {
    /// Map repo_id -> incremental update count since last full index.
    counts: HashMap<String, u32>,
    /// Map repo_id -> wall-clock time of last full re-index, as a Unix epoch
    /// timestamp (seconds). Stored as wall clock (not [`Instant`]) so the
    /// 7-day backstop survives a daemon restart — a monotonic `Instant` is
    /// meaningless across process lifetimes.
    last_full_reindex: HashMap<String, i64>,
}

/// Maximum interval between full re-indexes, in seconds (7 days). Even when
/// the count-based threshold hasn't been hit, a full re-index ensures the
/// graph doesn't silently drift from source over long periods.
const FULL_REINDEX_INTERVAL_SECS: i64 = 7 * 24 * 3600;

/// Current wall-clock time as a Unix epoch timestamp (seconds).
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl ReindexTracker {
    pub fn new() -> Self {
        Self {
            counts: HashMap::new(),
            last_full_reindex: HashMap::new(),
        }
    }

    /// Rehydrate a tracker from persisted state (e.g. rows loaded from the
    /// job-queue's `reindex_state` table at daemon startup). Each tuple is
    /// `(repo_id, update_count, last_full_unix)`; a `None` timestamp means the
    /// repo has never had a recorded full re-index.
    pub fn from_persisted(rows: impl IntoIterator<Item = (String, u32, Option<i64>)>) -> Self {
        let mut tracker = Self::new();
        tracker.load_persisted(rows);
        tracker
    }

    /// Replace the in-memory cache with persisted state. Used to rehydrate an
    /// already-constructed tracker (e.g. inside the worker pool) at startup.
    pub fn load_persisted(&mut self, rows: impl IntoIterator<Item = (String, u32, Option<i64>)>) {
        self.counts.clear();
        self.last_full_reindex.clear();
        for (repo_id, count, last_full) in rows {
            if count > 0 {
                self.counts.insert(repo_id.clone(), count);
            }
            if let Some(ts) = last_full {
                self.last_full_reindex.insert(repo_id, ts);
            }
        }
    }

    /// Record an incremental update for a repo.
    pub fn record_incremental(&mut self, repo_id: &str) {
        *self.counts.entry(repo_id.to_string()).or_insert(0) += 1;
    }

    /// Check if a repo needs a full re-index based on the proportional
    /// threshold: `max(150, file_count * 0.5%)`, OR if more than 7 days
    /// have elapsed since the last full re-index.
    pub fn needs_full_reindex(&self, repo_id: &str, file_count: u64) -> bool {
        // Count-based threshold.
        let threshold = std::cmp::max(150, (file_count as f64 * 0.005) as u32);
        if self.counts.get(repo_id).copied().unwrap_or(0) >= threshold {
            return true;
        }
        // Time-based backstop: trigger a full re-index if the last one was
        // more than 7 days ago, compared against the wall clock so the timer
        // survives restarts. Repos that have never been reset (no entry) are
        // not triggered by time — the count threshold handles their first full
        // index, and `reset()` records the timestamp for future checks.
        match self.last_full_reindex.get(repo_id) {
            Some(last) => now_unix() - last >= FULL_REINDEX_INTERVAL_SECS,
            None => false,
        }
    }

    /// Reset the incremental count for a repo (after a full re-index).
    pub fn reset(&mut self, repo_id: &str) {
        self.counts.remove(repo_id);
        self.last_full_reindex
            .insert(repo_id.to_string(), now_unix());
    }

    /// Current incremental count for a repo.
    pub fn count(&self, repo_id: &str) -> u32 {
        self.counts.get(repo_id).copied().unwrap_or(0)
    }

    /// Wall-clock time (Unix epoch seconds) of the last full re-index for a
    /// repo, or `None` if it has never been recorded. Used to write the
    /// tracker's state through to the persisted store.
    pub fn last_full_unix(&self, repo_id: &str) -> Option<i64> {
        self.last_full_reindex.get(repo_id).copied()
    }

    /// 0.25% random spot-check: returns true with probability 1/400.
    pub fn random_spot_check() -> bool {
        rand::random_ratio(1, 400)
    }
}

impl Default for ReindexTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scheduler_with_repo(last_commit_ago: Option<Duration>) -> PollScheduler {
        let mut sched = PollScheduler::new(Duration::from_secs(45), Duration::from_secs(8 * 3600));
        sched.add_repo(
            "test-repo".into(),
            "https://github.com/org/test".into(),
            None,
            None,
        );

        if let Some(ago) = last_commit_ago {
            let commit_time = Instant::now() - ago;
            sched.update_commit_time("test-repo", commit_time);
        }

        sched
    }

    #[test]
    fn adaptive_interval_active_repo() {
        // Last commit 1 minute ago -> half = 30s -> clamped to floor 45s
        let sched = scheduler_with_repo(Some(Duration::from_secs(60)));
        let interval = sched.next_interval(&sched.repos[0]);
        assert_eq!(interval, Duration::from_secs(45));
    }

    #[test]
    fn adaptive_interval_dormant_repo() {
        // Last commit 3 days ago -> half = 1.5 days -> clamped to ceiling 8h
        let three_days = Duration::from_secs(3 * 24 * 3600);
        let sched = scheduler_with_repo(Some(three_days));
        let interval = sched.next_interval(&sched.repos[0]);
        assert_eq!(interval, Duration::from_secs(8 * 3600));
    }

    #[test]
    fn adaptive_interval_moderate() {
        // Last commit 1 hour ago -> half ~= 30 min -> within bounds
        let sched = scheduler_with_repo(Some(Duration::from_secs(3600)));
        let interval = sched.next_interval(&sched.repos[0]);
        // Allow small timing slop from elapsed() measurement
        let secs = interval.as_secs();
        assert!(
            (1799..=1801).contains(&secs),
            "expected ~1800s, got {secs}s"
        );
    }

    #[test]
    fn poll_override_fixed() {
        let mut sched = PollScheduler::new(Duration::from_secs(45), Duration::from_secs(8 * 3600));
        sched.add_repo(
            "fixed-repo".into(),
            "https://github.com/org/fixed".into(),
            Some(PollOverride::Fixed(Duration::from_secs(60))),
            None,
        );
        let interval = sched.next_interval(&sched.repos[0]);
        assert_eq!(interval, Duration::from_secs(60));
    }

    #[test]
    fn poll_override_never() {
        let mut sched = PollScheduler::new(Duration::from_secs(45), Duration::from_secs(8 * 3600));
        sched.add_repo(
            "never-repo".into(),
            "https://github.com/org/never".into(),
            Some(PollOverride::Never),
            None,
        );
        let interval = sched.next_interval(&sched.repos[0]);
        assert_eq!(interval, Duration::MAX);
    }

    #[test]
    fn committer_date_clock_drift() {
        // If last_commit_time is very recent (essentially "now"), the elapsed
        // time is near zero, so half is near zero, clamped to min_poll.
        let sched = scheduler_with_repo(Some(Duration::ZERO));
        let interval = sched.next_interval(&sched.repos[0]);
        assert_eq!(
            interval,
            Duration::from_secs(45),
            "zero elapsed clamps to floor"
        );
    }

    #[test]
    fn webhook_healthy_extends_floor() {
        // Last commit 5 min ago -> half = 2.5 min = 150s
        // With webhook healthy: floor is 300s, so clamped up to 300s
        let mut sched = scheduler_with_repo(Some(Duration::from_secs(300)));
        sched.set_webhook_healthy(true);
        let interval = sched.next_interval(&sched.repos[0]);
        assert_eq!(interval, Duration::from_secs(300));
    }

    #[test]
    fn no_commit_time_defaults_to_max() {
        // No last_commit_time -> defaults to max_poll for since_last
        // half of max_poll = 4h = 14400s
        let sched = scheduler_with_repo(None);
        let interval = sched.next_interval(&sched.repos[0]);
        assert_eq!(interval, Duration::from_secs(4 * 3600));
    }

    #[test]
    fn due_repos_skips_never() {
        let mut sched = PollScheduler::new(Duration::from_secs(45), Duration::from_secs(8 * 3600));
        sched.add_repo("repo-a".into(), "https://a.com".into(), None, None);
        sched.add_repo(
            "repo-b".into(),
            "https://b.com".into(),
            Some(PollOverride::Never),
            None,
        );

        let due = sched.due_repos();
        let ids: Vec<&str> = due.iter().map(|(id, _, _)| id.as_str()).collect();
        assert!(ids.contains(&"repo-a"), "active repo should be due");
        assert!(
            !ids.contains(&"repo-b"),
            "never-poll repo should be skipped"
        );
    }

    #[test]
    fn remove_repo_works() {
        let mut sched = PollScheduler::with_defaults();
        sched.add_repo("repo-a".into(), "https://a.com".into(), None, None);
        sched.add_repo("repo-b".into(), "https://b.com".into(), None, None);
        assert_eq!(sched.repo_count(), 2);
        sched.remove_repo("repo-a");
        assert_eq!(sched.repo_count(), 1);
    }

    // --- ReindexTracker tests ---

    #[test]
    fn threshold_small_repo() {
        // 100 files -> 100 * 0.005 = 0.5 -> max(150, 0) = 150
        let tracker = ReindexTracker::new();
        assert!(!tracker.needs_full_reindex("repo", 100));
    }

    #[test]
    fn threshold_large_repo() {
        // 50000 files -> 50000 * 0.005 = 250 -> max(150, 250) = 250
        let mut tracker = ReindexTracker::new();
        for _ in 0..249 {
            tracker.record_incremental("repo");
        }
        assert!(!tracker.needs_full_reindex("repo", 50_000));
        tracker.record_incremental("repo");
        assert!(tracker.needs_full_reindex("repo", 50_000));
    }

    #[test]
    fn threshold_floor_at_150() {
        // Even with 1000 files (1000 * 0.005 = 5), floor is 150
        let mut tracker = ReindexTracker::new();
        for _ in 0..149 {
            tracker.record_incremental("repo");
        }
        assert!(!tracker.needs_full_reindex("repo", 1000));
        tracker.record_incremental("repo");
        assert!(tracker.needs_full_reindex("repo", 1000));
    }

    #[test]
    fn record_and_check() {
        let mut tracker = ReindexTracker::new();
        assert_eq!(tracker.count("repo"), 0);

        tracker.record_incremental("repo");
        tracker.record_incremental("repo");
        assert_eq!(tracker.count("repo"), 2);

        tracker.reset("repo");
        assert_eq!(tracker.count("repo"), 0);
    }

    #[test]
    fn spot_check_probability() {
        // Run 100_000 trials, expect ~250 hits (0.25%).
        let mut hits = 0;
        for _ in 0..100_000 {
            if ReindexTracker::random_spot_check() {
                hits += 1;
            }
        }
        // Expected: 250. Allow wide tolerance for statistical safety.
        assert!(
            hits > 50 && hits < 600,
            "spot check hits {hits} outside expected range"
        );
    }

    #[test]
    fn multiple_repos_independent() {
        let mut tracker = ReindexTracker::new();
        for _ in 0..150 {
            tracker.record_incremental("repo-a");
        }
        assert!(tracker.needs_full_reindex("repo-a", 100));
        assert!(!tracker.needs_full_reindex("repo-b", 100));
    }

    #[test]
    fn reset_records_wall_clock_and_does_not_immediately_retrigger() {
        let mut tracker = ReindexTracker::new();
        tracker.reset("repo");
        // A fresh reset stamps "now", so the 7-day backstop must not fire.
        assert!(!tracker.needs_full_reindex("repo", 100));
        assert!(
            tracker.last_full_unix("repo").is_some(),
            "reset should stamp a wall-clock timestamp"
        );
    }

    #[test]
    fn rehydrate_restores_count_and_last_full() {
        let last_full = now_unix() - 3600; // 1 hour ago
        let tracker =
            ReindexTracker::from_persisted([("repo".to_string(), 42u32, Some(last_full))]);
        assert_eq!(tracker.count("repo"), 42);
        assert_eq!(tracker.last_full_unix("repo"), Some(last_full));
        // 1 hour ago is well within the 7-day window — no forced full.
        assert!(!tracker.needs_full_reindex("repo", 100));
    }

    #[test]
    fn rehydrated_at_threshold_count_triggers_full() {
        // A restored count at/over the floor threshold must still force a full.
        let tracker = ReindexTracker::from_persisted([("repo".to_string(), 150u32, None)]);
        assert!(tracker.needs_full_reindex("repo", 100));
    }

    #[test]
    fn rehydrated_old_last_full_triggers_time_backstop() {
        // Persisted last_full 8 days ago should fire the wall-clock backstop
        // even with a zero update count.
        let eight_days_ago = now_unix() - 8 * 24 * 3600;
        let tracker =
            ReindexTracker::from_persisted([("repo".to_string(), 0u32, Some(eight_days_ago))]);
        assert!(
            tracker.needs_full_reindex("repo", 100),
            "7-day wall-clock backstop should fire from a persisted old last_full"
        );
    }
}
