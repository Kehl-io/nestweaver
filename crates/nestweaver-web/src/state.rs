use nestweaver_store::{GraphStore, TantivyIndex};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct GraphEvent {
    pub event_type: String,
    pub payload: serde_json::Value,
}

pub struct AppState {
    pub store: Arc<GraphStore>,
    pub tantivy: Option<Arc<TantivyIndex>>,
    pub event_tx: broadcast::Sender<GraphEvent>,
    pub db_path: PathBuf,
    pub file_lock: Mutex<()>,
}

impl AppState {
    pub fn new(store: GraphStore, tantivy: Option<TantivyIndex>, db_path: PathBuf) -> Arc<Self> {
        let (event_tx, _) = broadcast::channel(256);
        Arc::new(Self {
            store: Arc::new(store),
            tantivy: tantivy.map(Arc::new),
            event_tx,
            db_path,
            file_lock: Mutex::new(()),
        })
    }

    pub fn new_with_store(
        store: Arc<GraphStore>,
        tantivy: Option<TantivyIndex>,
        db_path: PathBuf,
    ) -> Arc<Self> {
        let (event_tx, _) = broadcast::channel(256);
        Arc::new(Self {
            store,
            tantivy: tantivy.map(Arc::new),
            event_tx,
            db_path,
            file_lock: Mutex::new(()),
        })
    }

    pub fn new_with_arc_tantivy(
        store: Arc<GraphStore>,
        tantivy: Option<Arc<TantivyIndex>>,
        db_path: PathBuf,
    ) -> Arc<Self> {
        let (event_tx, _) = broadcast::channel(256);
        Arc::new(Self {
            store,
            tantivy,
            event_tx,
            db_path,
            file_lock: Mutex::new(()),
        })
    }
}

/// Shared state for admin API routes. Provides access to daemon-level
/// resources (store, queue depth, drain state) that the admin API needs.
pub struct AdminState {
    pub admin_token: String,
    pub daemon_store: Arc<GraphStore>,
    pub instance_id: String,
    pub start_time: Instant,
    pub active_reads: Arc<AtomicU32>,
    pub active_writes: Arc<AtomicU32>,
    pub drained: Arc<AtomicBool>,
    pub indexing_queue_depth: Arc<AtomicU32>,
    /// Path to the brain database, used to derive the jobs database path.
    pub db_path: std::path::PathBuf,
    /// Path to instance.toml for hot-reload. `None` when no config was supplied.
    pub config_path: Option<std::path::PathBuf>,
    /// Channel to send commands to the live poll scheduler. `None` when no
    /// scheduler is running (non-server mode or no admin token).
    pub scheduler_tx:
        Option<tokio::sync::mpsc::Sender<nestweaver_engine::scheduler::SchedulerCommand>>,
    /// Webhook allowed repos set, shared with the webhook handler via RwLock.
    /// Reload updates this so new repos are accepted without restart.
    pub webhook_allowed_repos:
        Option<Arc<std::sync::RwLock<Option<std::collections::HashSet<String>>>>>,
    /// Webhook per-repo branch map, shared with the webhook handler via RwLock.
    pub webhook_repo_branches:
        Option<Arc<std::sync::RwLock<std::collections::HashMap<String, String>>>>,
}
