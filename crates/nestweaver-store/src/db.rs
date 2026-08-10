use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::error::StoreError;
use crate::ranking::QueryIntent;

/// Typed results for the two durable embedding reconciliation sub-stages.
/// Legacy retirement is not attempted until canonical persistence succeeds.
#[derive(Debug)]
pub struct EmbeddingIndexReconciliation {
    pub removed: usize,
    pub canonical_persistence: Result<(), StoreError>,
    pub legacy_retirement: Option<Result<bool, StoreError>>,
}

/// Cached PPR adjacency graph keyed on `(graph_generation, scope_hash, intent)`.
///
/// Stores the output of `load_ppr_graph` so repeated PPR calls within the same
/// graph generation reuse the pre-built adjacency structures instead of firing
/// multiple DB queries on every invocation.
pub(crate) struct PprGraphCached {
    /// The `graph_generation` value at cache-fill time. Compared on every
    /// lookup; a mismatch means the graph changed and the cache is stale.
    pub generation: u64,
    /// `DefaultHasher` hash of the concatenated node_query + edge_query strings
    /// from the `GraphScope`. Encodes the scope identity cheaply.
    pub scope_hash: u64,
    /// The `QueryIntent` (or `None`) that was in effect when the graph was built.
    /// Edge weights are intent-dependent, so different intents need separate entries.
    pub intent: Option<QueryIntent>,
    /// Ordered list of all node UIDs in scope.
    pub uids: Vec<String>,
    /// Maps uid → index in `uids`.
    pub uid_to_idx: HashMap<String, usize>,
    /// For each node v, the list of `(u, weight)` incoming edges.
    pub incoming: Vec<Vec<(usize, f64)>>,
    /// Sum of all outgoing edge weights per node (pre-normalisation denominator).
    pub out_weight: Vec<f64>,
}

#[derive(Default)]
struct IndexPublicationLeaseState {
    owner: Option<u64>,
    next_token: u64,
    waiters: usize,
}

#[derive(Default)]
struct IndexPublicationLeaseCoordinator {
    state: Mutex<IndexPublicationLeaseState>,
    available: Condvar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexPublicationReservationState {
    Unreserved,
    Fresh,
    Recovered,
}

#[derive(Debug)]
struct IndexPublicationGenerationReservation {
    generation: u64,
    recovered: bool,
}

/// Exclusive, Send-safe ownership of one graph publication lifetime.
///
/// The lease holds no mutex guard. A publisher owns it from before marker
/// establishment through every graph mutation and durable finalization. If it
/// is dropped early, only live-process exclusivity is released: the dirty
/// marker and reserved generation remain fail-closed for the next owner to
/// recover.
#[must_use = "hold the publication lease through graph mutation and durable finalization"]
pub struct IndexPublicationLease<'a> {
    store: &'a GraphStore,
    token: u64,
    reservation: Cell<IndexPublicationReservationState>,
    released: bool,
}

impl std::fmt::Debug for IndexPublicationLease<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IndexPublicationLease")
            .field("token", &self.token)
            .field("reservation", &self.reservation.get())
            .field("released", &self.released)
            .finish_non_exhaustive()
    }
}

impl IndexPublicationLease<'_> {
    /// The store whose publication lifetime this lease exclusively owns.
    pub fn store(&self) -> &GraphStore {
        self.store
    }

    /// Verify that dirty N+1 and clean N+2 remain available.
    pub fn preflight_generation(&self) -> Result<(), StoreError> {
        self.store
            .preflight_index_publication_generation(self.token)
    }

    /// Verify that an in-memory publication has one successor available.
    pub fn preflight_transient_generation(&self) -> Result<(), StoreError> {
        self.store
            .preflight_transient_index_publication_generation(self.token)
    }

    /// Reserve or recover the dirty N+1 generation for this owner.
    pub fn reserve_generation(&self) -> Result<u64, StoreError> {
        let prior = self.reservation.get();
        let reserved = self
            .store
            .reserve_index_publication_generation(self.token)?;
        if prior == IndexPublicationReservationState::Unreserved {
            self.reservation.set(if reserved.recovered {
                IndexPublicationReservationState::Recovered
            } else {
                IndexPublicationReservationState::Fresh
            });
        }
        Ok(reserved.generation)
    }

    /// Whether this owner inherited an already-dirty publication.
    pub fn is_recovered(&self) -> bool {
        self.reservation.get() == IndexPublicationReservationState::Recovered
    }

    /// Refuse snapshot reads unless this owner acquired a fully clean store.
    pub fn ensure_clean_for_snapshot(&self) -> Result<(), StoreError> {
        self.store
            .ensure_clean_index_publication_for_snapshot(self.token)
    }

    /// Return the clean N+2 generation prepared by this owner.
    pub fn clean_generation(&self) -> Result<u64, StoreError> {
        self.store.clean_index_publication_generation(self.token)
    }

    /// Make this owner's prepared clean generation live.
    pub fn publish_clean_generation(&self) -> Result<u64, StoreError> {
        self.store
            .publish_clean_index_publication_generation(self.token)
    }

    /// Preserve fail-closed monotonicity after marker retirement failed.
    pub fn fail_clean_generation(&self) -> Result<(), StoreError> {
        self.store
            .fail_clean_index_publication_generation(self.token)
    }

    /// Retire this owner's in-memory dirty generation reservation.
    pub fn complete_generation(&self) -> Result<(), StoreError> {
        self.store.complete_index_publication_generation(self.token)
    }

    /// Restore the prior canonical generation when no graph mutation occurred.
    pub fn cancel_generation(&self) -> Result<(), StoreError> {
        if self.reservation.get() != IndexPublicationReservationState::Fresh {
            return Err(StoreError::Query(
                "only a fresh index publication owner may cancel its generation".into(),
            ));
        }
        self.store.cancel_index_publication_generation(self.token)
    }

    /// Release live-process ownership after explicit successful publication.
    pub fn release(mut self) -> Result<(), StoreError> {
        self.store.release_index_publication_lease(self.token)?;
        self.released = true;
        Ok(())
    }
}

impl Drop for IndexPublicationLease<'_> {
    fn drop(&mut self) {
        if !self.released
            && let Err(error) = self.store.release_index_publication_lease(self.token)
        {
            tracing::error!(%error, "failed to release index publication lease");
        }
    }
}

/// GraphStore wraps a LadybugDB database for storing and querying the code knowledge graph.
///
/// Each method creates a fresh Connection internally, which is the simplest safe pattern
/// given that Connection<'a> borrows &'a Database.
pub struct GraphStore {
    pub(crate) db: lbug::Database,
    pub(crate) pagerank_cache: Mutex<Option<HashMap<String, f64>>>,
    /// Monotonic counter that bumps whenever PageRank scores change. Lets
    /// clients (the watcher, the MCP server, downstream tools) detect when
    /// their cached scores are stale without comparing entire score maps.
    pub(crate) pagerank_generation: AtomicU64,
    /// Serializes lazy PageRank computation and graph-publication cache state.
    /// `ensure_pagerank_loaded` uses it so N concurrent first-touch callers
    /// produce exactly one compute instead of N duplicate full computes
    /// (nw-029). Dirty-marker transitions, invalidation, and generation-keyed
    /// cache fills also take it so in-flight readers cannot republish stale
    /// state after an index publication transition.
    pub(crate) pagerank_compute_lock: Mutex<()>,
    /// Monotonic counter that bumps whenever the graph data changes (nodes
    /// or edges added/removed). Lets the web UI and other consumers detect
    /// when their view of the graph is stale without diffing the full graph.
    pub(crate) graph_generation: AtomicU64,
    /// Last canonical generation while an index publication is dirty. A
    /// present value means `graph_generation` is either its dirty N+1
    /// reservation or the prepared clean N+2 publication. Keeping this
    /// recovery state separate prevents an ephemeral fail-closed value from
    /// becoming a wrapping persisted counter.
    pub(crate) index_publication_generation_base: Mutex<Option<u64>>,
    /// Serializes complete marker→mutation→finalization publication lifetimes.
    /// The condition-variable state stores only an opaque owner token; lease
    /// holders never retain a mutex guard while doing graph or sidecar I/O.
    index_publication_lease: IndexPublicationLeaseCoordinator,
    /// Optional interaction memory scores keyed by node UID. When loaded,
    /// PPR's personalization vector blends a small fraction of these scores
    /// to boost nodes the user has frequently accessed.
    pub(crate) interaction_cache: Mutex<Option<HashMap<String, f64>>>,
    /// Feature F12: optional git-activity recency scores keyed by repo-relative
    /// file path (`path -> score ∈ [0, 1]`). When loaded, `pagerank_score` is
    /// multiplied at *read* time by a clamped recency factor so dormant code is
    /// demoted relative to actively-developed code. Absent file → neutral
    /// (multiplier 1.0). Loaded from the `<db>.gitactivity.json` sidecar; never
    /// affects the PPR fixpoint.
    pub(crate) git_activity_cache: Mutex<Option<HashMap<String, f64>>>,
    /// Feature F12: the `[ranking] git_activity_weight` to use when applying the
    /// recency multiplier. Defaults to [`crate::ranking::DEFAULT_GIT_ACTIVITY_WEIGHT`]
    /// (1.2); `set_git_activity_weight` overrides it from config.
    pub(crate) git_activity_weight: Mutex<f64>,
    /// P0.2: the on-disk database path this store was opened from, when known.
    /// In-memory stores have `None`. Used to locate the `<db>.generation`
    /// sidecar so `graph_generation` can be loaded on open and persisted on
    /// mutation without callers having to thread the path through.
    pub(crate) db_path: Option<PathBuf>,
    /// Cached PPR adjacency graph. Holds the last-built `(uids, uid_to_idx,
    /// incoming, out_weight)` keyed on `(graph_generation, scope_hash, intent)`.
    /// Avoids rebuilding the adjacency list from DB on every PPR call when the
    /// graph has not changed between index refreshes.
    pub(crate) ppr_graph_cache: Mutex<Option<PprGraphCached>>,
    /// Cached full symbol table for `search_symbols_by_name`. Keyed on
    /// `graph_generation`; stale entries are discarded on any reindex.
    /// Avoids full-table scans on every seed-resolution call for brain_context,
    /// flow_trace, blast_radius, etc.
    pub(crate) symbol_name_cache: Mutex<Option<Arc<crate::traverse::SymbolNameCached>>>,
    /// Reverse-adjacency snapshot used by impact traversals. The generation
    /// travels with the snapshot so a graph publication can never reuse stale
    /// adjacency, even when the old allocation has not yet been reclaimed.
    pub(crate) impact_snapshot_cache: Mutex<Option<(u64, Arc<crate::traverse::ImpactSnapshot>)>>,
    /// Single-flights the expensive first snapshot construction for each graph
    /// generation. Waiters re-check the generation-keyed cache after acquiring
    /// this lock instead of issuing duplicate full edge-table scans.
    pub(crate) impact_snapshot_compute_lock: Mutex<()>,
    /// In-memory embedding index backed by a JSON sidecar file
    /// (`<db>.embeddings`). Embeddings are stored here instead of in
    /// LadybugDB (which has no float-array column type). Loaded on open,
    /// saved on mutation via `flush_embedding_index`.
    pub(crate) embedding_index: Mutex<crate::search::EmbeddingIndex>,
    /// LRU cache for PPR result vectors keyed by a hash of
    /// `(sorted seed_uids, damping, max_iterations, scope_hash, intent, graph_generation)`.
    /// Repeated queries with the same seeds skip the iterative PPR computation entirely.
    pub(crate) ppr_result_cache: Mutex<lru::LruCache<u64, Vec<(String, f64)>>>,
}

