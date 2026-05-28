//! Agent interaction tracking and decay-based scoring.
//!
//! Tracks how an AI agent accesses nodes (notes, symbols, etc.) during
//! sessions, and uses temporal decay to compute relevance scores.  Scores
//! are persisted as a JSON sidecar (`<db>.interactions.json`) using the
//! same atomic-write pattern as [`crate::extensions`].
//!
//! ## Scoring model
//!
//! Each node accumulates counters from four event types:
//!
//! | Event     | Counter incremented   | Weight |
//! |-----------|-----------------------|--------|
//! | Query     | `query_seed_count`    | 0.5    |
//! | Access    | `access_count`        | 0.3    |
//! | FollowUp  | `result_used_count`   | 1.0    |
//! | Impact    | (same as Query seeds) | 0.5    |
//!
//! Shown results from a Query contribute `result_shown_count` (weight 0.1).
//!
//! The raw weighted sum is compressed with `ln(1 + x)`, multiplied by an
//! exponential temporal decay (14-day half-life), and optionally penalised
//! when the node's content hash has changed since last access.  Nodes
//! accessed from 3+ distinct sessions receive a floor score of 0.1.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

// ── Constants ────────────────────────────────────────────────────────────

/// Exponential decay rate (lambda). Gives a ~14-day half-life.
const DEFAULT_LAMBDA: f64 = 0.05;

/// Multiplicative penalty applied when the node's content hash has
/// changed since the last access.
const HASH_CHANGE_PENALTY: f64 = 0.5;

/// Minimum number of distinct sessions before the floor applies.
const SESSION_FLOOR_THRESHOLD: u32 = 3;

/// Minimum score for nodes accessed from enough distinct sessions.
const SESSION_FLOOR_SCORE: f64 = 0.1;

// ── Data types ───────────────────────────────────────────────────────────

/// The kind of interaction event recorded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EventType {
    /// A query operation (brain_context, brain_search, project_context).
    Query,
    /// A direct access (note_get, backlinks, get_summary).
    Access,
    /// An access within 30 s of a query to a node that appeared in results.
    FollowUp,
    /// An impact or blast-radius operation.
    Impact,
}

/// A single recorded interaction event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InteractionEvent {
    pub timestamp: f64,
    pub event_type: EventType,
    pub tool_name: String,
    pub uids: Vec<String>,
    pub session_id: String,
}

/// Per-node aggregated interaction scores.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeScore {
    pub access_count: u32,
    pub query_seed_count: u32,
    pub result_used_count: u32,
    pub result_shown_count: u32,
    pub last_accessed: f64,
    pub content_hash_at_access: Option<String>,
    pub distinct_sessions: u32,
    pub computed_score: f64,

    /// Internal: set of session IDs seen so far (not serialised to the
    /// public API but kept in the sidecar for accurate distinct-session
    /// tracking across flushes).
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    pub session_ids: HashSet<String>,
}

/// The on-disk interaction store persisted as a sidecar.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InteractionStore {
    pub version: u32,
    pub last_compacted: f64,
    pub node_scores: HashMap<String, NodeScore>,
}

// ── InteractionTracker ───────────────────────────────────────────────────

/// Records interaction events in memory and periodically flushes them to
/// the sidecar file, merging counters with any previously-stored data.
pub struct InteractionTracker {
    db_path: PathBuf,
    events: Mutex<Vec<InteractionEvent>>,
    session_id: String,
    last_query_time: Mutex<f64>,
    last_query_results: Mutex<Vec<String>>,
    flush_threshold: usize,
}

impl InteractionTracker {
    /// Create a new tracker for the database at `db_path`.
    ///
    /// A random session ID is generated automatically.  Events are
    /// auto-flushed when the in-memory buffer reaches 50 entries.
    pub fn new(db_path: &Path) -> Self {
        Self {
            db_path: db_path.to_path_buf(),
            events: Mutex::new(Vec::new()),
            session_id: generate_session_id(),
            last_query_time: Mutex::new(0.0),
            last_query_results: Mutex::new(Vec::new()),
            flush_threshold: 50,
        }
    }

    /// Create a tracker with a custom flush threshold and session ID
    /// (useful for tests).
    #[cfg(test)]
    fn new_with_options(db_path: &Path, flush_threshold: usize, session_id: &str) -> Self {
        Self {
            db_path: db_path.to_path_buf(),
            events: Mutex::new(Vec::new()),
            session_id: session_id.to_string(),
            last_query_time: Mutex::new(0.0),
            last_query_results: Mutex::new(Vec::new()),
            flush_threshold,
        }
    }

