use nestweaver_store::{GraphStore, TantivyIndex};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
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