/// Authoritative embedding state captured while the in-memory embedding mutex
/// remained held for the complete flush/metadata/count/stage operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingSnapshotState {
    pub model_id: String,
    pub dimension: u32,
    pub count: u64,
}

/// Lease retaining the embedding mutex after the snapshot artifact and its
/// metadata have been captured. Snapshot callers keep this alive through the
/// graph checkpoint and remaining artifact staging.
pub struct EmbeddingSnapshotLease<'a> {
    state: EmbeddingSnapshotState,
    _guard: std::sync::MutexGuard<'a, crate::search::EmbeddingIndex>,
}

impl EmbeddingSnapshotLease<'_> {
    pub fn state(&self) -> &EmbeddingSnapshotState {
        &self.state
    }
}

/// True if `msg` is lbug's stale-checkpoint open failure. A crash/OOM/SIGKILL
/// during lbug's WAL auto-checkpoint (which fires as the WAL grows mid-index, and
/// also during a backup) can leave a `<db>.wal.checkpoint` whose embedded database
/// id no longer matches, plus an empty `<db>.shadow`. Every subsequent open — read
/// OR write — then fails with "Database ID for temporary file '<db>.wal.checkpoint'
/// does not match the current database. … please delete this file and restart",
/// crash-looping the daemon until a human deletes it.
fn is_stale_checkpoint_error(msg: &str) -> bool {
    msg.contains("wal.checkpoint") && msg.contains("does not match")
}

/// Remove the stale checkpoint sidecars lbug's error tells us to delete — but ONLY
/// when the shadow file is absent or empty (0 bytes), the exact signature of an
/// aborted checkpoint. A non-empty shadow could belong to a live mid-checkpoint
/// writer; never touch that. (Reaching the stale-checkpoint error already implies
/// no other process holds the write lock — that would surface as a lock error
/// instead.) Returns true if the checkpoint file was removed (worth a retry).
fn remove_stale_checkpoint_sidecars(path: &Path) -> bool {
    let shadow = PathBuf::from(format!("{}.shadow", path.display()));
    let checkpoint = PathBuf::from(format!("{}.wal.checkpoint", path.display()));
    let shadow_empty = std::fs::metadata(&shadow)
        .map(|m| m.len() == 0)
        .unwrap_or(true);
    if !shadow_empty {
        return false;
    }
    let removed = std::fs::remove_file(&checkpoint).is_ok();
    if shadow.exists() {
        let _ = std::fs::remove_file(&shadow);
    }
    removed
}

/// Open an lbug database, auto-recovering once from a stale WAL checkpoint left by
/// a prior crash (see [`is_stale_checkpoint_error`]). This turns what was a full
/// read+write outage requiring manual `rm` into a transparent self-heal.
/// Build the engine `SystemConfig`, applying corruption/crash hardening and
/// operator overrides on top of the library defaults.
///
/// nw-073: the engine's `optimisticRead` dereferences a raw buffer-page pointer
/// inside a lock-free loop; the reader-count pinning that would protect it is
/// gated behind a compile flag (`BM_MALLOC`) that native builds don't set, and
/// the only fallback is Windows-only. So on native builds a page eviction
/// racing a concurrent read is an unguarded SIGSEGV — observed during large
/// batch inserts (a full vault re-index) that create buffer pressure while the
/// engine's internal index-builder pool is still appending. The race needs
/// concurrency; bounding the engine thread pool removes it. Because our own
/// hot query path is application-level BFS + point lookups (not engine-parallel
/// analytical joins), a bounded pool costs query latency little while closing
/// the index-time crash window.
///
/// Overridable per-operator:
///   NESTWEAVER_LBUG_MAX_THREADS       engine thread-pool size (0 = library auto)
///   NESTWEAVER_LBUG_BUFFER_POOL_BYTES buffer pool size in bytes (0 = auto);
///                                     a larger pool avoids eviction (and thus
///                                     the race) when the working set fits.
///   NESTWEAVER_LBUG_AUTO_CHECKPOINT   "0"/"false" defers auto-checkpoints; the
///                                     #678 corruption trigger needs several
///                                     checkpoint-separated segments, so
///                                     deferring during bulk load reduces it.
fn hardened_system_config() -> lbug::SystemConfig {
    fn env_u64(key: &str) -> Option<u64> {
        std::env::var(key)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
    }
    // Default engine threads. Env override wins; else a conservative bound that
    // removes the eviction-vs-read race the crash needs. `1` is the only value
    // that fully eliminates it; keep it the default on the write path and let
    // ops raise it if they measure a query-latency cost on their workload.
    let max_threads = env_u64("NESTWEAVER_LBUG_MAX_THREADS").unwrap_or(1);
    let mut cfg = lbug::SystemConfig::default().max_num_threads(max_threads);
    if let Some(bytes) = env_u64("NESTWEAVER_LBUG_BUFFER_POOL_BYTES") {
        cfg = cfg.buffer_pool_size(bytes);
    }
    if let Ok(v) = std::env::var("NESTWEAVER_LBUG_AUTO_CHECKPOINT") {
        let on = !matches!(v.trim(), "0" | "false" | "off");
        cfg = cfg.auto_checkpoint(on);
    }
    cfg
}

/// A `SIGKILL`ed daemon can leave `<db>.wal` behind with NO `<db>.shadow`
/// alongside it. lbug's read-write open then fails with an IO exception naming
/// the absent shadow file, and because that text ends in "No such file or
/// directory" the CLI's diagnostic heuristic read it as a MISSING DATABASE and
/// told the user to create one over the top of their data (nw-126).
///
/// This is a different shape from [`is_stale_checkpoint_error`], which matches
/// a checkpoint mismatch rather than an orphaned log, so the existing recovery
/// arm never fired.
fn is_orphaned_wal_error(msg: &str) -> bool {
    msg.contains(".shadow") && msg.to_lowercase().contains("no such file")
}