    /// Record a query event.
    ///
    /// `seed_uids` are the UIDs that seeded the query; `result_uids` are
    /// those returned (shown) to the agent.  Both sets are tracked for
    /// scoring.
    pub fn record_query(&self, tool: &str, seed_uids: &[String], result_uids: &[String]) {
        let now = now_epoch();
        let mut all_uids: Vec<String> = Vec::with_capacity(seed_uids.len() + result_uids.len());
        all_uids.extend_from_slice(seed_uids);
        all_uids.extend_from_slice(result_uids);

        let event = InteractionEvent {
            timestamp: now,
            event_type: EventType::Query,
            tool_name: tool.to_string(),
            uids: all_uids,
            session_id: self.session_id.clone(),
        };

        // Update last-query bookkeeping *before* pushing the event so
        // that a subsequent `record_access` in the same instant sees the
        // query results.
        {
            let mut lqt = self
                .last_query_time
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *lqt = now;
        }
        {
            let mut lqr = self
                .last_query_results
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *lqr = result_uids.to_vec();
        }

        self.push_event(event);
    }

    /// Record an access event.
    ///
    /// If the access is within 30 seconds of a query and the target UID
    /// was among the query's results, the event is upgraded to a
    /// [`EventType::FollowUp`].
    pub fn record_access(&self, tool: &str, target_uid: &str) {
        let now = now_epoch();

        let is_follow_up = {
            let lqt = self
                .last_query_time
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let lqr = self
                .last_query_results
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            (now - *lqt) <= 30.0 && lqr.iter().any(|u| u == target_uid)
        };

        let event_type = if is_follow_up {
            EventType::FollowUp
        } else {
            EventType::Access
        };

        let event = InteractionEvent {
            timestamp: now,
            event_type,
            tool_name: tool.to_string(),
            uids: vec![target_uid.to_string()],
            session_id: self.session_id.clone(),
        };

        self.push_event(event);
    }

    /// Record an impact/blast-radius event.
    pub fn record_impact(&self, tool: &str, seed_uids: &[String]) {
        let now = now_epoch();
        let event = InteractionEvent {
            timestamp: now,
            event_type: EventType::Impact,
            tool_name: tool.to_string(),
            uids: seed_uids.to_vec(),
            session_id: self.session_id.clone(),
        };

        self.push_event(event);
    }

    /// Flush buffered events to the sidecar, consolidating them into the
    /// persisted [`InteractionStore`].
    pub fn flush(&self) -> Result<(), anyhow::Error> {
        let events: Vec<InteractionEvent> = {
            let mut buf = self.events.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *buf)
        };

        if events.is_empty() {
            return Ok(());
        }

        // Load existing store (or start fresh).
        let mut store = load_interaction_store(&self.db_path).unwrap_or_default();

        consolidate_events(&mut store, &events);

        let now = now_epoch();
        recompute_all_scores(&mut store, now);
        store.last_compacted = now;

        save_interaction_store(&self.db_path, &store)?;

        Ok(())
    }

    /// Push an event into the buffer and auto-flush when the threshold is
    /// reached.
    fn push_event(&self, event: InteractionEvent) {
        let should_flush = {
            let mut buf = self.events.lock().unwrap_or_else(|e| e.into_inner());
            buf.push(event);
            buf.len() >= self.flush_threshold
        };

        if should_flush && let Err(e) = self.flush() {
            tracing::warn!("auto-flush failed: {e}");
        }
    }
}

// ── Consolidation ────────────────────────────────────────────────────────

