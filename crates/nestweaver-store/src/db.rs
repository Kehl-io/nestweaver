use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::StoreError;

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
    /// Monotonic counter that bumps whenever the graph data changes (nodes
    /// or edges added/removed). Lets the web UI and other consumers detect
    /// when their view of the graph is stale without diffing the full graph.
    pub(crate) graph_generation: AtomicU64,
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
}

impl GraphStore {
    /// Create a new persistent database at `path`, initialising schema tables.
    pub fn create(path: &Path) -> Result<Self, StoreError> {
        let db = lbug::Database::new(path, lbug::SystemConfig::default())?;
        let store = GraphStore {
            db,
            pagerank_cache: Mutex::new(None),
            pagerank_generation: AtomicU64::new(0),
            graph_generation: AtomicU64::new(0),
            interaction_cache: Mutex::new(None),
            git_activity_cache: Mutex::new(None),
            git_activity_weight: Mutex::new(crate::ranking::DEFAULT_GIT_ACTIVITY_WEIGHT),
            db_path: Some(path.to_path_buf()),
        };
        store.init_schema()?;
        store.load_graph_generation(&store.generation_sidecar_path());
        Ok(store)
    }

    /// Open an existing persistent database at `path`.
    /// Runs schema migrations to ensure any new tables/columns from newer
    /// versions are present (all statements are idempotent).
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let db = lbug::Database::new(path, lbug::SystemConfig::default())?;
        let store = GraphStore {
            db,
            pagerank_cache: Mutex::new(None),
            pagerank_generation: AtomicU64::new(0),
            graph_generation: AtomicU64::new(0),
            interaction_cache: Mutex::new(None),
            git_activity_cache: Mutex::new(None),
            git_activity_weight: Mutex::new(crate::ranking::DEFAULT_GIT_ACTIVITY_WEIGHT),
            db_path: Some(path.to_path_buf()),
        };
        store.init_schema()?;
        store.load_graph_generation(&store.generation_sidecar_path());
        Ok(store)
    }

    /// Open an existing database in read-only mode. Allows concurrent access
    /// while another process (e.g. the web UI) holds the write lock.
    pub fn open_read_only(path: &Path) -> Result<Self, StoreError> {
        let config = lbug::SystemConfig::default().read_only(true);
        let db = lbug::Database::new(path, config)?;
        let store = GraphStore {
            db,
            pagerank_cache: Mutex::new(None),
            pagerank_generation: AtomicU64::new(0),
            graph_generation: AtomicU64::new(0),
            interaction_cache: Mutex::new(None),
            git_activity_cache: Mutex::new(None),
            git_activity_weight: Mutex::new(crate::ranking::DEFAULT_GIT_ACTIVITY_WEIGHT),
            db_path: Some(path.to_path_buf()),
        };
        store.load_graph_generation(&store.generation_sidecar_path());
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
            graph_generation: AtomicU64::new(0),
            interaction_cache: Mutex::new(None),
            git_activity_cache: Mutex::new(None),
            git_activity_weight: Mutex::new(crate::ranking::DEFAULT_GIT_ACTIVITY_WEIGHT),
            db_path: None,
        };
        store.init_schema()?;
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
        self.graph_generation.fetch_add(1, Ordering::AcqRel);
    }

    /// The on-disk database path this store was opened from, when known
    /// (`None` for in-memory stores). Lets callers locate sidecars.
    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
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
                PRIMARY KEY(uid))",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;

        // Migration: add `name` column to pre-existing Repo tables that lack it.
        let _ = conn.query("ALTER TABLE Repo ADD name STRING DEFAULT ''");

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
                PRIMARY KEY(uid))",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;

        // Migration: add `end_line` to pre-existing Symbol tables that lack it
        // (P0.1). Old rows default to 0 until re-indexed with `index --force`.
        let _ = conn.query("ALTER TABLE Symbol ADD end_line INT64 DEFAULT 0");

        // Migration (F2.0): add `framework_hint` to pre-existing Symbol tables.
        // Stored as "framework:role" (e.g. "spring:controller"); empty for none.
        let _ = conn.query("ALTER TABLE Symbol ADD framework_hint STRING DEFAULT ''");

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
                FROM Section TO Note, confidence FLOAT, display STRING)",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;

        conn.query(
            "CREATE REL TABLE IF NOT EXISTS WIKILINK_TO_HEADING(\
                FROM Section TO Heading, confidence FLOAT, display STRING)",
        )
        .map_err(|e| StoreError::Query(e.to_string()))?;

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

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