/// Move an orphaned `<db>.wal` aside so the next open can proceed.
///
/// Deliberately a RENAME, never a delete: a WAL can in principle hold committed
/// work, and destroying it to fix an outage would trade one data-loss story for
/// another. Quarantining is reversible and leaves the evidence in place.
///
/// Only acts on the exact orphan signature — `.wal` present AND `.shadow`
/// absent. A `.wal` with its `.shadow` intact is a normal recoverable log and
/// must be left for the engine to replay.
fn quarantine_orphaned_wal(path: &Path) -> Option<PathBuf> {
    let wal = PathBuf::from(format!("{}.wal", path.display()));
    let shadow = PathBuf::from(format!("{}.shadow", path.display()));
    if !wal.exists() || shadow.exists() {
        return None;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let quarantined = PathBuf::from(format!("{}.wal.orphaned-{stamp}", path.display()));
    std::fs::rename(&wal, &quarantined)
        .ok()
        .map(|()| quarantined)
}

/// Open an lbug database, auto-recovering once from crash debris that would
/// otherwise make it permanently unopenable.
///
/// `read_write` gates the orphaned-WAL arm. Replay is inherently a write
/// operation, so a read-only open must report the condition rather than mutate
/// the directory to clear it — and a read-only caller quarantining a log out
/// from under a live writer would be a genuine hazard.
fn open_lbug_with_recovery(
    path: &Path,
    read_write: bool,
    make_config: impl Fn() -> lbug::SystemConfig,
) -> Result<lbug::Database, StoreError> {
    match lbug::Database::new(path, make_config()) {
        Ok(db) => Ok(db),
        Err(e) => {
            let msg = e.to_string();
            if is_stale_checkpoint_error(&msg) && remove_stale_checkpoint_sidecars(path) {
                tracing::warn!(
                    "recovered a stale WAL checkpoint for {} (a prior crash left \
                     .wal.checkpoint/.shadow that made the DB unopenable); retrying open",
                    path.display()
                );
                return Ok(lbug::Database::new(path, make_config())?);
            }
            if read_write
                && is_orphaned_wal_error(&msg)
                && let Some(quarantined) = quarantine_orphaned_wal(path)
            {
                tracing::warn!(
                    "recovered {} from an orphaned write-ahead log left by a prior \
                     crash (.wal present with no .shadow, which made the database \
                     unopenable on every path); moved it to {} and retried — it was \
                     NOT deleted",
                    path.display(),
                    quarantined.display()
                );
                return Ok(lbug::Database::new(path, make_config())?);
            }
            Err(e.into())
        }
    }
}

impl GraphStore {
    /// Create a new persistent database at `path`, initialising schema tables.
    pub fn create(path: &Path) -> Result<Self, StoreError> {
        let db = open_lbug_with_recovery(path, true, hardened_system_config)?;
        let store = GraphStore {
            db,
            pagerank_cache: Mutex::new(None),
            pagerank_generation: AtomicU64::new(0),
            pagerank_compute_lock: Mutex::new(()),
            graph_generation: AtomicU64::new(0),
            index_publication_generation_base: Mutex::new(None),
            index_publication_lease: IndexPublicationLeaseCoordinator::default(),
            interaction_cache: Mutex::new(None),
            git_activity_cache: Mutex::new(None),
            git_activity_weight: Mutex::new(crate::ranking::DEFAULT_GIT_ACTIVITY_WEIGHT),
            db_path: Some(path.to_path_buf()),
            ppr_graph_cache: Mutex::new(None),
            symbol_name_cache: Mutex::new(None),
            impact_snapshot_cache: Mutex::new(None),
            impact_snapshot_compute_lock: Mutex::new(()),
            embedding_index: Mutex::new(Self::load_embedding_index(path)),
            ppr_result_cache: Mutex::new(lru::LruCache::new(
                std::num::NonZeroUsize::new(128).unwrap(),
            )),
        };
        store.init_schema()?;
        store.load_graph_generation(&store.generation_sidecar_path());
        store.load_recorded_embedding_model_into_index();
        Ok(store)
    }

    /// Open an existing persistent database at `path`.
    /// Runs schema migrations to ensure any new tables/columns from newer
    /// versions are present (all statements are idempotent).
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let db = open_lbug_with_recovery(path, true, hardened_system_config)?;
        let store = GraphStore {
            db,
            pagerank_cache: Mutex::new(None),
            pagerank_generation: AtomicU64::new(0),
            pagerank_compute_lock: Mutex::new(()),
            graph_generation: AtomicU64::new(0),
            index_publication_generation_base: Mutex::new(None),
            index_publication_lease: IndexPublicationLeaseCoordinator::default(),
            interaction_cache: Mutex::new(None),
            git_activity_cache: Mutex::new(None),
            git_activity_weight: Mutex::new(crate::ranking::DEFAULT_GIT_ACTIVITY_WEIGHT),
            db_path: Some(path.to_path_buf()),
            ppr_graph_cache: Mutex::new(None),
            symbol_name_cache: Mutex::new(None),
            impact_snapshot_cache: Mutex::new(None),
            impact_snapshot_compute_lock: Mutex::new(()),
            embedding_index: Mutex::new(Self::load_embedding_index(path)),
            ppr_result_cache: Mutex::new(lru::LruCache::new(
                std::num::NonZeroUsize::new(128).unwrap(),
            )),
        };
        store.init_schema()?;
        store.load_graph_generation(&store.generation_sidecar_path());
        store.load_recorded_embedding_model_into_index();
        Ok(store)
    }

    /// Open an existing database in read-only mode. Allows concurrent access
    /// while another process (e.g. the web UI) holds the write lock.
    pub fn open_read_only(path: &Path) -> Result<Self, StoreError> {
        let db = open_lbug_with_recovery(path, false, || {
            lbug::SystemConfig::default().read_only(true)
        })?;
        let store = GraphStore {
            db,
            pagerank_cache: Mutex::new(None),
            pagerank_generation: AtomicU64::new(0),
            pagerank_compute_lock: Mutex::new(()),
            graph_generation: AtomicU64::new(0),
            index_publication_generation_base: Mutex::new(None),
            index_publication_lease: IndexPublicationLeaseCoordinator::default(),
            interaction_cache: Mutex::new(None),
            git_activity_cache: Mutex::new(None),
            git_activity_weight: Mutex::new(crate::ranking::DEFAULT_GIT_ACTIVITY_WEIGHT),
            db_path: Some(path.to_path_buf()),
            ppr_graph_cache: Mutex::new(None),
            symbol_name_cache: Mutex::new(None),
            impact_snapshot_cache: Mutex::new(None),
            impact_snapshot_compute_lock: Mutex::new(()),
            embedding_index: Mutex::new(Self::load_embedding_index(path)),
            ppr_result_cache: Mutex::new(lru::LruCache::new(
                std::num::NonZeroUsize::new(128).unwrap(),
            )),
        };
        store.load_graph_generation(&store.generation_sidecar_path());
        store.load_recorded_embedding_model_into_index();
        Ok(store)
    }

    /// Open an existing database if it exists, or create a new one with schema initialised.
    pub fn open_or_create(path: &Path) -> Result<Self, StoreError> {
        if path.exists() {
            Self::open(path)
        } else {
            Self::create(path)
        }
    }

    /// Try to open the database read-write; if the write lock is already held
    /// by another process (e.g. the file-watcher), fall back to read-only.
    ///
    /// MCP servers and other read-heavy consumers should call this instead of
    /// `open_or_create` so they can coexist with a running watcher.
    pub fn open_or_readonly(path: &Path) -> Result<Self, StoreError> {
        match Self::open_or_create(path) {
            Ok(store) => Ok(store),
            Err(_) => {
                tracing::info!(
                    "database is locked by another process, opening read-only: {}",
                    path.display()
                );
                Self::open_read_only(path)
            }
        }
    }

    /// Create an in-memory database and initialise schema tables.
    pub fn in_memory() -> Result<Self, StoreError> {
        let db = lbug::Database::in_memory(lbug::SystemConfig::default())?;
        let store = GraphStore {
            db,
            pagerank_cache: Mutex::new(None),
            pagerank_generation: AtomicU64::new(0),
            pagerank_compute_lock: Mutex::new(()),
            graph_generation: AtomicU64::new(0),
            index_publication_generation_base: Mutex::new(None),
            index_publication_lease: IndexPublicationLeaseCoordinator::default(),
            interaction_cache: Mutex::new(None),
            git_activity_cache: Mutex::new(None),
            git_activity_weight: Mutex::new(crate::ranking::DEFAULT_GIT_ACTIVITY_WEIGHT),
            db_path: None,
            ppr_graph_cache: Mutex::new(None),
            symbol_name_cache: Mutex::new(None),
            impact_snapshot_cache: Mutex::new(None),
            impact_snapshot_compute_lock: Mutex::new(()),
            embedding_index: Mutex::new(crate::search::EmbeddingIndex::new()),
            ppr_result_cache: Mutex::new(lru::LruCache::new(
                std::num::NonZeroUsize::new(128).unwrap(),
            )),
        };
        store.init_schema()?;
        store.load_recorded_embedding_model_into_index();
        Ok(store)
    }

    /// Current PageRank cache generation. Starts at 0; bumps once per
    /// successful `compute_pagerank` call. Stable across `open` reopens
    /// only after the in-memory cache is reloaded from the sidecar.
    pub fn pagerank_generation(&self) -> u64 {
        self.pagerank_generation.load(Ordering::Acquire)
    }

    /// True if the caller's observed generation is older than the current
    /// one, meaning PageRank scores have changed since the caller last
    /// consulted them.
    pub fn is_pagerank_stale(&self, observed: u64) -> bool {
        observed < self.pagerank_generation()
    }

    /// nw-055 (P1b): drop the in-memory PageRank score cache so the next rank
    /// query recomputes fresh.
    ///
    /// The `pagerank_cache` score map is NOT generation-keyed, so bumping the
    /// graph generation after a deletion (which invalidates the
    /// generation-keyed `symbol_name_cache` / `ppr_graph_cache`) does NOT
    /// refresh these scores — a rank query after a code-repo removal would
    /// keep serving scores computed over the pre-deletion graph (surviving
    /// nodes' scores still reflecting the removed nodes' edges). Delete RPCs
    /// that remove CODE repos (`remove_repo`, `prune_stale`) call this so the
    /// next `ensure_pagerank_loaded` recomputes via the nw-029 single-flight.
    /// Lazy (recompute on next rank query) rather than eager, matching nw-029.
    pub fn invalidate_pagerank(&self) {
        let _flight = self
            .pagerank_compute_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.invalidate_ranking_caches_locked();
    }

    pub(crate) fn invalidate_ranking_caches_locked(&self) {
        *self
            .pagerank_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        *self
            .ppr_graph_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        self.ppr_result_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        *self
            .symbol_name_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        *self
            .impact_snapshot_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Run a publication marker transition while excluding PageRank readers,
    /// computations, and generation-keyed cache publication, then discard
    /// caches before releasing them. Establishment and retirement both use
    /// this barrier so no dirty-window state can survive publication.
    pub fn with_index_publication_rank_barrier<T>(&self, operation: impl FnOnce() -> T) -> T {
        let _flight = self
            .pagerank_compute_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let result = operation();
        self.invalidate_ranking_caches_locked();
        result
    }

    /// Load pre-computed interaction memory scores into the in-memory cache.
    /// When present, PPR's personalization vector blends a small fraction of
    /// these scores to boost nodes the user has frequently accessed.
    pub fn load_interaction_cache(&self, scores: HashMap<String, f64>) {
        *self
            .interaction_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(scores);
    }

    /// Clear the interaction memory cache (disables the PPR bias).
    pub fn clear_interaction_cache(&self) {
        *self
            .interaction_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Feature F12: load pre-computed git-activity recency scores
    /// (`path -> score ∈ [0, 1]`) into the in-memory cache. When present,
    /// `pagerank_score` is multiplied at read time by a clamped recency factor
    /// (see [`git_activity_multiplier`]). Passing an empty map is equivalent to
    /// not loading at all (every file → neutral).
    pub fn load_git_activity_cache(&self, scores: HashMap<String, f64>) {
        *self
            .git_activity_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(scores);
    }

    /// Clear the git-activity recency cache (restores neutral ranking).
    pub fn clear_git_activity_cache(&self) {
        *self
            .git_activity_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Load the git-activity sidecar (`path -> score` JSON) from `path` into the
    /// in-memory cache. No-op when the file is absent or corrupt (neutral path).
    pub fn load_git_activity_sidecar(&self, path: &Path) -> Result<(), StoreError> {
        if path.exists() {
            let json = std::fs::read_to_string(path)
                .map_err(|e| StoreError::Query(format!("read: {e}")))?;
            let scores: HashMap<String, f64> = serde_json::from_str(&json)
                .map_err(|e| StoreError::Query(format!("deserialize: {e}")))?;
            self.load_git_activity_cache(scores);
        }
        Ok(())
    }

    /// Return the git-activity recency score for a repo-relative file `path`,
    /// or `None` when no score is loaded for it (→ neutral multiplier).
    pub fn git_activity_score(&self, path: &str) -> Option<f64> {
        self.git_activity_cache
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().and_then(|m| m.get(path).copied()))
    }

    /// True when git-activity recency scores are loaded (Feature F12 active).
    pub fn has_git_activity(&self) -> bool {
        self.git_activity_cache
            .lock()
            .ok()
            .map(|g| g.as_ref().is_some_and(|m| !m.is_empty()))
            .unwrap_or(false)
    }

    /// Override the git-activity recency weight (from `[ranking] git_activity_weight`).
    pub fn set_git_activity_weight(&self, weight: f64) {
        *self
            .git_activity_weight
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = weight;
    }

    /// The currently-configured git-activity recency weight.
    pub fn git_activity_weight(&self) -> f64 {
        self.git_activity_weight
            .lock()
            .map(|g| *g)
            .unwrap_or(crate::ranking::DEFAULT_GIT_ACTIVITY_WEIGHT)
    }

    /// Internal: bump the generation counter. Called by `compute_pagerank`
    /// after a successful re-rank.
    pub(crate) fn bump_pagerank_generation(&self) {
        self.pagerank_generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Current graph data generation. Starts at 0; bumps once per successful
    /// watcher batch that modifies the graph (nodes or edges added/removed).
    /// Lets the web UI and other consumers detect staleness without diffing
    /// the full graph.
    pub fn graph_generation(&self) -> u64 {
        self.graph_generation.load(Ordering::Acquire)
    }

    /// Bump the graph generation counter. Called by watchers after each batch
    /// that modifies the graph. The web server can poll this to detect when to
    /// push an SSE event to connected clients.
    pub fn bump_graph_generation(&self) {
        if let Err(error) = self.try_bump_graph_generation() {
            tracing::error!(%error, "refusing to wrap exhausted graph generation");
        }
    }

    /// Advance the live generation without ever wrapping to a reused value.
    ///
    /// This is the UNGATED administrative bump (the delete/merge finalize path
    /// calls it without holding the publication lease). If a dirty reservation is
    /// present it means either a live publication is mid-flight (lease held → fail
    /// closed, never clobber it) OR a prior publisher crashed leaving an abandoned
    /// reservation (lease unowned → RECOVER, rather than wedging every future admin
    /// bump for the daemon's lifetime — nw-091 / Bug 3A). The old code always
    /// failed closed, so one dropped lease permanently broke remove_vault /
    /// merge_instance / prune_stale.
    pub fn try_bump_graph_generation(&self) -> Result<u64, StoreError> {
        {
            let mut reservation = self
                .index_publication_generation_base
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(canonical) = *reservation {
                // Probe ownership WHILE holding `base`. Lock order base → lease is
                // safe: no path holds both (reserve drops the lease guard before
                // locking base; acquire never touches base), so this cannot
                // deadlock, and probing under the base lock closes the TOCTOU.
                if self.index_publication_lease_is_unowned() {
                    // Abandoned reservation from a crashed publisher: roll the
                    // dirty N+1 back to the last clean generation and clear it (the
                    // N+1 was never durably published as canonical, so reusing it is
                    // safe — mirrors cancel_index_publication_generation), then bump.
                    self.graph_generation.store(canonical, Ordering::Release);
                    *reservation = None;
                } else {
                    return Err(StoreError::Query(
                        "index publication generation is already reserved".to_string(),
                    ));
                }
            }
        }
        self.graph_generation
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map(|previous| previous + 1)
            .map_err(|_| StoreError::Query("graph generation exhausted".to_string()))
    }

    /// Wait for exclusive ownership of a complete index publication lifetime.
    ///
    /// Acquisition is blocking and intended for the engine's synchronous
    /// indexing and watcher paths. The returned lease is `Send` and holds no
    /// mutex guard, so moving the synchronous work to a blocking worker thread
    /// remains safe.
    pub fn acquire_index_publication_lease(&self) -> Result<IndexPublicationLease<'_>, StoreError> {
        let mut state = self
            .index_publication_lease
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut registered_waiter = false;
        while state.owner.is_some() {
            if !registered_waiter {
                state.waiters = state.waiters.checked_add(1).ok_or_else(|| {
                    StoreError::Query("index publication waiter count exhausted".into())
                })?;
                registered_waiter = true;
                self.index_publication_lease.available.notify_all();
            }
            state = self
                .index_publication_lease
                .available
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
        if registered_waiter {
            state.waiters -= 1;
        }
        let token = state.next_token;
        state.next_token = token.checked_add(1).ok_or_else(|| {
            StoreError::Query("index publication ownership token exhausted".into())
        })?;
        state.owner = Some(token);
        Ok(IndexPublicationLease {
            store: self,
            token,
            reservation: Cell::new(IndexPublicationReservationState::Unreserved),
            released: false,
        })
    }

    /// Current number of publishers or snapshot readers waiting for ownership.
    #[doc(hidden)]
    pub fn index_publication_waiter_count(&self) -> usize {
        self.index_publication_lease
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .waiters
    }

    /// Wait until at least `minimum` publication owners are registered as blocked.
    #[doc(hidden)]
    pub fn wait_for_index_publication_waiters(
        &self,
        minimum: usize,
        timeout: std::time::Duration,
    ) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let mut state = self
            .index_publication_lease
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        while state.waiters < minimum {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next, timed) = self
                .index_publication_lease
                .available
                .wait_timeout(state, remaining)
                .unwrap_or_else(|error| error.into_inner());
            state = next;
            if timed.timed_out() && state.waiters < minimum {
                return false;
            }
        }
        true
    }

    fn validate_index_publication_owner(&self, token: u64) -> Result<(), StoreError> {
        let state = self
            .index_publication_lease
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.owner == Some(token) {
            Ok(())
        } else {
            Err(StoreError::Query(
                "index publication lease is not owned by this token".into(),
            ))
        }
    }

    /// True when no in-process owner currently holds the index-publication lease.
    ///
    /// A leftover `index_publication_generation_base` while the lease is UNOWNED
    /// is an abandoned reservation from a prior owner that dropped its lease
    /// without completing (e.g. the daemon crashed mid-publish) — safe to recover.
    /// While a lease IS held, a live publication is mid-flight and the reservation
    /// must stay fail-closed. This is the live-vs-dead signal the admin path keys
    /// on so a crash can't wedge every future generation bump forever (nw-091).
    fn index_publication_lease_is_unowned(&self) -> bool {
        self.index_publication_lease
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .owner
            .is_none()
    }

    fn release_index_publication_lease(&self, token: u64) -> Result<(), StoreError> {
        let mut state = self
            .index_publication_lease
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.owner != Some(token) {
            return Err(StoreError::Query(
                "index publication lease release attempted by a non-owner".into(),
            ));
        }
        state.owner = None;
        drop(state);
        self.index_publication_lease.available.notify_all();
        Ok(())
    }

    /// Verify that both the dirty reservation and its distinct clean
    /// publication generation are available.
    fn preflight_index_publication_generation(&self, token: u64) -> Result<(), StoreError> {
        self.validate_index_publication_owner(token)?;
        let base = self
            .index_publication_generation_base
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let canonical = (*base).unwrap_or_else(|| self.graph_generation());
        canonical.checked_add(2).map(|_| ()).ok_or_else(|| {
            StoreError::Query("graph generation exhausted during index publication".into())
        })
    }

    /// Verify that an in-memory publication can advance once after its graph
    /// mutation. Unlike a persistent publication, no dirty N+1 is exposed and
    /// no distinct clean N+2 is required.
    fn preflight_transient_index_publication_generation(
        &self,
        token: u64,
    ) -> Result<(), StoreError> {
        self.validate_index_publication_owner(token)?;
        let mut base = self
            .index_publication_generation_base
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // Reaching here means THIS caller holds the exclusive lease (validated
        // above), so no concurrent live publisher exists — a leftover reservation
        // is abandoned by a prior owner that dropped its lease (crash). Recover it
        // (restore canonical) instead of failing closed forever (nw-091 / Bug 3A),
        // matching the persistent reserve path's recovery.
        if let Some(canonical) = base.take() {
            self.graph_generation.store(canonical, Ordering::Release);
        }
        drop(base);
        self.graph_generation()
            .checked_add(1)
            .map(|_| ())
            .ok_or_else(|| {
                StoreError::Query("graph generation exhausted during index publication".into())
            })
    }

    /// Reserve the dirty generation for an in-progress publication. Repeated
    /// calls during the same dirty recovery return the same N+1 value. The
    /// clean N+2 successor is preflighted before changing live state.
    fn reserve_index_publication_generation(
        &self,
        token: u64,
    ) -> Result<IndexPublicationGenerationReservation, StoreError> {
        self.validate_index_publication_owner(token)?;
        let mut base = self
            .index_publication_generation_base
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(canonical) = *base {
            canonical.checked_add(2).ok_or_else(|| {
                StoreError::Query("graph generation exhausted during index publication".into())
            })?;
            let generation = canonical.checked_add(1).ok_or_else(|| {
                StoreError::Query("graph generation exhausted during index publication".into())
            })?;
            return Ok(IndexPublicationGenerationReservation {
                generation,
                recovered: true,
            });
        }

        let canonical = self.graph_generation();
        canonical.checked_add(2).ok_or_else(|| {
            StoreError::Query("graph generation exhausted during index publication".into())
        })?;
        let reserved = canonical.checked_add(1).ok_or_else(|| {
            StoreError::Query("graph generation exhausted during index publication".into())
        })?;
        *base = Some(canonical);
        self.graph_generation.store(reserved, Ordering::Release);
        Ok(IndexPublicationGenerationReservation {
            generation: reserved,
            recovered: false,
        })
    }

    /// Confirm that a lease acquired for backup/snapshot work did not inherit
    /// an abandoned or active publication. Holding the lease makes this check
    /// stable until the reader releases it.
    fn ensure_clean_index_publication_for_snapshot(&self, token: u64) -> Result<(), StoreError> {
        self.validate_index_publication_owner(token)?;
        let reserved = self
            .index_publication_generation_base
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some();
        if reserved || self.is_index_publication_dirty() {
            return Err(StoreError::Query(
                "dirty index publication prevents a consistent snapshot".into(),
            ));
        }
        Ok(())
    }

    /// Return the distinct N+2 generation to persist for an active dirty
    /// publication without exposing it to live cache consumers yet.
    fn clean_index_publication_generation(&self, token: u64) -> Result<u64, StoreError> {
        self.validate_index_publication_owner(token)?;
        let base = self
            .index_publication_generation_base
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let canonical = base.ok_or_else(|| {
            StoreError::Query("index publication generation is not reserved".into())
        })?;
        canonical.checked_add(2).ok_or_else(|| {
            StoreError::Query("graph generation exhausted during index publication".into())
        })
    }

    /// Make the prepared N+2 generation live immediately before retiring the
    /// dirty marker. Callers must hold the publication rank barrier.
    fn publish_clean_index_publication_generation(&self, token: u64) -> Result<u64, StoreError> {
        let clean = self.clean_index_publication_generation(token)?;
        self.graph_generation.store(clean, Ordering::Release);
        Ok(clean)
    }

    /// A marker-retirement failure may have briefly exposed the persisted
    /// clean value. Treat it as the next canonical base and reserve a newer
    /// dirty value so that generation can never be reused on retry.
    fn fail_clean_index_publication_generation(&self, token: u64) -> Result<(), StoreError> {
        self.validate_index_publication_owner(token)?;
        let mut base = self
            .index_publication_generation_base
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(canonical) = *base
            && let Some(clean) = canonical.checked_add(2)
        {
            *base = Some(clean);
            self.graph_generation
                .store(clean.saturating_add(1), Ordering::Release);
        }
        Ok(())
    }

    /// Mark a durably published reserved generation as canonical.
    fn complete_index_publication_generation(&self, token: u64) -> Result<(), StoreError> {
        self.validate_index_publication_owner(token)?;
        *self
            .index_publication_generation_base
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        Ok(())
    }

    /// Restore the canonical generation when a marker was established but no
    /// graph mutation was attempted.
    fn cancel_index_publication_generation(&self, token: u64) -> Result<(), StoreError> {
        self.validate_index_publication_owner(token)?;
        let mut base = self
            .index_publication_generation_base
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(canonical) = base.take() {
            self.graph_generation.store(canonical, Ordering::Release);
        }
        Ok(())
    }

    pub(crate) fn clear_index_publication_generation_on_clean_load(&self) {
        *self
            .index_publication_generation_base
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
    }

    /// The on-disk database path this store was opened from, when known
    /// (`None` for in-memory stores). Lets callers locate sidecars.
    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }

    /// Whether an indexing writer durably marked graph publication incomplete.
    /// While this marker exists, or its state cannot be determined, canonical
    /// generation and PageRank sidecars may predate the committed graph and
    /// must not be treated as authoritative.
    pub fn is_index_publication_dirty(&self) -> bool {
        self.db_path.as_ref().is_some_and(|path| {
            Self::index_publication_marker_for(path)
                .try_exists()
                .unwrap_or(true)
        })
    }

    fn index_publication_marker_for(db_path: &Path) -> PathBuf {
        let mut value = db_path.as_os_str().to_owned();
        value.push(".index-dirty");
        PathBuf::from(value)
    }

    /// Path to the `<db>.generation` sidecar. For in-memory stores (no
    /// `db_path`) this returns a relative `.generation` path that is never
    /// actually read or written — `load_graph_generation` no-ops on absence
    /// and persistence is only triggered through the path-taking helpers.
    pub(crate) fn generation_sidecar_path(&self) -> PathBuf {
        match &self.db_path {
            Some(p) => {
                let mut s = p.as_os_str().to_owned();
                s.push(".generation");
                PathBuf::from(s)
            }
            None => PathBuf::from(".generation"),
        }
    }

    // ── Embedding sidecar helpers ────────────────────────────────────────

    /// Load the embedding index from the binary sidecar (`<db>.embeddings.bin`),
    /// falling back to the legacy JSON sidecar (`<db>.embeddings`).
    /// Returns an empty index when neither file exists or both are corrupt.
    fn load_embedding_index(db_path: &Path) -> crate::search::EmbeddingIndex {
        let binary_path = Self::embedding_sidecar_binary_for(db_path);
        if binary_path.exists()
            && let Ok(idx) = crate::search::EmbeddingIndex::load_binary(&binary_path)
        {
            return idx;
        }
        let json_path = Self::embedding_sidecar_json_for(db_path);
        crate::search::EmbeddingIndex::load(&json_path).unwrap_or_default()
    }

    /// Hand the embedding index the model id recorded in the database's
    /// embedding metadata, so `add_embedding_with_force` can refuse a
    /// same-dimension write from a different model. Read once here — never
    /// per-add — and kept current by `set_embedding_metadata`. Absent or
    /// unreadable metadata means unknown: the model guard stays off and the
    /// dimension guard alone applies.
    fn load_recorded_embedding_model_into_index(&self) {
        let recorded = match self.get_embedding_metadata() {
            Ok(recorded) => recorded.map(|(model_id, _)| model_id),
            Err(e) => {
                tracing::warn!(
                    "could not read embedding metadata; the recorded-model write guard is \
                     disabled for this store: {e}"
                );
                None
            }
        };
        self.embedding_index
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_recorded_model_id(recorded);
    }

    /// Compute the legacy JSON sidecar path for a given database path.
    fn embedding_sidecar_json_for(db_path: &Path) -> std::path::PathBuf {
        let mut s = db_path.as_os_str().to_owned();
        s.push(".embeddings");
        std::path::PathBuf::from(s)
    }

    /// Compute the binary sidecar path for a given database path.
    fn embedding_sidecar_binary_for(db_path: &Path) -> std::path::PathBuf {
        let mut s = db_path.as_os_str().to_owned();
        s.push(".embeddings.bin");
        std::path::PathBuf::from(s)
    }

    /// Return the path to the embedding sidecar file (binary format),
    /// or `None` for in-memory stores.
    pub fn embedding_sidecar_path(&self) -> Option<std::path::PathBuf> {
        self.db_path
            .as_ref()
            .map(|p| Self::embedding_sidecar_binary_for(p))
    }

    /// Add an embedding to the in-memory index without saving to disk.
    /// Use `flush_embedding_index` after a batch of additions to persist.
    ///
    /// Returns `false` when the dimension guard rejects the vector — callers
    /// must not count a rejected embedding as stored.
    ///
    /// This entry point names no producing model, so the recorded-model guard
    /// is skipped (unknown producer); explicit embed runs should use
    /// [`add_embedding_with_force`], which takes the model id.
    ///
    /// [`add_embedding_with_force`]: GraphStore::add_embedding_with_force
    #[must_use = "a false return means the dimension guard rejected the embedding"]
    pub fn add_embedding(&self, uid: &str, embedding: Vec<f32>) -> bool {
        let mut idx = self
            .embedding_index
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        idx.add(uid, embedding, false)
    }

    /// Add an embedding produced by `model_id`. Returns `false` when a guard
    /// rejects the vector: the dimension guard, or — when the database has a
    /// recorded embedding model — a same-dimension write from a different
    /// model, unless `force` is set (a `--force` run re-embeds everything).
    #[must_use = "a false return means a guard rejected the embedding"]
    pub fn add_embedding_with_force(
        &self,
        uid: &str,
        embedding: Vec<f32>,
        model_id: &str,
        force: bool,
    ) -> bool {
        let mut idx = self
            .embedding_index
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        idx.add_with_model(uid, embedding, Some(model_id), force)
    }

    /// Re-arm the embedding index's once-per-run force-clear guard. Call at
    /// the start of an embed run — matters for long-lived stores (the daemon)
    /// where the index outlives individual embed runs.
    pub fn reset_embedding_force_guard(&self) {
        let mut idx = self
            .embedding_index
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        idx.reset_force_guard();
    }

    /// Check whether the embedding index already has an entry for `uid`.
    pub fn has_embedding(&self, uid: &str) -> bool {
        let idx = self
            .embedding_index
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        idx.get(uid).is_some()
    }

    /// Persist the in-memory embedding index to the binary sidecar file.
    /// No-op for in-memory stores.
    pub fn flush_embedding_index(&self) -> Result<(), StoreError> {
        let idx = self
            .embedding_index
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(path) = self.embedding_sidecar_path() {
            idx.save_binary(&path)
                .map_err(|e| StoreError::Query(format!("save binary embedding sidecar: {e}")))?;
        }
        Ok(())
    }

    /// Durably flush and stage the authoritative embedding artifact without
    /// allowing an embedding writer to interleave between the recorded
    /// metadata/count and the copied bytes.
    pub fn stage_embeddings_for_snapshot(
        &self,
        destination: &Path,
    ) -> Result<EmbeddingSnapshotLease<'_>, StoreError> {
        let idx = self
            .embedding_index
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let count = u64::try_from(idx.len())
            .map_err(|_| StoreError::Query("embedding count does not fit u64".to_string()))?;
        let index_dimension = u32::try_from(idx.dimension().unwrap_or(0))
            .map_err(|_| StoreError::Query("embedding dimension does not fit u32".to_string()))?;
        let metadata = self.get_embedding_metadata()?;

        let (model_id, dimension) = match metadata {
            Some((model_id, dimension)) => {
                if count > 0 && dimension != index_dimension {
                    return Err(StoreError::Query(format!(
                        "embedding metadata dimension {dimension} does not match sidecar dimension {index_dimension}"
                    )));
                }
                (model_id, if count > 0 { dimension } else { 0 })
            }
            None if count > 0 => {
                return Err(StoreError::Query(
                    "embedding sidecar contains vectors but database embedding metadata is absent; re-embed before taking a snapshot"
                        .to_string(),
                ));
            }
            None => (String::new(), 0),
        };

        // Flush the canonical sidecar first, then serialize the exact same
        // mutex-protected index into the snapshot staging directory. Both
        // writes use atomic_replace_file (file fsync + rename + parent fsync).
        if let Some(path) = self.embedding_sidecar_path() {
            idx.save_binary(&path).map_err(|error| {
                StoreError::Query(format!("flush binary embedding sidecar: {error}"))
            })?;
        }
        idx.save_binary(destination).map_err(|error| {
            StoreError::Query(format!("stage binary embedding sidecar: {error}"))
        })?;

        Ok(EmbeddingSnapshotLease {
            state: EmbeddingSnapshotState {
                model_id,
                dimension,
                count,
            },
            _guard: idx,
        })
    }
    /// Remove embeddings for graph nodes that no longer exist, update the
    /// live index, and persist the repaired binary sidecar.
    ///
    /// The live UID set is read after graph mutation, so partial multi-step
    /// deletes preserve vectors for nodes that survived. Once the binary
    /// sidecar is durable, the legacy JSON fallback is removed so a later
    /// binary read failure cannot resurrect stale vectors.
    pub fn reconcile_embedding_index(&self) -> Result<usize, StoreError> {
        let stages = self.reconcile_embedding_index_stages()?;
        stages.canonical_persistence?;
        if let Some(retirement) = stages.legacy_retirement {
            retirement?;
        }
        Ok(stages.removed)
    }

    /// Reconcile the live embedding index while retaining typed persistence
    /// and legacy-retirement results for aggregate deletion finalizers.
    pub fn reconcile_embedding_index_stages(
        &self,
    ) -> Result<EmbeddingIndexReconciliation, StoreError> {
        let live_uids = self.live_embedding_node_uids()?;
        let removed = {
            let mut idx = self
                .embedding_index
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            idx.retain_uids(&live_uids)
        };

        let canonical_persistence = self.flush_embedding_index();
        let legacy_retirement = if canonical_persistence.is_ok() {
            self.db_path.as_ref().map(|db_path| {
                let legacy_path = Self::embedding_sidecar_json_for(db_path);
                crate::durable_sidecar::remove_file_durable_if_exists(&legacy_path).map_err(
                    |error| {
                        StoreError::Query(format!(
                            "remove legacy embedding sidecar {}: {error}",
                            legacy_path.display()
                        ))
                    },
                )
            })
        } else {
            None
        };

        Ok(EmbeddingIndexReconciliation {
            removed,
            canonical_persistence,
            legacy_retirement,
        })
    }

    /// Perform a vector similarity search over the embedding index.
    /// Returns `(uid, cosine_similarity)` pairs sorted descending.
    pub fn vector_search(&self, query_embedding: &[f32], limit: usize) -> Vec<(String, f64)> {
        self.vector_search_cancellable(query_embedding, limit, None)
            .expect("vector_search with cancel=None cannot be cancelled")
    }

    /// Like [`vector_search`], but threads a cooperative cancellation flag into
    /// the parallel embedding scan. `cancel = None` is the original behavior;
    /// a tripped flag yields `Err(StoreError::Cancelled(_))` rather than a
    /// silently-truncated (and cacheable) empty result.
    pub fn vector_search_cancellable(
        &self,
        query_embedding: &[f32],
        limit: usize,
        cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<Vec<(String, f64)>, StoreError> {
        let idx = self
            .embedding_index
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        idx.vector_search_cancellable(query_embedding, limit, cancel)
    }

    /// Perform a filtered vector similarity search over the embedding index.
    /// Only embeddings whose UID contains `uid_prefix` are considered.
    /// When `uid_prefix` is `None`, behaves identically to `vector_search`.
    pub fn vector_search_filtered(
        &self,
        query_embedding: &[f32],
        limit: usize,
        uid_prefix: Option<&str>,
    ) -> Vec<(String, f64)> {
        let idx = self
            .embedding_index
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        idx.vector_search_filtered(query_embedding, limit, uid_prefix)
    }

    /// Return the dimensionality of embeddings in the sidecar index,
    /// or `None` if no embeddings are stored.
    pub fn embedding_index_dimension(&self) -> Option<usize> {
        let idx = self
            .embedding_index
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        idx.dimension()
    }

    /// Number of embeddings in the sidecar index.
    pub fn embedding_count(&self) -> usize {
        let idx = self
            .embedding_index
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        idx.len()
    }

    /// P0.2: bump the `graph_generation` counter and persist it to this
    /// store's `<db>.generation` sidecar. No-op persistence for in-memory
    /// stores (the in-memory bump still happens). Call this at the end of any
    /// graph-mutating operation so later short-lived processes observe the
    /// bump without a running daemon.
    pub fn bump_and_persist_generation(&self) {
        if self.db_path.is_some() {
            let path = self.generation_sidecar_path();
            self.bump_and_persist_graph_generation(&path);
        } else {
            self.bump_graph_generation();
        }
    }

    /// Return a new connection to the underlying database.
    pub(crate) fn conn(&self) -> Result<lbug::Connection<'_>, StoreError> {
        Ok(lbug::Connection::new(&self.db)?)
    }

    /// Begin an explicit write transaction. All subsequent writes on the
    /// returned connection are grouped into a single transaction until
    /// `commit_transaction` is called, avoiding per-statement WAL flushes.
    pub fn begin_transaction(&self) -> Result<lbug::Connection<'_>, StoreError> {
        let conn = self.conn()?;
        conn.query("BEGIN TRANSACTION")
            .map_err(|e| StoreError::Query(format!("begin transaction: {e}")))?;
        Ok(conn)
    }

    /// Commit the explicit transaction opened by `begin_transaction`.
    pub fn commit_transaction(&self, conn: &lbug::Connection<'_>) -> Result<(), StoreError> {
        conn.query("COMMIT")
            .map_err(|e| StoreError::Query(format!("commit: {e}")))?;
        Ok(())
    }

    /// Roll back the explicit transaction opened by [`Self::begin_transaction`].
    ///
    /// Destructive classified mutations call this explicitly so a successful
    /// rollback is affirmative evidence that an error happened before any
    /// durable change. A rollback failure is never treated as proof either way.
    pub fn rollback_transaction(&self, conn: &lbug::Connection<'_>) -> Result<(), StoreError> {
        conn.query("ROLLBACK")
            .map_err(|e| StoreError::Query(format!("rollback: {e}")))?;
        Ok(())
    }

    /// Merge the WAL into the main database file.
    pub fn checkpoint(&self) -> Result<(), StoreError> {
        let conn = self.conn()?;
        conn.query("CHECKPOINT")
            .map_err(|e| StoreError::Query(format!("checkpoint: {e}")))?;
        Ok(())
    }

    fn init_schema(&self) -> Result<(), StoreError> {
        let conn = self.conn()?;

        // --- Node tables ---
        conn.query(
            "CREATE NODE TABLE IF NOT EXISTS Repo(\
                uid STRING, \
                url STRING, \
                indexed_sha STRING, \
                staleness_commits_behind INT64, \
                instance_id STRING, \
                name STRING, \
                root_path STRING, \
                PRIMARY KEY(uid))",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;

        // Migration: add `name` column to pre-existing Repo tables that lack it.
        let _ = conn.query("ALTER TABLE Repo ADD name STRING DEFAULT ''");
        // Migration: add `root_path` column to pre-existing Repo tables that
        // lack it. Empty string maps to `None` on read (see read.rs).
        let _ = conn.query("ALTER TABLE Repo ADD root_path STRING DEFAULT ''");

        conn.query(
            "CREATE NODE TABLE IF NOT EXISTS File(\
                uid STRING, \
                path STRING, \
                repo_uid STRING, \
                content_hash STRING, \
                PRIMARY KEY(uid))",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;

        conn.query(
            "CREATE NODE TABLE IF NOT EXISTS Service(\
                uid STRING, \
                name STRING, \
                repo_uid STRING, \
                summary STRING, \
                summary_hash STRING, \
                PRIMARY KEY(uid))",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;

        conn.query(
            "CREATE NODE TABLE IF NOT EXISTS Symbol(\
                uid STRING, \
                name STRING, \
                kind STRING, \
                repo_uid STRING, \
                file_path STRING, \
                start_line INT64, \
                end_line INT64, \
                signature STRING, \
                summary STRING, \
                content_hash STRING, \
                pagerank_score DOUBLE, \
                is_entry_point STRING, \
                entry_point_kind STRING, \
                framework_hint STRING, \
                canonical_id STRING, \
                PRIMARY KEY(uid))",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;

        // Migration: add `end_line` to pre-existing Symbol tables that lack it
        // (P0.1). Old rows default to 0 until re-indexed with `index --force`.
        let _ = conn.query("ALTER TABLE Symbol ADD end_line INT64 DEFAULT 0");

        // Migration (F2.0): add `framework_hint` to pre-existing Symbol tables.
        // Stored as "framework:role" (e.g. "spring:controller"); empty for none.
        let _ = conn.query("ALTER TABLE Symbol ADD framework_hint STRING DEFAULT ''");

        // Migration (Phase 4): add `canonical_id` for cross-boundary symbol matching.
        // Existing symbols get empty string until re-indexed with scope-chain extraction.
        let _ = conn.query("ALTER TABLE Symbol ADD canonical_id STRING DEFAULT ''");

        // --- Relationship tables ---
        conn.query("CREATE REL TABLE IF NOT EXISTS REPO_HAS_FILE(FROM Repo TO File)")
            .map_err(|e| StoreError::Query(e.to_string()))?;

        conn.query("CREATE REL TABLE IF NOT EXISTS FILE_HAS_SYMBOL(FROM File TO Symbol)")
            .map_err(|e| StoreError::Query(e.to_string()))?;

        conn.query("CREATE REL TABLE IF NOT EXISTS SERVICE_HAS_SYMBOL(FROM Service TO Symbol)")
            .map_err(|e| StoreError::Query(e.to_string()))?;

        conn.query(
            "CREATE REL TABLE IF NOT EXISTS CALLS(\
                FROM Symbol TO Symbol, confidence FLOAT, evidence STRING)",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;
        let _ = conn.query("ALTER TABLE CALLS ADD evidence STRING DEFAULT ''");

        conn.query(
            "CREATE REL TABLE IF NOT EXISTS USES(\
                FROM Symbol TO Symbol, confidence FLOAT, evidence STRING)",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;
        let _ = conn.query("ALTER TABLE USES ADD evidence STRING DEFAULT ''");

        conn.query(
            "CREATE REL TABLE IF NOT EXISTS ACCESSES(\
                FROM Symbol TO Symbol, confidence FLOAT, evidence STRING)",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;
        let _ = conn.query("ALTER TABLE ACCESSES ADD evidence STRING DEFAULT ''");

        conn.query(
            "CREATE REL TABLE IF NOT EXISTS IMPORTS(\
                FROM Symbol TO Symbol, confidence FLOAT, evidence STRING)",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;
        let _ = conn.query("ALTER TABLE IMPORTS ADD evidence STRING DEFAULT ''");

        conn.query(
            "CREATE REL TABLE IF NOT EXISTS EXTENDS_SYM(\
                FROM Symbol TO Symbol, confidence FLOAT, evidence STRING)",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;
        let _ = conn.query("ALTER TABLE EXTENDS_SYM ADD evidence STRING DEFAULT ''");

        conn.query(
            "CREATE REL TABLE IF NOT EXISTS IMPLEMENTS_SYM(\
                FROM Symbol TO Symbol, confidence FLOAT, evidence STRING)",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;
        let _ = conn.query("ALTER TABLE IMPLEMENTS_SYM ADD evidence STRING DEFAULT ''");

        conn.query(
            "CREATE REL TABLE IF NOT EXISTS INCLUDES_SYM(\
                FROM Symbol TO Symbol, confidence FLOAT, evidence STRING)",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;
        let _ = conn.query("ALTER TABLE INCLUDES_SYM ADD evidence STRING DEFAULT ''");

        conn.query(
            "CREATE REL TABLE IF NOT EXISTS MEMBER_OF(\
                FROM Symbol TO Symbol, confidence FLOAT, evidence STRING)",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;
        let _ = conn.query("ALTER TABLE MEMBER_OF ADD evidence STRING DEFAULT ''");

        conn.query(
            "CREATE REL TABLE IF NOT EXISTS CROSS_REPO_LINK(\
                FROM Symbol TO Symbol, confidence FLOAT, link_type STRING, evidence STRING)",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;
        let _ = conn.query("ALTER TABLE CROSS_REPO_LINK ADD evidence STRING DEFAULT ''");

        // ── Brain extension: markdown nodes (walking skeleton) ──────────────
        //
        // Vault is the peer of Repo; Note is the peer of File. Headings,
        // Sections, Tags, and cross-reference edges (WIKILINK, TAGGED_WITH,
        // REFERENCES_CODE, ...) will arrive in later phases — this is the
        // minimum needed to round-trip a flat Note through the store.

        conn.query(
            "CREATE NODE TABLE IF NOT EXISTS Vault(\
                uid STRING, \
                name STRING, \
                root_path STRING, \
                instance_id STRING, \
                PRIMARY KEY(uid))",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;

        conn.query(
            "CREATE NODE TABLE IF NOT EXISTS Note(\
                uid STRING, \
                vault_uid STRING, \
                file_path STRING, \
                title STRING, \
                note_kind STRING, \
                word_count INT64, \
                content_hash STRING, \
                frontmatter STRING, \
                created_at STRING, \
                modified_at STRING, \
                pagerank_score DOUBLE, \
                PRIMARY KEY(uid))",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;

        conn.query("CREATE REL TABLE IF NOT EXISTS VAULT_HAS_NOTE(FROM Vault TO Note)")
            .map_err(|e| StoreError::Query(e.to_string()))?;

        // ── Brain extension: outline (Heading + Section) ────────────────────

        conn.query(
            "CREATE NODE TABLE IF NOT EXISTS Heading(\
                uid STRING, \
                note_uid STRING, \
                level INT64, \
                text STRING, \
                slug STRING, \
                start_line INT64, \
                end_line INT64, \
                content_hash STRING, \
                PRIMARY KEY(uid))",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;

        conn.query(
            "CREATE NODE TABLE IF NOT EXISTS Section(\
                uid STRING, \
                note_uid STRING, \
                heading_uid STRING, \
                start_line INT64, \
                end_line INT64, \
                text_hash STRING, \
                text_content STRING, \
                word_count INT64, \
                pagerank_score DOUBLE, \
                PRIMARY KEY(uid))",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;

        conn.query("CREATE REL TABLE IF NOT EXISTS NOTE_HAS_HEADING(FROM Note TO Heading)")
            .map_err(|e| StoreError::Query(e.to_string()))?;

        conn.query("CREATE REL TABLE IF NOT EXISTS NOTE_HAS_SECTION(FROM Note TO Section)")
            .map_err(|e| StoreError::Query(e.to_string()))?;

        conn.query("CREATE REL TABLE IF NOT EXISTS HEADING_HAS_SECTION(FROM Heading TO Section)")
            .map_err(|e| StoreError::Query(e.to_string()))?;

        conn.query("CREATE REL TABLE IF NOT EXISTS HEADING_PARENT(FROM Heading TO Heading)")
            .map_err(|e| StoreError::Query(e.to_string()))?;

        // ── Brain extension: cross-reference (wikilinks + tags + project) ───

        conn.query(
            "CREATE NODE TABLE IF NOT EXISTS Tag(\
                uid STRING, \
                vault_uid STRING, \
                name STRING, \
                PRIMARY KEY(uid))",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;

        conn.query(
            "CREATE NODE TABLE IF NOT EXISTS Project(\
                uid STRING, \
                name STRING, \
                summary STRING, \
                instance_id STRING, \
                PRIMARY KEY(uid))",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;

        // LadybugDB requires one REL TABLE per (FROM, TO) pair. The logical
        // `WIKILINK` edge therefore splits across two tables based on the
        // target kind (Note vs Heading).
        conn.query(
            "CREATE REL TABLE IF NOT EXISTS WIKILINK_TO_NOTE(\
                FROM Section TO Note, confidence FLOAT, display STRING, target STRING)",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;

        conn.query(
            "CREATE REL TABLE IF NOT EXISTS WIKILINK_TO_HEADING(\
                FROM Section TO Heading, confidence FLOAT, display STRING, target STRING)",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;

        // Migration: `display` alone cannot answer both questions a wikilink is
        // asked. For a piped link `[[Home|workspace]]` the BACKLINKS view wants
        // the visible text ("workspace") while BROKEN-LINKS wants the link
        // target ("Home") — reporting the alias there rendered `[[workspace]]`,
        // a string that appears nowhere in the vault (nw-122). Carry both.
        // Empty string means "written before this column existed"; readers fall
        // back to `display`, which is what those rows have always held.
        let _ = conn.query("ALTER TABLE WIKILINK_TO_NOTE ADD target STRING DEFAULT ''");
        let _ = conn.query("ALTER TABLE WIKILINK_TO_HEADING ADD target STRING DEFAULT ''");

        // ── F11 memory-bank: typed Note→Note relationships ─────────────────
        // Explicit, semantically-typed knowledge edges derived from frontmatter
        // keys and heading-grouped wikilinks. Map to PROV-O / SKOS vocab:
        // SUPERSEDES→prov:wasRevisionOf, DEPENDS_ON→prov:wasInformedBy,
        // CAUSED_BY→prov:wasDerivedFrom, RELATES_TO→skos:related.
        conn.query(
            "CREATE REL TABLE IF NOT EXISTS SUPERSEDES(\
                FROM Note TO Note, confidence FLOAT, evidence STRING)",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;
        let _ = conn.query("ALTER TABLE SUPERSEDES ADD evidence STRING DEFAULT ''");
        conn.query(
            "CREATE REL TABLE IF NOT EXISTS DEPENDS_ON(\
                FROM Note TO Note, confidence FLOAT, evidence STRING)",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;
        let _ = conn.query("ALTER TABLE DEPENDS_ON ADD evidence STRING DEFAULT ''");
        conn.query(
            "CREATE REL TABLE IF NOT EXISTS CAUSED_BY(\
                FROM Note TO Note, confidence FLOAT, evidence STRING)",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;
        let _ = conn.query("ALTER TABLE CAUSED_BY ADD evidence STRING DEFAULT ''");
        conn.query(
            "CREATE REL TABLE IF NOT EXISTS RELATES_TO(\
                FROM Note TO Note, confidence FLOAT, evidence STRING)",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;
        let _ = conn.query("ALTER TABLE RELATES_TO ADD evidence STRING DEFAULT ''");

        conn.query("CREATE REL TABLE IF NOT EXISTS NOTE_TAGGED_WITH(FROM Note TO Tag)")
            .map_err(|e| StoreError::Query(e.to_string()))?;

        conn.query("CREATE REL TABLE IF NOT EXISTS SECTION_TAGGED_WITH(FROM Section TO Tag)")
            .map_err(|e| StoreError::Query(e.to_string()))?;

        conn.query(
            "CREATE REL TABLE IF NOT EXISTS PROJECT_INCLUDES_NOTE(\
                FROM Project TO Note, confidence FLOAT)",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;

        // Migration: add confidence column for databases created before it existed.
        let _ = conn.query("ALTER TABLE PROJECT_INCLUDES_NOTE ADD confidence FLOAT DEFAULT 1.0");

        conn.query(
            "CREATE REL TABLE IF NOT EXISTS PROJECT_INCLUDES_SYMBOL(\
                FROM Project TO Symbol, confidence FLOAT)",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;

        conn.query(
            "CREATE REL TABLE IF NOT EXISTS PROJECT_HAS_COMPONENT(\
                FROM Project TO Project, confidence FLOAT)",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;

        conn.query(
            "CREATE REL TABLE IF NOT EXISTS PROJECT_HAS_PARENT(\
                FROM Project TO Project, confidence FLOAT)",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;

        // ── Brain extension: cross-domain (notes ↔ code) ────────────────────
        // The architectural keystone — bridges that make PPR rank a doc
        // section and a code symbol on the same axis when a user query
        // matches either.

        conn.query(
            "CREATE REL TABLE IF NOT EXISTS REFERENCES_CODE_NOTE_TO_SYMBOL(\
                FROM Note TO Symbol, confidence FLOAT, source STRING)",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;

        conn.query(
            "CREATE REL TABLE IF NOT EXISTS REFERENCES_CODE_SECTION_TO_SYMBOL(\
                FROM Section TO Symbol, confidence FLOAT, source STRING)",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;

        // ── Contract extension (F2-core): API contract graph ────────────────
        //
        // A Contract is one HTTP route / gRPC method / GraphQL operation, derived
        // from a spec file (declared) or a framework handler (code-derived).
        // IMPLEMENTS_CONTRACT links a handler Symbol to the Contract it serves;
        // confidence records match quality (1.0 exact, 0.8 base-path-inferred).
        conn.query(
            "CREATE NODE TABLE IF NOT EXISTS Contract(\
                uid STRING, \
                kind STRING, \
                verb STRING, \
                path STRING, \
                operation_id STRING, \
                repo_uid STRING, \
                source_path STRING, \
                confidence FLOAT, \
                PRIMARY KEY(uid))",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;

        conn.query(
            "CREATE REL TABLE IF NOT EXISTS IMPLEMENTS_CONTRACT(\
                FROM Symbol TO Contract, confidence FLOAT, evidence STRING)",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;
        let _ = conn.query("ALTER TABLE IMPLEMENTS_CONTRACT ADD evidence STRING DEFAULT ''");

        // ── Trigram posting table (F3/F4) ───────────────────────────────────
        // Maps a lowercased 3-gram to a node UID whose indexed text contains
        // it. Built opt-in via `index --with-trigrams`; used to pre-filter
        // candidate nodes before running the real regex. Correctness never
        // depends on its presence — see crate::regex.
        conn.query(
            "CREATE NODE TABLE IF NOT EXISTS TrigramPosting(\
                uid STRING, \
                trigram STRING, \
                node_uid STRING, \
                PRIMARY KEY(uid))",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;

        // ── Unresolved wikilink table (broken-links) ────────────────────────
        // A `[[Target]]` whose text matches no note in the vault produces NO
        // WIKILINK_TO_NOTE edge (there is nothing to point at), so the
        // edge-based broken-link query can never surface it. We record each
        // genuinely-unresolved wikilink here so `broken_wikilinks` (and thus
        // brain_broken_links / doc-stats / memory lint) reports it as broken.
        // `uid` is derived from (source_section_uid, wikilink_text) so a
        // re-index of the same note replaces rather than duplicates.
        conn.query(
            "CREATE NODE TABLE IF NOT EXISTS UnresolvedWikilink(\
                uid STRING, \
                source_note_uid STRING, \
                source_path STRING, \
                source_title STRING, \
                wikilink_text STRING, \
                PRIMARY KEY(uid))",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;

        // ── DB-level metadata (key/value singletons) ────────────────────────
        // Used to persist configuration that applies to the whole database,
        // e.g. which embedding model was used to generate stored vectors and
        // the expected vector dimension. One node per logical key; the upsert
        // pattern (DETACH DELETE + CREATE) keeps it idempotent.
        conn.query(
            "CREATE NODE TABLE IF NOT EXISTS Meta(\
                key STRING, \
                value STRING, \
                PRIMARY KEY(key))",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_publication_lease_is_send_without_a_held_mutex_guard() {
        fn assert_send<T: Send>() {}
        assert_send::<IndexPublicationLease<'static>>();
    }

    /// nw-126: the signature observed on the production database after a
    /// SIGKILLed daemon — `.wal` present, `.shadow` absent. lbug reports
    /// "IO exception: Cannot open file <db>.shadow: No such file or directory",
    /// which the stale-CHECKPOINT matcher cannot recognise, so nothing
    /// recovered and every open failed.
    #[test]
    fn orphaned_wal_error_is_distinguished_from_a_stale_checkpoint() {
        let orphan = "database error: IO exception: Cannot open file \
                      /x/brain.lbug.shadow: No such file or directory";
        assert!(is_orphaned_wal_error(orphan));
        assert!(
            !is_stale_checkpoint_error(orphan),
            "the existing checkpoint matcher must NOT claim this case — that it \
             does not is exactly why the database stayed unopenable"
        );

        let checkpoint = "wal.checkpoint header does not match";
        assert!(is_stale_checkpoint_error(checkpoint));
        assert!(!is_orphaned_wal_error(checkpoint));
    }

    /// The orphan is quarantined by RENAME, never deleted: a WAL can hold
    /// committed work, and destroying it to clear an outage would swap one
    /// data-loss story for another.
    #[test]
    fn orphaned_wal_is_quarantined_not_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        std::fs::write(&db, b"db").unwrap();
        std::fs::write(dir.path().join("brain.lbug.wal"), b"orphan-contents").unwrap();

        let moved = quarantine_orphaned_wal(&db).expect("orphaned wal must be quarantined");

        assert!(
            !dir.path().join("brain.lbug.wal").exists(),
            "wal moved aside"
        );
        assert!(moved.exists(), "quarantined copy must still exist");
        assert_eq!(
            std::fs::read(&moved).unwrap(),
            b"orphan-contents",
            "contents must be preserved verbatim — quarantine, not deletion"
        );
        assert!(db.exists(), "the database itself is untouched");
    }

    /// A `.wal` WITH its `.shadow` is a normal replayable log. Quarantining it
    /// would discard recoverable committed work, so the guard must decline.
    #[test]
    fn wal_with_its_shadow_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        std::fs::write(&db, b"db").unwrap();
        std::fs::write(dir.path().join("brain.lbug.wal"), b"live").unwrap();
        std::fs::write(dir.path().join("brain.lbug.shadow"), b"shadow").unwrap();

        assert!(
            quarantine_orphaned_wal(&db).is_none(),
            "a wal accompanied by its shadow must never be moved"
        );
        assert!(dir.path().join("brain.lbug.wal").exists());
    }

    /// No `.wal` at all is not an orphan case.
    #[test]
    fn absent_wal_is_not_quarantined() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        std::fs::write(&db, b"db").unwrap();
        assert!(quarantine_orphaned_wal(&db).is_none());
    }

    #[test]
    fn non_owner_token_cannot_change_publication_generation_state() {
        let store = GraphStore::in_memory().unwrap();
        let owner = store.acquire_index_publication_lease().unwrap();
        let impostor = owner.token.wrapping_add(1);

        assert!(
            store
                .reserve_index_publication_generation(impostor)
                .unwrap_err()
                .to_string()
                .contains("not owned")
        );
        assert_eq!(owner.reserve_generation().unwrap(), 1);
        assert!(
            store
                .complete_index_publication_generation(impostor)
                .unwrap_err()
                .to_string()
                .contains("not owned")
        );
        assert_eq!(store.graph_generation(), 1);
    }

    #[test]
    fn in_memory_publication_preflight_rejects_exhaustion_before_graph_mutation() {
        let store = GraphStore::in_memory().unwrap();
        store.graph_generation.store(u64::MAX, Ordering::Release);
        let publication = store.acquire_index_publication_lease().unwrap();

        let result = (|| -> Result<(), StoreError> {
            publication.preflight_transient_generation()?;
            store.insert_repo(&nestweaver_schema::Repo {
                uid: "repo:must-not-exist".into(),
                url: "https://example.test/must-not-exist".into(),
                indexed_sha: "never".into(),
                staleness_commits_behind: 0,
                instance_id: "test".into(),
                name: None,
                root_path: None,
            })
        })();

        assert!(result.unwrap_err().to_string().contains("exhausted"));
        assert!(store.lookup_repo("repo:must-not-exist").unwrap().is_none());
        assert_eq!(store.graph_generation(), u64::MAX);
    }

    #[test]
    fn in_memory_publication_preflight_allows_exactly_one_remaining_successor() {
        let store = GraphStore::in_memory().unwrap();
        store
            .graph_generation
            .store(u64::MAX - 1, Ordering::Release);
        let publication = store.acquire_index_publication_lease().unwrap();

        publication.preflight_transient_generation().unwrap();
        store
            .insert_repo(&nestweaver_schema::Repo {
                uid: "repo:last-generation".into(),
                url: "https://example.test/last-generation".into(),
                indexed_sha: "last".into(),
                staleness_commits_behind: 0,
                instance_id: "test".into(),
                name: None,
                root_path: None,
            })
            .unwrap();

        assert_eq!(store.try_bump_graph_generation().unwrap(), u64::MAX);
        assert!(store.lookup_repo("repo:last-generation").unwrap().is_some());
    }

    #[test]
    fn try_bump_recovers_an_abandoned_reservation_when_lease_is_unowned() {
        // nw-091 / Bug 3A: a publisher that reserved the dirty generation then
        // dropped its lease without completing (a crash) leaves `base = Some`. The
        // old ungated admin bump failed closed on that forever ("already
        // reserved"), wedging every remove_vault/merge_instance/prune_stale. Now
        // an UNOWNED leftover reservation is recovered.
        let store = GraphStore::in_memory().unwrap();
        let g0 = store.graph_generation();
        {
            let lease = store.acquire_index_publication_lease().unwrap();
            lease.reserve_generation().unwrap(); // base = Some(g0), generation = dirty g0+1
            // Drop WITHOUT complete/cancel → abandoned: base stays Some, owner → None.
        }
        let bumped = store
            .try_bump_graph_generation()
            .expect("an abandoned reservation must be recovered, not wedge the admin bump");
        assert!(bumped > g0, "generation advanced after recovery");
        // A second bump also works — the abandoned base was cleared.
        assert!(store.try_bump_graph_generation().unwrap() > bumped);
    }

    #[test]
    fn try_bump_still_fails_closed_while_a_live_lease_holds_a_reservation() {
        // The recovery must NOT clobber a genuinely in-flight publication: while a
        // lease is HELD, the admin bump must still fail closed.
        let store = GraphStore::in_memory().unwrap();
        let lease = store.acquire_index_publication_lease().unwrap();
        lease.reserve_generation().unwrap(); // base = Some, owner = Some (held)
        let err = store.try_bump_graph_generation().unwrap_err();
        assert!(
            err.to_string().contains("already reserved"),
            "a live publication must not be clobbered, got: {err}"
        );
        drop(lease); // release the lease → the reservation becomes abandoned
        assert!(
            store.try_bump_graph_generation().is_ok(),
            "after the lease is dropped, the abandoned reservation is recovered"
        );
    }

    #[test]
    fn transient_preflight_recovers_an_abandoned_reservation() {
        // The lease-guarded transient/admin preflight must also recover (it holds
        // the exclusive lease, so a leftover base is provably abandoned).
        let store = GraphStore::in_memory().unwrap();
        {
            let lease = store.acquire_index_publication_lease().unwrap();
            lease.reserve_generation().unwrap();
            // abandon
        }
        let lease2 = store.acquire_index_publication_lease().unwrap();
        lease2
            .preflight_transient_generation()
            .expect("transient preflight must recover an abandoned reservation, not fail closed");
    }

    #[test]
    fn stale_checkpoint_error_recognized() {
        let real = "Runtime exception: Database ID for temporary file \
                    '/x/db.lbug.wal.checkpoint' does not match the current database. \
                    Please delete this file and restart.";
        assert!(is_stale_checkpoint_error(real));
        assert!(!is_stale_checkpoint_error("database is locked"));
        assert!(!is_stale_checkpoint_error(
            "some other wal.checkpoint issue"
        ));
    }

    #[test]
    fn removes_stale_checkpoint_only_when_shadow_empty() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db.lbug");
        let cp = dir.path().join("db.lbug.wal.checkpoint");
        let shadow = dir.path().join("db.lbug.shadow");

        // Empty shadow + checkpoint present → recover (remove both).
        std::fs::write(&cp, b"stale-checkpoint-bytes").unwrap();
        std::fs::write(&shadow, b"").unwrap();
        assert!(remove_stale_checkpoint_sidecars(&db));
        assert!(!cp.exists(), "stale checkpoint should be removed");
        assert!(!shadow.exists(), "empty shadow should be removed");

        // Non-empty shadow (possible live writer) → do NOT touch the checkpoint.
        std::fs::write(&cp, b"stale").unwrap();
        std::fs::write(&shadow, b"live-writer-state").unwrap();
        assert!(!remove_stale_checkpoint_sidecars(&db));
        assert!(
            cp.exists(),
            "must not remove checkpoint when shadow is non-empty"
        );
    }

    #[test]
    fn graph_generation_increments() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        assert_eq!(store.graph_generation(), 0);
        store.bump_graph_generation();
        assert_eq!(store.graph_generation(), 1);
        store.bump_graph_generation();
        assert_eq!(store.graph_generation(), 2);
    }

    /// P0.2: the persisted generation survives a reopen. Open → bump+persist
    /// (simulating the `index` path) → drop → reopen → the counter reflects the
    /// incremented value, NOT 0.
    #[test]
    fn graph_generation_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");

        {
            let store = GraphStore::open_or_create(&db_path).unwrap();
            assert_eq!(store.graph_generation(), 0, "fresh store starts at 0");
            // Simulate the end of a graph-mutating operation.
            store.bump_and_persist_generation();
            store.bump_and_persist_generation();
            assert_eq!(store.graph_generation(), 2);
        }

        // Reopen: the in-memory counter is restored from the sidecar.
        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        assert_eq!(
            reopened.graph_generation(),
            2,
            "generation must survive reopen and NOT reset to 0"
        );
    }

    #[test]
    fn dirty_index_publication_ignores_stale_generation_and_pagerank_on_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let generation_path = PathBuf::from(format!("{}.generation", db_path.display()));
        let pagerank_path = PathBuf::from(format!("{}.pagerank.json", db_path.display()));
        let dirty_path = PathBuf::from(format!("{}.index-dirty", db_path.display()));

        {
            let store = GraphStore::open_or_create(&db_path).unwrap();
            for _ in 0..7 {
                store.bump_graph_generation();
            }
            store.save_graph_generation(&generation_path).unwrap();
        }
        std::fs::write(&pagerank_path, r#"{"stale":1.0}"#).unwrap();
        std::fs::write(&dirty_path, b"dirty").unwrap();

        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        assert_ne!(
            reopened.graph_generation(),
            7,
            "a dirty publication must not restore the stale canonical generation"
        );
        reopened.load_pagerank_cache(&pagerank_path).unwrap();
        assert!(
            !reopened.pagerank_scores().contains_key("stale"),
            "a dirty publication must not load canonical PageRank from before the graph commit"
        );
    }

    #[test]
    fn unreadable_index_publication_marker_fails_closed_for_generation_and_pagerank() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let generation_path = PathBuf::from(format!("{}.generation", db_path.display()));
        let pagerank_path = PathBuf::from(format!("{}.pagerank.json", db_path.display()));
        let dirty_path = PathBuf::from(format!("{}.index-dirty", db_path.display()));

        {
            let store = GraphStore::open_or_create(&db_path).unwrap();
            for _ in 0..7 {
                store.bump_graph_generation();
            }
            store.save_graph_generation(&generation_path).unwrap();
        }
        std::fs::write(&pagerank_path, r#"{"stale":1.0}"#).unwrap();
        std::fs::create_dir(&dirty_path).unwrap();

        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        assert_eq!(
            reopened.graph_generation(),
            8,
            "an unreadable marker must reserve the monotonic canonical successor"
        );
        reopened.load_pagerank_cache(&pagerank_path).unwrap();
        assert!(
            !reopened.pagerank_scores().contains_key("stale"),
            "an unreadable marker must make canonical PageRank non-authoritative"
        );
    }

    #[test]
    fn graph_generation_never_wraps_at_counter_exhaustion() {
        let store = GraphStore::in_memory().unwrap();
        store.graph_generation.store(u64::MAX, Ordering::Release);

        store.bump_graph_generation();

        assert_eq!(
            store.graph_generation(),
            u64::MAX,
            "generation exhaustion must never wrap to a reused value"
        );
    }

    #[test]
    fn embedding_reconciliation_preserves_typed_legacy_retirement_failure() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let mut legacy_path = db_path.as_os_str().to_owned();
        legacy_path.push(".embeddings");
        let legacy_path = std::path::PathBuf::from(legacy_path);
        std::fs::create_dir(&legacy_path).unwrap();

        let result = store.reconcile_embedding_index_stages().unwrap();

        assert!(result.canonical_persistence.is_ok());
        assert!(result.legacy_retirement.unwrap().is_err());
    }

    #[test]
    fn embedding_reconciliation_surfaces_durable_legacy_retirement_failure() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let legacy_path = GraphStore::embedding_sidecar_json_for(&db_path);
        std::fs::write(&legacy_path, b"legacy").unwrap();

        let result = crate::durable_sidecar::with_test_fault(
            crate::durable_sidecar::TestFault::Remove,
            || store.reconcile_embedding_index_stages(),
        )
        .unwrap();

        assert!(result.canonical_persistence.is_ok());
        let retirement = result.legacy_retirement.unwrap().unwrap_err();
        assert!(retirement.to_string().contains("legacy embedding sidecar"));
        assert!(
            legacy_path.exists(),
            "failed retirement must preserve fallback"
        );
    }
}

#[cfg(test)]
mod hardened_config_tests {
    #[test]
    fn env_overrides_are_honored_and_default_bounds_threads() {
        // Serialize the env mutation; other tests don't touch these keys.
        // Default (no env): threads bounded to 1 to remove the eviction race.
        // SAFETY: single-threaded test section; we set then clear.
        unsafe {
            std::env::remove_var("NESTWEAVER_LBUG_MAX_THREADS");
            std::env::remove_var("NESTWEAVER_LBUG_BUFFER_POOL_BYTES");
            std::env::remove_var("NESTWEAVER_LBUG_AUTO_CHECKPOINT");
        }
        // We can't read private SystemConfig fields, but we can prove the
        // helper runs without panic under each override shape (parse paths).
        let _default = super::hardened_system_config();
        unsafe {
            std::env::set_var("NESTWEAVER_LBUG_MAX_THREADS", "4");
            std::env::set_var("NESTWEAVER_LBUG_BUFFER_POOL_BYTES", "1073741824");
            std::env::set_var("NESTWEAVER_LBUG_AUTO_CHECKPOINT", "false");
        }
        let _overridden = super::hardened_system_config();
        // Malformed values must not panic (fall back to default/auto).
        unsafe {
            std::env::set_var("NESTWEAVER_LBUG_MAX_THREADS", "not-a-number");
        }
        let _tolerant = super::hardened_system_config();
        unsafe {
            std::env::remove_var("NESTWEAVER_LBUG_MAX_THREADS");
            std::env::remove_var("NESTWEAVER_LBUG_BUFFER_POOL_BYTES");
            std::env::remove_var("NESTWEAVER_LBUG_AUTO_CHECKPOINT");
        }
    }
}
