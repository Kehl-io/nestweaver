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

/// Weight of a [`EventType::TerminalSuccess`] signal in the raw weighted
/// sum. Set above the FollowUp weight (1.0) because a terminal-success
/// signal means the agent stopped searching — the clearest evidence the
/// surfaced context was useful.
const TERMINAL_SUCCESS_WEIGHT: f64 = 1.5;

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
    /// A heuristic "the agent found what it needed" signal. Recorded at
    /// clean session shutdown for the UIDs most recently surfaced when the
    /// agent stopped searching (see the MCP server shutdown path). This is
    /// the strongest positive signal — it indicates the retrieved context
    /// was good enough that the agent ended the session without more
    /// searching.
    TerminalSuccess,
}

impl std::fmt::Display for EventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            EventType::Query => "query",
            EventType::Access => "access",
            EventType::FollowUp => "follow_up",
            EventType::Impact => "impact",
            EventType::TerminalSuccess => "terminal_success",
        };
        f.write_str(s)
    }
}

/// A single recorded interaction event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InteractionEvent {
    pub timestamp: f64,
    pub event_type: EventType,
    pub tool_name: String,
    pub uids: Vec<String>,
    pub session_id: String,
    /// For `Query` events: how many of the leading `uids` are seeds.
    /// The remainder are shown results.  `0` means "all are seeds" (or
    /// the boundary is unknown — legacy events).
    #[serde(default)]
    pub seed_count: usize,
}

/// Per-node aggregated interaction scores.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeScore {
    pub access_count: u32,
    pub query_seed_count: u32,
    pub result_used_count: u32,
    pub result_shown_count: u32,
    /// Number of [`EventType::TerminalSuccess`] signals — the strongest
    /// positive signal (agent stopped searching after this node surfaced).
    #[serde(default)]
    pub terminal_success_count: u32,
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
    /// Name of the most recent recorded tool call (for the terminal-success
    /// shutdown heuristic).
    last_tool: Mutex<Option<String>>,
    flush_threshold: usize,
    last_flush: Mutex<std::time::Instant>,
}

impl InteractionTracker {
    /// Create a new tracker for the database at `db_path`.
    ///
    /// A random session ID is generated automatically.  Events are
    /// auto-flushed when the in-memory buffer reaches 50 entries.
    /// Touches the sidecar file (creating an empty store if absent) so
    /// downstream `brain status` reads can distinguish "tracking enabled
    /// but no events yet" from "tracking disabled".
    pub fn new(db_path: &Path) -> Self {
        let path = interaction_sidecar_path(db_path);
        if !path.exists()
            && let Some(parent) = path.parent()
            && std::fs::create_dir_all(parent).is_ok()
        {
            let empty = InteractionStore::default();
            if let Ok(text) = serde_json::to_string(&empty) {
                let _ = std::fs::write(&path, text);
            }
        }
        Self {
            db_path: db_path.to_path_buf(),
            events: Mutex::new(Vec::new()),
            session_id: generate_session_id(),
            last_query_time: Mutex::new(0.0),
            last_query_results: Mutex::new(Vec::new()),
            last_tool: Mutex::new(None),
            flush_threshold: 5,
            last_flush: Mutex::new(std::time::Instant::now()),
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
            last_tool: Mutex::new(None),
            flush_threshold,
            last_flush: Mutex::new(std::time::Instant::now()),
        }
    }

    /// Record a query event.
    ///
    /// `seed_uids` are the UIDs that seeded the query; `result_uids` are
    /// those returned (shown) to the agent.  Both sets are tracked for
    /// scoring.
    pub fn record_query(&self, tool: &str, seed_uids: &[String], result_uids: &[String]) {
        let now = now_epoch();
        let seed_count = seed_uids.len();
        let mut all_uids: Vec<String> = Vec::with_capacity(seed_uids.len() + result_uids.len());
        all_uids.extend_from_slice(seed_uids);
        all_uids.extend_from_slice(result_uids);

        let event = InteractionEvent {
            timestamp: now,
            event_type: EventType::Query,
            tool_name: tool.to_string(),
            uids: all_uids,
            session_id: self.session_id.clone(),
            seed_count,
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
        self.set_last_tool(tool);

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
            seed_count: 0,
        };
        self.set_last_tool(tool);

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
            seed_count: seed_uids.len(),
        };
        self.set_last_tool(tool);

