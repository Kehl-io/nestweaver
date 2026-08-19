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

    /// Re-derive an inherited publication base whose N+1/N+2 successors are
    /// unavailable. See
    /// [`GraphStore::rederive_unavailable_index_publication_base`].
    pub fn rederive_unavailable_generation_base(
        &self,
        generation_path: &std::path::Path,
    ) -> Result<Option<u64>, StoreError> {
        self.store
            .rederive_unavailable_index_publication_base(self.token, generation_path)
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

    /// Compute PageRank over `scope` while this owner's publication is still
    /// dirty, so the fresh sidecar can land BEFORE the marker retires. The
    /// fail-closed guards on [`GraphStore::compute_pagerank`] stop outsiders
    /// from ranking mid-publication state; the publication owner ranks the
    /// graph it just committed, which is exactly the state the marker's
    /// retirement then vouches for. Token-validated like every other lease
    /// method.
    pub fn compute_pagerank(
        &self,
        damping: f64,
        iterations: u32,
        scope: &crate::ranking::GraphScope,
    ) -> Result<(), StoreError> {
        self.store.validate_index_publication_owner(self.token)?;
        self.store
            .compute_pagerank_for_publication_owner(damping, iterations, scope)
    }

    /// Persist the freshly computed PageRank sidecar during this owner's dirty
    /// publication window. Same exemption as [`Self::compute_pagerank`]: the
    /// guarded [`GraphStore::save_pagerank_cache`] refuses to persist
    /// mid-publication state for outsiders, but the owner must persist before
    /// the marker retires or a crash in between leaves a clean-reporting
    /// publication with no ranks.
    pub fn save_pagerank(&self, path: &Path) -> Result<(), StoreError> {
        self.store.validate_index_publication_owner(self.token)?;
        let clean_generation = self.clean_generation()?;
        self.store
            .save_pagerank_cache_for_publication_owner(path, clean_generation)
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
    /// Algorithm and scope fingerprint for the scores currently held in
    /// `pagerank_cache`. It is persisted with the scores so a loader cannot
    /// confuse code-only, notes-only, unified, or parameter-incompatible
    /// ranks.
    pub(crate) pagerank_artifact_fingerprint: Mutex<Option<String>>,
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
    /// Generation/digest-keyed trigram scope trust verdict. The digest keeps
    /// raw/import mutations that do not bump the in-process generation from
    /// reusing a narrower stale verdict.
    pub(crate) trigram_scope_cache: Mutex<Option<crate::regex::TrigramScopeCache>>,
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
    /// Private filesystem namespace for rebuildable sidecars owned by an
    /// in-memory graph. Keeping the TempDir alive makes in-memory stores use
    /// the same regex-v3 implementation as persistent stores without creating
    /// graph-resident postings.
    regex_ephemeral_root: Option<tempfile::TempDir>,
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

/// Persistent identity of one NestWeaver brain and its current publication
/// slot. These IDs describe the data, not the configured instance name or the
/// database path used for daemon runtime files.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PublicationIdentity {
    pub brain_uuid: String,
    pub publication_uuid: String,
}

impl PublicationIdentity {
    /// Create the identity for a brand-new logical brain.
    pub fn new_brain() -> Self {
        Self {
            brain_uuid: uuid::Uuid::new_v4().to_string(),
            publication_uuid: uuid::Uuid::new_v4().to_string(),
        }
    }

    /// Create a fresh publication slot for this same logical brain.
    ///
    /// The current publication UUID is deliberately not retained: a staged
    /// rebuild is a different complete graph/artifact lineage even though it
    /// represents the same user data.
    pub fn next_publication(&self) -> Result<Self, StoreError> {
        self.validate()?;
        let next = Self {
            brain_uuid: self.brain_uuid.clone(),
            publication_uuid: uuid::Uuid::new_v4().to_string(),
        };
        next.validate()?;
        Ok(next)
    }

    /// Validate both UUIDs and the invariant that the two identities differ.
    pub fn validate(&self) -> Result<(), StoreError> {
        let parse = |name: &str, value: &str| {
            let parsed = uuid::Uuid::parse_str(value).map_err(|error| {
                StoreError::Query(format!("invalid {name} metadata '{value}': {error}"))
            })?;
            if parsed.is_nil() {
                return Err(StoreError::Query(format!(
                    "invalid {name} metadata: nil UUID is not a data identity"
                )));
            }
            Ok(parsed)
        };
        let brain_uuid = parse("brain_uuid", &self.brain_uuid)?;
        let publication_uuid = parse("publication_uuid", &self.publication_uuid)?;
        if brain_uuid == publication_uuid {
            return Err(StoreError::Query(
                "brain_uuid and publication_uuid must be distinct".to_string(),
            ));
        }
        Ok(())
    }
}

const BRAIN_UUID_META_KEY: &str = "publication.brain_uuid";
const PUBLICATION_UUID_META_KEY: &str = "publication.publication_uuid";

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
            pagerank_artifact_fingerprint: Mutex::new(None),
            pagerank_generation: AtomicU64::new(0),
            pagerank_compute_lock: Mutex::new(()),
            graph_generation: AtomicU64::new(0),
            trigram_scope_cache: Mutex::new(None),
            index_publication_generation_base: Mutex::new(None),
            index_publication_lease: IndexPublicationLeaseCoordinator::default(),
            interaction_cache: Mutex::new(None),
            git_activity_cache: Mutex::new(None),
            git_activity_weight: Mutex::new(crate::ranking::DEFAULT_GIT_ACTIVITY_WEIGHT),
            db_path: Some(path.to_path_buf()),
            regex_ephemeral_root: None,
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
        store.ensure_publication_identity()?;
        store.load_graph_generation(&store.generation_sidecar_path());
        store.load_recorded_embedding_model_into_index();
        Ok(store)
    }

    /// Create a new persistent database with an explicitly chosen publication
    /// identity.
    ///
    /// This is the safe creation primitive for a staged full rebuild: callers
    /// derive `identity` with [`PublicationIdentity::next_publication`], so the
    /// new slot inherits the incumbent brain UUID but cannot masquerade as the
    /// incumbent publication. The destination must not already exist; this API
    /// never adopts or rewrites identity on an existing database.
    pub fn create_with_publication_identity(
        path: &Path,
        identity: &PublicationIdentity,
    ) -> Result<Self, StoreError> {
        identity.validate()?;
        if path.exists() {
            return Err(StoreError::Query(format!(
                "refusing to create staged publication over existing database {}",
                path.display()
            )));
        }

        let db = open_lbug_with_recovery(path, true, hardened_system_config)?;
        let store = GraphStore {
            db,
            pagerank_cache: Mutex::new(None),
            pagerank_artifact_fingerprint: Mutex::new(None),
            pagerank_generation: AtomicU64::new(0),
            pagerank_compute_lock: Mutex::new(()),
            graph_generation: AtomicU64::new(0),
            trigram_scope_cache: Mutex::new(None),
            index_publication_generation_base: Mutex::new(None),
            index_publication_lease: IndexPublicationLeaseCoordinator::default(),
            interaction_cache: Mutex::new(None),
            git_activity_cache: Mutex::new(None),
            git_activity_weight: Mutex::new(crate::ranking::DEFAULT_GIT_ACTIVITY_WEIGHT),
            db_path: Some(path.to_path_buf()),
            regex_ephemeral_root: None,
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
        store.initialize_publication_identity(identity)?;
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
            pagerank_artifact_fingerprint: Mutex::new(None),
            pagerank_generation: AtomicU64::new(0),
            pagerank_compute_lock: Mutex::new(()),
            graph_generation: AtomicU64::new(0),
            trigram_scope_cache: Mutex::new(None),
            index_publication_generation_base: Mutex::new(None),
            index_publication_lease: IndexPublicationLeaseCoordinator::default(),
            interaction_cache: Mutex::new(None),
            git_activity_cache: Mutex::new(None),
            git_activity_weight: Mutex::new(crate::ranking::DEFAULT_GIT_ACTIVITY_WEIGHT),
            db_path: Some(path.to_path_buf()),
            regex_ephemeral_root: None,
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
        store.ensure_publication_identity()?;
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
            pagerank_artifact_fingerprint: Mutex::new(None),
            pagerank_generation: AtomicU64::new(0),
            pagerank_compute_lock: Mutex::new(()),
            graph_generation: AtomicU64::new(0),
            trigram_scope_cache: Mutex::new(None),
            index_publication_generation_base: Mutex::new(None),
            index_publication_lease: IndexPublicationLeaseCoordinator::default(),
            interaction_cache: Mutex::new(None),
            git_activity_cache: Mutex::new(None),
            git_activity_weight: Mutex::new(crate::ranking::DEFAULT_GIT_ACTIVITY_WEIGHT),
            db_path: Some(path.to_path_buf()),
            regex_ephemeral_root: None,
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
        let regex_ephemeral_root = tempfile::Builder::new()
            .prefix("nestweaver-regex-v3-memory-")
            .tempdir()
            .map_err(|error| StoreError::Query(format!("create in-memory regex root: {error}")))?;
        let store = GraphStore {
            db,
            pagerank_cache: Mutex::new(None),
            pagerank_artifact_fingerprint: Mutex::new(None),
            pagerank_generation: AtomicU64::new(0),
            pagerank_compute_lock: Mutex::new(()),
            graph_generation: AtomicU64::new(0),
            trigram_scope_cache: Mutex::new(None),
            index_publication_generation_base: Mutex::new(None),
            index_publication_lease: IndexPublicationLeaseCoordinator::default(),
            interaction_cache: Mutex::new(None),
            git_activity_cache: Mutex::new(None),
            git_activity_weight: Mutex::new(crate::ranking::DEFAULT_GIT_ACTIVITY_WEIGHT),
            db_path: None,
            regex_ephemeral_root: Some(regex_ephemeral_root),
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
        store.ensure_publication_identity()?;
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
            .pagerank_artifact_fingerprint
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

    /// Acquire the publication lease only if it is free right now.
    ///
    /// [`Self::acquire_index_publication_lease`] blocks, which is correct for a
    /// publisher that must run. Abandoned-publication recovery is opportunistic:
    /// if a live in-process publisher already owns the lease then the
    /// publication is by definition not abandoned, and recovery must decline
    /// rather than queue behind it.
    pub fn try_acquire_index_publication_lease(
        &self,
    ) -> Result<Option<IndexPublicationLease<'_>>, StoreError> {
        let mut state = self
            .index_publication_lease
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.owner.is_some() {
            return Ok(None);
        }
        let token = state.next_token;
        state.next_token = token.checked_add(1).ok_or_else(|| {
            StoreError::Query("index publication ownership token exhausted".into())
        })?;
        state.owner = Some(token);
        Ok(Some(IndexPublicationLease {
            store: self,
            token,
            reservation: Cell::new(IndexPublicationReservationState::Unreserved),
            released: false,
        }))
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
    pub fn index_publication_lease_is_unowned(&self) -> bool {
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

    /// Re-derive a fail-closed publication base whose successors are
    /// unavailable, so an interrupted publication can be completed at all.
    ///
    /// When `.generation` is missing or unparseable *while* the marker is
    /// present, `load_fail_closed_index_publication_generation` takes
    /// `canonical = u64::MAX` — deliberately, so nothing can complete a
    /// publication on top of an unknown canonical value. The cost is that
    /// `checked_add(2)` then overflows in every preflight and reserve path, so
    /// the publication can never complete and the user sees a *different*
    /// error (`graph generation exhausted during index publication`) rather
    /// than the dirty-publication one. Recovery that ignored this arm would
    /// appear to fix nothing.
    ///
    /// The fix is to re-derive rather than to add to `MAX`. The note that
    /// specified this work says "re-derive from the database"; there is no
    /// generation column in the database — the counter's only durable home is
    /// the `<db>.generation` sidecar — so re-derivation re-reads that sidecar
    /// and, when it is still unreadable, falls back to `0`. `0` is not a new
    /// risk: it is exactly what a *clean* open of a store with no `.generation`
    /// sidecar already uses (`load_graph_generation` leaves the freshly-opened
    /// `AtomicU64::new(0)` untouched), so recovery lands on the same canonical
    /// value the clean path would have.
    ///
    /// Returns the canonical base in force after the call, or `None` when the
    /// base did not need re-deriving.
    fn rederive_unavailable_index_publication_base(
        &self,
        token: u64,
        generation_path: &Path,
    ) -> Result<Option<u64>, StoreError> {
        self.validate_index_publication_owner(token)?;
        let mut base = self
            .index_publication_generation_base
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(canonical) = *base else {
            return Ok(None);
        };
        if canonical.checked_add(2).is_some() {
            return Ok(None);
        }
        let rederived = std::fs::read_to_string(generation_path)
            .ok()
            .and_then(|contents| contents.trim().parse::<u64>().ok())
            .filter(|value| value.checked_add(2).is_some())
            .unwrap_or(0);
        *base = Some(rederived);
        self.graph_generation
            .store(rederived.saturating_add(1), Ordering::Release);
        Ok(Some(rederived))
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
        crate::index_publication::marker_path(db_path)
    }

    /// Path of this store's durable publication marker, when it is file-backed.
    /// In-memory stores have no marker (and nothing to reconcile on a later
    /// open), so they return `None`.
    pub fn index_publication_marker_path(&self) -> Option<PathBuf> {
        self.db_path
            .as_deref()
            .map(Self::index_publication_marker_for)
    }

    /// Three-state read of this store's publication marker. Unlike
    /// [`Self::is_index_publication_dirty`], which collapses "cannot tell" into
    /// "dirty", this preserves the distinction recovery depends on.
    pub fn index_publication_marker_state(&self) -> crate::index_publication::MarkerState {
        match self.db_path.as_deref() {
            Some(path) => crate::index_publication::read_marker(path),
            None => crate::index_publication::MarkerState::Absent,
        }
    }

    /// Poll the FILE-BACKED dirty check until the publication is clean or
    /// `timeout` elapses. Returns true when the publication is clean.
    ///
    /// This deliberately polls [`Self::is_index_publication_dirty`] rather than
    /// waiting on `index_publication_lease.available`. That condition variable
    /// is **in-process**: the MCP reader commonly runs in a different process
    /// from the indexing writer and opens the store read-only, so the condvar
    /// can never fire for it and an in-process test of a condvar-based wait
    /// would pass for the wrong reason. The marker file is genuinely
    /// cross-process.
    ///
    /// It also deliberately does NOT acquire the publication lease. Acquisition
    /// is exclusive and blocking, so a waiting reader would serialize every
    /// other reader behind the writer — turning a latency blip into an outage.
    pub fn wait_until_index_publication_clean(&self, timeout: std::time::Duration) -> bool {
        self.wait_until_index_publication_clean_interruptible(timeout, &|| false)
    }

    /// [`Self::wait_until_index_publication_clean`], but polls `abort` on every
    /// iteration and gives up early when it returns true.
    ///
    /// The MCP dispatch boundary already threads a cancellation flag for query
    /// timeouts and client disconnects. Without this, a cancelled call would
    /// still sleep out the whole budget before noticing nobody is listening.
    pub fn wait_until_index_publication_clean_interruptible(
        &self,
        timeout: std::time::Duration,
        abort: &dyn Fn() -> bool,
    ) -> bool {
        if !self.is_index_publication_dirty() {
            return true;
        }
        if timeout.is_zero() || abort() {
            return false;
        }
        let deadline = std::time::Instant::now() + timeout;
        let mut backoff = std::time::Duration::from_millis(5);
        let max_backoff = std::time::Duration::from_millis(100);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return !self.is_index_publication_dirty();
            }
            std::thread::sleep(backoff.min(remaining));
            if !self.is_index_publication_dirty() {
                return true;
            }
            if abort() {
                return false;
            }
            backoff = (backoff * 2).min(max_backoff);
        }
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
            && let Ok(idx) = crate::search::EmbeddingIndex::load_binary_v2(&binary_path)
        {
            return idx;
        }
        if binary_path.exists() {
            tracing::warn!(
                path = %binary_path.display(),
                "legacy or invalid embedding artifact is unavailable; run a full re-embed for embedding pipeline v2"
            );
        }
        crate::search::EmbeddingIndex::new()
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
        let pipeline_fingerprint = self
            .get_embedding_pipeline()
            .ok()
            .flatten()
            .and_then(|pipeline| pipeline.fingerprint().ok());
        let mut index = self
            .embedding_index
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(envelope) = index.artifact_envelope().cloned() {
            let identity = self.publication_identity().ok().flatten();
            let compatible = identity.as_ref().is_some_and(|identity| {
                envelope.brain_uuid == identity.brain_uuid
                    && envelope.publication_uuid == identity.publication_uuid
            }) && envelope.producer_version == env!("CARGO_PKG_VERSION")
                && envelope.source_graph_generation <= self.graph_generation()
                && pipeline_fingerprint.as_deref()
                    == envelope.pipeline.fingerprint().ok().as_deref();
            if !compatible {
                tracing::warn!(
                    "embedding-v2 artifact is foreign, stale, or pipeline-incompatible; semantic search is unavailable until a full re-embed"
                );
                index.clear();
            }
        }
        if !index.is_empty()
            && let (Some(db_path), Some(identity), Some(pipeline)) = (
                self.db_path.as_ref(),
                self.publication_identity().ok().flatten(),
                self.get_embedding_pipeline().ok().flatten(),
            )
        {
            let journal = Self::embedding_journal_for(db_path);
            if let Err(error) = index.replay_journal_v2(&journal, &identity, &pipeline) {
                tracing::warn!(%error, "embedding journal is invalid; semantic search is unavailable until a full re-embed");
                index.clear();
            }
        }
        index.set_recorded_model_id(recorded);
        index.set_recorded_pipeline_fingerprint(pipeline_fingerprint);
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

    fn embedding_journal_for(db_path: &Path) -> std::path::PathBuf {
        let mut value = db_path.as_os_str().to_owned();
        value.push(".embeddings.journal");
        std::path::PathBuf::from(value)
    }

    /// Return the path to the embedding sidecar file (binary format),
    /// or `None` for in-memory stores.
    pub fn embedding_sidecar_path(&self) -> Option<std::path::PathBuf> {
        self.db_path
            .as_ref()
            .map(|p| Self::embedding_sidecar_binary_for(p))
    }

    /// Return the dedicated per-scope regex-v3 sidecar root.
    pub fn regex_sidecar_root(&self) -> Option<std::path::PathBuf> {
        self.db_path
            .as_ref()
            .map(|path| {
                let mut value = path.as_os_str().to_owned();
                value.push(".regex-v3");
                std::path::PathBuf::from(value)
            })
            .or_else(|| {
                self.regex_ephemeral_root
                    .as_ref()
                    .map(|root| root.path().join("regex-v3"))
            })
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

    #[must_use = "a false return means a pipeline or dimension guard rejected the embedding"]
    pub fn add_embedding_with_pipeline(
        &self,
        uid: &str,
        embedding: Vec<f32>,
        pipeline: &nestweaver_schema::EmbeddingPipelineV2,
        force: bool,
    ) -> bool {
        let mut index = self
            .embedding_index
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        index.add_with_pipeline(uid, embedding, pipeline, force)
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
        let mut idx = self
            .embedding_index
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(path) = self.embedding_sidecar_path() {
            if idx.is_empty() {
                return Ok(());
            }
            let identity = self.publication_identity()?.ok_or_else(|| {
                StoreError::Query("embedding v2 requires publication identity".to_string())
            })?;
            let pipeline = self.get_embedding_pipeline()?.ok_or_else(|| {
                StoreError::Query(
                    "embedding vectors have no pipeline-v2 metadata; run a full re-embed"
                        .to_string(),
                )
            })?;
            let db_path = self
                .db_path
                .as_ref()
                .expect("sidecar path requires database path");
            let journal = Self::embedding_journal_for(db_path);
            if !path.exists() {
                idx.save_binary_v2(&path, &identity, self.graph_generation(), &pipeline)
                    .map_err(|e| StoreError::Query(format!("save embedding-v2 sidecar: {e}")))?;
                idx.mark_base_persisted();
                crate::durable_sidecar::remove_file_durable_if_exists(&journal).map_err(
                    |error| StoreError::Query(format!("retire embedding journal: {error}")),
                )?;
            } else {
                idx.append_journal_v2(&journal, &identity, &pipeline)
                    .map_err(|e| StoreError::Query(format!("append embedding journal: {e}")))?;
                if idx.should_compact_journal(&journal) {
                    idx.save_binary_v2(&path, &identity, self.graph_generation(), &pipeline)
                        .map_err(|e| {
                            StoreError::Query(format!("compact embedding-v2 sidecar: {e}"))
                        })?;
                    idx.mark_base_persisted();
                    crate::durable_sidecar::remove_file_durable_if_exists(&journal).map_err(
                        |error| {
                            StoreError::Query(format!(
                                "retire compacted embedding journal: {error}"
                            ))
                        },
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Fold the append journal into a complete sibling-safe base snapshot.
    /// Backup/cutover paths use this to package one self-contained artifact.
    pub fn compact_embedding_index(&self) -> Result<(), StoreError> {
        let mut index = self
            .embedding_index
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if index.is_empty() {
            return Ok(());
        }
        let Some(path) = self.embedding_sidecar_path() else {
            return Ok(());
        };
        let identity = self.publication_identity()?.ok_or_else(|| {
            StoreError::Query("embedding compaction requires publication identity".to_string())
        })?;
        let pipeline = self.get_embedding_pipeline()?.ok_or_else(|| {
            StoreError::Query("embedding compaction requires pipeline-v2 metadata".to_string())
        })?;
        index
            .save_binary_v2(&path, &identity, self.graph_generation(), &pipeline)
            .map_err(|error| StoreError::Query(format!("compact embedding-v2 sidecar: {error}")))?;
        index.mark_base_persisted();
        let journal = Self::embedding_journal_for(
            self.db_path
                .as_ref()
                .expect("embedding sidecar requires database path"),
        );
        crate::durable_sidecar::remove_file_durable_if_exists(&journal)
            .map_err(|error| StoreError::Query(format!("retire embedding journal: {error}")))?;
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
        let pipeline = self.get_embedding_pipeline()?;

        let (model_id, dimension) = match pipeline.as_ref() {
            Some(pipeline) => {
                let dimension = pipeline.produced_dimension;
                if count > 0 && dimension != index_dimension {
                    return Err(StoreError::Query(format!(
                        "embedding metadata dimension {dimension} does not match sidecar dimension {index_dimension}"
                    )));
                }
                (
                    pipeline.model_id.clone(),
                    if count > 0 { dimension } else { 0 },
                )
            }
            None if count > 0 => {
                return Err(StoreError::Query(
                    "embedding sidecar contains vectors but database embedding metadata is absent; re-embed before taking a snapshot"
                        .to_string(),
                ));
            }
            None => (String::new(), 0),
        };

        // An empty semantic index has no artifact. Requiring or inventing a
        // pipeline for zero vectors would falsely claim a semantic space and
        // make graph-only snapshots impossible.
        if count == 0 {
            return Ok(EmbeddingSnapshotLease {
                state: EmbeddingSnapshotState {
                    model_id,
                    dimension,
                    count,
                },
                _guard: idx,
            });
        }

        // Flush the canonical sidecar first, then serialize the exact same
        // mutex-protected index into the snapshot staging directory. Both
        // writes use atomic_replace_file (file fsync + rename + parent fsync).
        if let Some(path) = self.embedding_sidecar_path() {
            let identity = self.publication_identity()?.ok_or_else(|| {
                StoreError::Query("embedding snapshot requires publication identity".to_string())
            })?;
            let pipeline = pipeline.as_ref().ok_or_else(|| {
                StoreError::Query("embedding snapshot requires pipeline-v2 metadata".to_string())
            })?;
            idx.save_binary_v2(&path, &identity, self.graph_generation(), pipeline)
                .map_err(|error| {
                    StoreError::Query(format!("flush embedding-v2 sidecar: {error}"))
                })?;
        }
        let identity = self.publication_identity()?.ok_or_else(|| {
            StoreError::Query("embedding snapshot requires publication identity".to_string())
        })?;
        let pipeline = pipeline.as_ref().ok_or_else(|| {
            StoreError::Query("embedding snapshot requires pipeline-v2 metadata".to_string())
        })?;
        idx.save_binary_v2(destination, &identity, self.graph_generation(), pipeline)
            .map_err(|error| StoreError::Query(format!("stage embedding-v2 sidecar: {error}")))?;

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

    fn publication_meta_value_on(
        conn: &lbug::Connection<'_>,
        key: &str,
    ) -> Result<Option<String>, StoreError> {
        let mut statement = conn
            .prepare("MATCH (m:Meta {key: $key}) RETURN m.value")
            .map_err(|error| StoreError::Query(format!("prepare publication identity: {error}")))?;
        let mut rows = conn
            .execute(
                &mut statement,
                vec![("key", lbug::Value::String(key.to_string()))],
            )
            .map_err(|error| StoreError::Query(format!("read publication identity: {error}")))?;
        rows.next()
            .map(|row| crate::read::extract_string(&row, 0))
            .transpose()
    }

    fn create_publication_meta_on(
        conn: &lbug::Connection<'_>,
        key: &str,
        value: &str,
    ) -> Result<(), StoreError> {
        let mut statement = conn
            .prepare("CREATE (:Meta {key: $key, value: $value})")
            .map_err(|error| StoreError::Query(format!("prepare publication identity: {error}")))?;
        conn.execute(
            &mut statement,
            vec![
                ("key", lbug::Value::String(key.to_string())),
                ("value", lbug::Value::String(value.to_string())),
            ],
        )
        .map_err(|error| StoreError::Query(format!("write publication identity: {error}")))?;
        Ok(())
    }

    /// Read the database-bound identity. A legacy read-only database may have
    /// neither key and returns `None`; a partial or malformed identity is an
    /// error because silently inventing the missing half could attach foreign
    /// artifacts to this graph.
    pub fn publication_identity(&self) -> Result<Option<PublicationIdentity>, StoreError> {
        let conn = self.conn()?;
        let brain_uuid = Self::publication_meta_value_on(&conn, BRAIN_UUID_META_KEY)?;
        let publication_uuid = Self::publication_meta_value_on(&conn, PUBLICATION_UUID_META_KEY)?;
        match (brain_uuid, publication_uuid) {
            (None, None) => Ok(None),
            (Some(brain_uuid), Some(publication_uuid)) => {
                let identity = PublicationIdentity {
                    brain_uuid,
                    publication_uuid,
                };
                identity.validate()?;
                Ok(Some(identity))
            }
            (brain_uuid, publication_uuid) => Err(StoreError::Query(format!(
                "incomplete publication identity: brain_uuid={}, publication_uuid={}; repair or rebuild the database",
                brain_uuid.is_some(),
                publication_uuid.is_some()
            ))),
        }
    }

    /// Initialize identity for a new or legacy writable database exactly once.
    /// Both keys are committed in one LadybugDB transaction. Existing identity
    /// is validated and returned unchanged.
    pub fn ensure_publication_identity(&self) -> Result<PublicationIdentity, StoreError> {
        if let Some(identity) = self.publication_identity()? {
            return Ok(identity);
        }

        let identity = PublicationIdentity::new_brain();
        self.initialize_publication_identity(&identity)
    }

    /// Assert that this store is the brain selected by configuration.
    /// UUID spellings are compared by value rather than text so alternate
    /// encodings cannot bypass the binding.
    pub fn assert_brain_uuid(&self, expected: &str) -> Result<PublicationIdentity, StoreError> {
        let expected = uuid::Uuid::parse_str(expected).map_err(|error| {
            StoreError::Query(format!("invalid expected_brain_uuid '{expected}': {error}"))
        })?;
        if expected.is_nil() {
            return Err(StoreError::Query(
                "invalid expected_brain_uuid: nil UUID is not a data identity".to_string(),
            ));
        }
        let identity = self.publication_identity()?.ok_or_else(|| {
            StoreError::Query(
                "database has no publication identity; open it writable to initialize identity before binding it to configuration"
                    .to_string(),
            )
        })?;
        let actual = uuid::Uuid::parse_str(&identity.brain_uuid).map_err(|error| {
            StoreError::Query(format!(
                "invalid brain_uuid metadata '{}': {error}",
                identity.brain_uuid
            ))
        })?;
        if actual != expected {
            return Err(StoreError::Query(format!(
                "brain identity mismatch: configuration expects {expected}, but database {} contains {}. Inspect the database with `nestweaver instance identity --db <path>`; if this database is intentionally correct, bind the config with `nestweaver instance adopt-identity <config> --db <path>`. Otherwise restore or rebuild the expected brain",
                self.db_path
                    .as_deref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "<in-memory>".to_string()),
                identity.brain_uuid
            )));
        }
        Ok(identity)
    }

    fn initialize_publication_identity(
        &self,
        identity: &PublicationIdentity,
    ) -> Result<PublicationIdentity, StoreError> {
        identity.validate()?;
        let conn = self.conn()?;
        conn.query("BEGIN TRANSACTION")
            .map_err(|error| StoreError::Query(format!("begin publication identity: {error}")))?;
        let result = (|| {
            // Re-check under the write transaction so two openers cannot each
            // publish a different identity for the same legacy database.
            let brain_uuid = Self::publication_meta_value_on(&conn, BRAIN_UUID_META_KEY)?;
            let publication_uuid =
                Self::publication_meta_value_on(&conn, PUBLICATION_UUID_META_KEY)?;
            match (brain_uuid, publication_uuid) {
                (None, None) => {
                    Self::create_publication_meta_on(
                        &conn,
                        BRAIN_UUID_META_KEY,
                        &identity.brain_uuid,
                    )?;
                    Self::create_publication_meta_on(
                        &conn,
                        PUBLICATION_UUID_META_KEY,
                        &identity.publication_uuid,
                    )?;
                    Ok(identity.clone())
                }
                (Some(brain_uuid), Some(publication_uuid)) => {
                    let existing = PublicationIdentity {
                        brain_uuid,
                        publication_uuid,
                    };
                    existing.validate()?;
                    if existing != *identity {
                        Err(StoreError::Query(format!(
                            "refusing to replace existing publication identity {}/{} with {}/{}",
                            existing.brain_uuid,
                            existing.publication_uuid,
                            identity.brain_uuid,
                            identity.publication_uuid
                        )))
                    } else {
                        Ok(existing)
                    }
                }
                (brain_uuid, publication_uuid) => Err(StoreError::Query(format!(
                    "incomplete publication identity: brain_uuid={}, publication_uuid={}; repair or rebuild the database",
                    brain_uuid.is_some(),
                    publication_uuid.is_some()
                ))),
            }
        })();

        match result {
            Ok(identity) => {
                conn.query("COMMIT").map_err(|error| {
                    StoreError::Query(format!("commit publication identity: {error}"))
                })?;
                Ok(identity)
            }
            Err(error) => {
                let _ = conn.query("ROLLBACK");
                Err(error)
            }
        }
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

        // Regex v3 stores postings in disposable per-scope Tantivy shards.
        // Fresh graphs intentionally create no graph-resident posting tables;
        // older tables may remain until the mandatory fresh-reindex cutover.

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
    fn publication_identity_is_created_once_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.lbug");
        let created = GraphStore::create(&path).unwrap();
        let first = created.publication_identity().unwrap().unwrap();
        assert!(uuid::Uuid::parse_str(&first.brain_uuid).is_ok());
        assert!(uuid::Uuid::parse_str(&first.publication_uuid).is_ok());
        assert_ne!(first.brain_uuid, first.publication_uuid);
        drop(created);

        let reopened = GraphStore::open(&path).unwrap();
        assert_eq!(
            reopened.publication_identity().unwrap(),
            Some(first.clone())
        );
        drop(reopened);

        let read_only = GraphStore::open_read_only(&path).unwrap();
        assert_eq!(read_only.publication_identity().unwrap(), Some(first));
    }

    #[test]
    fn staged_publication_inherits_brain_and_gets_fresh_publication() {
        let dir = tempfile::tempdir().unwrap();
        let incumbent_path = dir.path().join("incumbent.lbug");
        let incumbent = GraphStore::create(&incumbent_path).unwrap();
        let incumbent_identity = incumbent.publication_identity().unwrap().unwrap();
        let staged_identity = incumbent_identity.next_publication().unwrap();
        assert_eq!(staged_identity.brain_uuid, incumbent_identity.brain_uuid);
        assert_ne!(
            staged_identity.publication_uuid,
            incumbent_identity.publication_uuid
        );

        let staged_path = dir.path().join("staged.lbug");
        let staged =
            GraphStore::create_with_publication_identity(&staged_path, &staged_identity).unwrap();
        assert_eq!(
            staged.publication_identity().unwrap(),
            Some(staged_identity.clone())
        );
        drop(staged);

        let reopened = GraphStore::open_read_only(&staged_path).unwrap();
        assert_eq!(
            reopened.publication_identity().unwrap(),
            Some(staged_identity)
        );
    }

    #[test]
    fn staged_publication_creation_never_overwrites_an_existing_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing.lbug");
        let incumbent = GraphStore::create(&path).unwrap();
        let incumbent_identity = incumbent.publication_identity().unwrap().unwrap();
        let staged_identity = incumbent_identity.next_publication().unwrap();
        drop(incumbent);

        let error = GraphStore::create_with_publication_identity(&path, &staged_identity)
            .err()
            .expect("existing database must be refused")
            .to_string();
        assert!(
            error.contains("refusing to create staged publication"),
            "{error}"
        );

        let reopened = GraphStore::open_read_only(&path).unwrap();
        assert_eq!(
            reopened.publication_identity().unwrap(),
            Some(incumbent_identity)
        );
    }

    #[test]
    fn expected_brain_uuid_is_value_checked_and_mismatch_fails_closed() {
        let store = GraphStore::in_memory().unwrap();
        let identity = store.publication_identity().unwrap().unwrap();
        let expected = uuid::Uuid::parse_str(&identity.brain_uuid).unwrap();
        let alternate_spelling = expected.as_braced().to_string().to_uppercase();
        assert_eq!(
            store.assert_brain_uuid(&alternate_spelling).unwrap(),
            identity
        );

        let foreign = uuid::Uuid::new_v4().to_string();
        let error = store.assert_brain_uuid(&foreign).unwrap_err().to_string();
        assert!(error.contains("brain identity mismatch"), "{error}");
        assert!(error.contains(&foreign), "{error}");
        assert!(error.contains(&identity.brain_uuid), "{error}");
    }

    #[test]
    fn partial_publication_identity_fails_closed() {
        let store = GraphStore::in_memory().unwrap();
        let conn = store.conn().unwrap();
        let mut statement = conn
            .prepare("MATCH (m:Meta {key: $key}) DETACH DELETE m")
            .unwrap();
        conn.execute(
            &mut statement,
            vec![(
                "key",
                lbug::Value::String(PUBLICATION_UUID_META_KEY.to_string()),
            )],
        )
        .unwrap();

        let error = store.publication_identity().unwrap_err().to_string();
        assert!(error.contains("incomplete publication identity"), "{error}");
        let error = store.ensure_publication_identity().unwrap_err().to_string();
        assert!(error.contains("incomplete publication identity"), "{error}");
    }

    #[test]
    fn malformed_publication_identity_fails_closed() {
        let store = GraphStore::in_memory().unwrap();
        let conn = store.conn().unwrap();
        let mut statement = conn
            .prepare("MATCH (m:Meta {key: $key}) SET m.value = $value")
            .unwrap();
        conn.execute(
            &mut statement,
            vec![
                ("key", lbug::Value::String(BRAIN_UUID_META_KEY.to_string())),
                ("value", lbug::Value::String("not-a-uuid".to_string())),
            ],
        )
        .unwrap();

        let error = store.publication_identity().unwrap_err().to_string();
        assert!(error.contains("invalid brain_uuid metadata"), "{error}");
        let error = store.ensure_publication_identity().unwrap_err().to_string();
        assert!(error.contains("invalid brain_uuid metadata"), "{error}");
    }

    #[test]
    fn equivalent_publication_identity_uuid_encodings_cannot_bypass_distinctness() {
        let store = GraphStore::in_memory().unwrap();
        let identity = store.publication_identity().unwrap().unwrap();
        let publication_uuid = uuid::Uuid::parse_str(&identity.publication_uuid).unwrap();
        let conn = store.conn().unwrap();
        for (key, value) in [
            (
                BRAIN_UUID_META_KEY,
                publication_uuid.simple().to_string().to_uppercase(),
            ),
            (
                PUBLICATION_UUID_META_KEY,
                publication_uuid.hyphenated().to_string(),
            ),
        ] {
            let mut statement = conn
                .prepare("MATCH (m:Meta {key: $key}) SET m.value = $value")
                .unwrap();
            conn.execute(
                &mut statement,
                vec![
                    ("key", lbug::Value::String(key.to_string())),
                    ("value", lbug::Value::String(value)),
                ],
            )
            .unwrap();
        }

        let error = store.publication_identity().unwrap_err().to_string();
        assert!(error.contains("must be distinct"), "{error}");
    }

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
            reopened.pagerank_scores().is_err(),
            "a dirty publication must not load canonical PageRank from before the graph commit"
        );
        // nw-C1: the same marker is now READ BACK, not merely tested for
        // existence. An unparseable payload is still `Present` — dirty, but
        // carrying no writer to attribute the publication to.
        let state = reopened.index_publication_marker_state();
        assert!(state.is_dirty());
        assert_eq!(state.record().unwrap().writer_pid, None);
        assert!(!reopened.wait_until_index_publication_clean(std::time::Duration::from_millis(30)));
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
            reopened.pagerank_scores().is_err(),
            "an unreadable marker must make canonical PageRank non-authoritative"
        );
        // nw-C1: "cannot tell" must stay distinguishable from "present".
        // Recovery keys on this: an unreadable marker is never abandoned.
        let state = reopened.index_publication_marker_state();
        assert!(
            matches!(
                state,
                crate::index_publication::MarkerState::Undeterminable(_)
            ),
            "an unreadable marker must not read as a present, attributable one: {state:?}"
        );
        assert!(state.is_dirty());
        assert!(state.record().is_none());
    }

    #[test]
    fn wait_until_index_publication_clean_is_immediate_when_already_clean() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let started = std::time::Instant::now();
        assert!(store.wait_until_index_publication_clean(std::time::Duration::from_secs(5)));
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "a clean publication must not consume any of the wait budget"
        );
    }

    #[test]
    fn wait_until_index_publication_clean_times_out_on_a_persistent_marker() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        std::fs::write(crate::index_publication::marker_path(&db_path), b"1:1\n").unwrap();
        let started = std::time::Instant::now();
        assert!(!store.wait_until_index_publication_clean(std::time::Duration::from_millis(120)));
        assert!(started.elapsed() >= std::time::Duration::from_millis(100));
    }

    #[test]
    fn wait_until_index_publication_clean_observes_marker_removal_without_the_condvar() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let marker = crate::index_publication::marker_path(&db_path);
        let store = GraphStore::open_or_create(&db_path).unwrap();
        std::fs::write(&marker, b"1:1\n").unwrap();

        // The remover touches ONLY the file. It never acquires or releases the
        // publication lease, so `index_publication_lease.available` is never
        // notified — which is precisely the situation of an out-of-process
        // writer. A condvar-based wait could not wake here.
        let remover = std::thread::spawn({
            let marker = marker.clone();
            move || {
                std::thread::sleep(std::time::Duration::from_millis(60));
                std::fs::remove_file(&marker).unwrap();
            }
        });
        assert!(
            store.wait_until_index_publication_clean(std::time::Duration::from_secs(10)),
            "the wait must observe a file-only clean transition"
        );
        assert_eq!(
            store.index_publication_waiter_count(),
            0,
            "a waiting reader must never register as a publication-lease waiter"
        );
        remover.join().unwrap();
    }

    #[test]
    fn an_interruptible_wait_abandons_the_budget_when_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        std::fs::write(crate::index_publication::marker_path(&db_path), b"1:1\n").unwrap();
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let started = std::time::Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(60));
                cancelled.store(true, Ordering::Release);
            });
            assert!(!store.wait_until_index_publication_clean_interruptible(
                std::time::Duration::from_secs(30),
                &|| cancelled.load(Ordering::Acquire)
            ));
        });
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "a cancelled wait must not sleep out its whole budget"
        );
    }

    #[test]
    fn try_acquire_index_publication_lease_declines_a_held_lease() {
        let store = GraphStore::in_memory().unwrap();
        let held = store.acquire_index_publication_lease().unwrap();
        assert!(
            store
                .try_acquire_index_publication_lease()
                .unwrap()
                .is_none(),
            "a non-blocking acquire must decline rather than queue"
        );
        assert!(!store.index_publication_lease_is_unowned());
        drop(held);
        assert!(store.index_publication_lease_is_unowned());
        assert!(
            store
                .try_acquire_index_publication_lease()
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn an_unavailable_generation_base_is_rederived_rather_than_added_to() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        // Marker present with NO `.generation` sidecar: the fail-closed load
        // takes `canonical = u64::MAX`, so `checked_add(2)` overflows and the
        // publication could never complete.
        std::fs::write(crate::index_publication::marker_path(&db_path), b"1:1\n").unwrap();
        let store = GraphStore::open_or_create(&db_path).unwrap();
        assert_eq!(store.graph_generation(), u64::MAX);

        let lease = store.acquire_index_publication_lease().unwrap();
        assert!(
            lease.preflight_generation().is_err(),
            "the u64::MAX arm must block publication before re-derivation"
        );
        let generation_path = PathBuf::from(format!("{}.generation", db_path.display()));
        assert_eq!(
            lease
                .rederive_unavailable_generation_base(&generation_path)
                .unwrap(),
            Some(0),
            "an unreadable sidecar re-derives to the same canonical 0 a clean open uses"
        );
        assert_eq!(store.graph_generation(), 1);
        lease.preflight_generation().unwrap();
        assert_eq!(
            lease
                .rederive_unavailable_generation_base(&generation_path)
                .unwrap(),
            None,
            "a usable base is left alone"
        );
    }

    #[test]
    fn rederivation_prefers_a_parseable_generation_sidecar_over_zero() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let generation_path = PathBuf::from(format!("{}.generation", db_path.display()));
        std::fs::write(crate::index_publication::marker_path(&db_path), b"1:1\n").unwrap();
        let store = GraphStore::open_or_create(&db_path).unwrap();
        assert_eq!(store.graph_generation(), u64::MAX);
        // The sidecar appears (or becomes readable) after the fail-closed open.
        std::fs::write(&generation_path, b"41").unwrap();
        let lease = store.acquire_index_publication_lease().unwrap();
        assert_eq!(
            lease
                .rederive_unavailable_generation_base(&generation_path)
                .unwrap(),
            Some(41)
        );
        assert_eq!(store.graph_generation(), 42);
        assert_eq!(lease.clean_generation().unwrap(), 43);
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

    #[test]
    fn snapshot_embedding_lease_serializes_metadata_updates() {
        use std::sync::{Arc, mpsc};
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = Arc::new(GraphStore::open_or_create(&db_path).unwrap());
        store.set_embedding_metadata("model-a", 3).unwrap();
        assert!(store.add_embedding("symbol:a", vec![1.0, 0.0, 0.0]));

        let staged = dir.path().join("snapshot-embeddings.bin");
        let lease = store.stage_embeddings_for_snapshot(&staged).unwrap();
        assert_eq!(lease.state().model_id, "model-a");
        assert_eq!(lease.state().dimension, 3);
        assert_eq!(lease.state().count, 1);

        let updater = Arc::clone(&store);
        let (completed_tx, completed_rx) = mpsc::channel();
        let join = std::thread::spawn(move || {
            updater.set_embedding_metadata("model-b", 3).unwrap();
            completed_tx.send(()).unwrap();
        });

        assert!(
            completed_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "metadata update must wait while snapshot owns the embedding lease"
        );
        assert_eq!(
            store.get_embedding_metadata().unwrap(),
            Some(("model-a".to_string(), 3)),
            "snapshot lease must retain a coherent database/vector fingerprint"
        );

        drop(lease);
        completed_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("metadata update should complete once snapshot releases the lease");
        join.join().unwrap();
        assert_eq!(
            store.get_embedding_metadata().unwrap(),
            Some(("model-b".to_string(), 3))
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