/// Merge a batch of events into the interaction store's `node_scores`.
fn consolidate_events(store: &mut InteractionStore, events: &[InteractionEvent]) {
    for event in events {
        match event.event_type {
            EventType::Query => {
                // By convention the first N UIDs are seeds, the rest are
                // results.  Since `record_query` concatenates seed_uids
                // then result_uids, we need the seed count.  We store
                // all UIDs together so we process them uniformly here,
                // but to distinguish seeds from results we mark all UIDs
                // that appear in the event.  The caller already tagged
                // the event; however we don't store the split index.
                //
                // Instead, we increment `query_seed_count` for *all*
                // UIDs (conservative — seeds definitely queried, results
                // were returned) and additionally `result_shown_count`
                // for all UIDs.
                for uid in &event.uids {
                    let ns = store.node_scores.entry(uid.clone()).or_default();
                    ns.query_seed_count += 1;
                    ns.result_shown_count += 1;
                    ns.last_accessed = event.timestamp;
                    ns.session_ids.insert(event.session_id.clone());
                    ns.distinct_sessions = ns.session_ids.len() as u32;
                }
            }
            EventType::Access => {
                for uid in &event.uids {
                    let ns = store.node_scores.entry(uid.clone()).or_default();
                    ns.access_count += 1;
                    ns.last_accessed = event.timestamp;
                    ns.session_ids.insert(event.session_id.clone());
                    ns.distinct_sessions = ns.session_ids.len() as u32;
                }
            }
            EventType::FollowUp => {
                for uid in &event.uids {
                    let ns = store.node_scores.entry(uid.clone()).or_default();
                    ns.result_used_count += 1;
                    ns.last_accessed = event.timestamp;
                    ns.session_ids.insert(event.session_id.clone());
                    ns.distinct_sessions = ns.session_ids.len() as u32;
                }
            }
            EventType::Impact => {
                for uid in &event.uids {
                    let ns = store.node_scores.entry(uid.clone()).or_default();
                    ns.query_seed_count += 1;
                    ns.last_accessed = event.timestamp;
                    ns.session_ids.insert(event.session_id.clone());
                    ns.distinct_sessions = ns.session_ids.len() as u32;
                }
            }
        }
    }
}

/// Recompute `computed_score` for every node using the decay function.
fn recompute_all_scores(store: &mut InteractionStore, now: f64) {
    for ns in store.node_scores.values_mut() {
        ns.computed_score = compute_decayed_score(ns, now, false);
    }
}

// ── Decay function ───────────────────────────────────────────────────────

/// Compute a decay-adjusted interaction score for a node.
///
/// `content_hash_changed` should be `true` when the node's content has
/// been modified since `node.content_hash_at_access` was recorded,
/// applying a 50 % penalty.
pub fn compute_decayed_score(node: &NodeScore, now: f64, content_hash_changed: bool) -> f64 {
    let days_since = (now - node.last_accessed) / 86400.0;
    let temporal_decay = (-DEFAULT_LAMBDA * days_since).exp();
    let hash_penalty = if content_hash_changed {
        HASH_CHANGE_PENALTY
    } else {
        1.0
    };

    let raw = (node.access_count as f64 * 0.3)
        + (node.query_seed_count as f64 * 0.5)
        + (node.result_used_count as f64 * 1.0)
        + (node.result_shown_count as f64 * 0.1);
    let raw_normalized = raw.ln_1p();

    let floor = if node.distinct_sessions >= SESSION_FLOOR_THRESHOLD {
        SESSION_FLOOR_SCORE
    } else {
        0.0
    };

    (raw_normalized * temporal_decay * hash_penalty).max(floor)
}

// ── Sidecar I/O ──────────────────────────────────────────────────────────

/// Return the canonical sidecar path for interaction data.
pub fn interaction_sidecar_path(db_path: &Path) -> PathBuf {
    crate::sidecar_path(db_path, ".interactions.json")
}