        self.push_event(event);
    }

    /// Record a terminal-success event for the given UIDs.
    ///
    /// This is the heuristic "the agent found what it needed" signal,
    /// recorded at clean session shutdown for the UIDs most recently
    /// surfaced. It does not update `last_tool` (it is a synthetic,
    /// shutdown-time event, not a real tool call).
    pub fn record_terminal_success(&self, uids: &[String]) {
        if uids.is_empty() {
            return;
        }
        let event = InteractionEvent {
            timestamp: now_epoch(),
            event_type: EventType::TerminalSuccess,
            tool_name: "__terminal_success__".to_string(),
            uids: uids.to_vec(),
            session_id: self.session_id.clone(),
            seed_count: 0,
        };
        self.push_event(event);
    }

    /// Name of the most recently recorded tool call this session, if any.
    /// Used by the terminal-success shutdown heuristic.
    pub fn last_tool_name(&self) -> Option<String> {
        self.last_tool
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// UIDs most recently surfaced to the agent (results of the last query
    /// this session). Used by the terminal-success shutdown heuristic.
    pub fn last_surfaced_uids(&self) -> Vec<String> {
        self.last_query_results
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Record the name of the most recent tool call.
    fn set_last_tool(&self, tool: &str) {
        let mut lt = self.last_tool.lock().unwrap_or_else(|e| e.into_inner());
        *lt = Some(tool.to_string());
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

        self.flush_events(events)
    }

    /// Flush a pre-drained batch of events to the sidecar.
    fn flush_events(&self, events: Vec<InteractionEvent>) -> Result<(), anyhow::Error> {
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

    /// Return the number of events buffered but not yet flushed.
    pub fn pending_count(&self) -> usize {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Push an event into the buffer and auto-flush when the threshold is
    /// reached or 30 seconds have elapsed since the last flush.
    fn push_event(&self, event: InteractionEvent) {
        let mut buf = self.events.lock().unwrap_or_else(|e| e.into_inner());
        buf.push(event);
        let should_flush = buf.len() >= self.flush_threshold || {
            let last = self.last_flush.lock().unwrap_or_else(|e| e.into_inner());
            last.elapsed().as_secs() >= 30
        };
        if should_flush {
            let events: Vec<_> = buf.drain(..).collect();
            drop(buf);
            if let Err(e) = self.flush_events(events) {
                tracing::warn!("auto-flush failed: {e}");
            }
            *self.last_flush.lock().unwrap_or_else(|e| e.into_inner()) = std::time::Instant::now();
        }
    }
}

// ── Consolidation ────────────────────────────────────────────────────────

/// Merge a batch of events into the interaction store's `node_scores`.
fn consolidate_events(store: &mut InteractionStore, events: &[InteractionEvent]) {
    for event in events {
        match event.event_type {
            EventType::Query => {
                // The first `seed_count` UIDs are seeds (weight 0.5 via
                // `query_seed_count`); the remainder are shown results
                // (weight 0.1 via `result_shown_count`).
                let boundary = event.seed_count.min(event.uids.len());
                for (i, uid) in event.uids.iter().enumerate() {
                    let ns = store.node_scores.entry(uid.clone()).or_default();
                    if i < boundary {
                        ns.query_seed_count += 1;
                    } else {
                        ns.result_shown_count += 1;
                    }
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
            EventType::TerminalSuccess => {
                for uid in &event.uids {
                    let ns = store.node_scores.entry(uid.clone()).or_default();
                    ns.terminal_success_count += 1;
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
        + (node.result_shown_count as f64 * 0.1)
        // TerminalSuccess is the strongest signal: weight it >= FollowUp (1.0).
        + (node.terminal_success_count as f64 * TERMINAL_SUCCESS_WEIGHT);
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

/// Public wrapper around [`load_interaction_store`] for callers outside this
/// module (e.g. the Feature F17 training-export scaffold) that need the full
/// per-node counters, not just the derived scores. Returns `None` when the
/// sidecar is missing or unparseable.
pub fn load_interaction_store_public(db_path: &Path) -> Option<InteractionStore> {
    load_interaction_store(db_path)
}

/// Convenience view of the interaction sidecar used by the CLI
/// `interactions status` subcommand.  Combines the raw event log with
/// pre-computed per-node scores.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InteractionData {
    /// Total recorded events (across all nodes).
    pub event_count: usize,
    /// Pre-computed scores keyed by node UID.
    pub scores: HashMap<String, f64>,
    /// Timestamp of the oldest event (seconds since epoch), if any.
    pub oldest_timestamp: Option<f64>,
    /// Timestamp of the newest event (seconds since epoch), if any.
    pub newest_timestamp: Option<f64>,
}

/// Load the full interaction data from the sidecar.
///
/// Returns `None` if the file does not exist or cannot be parsed.
pub fn load_interaction_data(db_path: &Path) -> Option<InteractionData> {
    let store = load_interaction_store(db_path)?;
    let scores: HashMap<String, f64> = store
        .node_scores
        .iter()
        .map(|(k, v)| (k.clone(), v.computed_score))
        .filter(|(_, s)| *s > 0.0)
        .collect();
    let event_count: usize = store
        .node_scores
        .values()
        .map(|ns| (ns.access_count + ns.query_seed_count + ns.result_used_count) as usize)
        .sum();
    // Approximate oldest/newest from last_accessed timestamps.
    let oldest_timestamp = store
        .node_scores
        .values()
        .map(|ns| ns.last_accessed)
        .filter(|t| *t > 0.0)
        .reduce(f64::min);
    let newest_timestamp = store
        .node_scores
        .values()
        .map(|ns| ns.last_accessed)
        .filter(|t| *t > 0.0)
        .reduce(f64::max);
    Some(InteractionData {
        event_count,
        scores,
        oldest_timestamp,
        newest_timestamp,
    })
}

/// Load the aggregated [`NodeScore`] for a single UID from the sidecar.
///
/// Returns `None` if the sidecar is missing/unparseable or the UID has no
/// recorded interactions. Backs the CLI `interactions show --uid`.
pub fn load_node_score(db_path: &Path, uid: &str) -> Option<NodeScore> {
    let store = load_interaction_store(db_path)?;
    store.node_scores.get(uid).cloned()
}

/// Return the top `n` UIDs ranked by the counter for `kind`, paired with
/// that count. Used by the CLI `interactions show --top N --kind <kind>`.
///
/// Recognised kinds: `access`, `query`, `follow_up`, `impact`,
/// `terminal_success`, and `score` (rank by the computed decayed score).
/// An unknown kind yields an empty list.
pub fn top_uids_by_kind(db_path: &Path, kind: &str, n: usize) -> Vec<(String, f64)> {
    let Some(store) = load_interaction_store(db_path) else {
        return Vec::new();
    };
    let mut ranked: Vec<(String, f64)> = store
        .node_scores
        .iter()
        .filter_map(|(uid, ns)| {
            let v = match kind {
                "access" => ns.access_count as f64,
                "query" => ns.query_seed_count as f64,
                "follow_up" => ns.result_used_count as f64,
                "impact" => ns.query_seed_count as f64,
                "terminal_success" => ns.terminal_success_count as f64,
                "score" => ns.computed_score,
                _ => return None,
            };
            (v > 0.0).then(|| (uid.clone(), v))
        })
        .collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(n);
    ranked
}

/// Delete the interaction sidecar file. Returns `true` if a file was
/// removed.
pub fn clear_interaction_sidecar(db_path: &Path) -> bool {
    let path = interaction_sidecar_path(db_path);
    std::fs::remove_file(&path).is_ok()
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
        assert_eq!(events[0].seed_count, 1); // 1 seed, 2 results
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
                seed_count: 1, // uid-a is a seed, uid-b is a result
            },
            InteractionEvent {
                timestamp: 1010.0,
                event_type: EventType::Access,
                tool_name: "note_get".into(),
                uids: vec!["uid-a".into()],
                session_id: "s1".into(),
                seed_count: 0,
            },
            InteractionEvent {
                timestamp: 1020.0,
                event_type: EventType::FollowUp,
                tool_name: "note_get".into(),
                uids: vec!["uid-b".into()],
                session_id: "s1".into(),
                seed_count: 0,
            },
        ];

        let mut store = InteractionStore::default();
        consolidate_events(&mut store, &events);

        let a = store.node_scores.get("uid-a").unwrap();
        assert_eq!(a.query_seed_count, 1); // seed in the Query event
        assert_eq!(a.result_shown_count, 0); // not a shown result
        assert_eq!(a.access_count, 1);
        assert_eq!(a.result_used_count, 0);
        assert!((a.last_accessed - 1010.0).abs() < 0.001);

        let b = store.node_scores.get("uid-b").unwrap();
        assert_eq!(b.query_seed_count, 0); // not a seed
        assert_eq!(b.result_shown_count, 1); // shown result in the Query event
        assert_eq!(b.result_used_count, 1); // FollowUp event
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
    fn terminal_success_weights_at_least_as_much_as_follow_up() {
        // A node whose only signal is one FollowUp.
        let follow_up = NodeScore {
            result_used_count: 1,
            last_accessed: 100_000.0,
            distinct_sessions: 1,
            ..Default::default()
        };
        // A node whose only signal is one TerminalSuccess.
        let terminal = NodeScore {
            terminal_success_count: 1,
            last_accessed: 100_000.0,
            distinct_sessions: 1,
            ..Default::default()
        };

        let now = 100_000.0;
        let s_follow = compute_decayed_score(&follow_up, now, false);
        let s_terminal = compute_decayed_score(&terminal, now, false);

        assert!(s_follow > 0.0);
        assert!(
            s_terminal >= s_follow,
            "TerminalSuccess ({s_terminal}) should weight >= FollowUp ({s_follow})"
        );
        // And the constant itself must satisfy the requirement.
        const { assert!(TERMINAL_SUCCESS_WEIGHT >= 1.0) };
    }

    #[test]
    fn terminal_success_consolidates_and_persists() {
        let (tracker, db_path) = temp_tracker();
        tracker.record_terminal_success(&["uid-ts".into()]);
        tracker.flush().unwrap();

        let ns = load_node_score(&db_path, "uid-ts").expect("score recorded");
        assert_eq!(ns.terminal_success_count, 1);
        assert!(ns.computed_score > 0.0);
    }

    #[test]
    fn record_terminal_success_ignores_empty_uids() {
        let (tracker, _db) = temp_tracker();
        tracker.record_terminal_success(&[]);
        assert_eq!(tracker.pending_count(), 0);
    }

    #[test]
    fn last_tool_and_surfaced_uids_track_heuristic_inputs() {
        let (tracker, _db) = temp_tracker();
        tracker.record_query("brain_context", &["seed".into()], &["res-1".into()]);
        assert_eq!(tracker.last_tool_name().as_deref(), Some("brain_context"));
        assert_eq!(tracker.last_surfaced_uids(), vec!["res-1".to_string()]);

        tracker.record_access("note_get", "res-1");
        assert_eq!(tracker.last_tool_name().as_deref(), Some("note_get"));
    }

    #[test]
    fn load_node_score_returns_recorded_events() {
        let (tracker, db_path) = temp_tracker();
        tracker.record_query("brain_context", &["uid-q".into()], &[]);
        tracker.record_access("note_get", "uid-q");
        tracker.flush().unwrap();

        let ns = load_node_score(&db_path, "uid-q").expect("recorded");
        assert_eq!(ns.query_seed_count, 1);
        assert_eq!(ns.access_count, 1);
        assert!(load_node_score(&db_path, "missing").is_none());
    }

    #[test]
    fn top_uids_by_kind_ranks_by_counter() {
        let (tracker, db_path) = temp_tracker();
        tracker.record_terminal_success(&["a".into()]);
        tracker.record_terminal_success(&["a".into()]);
        tracker.record_terminal_success(&["b".into()]);
        tracker.flush().unwrap();

        let top = top_uids_by_kind(&db_path, "terminal_success", 5);
        assert_eq!(top.first().map(|(u, _)| u.as_str()), Some("a"));
        assert_eq!(top.len(), 2);
        // Unknown kind -> empty.
        assert!(top_uids_by_kind(&db_path, "bogus", 5).is_empty());
    }

    #[test]
    fn event_type_display_is_stable() {
        assert_eq!(EventType::TerminalSuccess.to_string(), "terminal_success");
        assert_eq!(EventType::FollowUp.to_string(), "follow_up");
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