/// Load the full interaction store from the sidecar.
fn load_interaction_store(db_path: &Path) -> Option<InteractionStore> {
    let path = interaction_sidecar_path(db_path);
    let text = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Load pre-computed interaction scores (UID -> score) for use in
/// ranking/boosting.  Returns `None` when the sidecar is missing or
/// cannot be parsed.
pub fn load_interaction_scores(db_path: &Path) -> Option<HashMap<String, f64>> {
    let path = interaction_sidecar_path(db_path);
    let text = std::fs::read_to_string(&path).ok()?;
    let store: InteractionStore = serde_json::from_str(&text).ok()?;
    Some(
        store
            .node_scores
            .iter()
            .map(|(k, v)| (k.clone(), v.computed_score))
            .filter(|(_, s)| *s > 0.0)
            .collect(),
    )
}

/// Persist the interaction store to its sidecar file using an atomic
/// write-then-rename pattern.
pub fn save_interaction_store(
    db_path: &Path,
    store: &InteractionStore,
) -> Result<(), anyhow::Error> {
    let path = interaction_sidecar_path(db_path);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string(store)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

// ── Helpers ──────────────────────────────────────────────────────────────

/// Current time as seconds since the Unix epoch.
fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Generate a short random session ID.
fn generate_session_id() -> String {
    use rand::RngExt;
    let mut rng = rand::rng();
    let id: u64 = rng.random();
    format!("s-{:016x}", id)
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    /// Helper: create a tracker backed by a temp file.
    fn temp_tracker() -> (InteractionTracker, PathBuf) {
        let tmp = NamedTempFile::new().unwrap();
        let db_path = tmp.path().to_path_buf();
        // Keep the tmp handle alive by leaking it (tests are short-lived).
        let _ = tmp.into_temp_path();
        let tracker = InteractionTracker::new_with_options(&db_path, 50, "test-session");
        (tracker, db_path)
    }

    #[test]
    fn record_query_stores_event() {
        let (tracker, _db) = temp_tracker();
        tracker.record_query(
            "brain_context",
            &["uid-seed".into()],
            &["uid-res1".into(), "uid-res2".into()],
        );

        let events = tracker.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::Query);
        assert_eq!(events[0].tool_name, "brain_context");
        assert_eq!(events[0].uids.len(), 3);
    }

    #[test]
    fn record_access_creates_follow_up_within_30s() {
        let (tracker, _db) = temp_tracker();

        // Record a query whose results include "uid-a".
        tracker.record_query("brain_context", &[], &["uid-a".into()]);

        // Immediately access "uid-a" — should be a FollowUp.
        tracker.record_access("note_get", "uid-a");

        let events = tracker.events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].event_type, EventType::FollowUp);
    }

    #[test]
    fn record_access_no_follow_up_after_30s() {
        let (tracker, _db) = temp_tracker();

        // Record a query, then manually set last_query_time to 60 s ago.
        tracker.record_query("brain_context", &[], &["uid-a".into()]);
        {
            let mut lqt = tracker.last_query_time.lock().unwrap();
            *lqt -= 60.0;
        }

        // Access now — should be a plain Access (>30 s gap).
        tracker.record_access("note_get", "uid-a");

        let events = tracker.events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].event_type, EventType::Access);
    }

    #[test]
    fn record_access_no_follow_up_for_unknown_uid() {
        let (tracker, _db) = temp_tracker();

        // Query returns "uid-a" but we access "uid-b".
        tracker.record_query("brain_context", &[], &["uid-a".into()]);
        tracker.record_access("note_get", "uid-b");

        let events = tracker.events.lock().unwrap();
        assert_eq!(events[1].event_type, EventType::Access);
    }

    #[test]
    fn consolidate_merges_into_scores() {
        let events = vec![
            InteractionEvent {
                timestamp: 1000.0,
                event_type: EventType::Query,
                tool_name: "brain_context".into(),
                uids: vec!["uid-a".into(), "uid-b".into()],
                session_id: "s1".into(),
            },
            InteractionEvent {
                timestamp: 1010.0,
                event_type: EventType::Access,
                tool_name: "note_get".into(),
                uids: vec!["uid-a".into()],
                session_id: "s1".into(),
            },
            InteractionEvent {
                timestamp: 1020.0,
                event_type: EventType::FollowUp,
                tool_name: "note_get".into(),
                uids: vec!["uid-b".into()],
                session_id: "s1".into(),
            },
        ];

        let mut store = InteractionStore::default();
        consolidate_events(&mut store, &events);

        let a = store.node_scores.get("uid-a").unwrap();
        assert_eq!(a.query_seed_count, 1);
        assert_eq!(a.result_shown_count, 1);
        assert_eq!(a.access_count, 1);
        assert_eq!(a.result_used_count, 0);
        assert!((a.last_accessed - 1010.0).abs() < 0.001);

        let b = store.node_scores.get("uid-b").unwrap();
        assert_eq!(b.query_seed_count, 1);
        assert_eq!(b.result_used_count, 1);
    }

    #[test]
    fn decay_reduces_old_scores() {
        let recent = NodeScore {
            access_count: 5,
            query_seed_count: 3,
            result_used_count: 2,
            result_shown_count: 4,
            last_accessed: 100_000.0,
            distinct_sessions: 1,
            ..Default::default()
        };
        let mut old = recent.clone();
        old.last_accessed = 100_000.0 - 30.0 * 86400.0; // 30 days ago

        let now = 100_000.0;
        let score_recent = compute_decayed_score(&recent, now, false);
        let score_old = compute_decayed_score(&old, now, false);

        assert!(
            score_recent > score_old,
            "recent ({score_recent}) should be > old ({score_old})"
        );
        assert!(score_old > 0.0, "old score should still be positive");
    }

    #[test]
    fn content_hash_change_applies_penalty() {
        let node = NodeScore {
            access_count: 5,
            query_seed_count: 3,
            result_used_count: 2,
            result_shown_count: 4,
            last_accessed: 100_000.0,
            distinct_sessions: 1,
            ..Default::default()
        };

        let now = 100_000.0;
        let normal = compute_decayed_score(&node, now, false);
        let penalised = compute_decayed_score(&node, now, true);

        let ratio = penalised / normal;
        assert!(
            (ratio - HASH_CHANGE_PENALTY).abs() < 0.001,
            "penalty ratio ({ratio}) should be ~{HASH_CHANGE_PENALTY}"
        );
    }

    #[test]
    fn session_floor_for_frequent_nodes() {
        let node = NodeScore {
            access_count: 1,
            last_accessed: 0.0, // very old
            distinct_sessions: 3,
            ..Default::default()
        };

        // 100 days in the future so temporal decay is extreme.
        let now = 100.0 * 86400.0;
        let score = compute_decayed_score(&node, now, false);

        assert!(
            score >= SESSION_FLOOR_SCORE,
            "score ({score}) should be >= floor ({SESSION_FLOOR_SCORE})"
        );
    }

    #[test]
    fn session_floor_not_applied_below_threshold() {
        let node = NodeScore {
            access_count: 1,
            last_accessed: 0.0,
            distinct_sessions: 2, // below threshold of 3
            ..Default::default()
        };

        // 100 days in the future so temporal decay drives the score well below 0.1.
        let now = 100.0 * 86400.0;
        let score = compute_decayed_score(&node, now, false);

        assert!(
            score < SESSION_FLOOR_SCORE,
            "score ({score}) should be < floor ({SESSION_FLOOR_SCORE}) with only 2 sessions"
        );
    }

    #[test]
    fn flush_writes_sidecar() {
        let (tracker, db_path) = temp_tracker();
        tracker.record_query("brain_context", &["uid-x".into()], &[]);
        tracker.flush().unwrap();

        let sidecar = interaction_sidecar_path(&db_path);
        assert!(sidecar.exists(), "sidecar file should be created by flush");

        let scores = load_interaction_scores(&db_path);
        assert!(scores.is_some());
        assert!(scores.unwrap().contains_key("uid-x"));
    }

    #[test]
    fn load_scores_from_empty_path_returns_none() {
        let scores = load_interaction_scores(Path::new("/tmp/nonexistent_nw_test.lbug"));
        assert!(scores.is_none());
    }

    #[test]
    fn multiple_flushes_accumulate() {
        let (tracker, db_path) = temp_tracker();

        // First flush.
        tracker.record_query("brain_context", &["uid-a".into()], &[]);
        tracker.flush().unwrap();

        let scores1 = load_interaction_scores(&db_path).unwrap();
        let s1 = scores1["uid-a"];

        // Second flush — same UID, scores should grow.
        tracker.record_query("brain_context", &["uid-a".into()], &[]);
        tracker.flush().unwrap();

        let scores2 = load_interaction_scores(&db_path).unwrap();
        let s2 = scores2["uid-a"];

        assert!(
            s2 > s1,
            "score after two flushes ({s2}) should be > after one ({s1})"
        );
    }

    #[test]
    fn auto_flush_on_threshold() {
        let tmp = NamedTempFile::new().unwrap();
        let db_path = tmp.path().to_path_buf();
        let _ = tmp.into_temp_path();

        // Threshold of 5 for faster test.
        let tracker = InteractionTracker::new_with_options(&db_path, 5, "test-session");

        // Record 5 events — should trigger auto-flush.
        for i in 0..5 {
            tracker.record_access("note_get", &format!("uid-{i}"));
        }

        // After auto-flush the buffer should be empty (flushed).
        let events = tracker.events.lock().unwrap();
        assert!(
            events.is_empty(),
            "buffer should be empty after auto-flush (has {} events)",
            events.len()
        );

        // Sidecar should exist.
        let sidecar = interaction_sidecar_path(&db_path);
        assert!(sidecar.exists(), "sidecar should be written by auto-flush");
    }

    #[test]
    fn impact_event_increments_query_seed_count() {
        let (tracker, db_path) = temp_tracker();
        tracker.record_impact("brain_impact", &["uid-imp".into()]);
        tracker.flush().unwrap();

        let store = load_interaction_store(&db_path).unwrap();
        let ns = store.node_scores.get("uid-imp").unwrap();
        assert_eq!(ns.query_seed_count, 1);
        assert_eq!(ns.access_count, 0);
    }
}
