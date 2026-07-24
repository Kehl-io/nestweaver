//! Tantivy BM25 index over the brain's text content.
//!
//! Lives alongside the LadybugDB graph and indexes everything searchable:
//! note titles, heading text, section bodies, tag names. Each Tantivy
//! document carries the graph node's UID + kind so search hits can be
//! resolved back into graph operations (fetch the note, run PPR from it,
//! etc.).
//!
//! Thread model: Tantivy's `IndexWriter` is exclusive (single-writer
//! per index). Read paths use cheap `Searcher` handles. The struct
//! wraps the writer in a `Mutex` so callers can share a `TantivyIndex`
//! across threads — the watcher and MCP server can both hold one.
//!
//! Persistence: the index lives at `<db_path>.tantivy/`. Tantivy handles
//! its own mmap/segment lifecycle; we just point it at the directory.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tantivy::collector::{Collector, SegmentCollector, TopDocs};
use tantivy::query::{Query, QueryParser};
use tantivy::schema::{Field, STORED, STRING, Schema, TEXT, Value};
use tantivy::store::StoreReader;
use tantivy::{
    DocAddress, DocId, Index, IndexReader, ReloadPolicy, Score, SegmentOrdinal, SegmentReader,
    TantivyDocument, Term, doc,
};
use thiserror::Error;

use crate::db::GraphStore;
use crate::error::StoreError;

/// Pseudo-relevance-feedback (PRF) tuning constants (Feature F7, PRF half).
///
/// PRF is a two-pass query-expansion technique that improves recall on
/// natural-language queries:
///
/// 1. **Pass 1** runs the original query and takes the top
///    [`PRF_TOP_K`] hits as a pseudo-relevant set.
/// 2. **Term mining** tokenizes those hits' bodies, keeps terms whose IDF
///    is above the median (rare, discriminating terms), ranks them by
///    `IDF × term-frequency-in-top-K`, drops query terms + stopwords, and
///    keeps the top [`PRF_EXPANSION_TERMS`].
/// 3. **Pass 2** re-runs BM25 with the original terms at weight 1.0 and the
///    mined terms at [`PRF_EXPANSION_WEIGHT`], capping the total query
///    length at [`PRF_MAX_QUERY_TERMS`] terms.
///
/// Query-drift guardrails (all documented inline at their use sites):
/// - **Cap N** ([`PRF_EXPANSION_TERMS`]) — bounds how far the query can move.
/// - **Prefer high IDF** (above-median filter) — only rare, topical terms
///   expand the query; common words can't drift it toward generic text.
/// - **Down-weight** ([`PRF_EXPANSION_WEIGHT`] = 0.3) — expansion terms
///   nudge ranking rather than dominating the original intent.
/// - **Length cap** ([`PRF_MAX_QUERY_TERMS`]) — a hard ceiling on pass-2
///   query size regardless of N.
pub const PRF_TOP_K: usize = 5;
/// Maximum number of expansion terms mined from the pseudo-relevant set.
pub const PRF_EXPANSION_TERMS: usize = 10;
/// Boost applied to expansion terms in the pass-2 query (original terms = 1.0).
pub const PRF_EXPANSION_WEIGHT: f32 = 0.3;
/// Hard cap on the total number of terms in the pass-2 query.
pub const PRF_MAX_QUERY_TERMS: usize = 64;

/// Maximum number of distinct logical entities retained while counting a
/// Tantivy query. Reaching this cap yields a lower-bound total instead of
/// growing memory with corpus cardinality.
pub const TANTIVY_LOGICAL_COUNT_CAP: usize = 100_000;

/// Maximum number of ranked hits any public search entry point may retain.
/// Cardinality precision has its own, independent 100K bound above.
pub const SEARCH_PRESENTATION_LIMIT_MAX: usize = 10_000;

/// Single search result from the BM25 index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub uid: String,
    pub kind: String,
    pub title: String,
    pub vault_uid: String,
    /// Owning note identity stored in the indexed document. Notes carry their
    /// own UID, headings/sections carry their parent note UID, and standalone
    /// entities leave this empty.
    #[serde(default)]
    pub note_uid: String,
    pub score: f32,
}

impl SearchHit {
    /// Return the same validated logical identity used by counted search.
    pub fn logical_identity(&self) -> Result<SearchLogicalIdentity, String> {
        logical_entity_identity(&self.uid, &self.kind, &self.note_uid)
    }
}

/// Whether a bounded search total is exact or only a guaranteed lower bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchTotalRelation {
    Exact,
    LowerBound,
}

/// Honest cardinality metadata shared by counted search APIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchTotal {
    pub value: usize,
    pub relation: SearchTotalRelation,
}

impl SearchTotal {
    pub const fn exact(value: usize) -> Self {
        Self {
            value,
            relation: SearchTotalRelation::Exact,
        }
    }

    pub const fn lower_bound(value: usize) -> Self {
        Self {
            value,
            relation: SearchTotalRelation::LowerBound,
        }
    }
}

/// A ranked Tantivy hit page plus limit-independent logical cardinality.
///
/// `hits` remain raw ranked index documents for compatibility. `total`
/// instead counts logical entities: a note and all of its heading/section
/// fragments share the owning `note_uid`; standalone entities use
/// `(kind, uid)`. Titles never participate in identity.
#[derive(Debug, Clone)]
pub struct TantivySearchPage {
    pub hits: Vec<SearchHit>,
    pub total: SearchTotal,
}

/// Counted result of the final PRF query plus its mined expansion terms.
#[derive(Debug, Clone)]
pub struct TantivyPrfSearchPage {
    pub hits: Vec<SearchHit>,
    pub total: SearchTotal,
    pub expansion_terms: Vec<String>,
}

/// Validated logical entity identity shared by counted totals and hit grouping.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SearchLogicalIdentity {
    Note(String),
    Standalone { kind: String, uid: String },
}

#[derive(Default)]
struct LogicalEntityCountState {
    identities: HashSet<SearchLogicalIdentity>,
    error: Option<String>,
}

/// Tantivy collector that keeps only a capped set of logical identities.
/// The shared state makes the cap global across segments, including when
/// Tantivy evaluates segment collectors concurrently.
struct LogicalEntityCountCollector {
    cap: usize,
    uid: Field,
    kind: Field,
    note_uid: Field,
    state: Arc<Mutex<LogicalEntityCountState>>,
    saturated: Arc<AtomicBool>,
}

impl LogicalEntityCountCollector {
    fn new(cap: usize, fields: &Fields) -> Self {
        Self {
            cap,
            uid: fields.uid,
            kind: fields.kind,
            note_uid: fields.note_uid,
            state: Arc::new(Mutex::new(LogicalEntityCountState::default())),
            saturated: Arc::new(AtomicBool::new(false)),
        }
    }
}

struct LogicalEntitySegmentCollector {
    cap: usize,
    uid: Field,
    kind: Field,
    note_uid: Field,
    store: StoreReader,
    state: Arc<Mutex<LogicalEntityCountState>>,
    saturated: Arc<AtomicBool>,
}

impl SegmentCollector for LogicalEntitySegmentCollector {
    type Fruit = ();

    fn collect(&mut self, doc_id: DocId, _score: Score) {
        if self.saturated.load(Ordering::Relaxed) {
            return;
        }

        let doc: TantivyDocument = match self.store.get(doc_id) {
            Ok(doc) => doc,
            Err(error) => {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                state.error = Some(format!("decode matched document: {error}"));
                self.saturated.store(true, Ordering::Relaxed);
                return;
            }
        };
        let uid = extract_text(&doc, self.uid);
        let kind = extract_text(&doc, self.kind);
        let note_uid = extract_text(&doc, self.note_uid);
        let identity = match logical_entity_identity(&uid, &kind, &note_uid) {
            Ok(identity) => identity,
            Err(error) => {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                state.error = Some(error);
                self.saturated.store(true, Ordering::Relaxed);
                return;
            }
        };

        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.error.is_some() {
            return;
        }
        if state.identities.len() >= self.cap {
            self.saturated.store(true, Ordering::Relaxed);
            return;
        }
        state.identities.insert(identity);
        if state.identities.len() >= self.cap {
            self.saturated.store(true, Ordering::Relaxed);
        }
    }

    fn harvest(self) -> Self::Fruit {}
}

fn logical_entity_identity(
    uid: &str,
    kind: &str,
    note_uid: &str,
) -> Result<SearchLogicalIdentity, String> {
    if kind.trim().is_empty() || kind.trim() != kind || kind.contains('\0') {
        return Err("invalid logical identity: kind is missing or malformed".to_string());
    }
    if uid.trim().is_empty() || uid.trim() != uid || uid.contains('\0') {
        return Err(format!(
            "invalid logical identity: {kind} document UID is missing or malformed"
        ));
    }

    match kind {
        "note" => {
            if note_uid.trim().is_empty() || note_uid.trim() != note_uid || note_uid.contains('\0')
            {
                return Err(format!(
                    "invalid logical identity: note {uid} has no owning note_uid"
                ));
            }
            if note_uid != uid {
                return Err(format!(
                    "invalid logical identity: note UID {uid} disagrees with note_uid {note_uid}"
                ));
            }
            Ok(SearchLogicalIdentity::Note(uid.to_string()))
        }
        "heading" | "section" => {
            if note_uid.trim().is_empty() || note_uid.trim() != note_uid || note_uid.contains('\0')
            {
                return Err(format!(
                    "invalid logical identity: {kind} {uid} has no owning note_uid"
                ));
            }
            Ok(SearchLogicalIdentity::Note(note_uid.to_string()))
        }
        _ => Ok(SearchLogicalIdentity::Standalone {
            kind: kind.to_string(),
            uid: uid.to_string(),
        }),
    }
}

impl Collector for LogicalEntityCountCollector {
    type Fruit = SearchTotal;
    type Child = LogicalEntitySegmentCollector;

    fn for_segment(
        &self,
        _segment_local_id: SegmentOrdinal,
        segment: &SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        Ok(LogicalEntitySegmentCollector {
            cap: self.cap,
            uid: self.uid,
            kind: self.kind,
            note_uid: self.note_uid,
            store: segment.get_store_reader(10)?,
            state: Arc::clone(&self.state),
            saturated: Arc::clone(&self.saturated),
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(&self, _segment_fruits: Vec<()>) -> tantivy::Result<Self::Fruit> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(error) = &state.error {
            return Err(tantivy::TantivyError::InternalError(error.clone()));
        }
        let value = state.identities.len();
        if self.saturated.load(Ordering::Relaxed) {
            Ok(SearchTotal::lower_bound(value))
        } else {
            Ok(SearchTotal::exact(value))
        }
    }
}

#[derive(Debug, Error)]
pub enum TantivyError {
    #[error("tantivy: {0}")]
    Tantivy(String),
    #[error("io: {0}")]
    Io(String),
    #[error("writer unavailable: index opened in read-only mode")]
    WriterUnavailable,
    #[error("presentation limit {limit} exceeds maximum {max}")]
    PresentationLimitExceeded { limit: usize, max: usize },
    #[error("counted search requires reindex: index schema does not store note ownership")]
    CountedSearchRequiresReindex,
    #[error("invalid index schema: {0}")]
    InvalidSchema(String),
    #[error("index was migrated to the current schema; reopen it before further use")]
    IndexReopenRequired,
}

impl From<tantivy::TantivyError> for TantivyError {
    fn from(e: tantivy::TantivyError) -> Self {
        TantivyError::Tantivy(e.to_string())
    }
}

impl From<std::io::Error> for TantivyError {
    fn from(e: std::io::Error) -> Self {
        TantivyError::Io(e.to_string())
    }
}

impl From<TantivyError> for StoreError {
    fn from(e: TantivyError) -> Self {
        match e {
            TantivyError::PresentationLimitExceeded { limit, max } => {
                StoreError::PresentationLimitExceeded { limit, max }
            }
            other => StoreError::Query(other.to_string()),
        }
    }
}

/// BM25 index over the brain's text content.
///
/// Two constructors:
/// - `open_or_create` — opens both a reader and writer. Use this in
///   processes that need to write (the brain watcher, `brain add`,
///   `brain reindex-search`).
/// - `open_reader_only` — opens only a reader. Use this in processes
///   that only need to search (CLI search, MCP server, web UI). This
///   avoids contending for the writer lock with a running watcher.
///
/// Write methods (`reindex_from_store`, `update_note`, `remove_note`)
/// return `TantivyError::WriterUnavailable` when called on a
/// reader-only instance.
pub struct TantivyIndex {
    index: Index,
    reader: IndexReader,
    writer: Option<Mutex<tantivy::IndexWriter>>,
    fields: Fields,
    path: PathBuf,
    counted_search_supported: bool,
    migration_completed: AtomicBool,
}

/// Field handles bundled together so we don't look them up by name on
/// every operation.
#[derive(Clone, Copy)]
struct Fields {
    uid: Field,
    kind: Field,
    title: Field,
    body: Field,
    vault_uid: Field,
    note_uid: Field, // for batch deletion of all docs belonging to a note
}

impl TantivyIndex {
    /// Open the on-disk index at `path` or create it if missing. The
    /// directory is created if needed. Existing indices keep their
    /// segments; callers wanting a fresh start should remove the
    /// directory first.
    pub fn open_or_create(path: &Path) -> Result<Self, TantivyError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        recover_interrupted_reindex(path)?;
        std::fs::create_dir_all(path)?;

        // Only create into an empty directory. A malformed non-empty index is
        // surfaced instead of being mistaken for a missing index.
        let index = match Index::open_in_dir(path) {
            Ok(idx) => idx,
            Err(_) if directory_is_empty(path)? => Index::create_in_dir(path, build_schema())?,
            Err(open_error) => return Err(open_error.into()),
        };
        let schema = index.schema();
        let schema = inspect_schema(&schema)?;

        // ~50 MB write buffer — sufficient for vaults up to ~50K
        // documents per the architecture doc's memory budget.
        let writer = index.writer(50_000_000)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;

        Ok(Self {
            index,
            reader,
            writer: Some(Mutex::new(writer)),
            fields: schema.fields,
            path: path.to_path_buf(),
            counted_search_supported: schema.current,
            migration_completed: AtomicBool::new(false),
        })
    }

    /// Open the on-disk index at `path` in read-only mode. No writer is
    /// acquired, so this will succeed even when another process (e.g. the
    /// brain watcher) holds the writer lock. Write operations on the
    /// returned instance will return `TantivyError::WriterUnavailable`.
    ///
    /// Returns `Err` if the index directory does not exist or is corrupt.
    pub fn open_reader_only(path: &Path) -> Result<Self, TantivyError> {
        recover_interrupted_reindex(path)?;
        let index = Index::open_in_dir(path)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        let schema = inspect_schema(&index.schema())?;

        Ok(Self {
            index,
            reader,
            writer: None,
            fields: schema.fields,
            path: path.to_path_buf(),
            counted_search_supported: schema.current,
            migration_completed: AtomicBool::new(false),
        })
    }

    /// Drop every document and rebuild from the current state of `store`.
    /// Use after a fresh `index_markdown_directory` or as a manual escape
    /// hatch (`nestweaver brain reindex-search`).
    ///
    /// When this call successfully migrates a legacy schema, it atomically
    /// replaces the index directory and retires this handle's cached Tantivy
    /// state. Every subsequent fallible operation on this handle returns
    /// [`TantivyError::IndexReopenRequired`]; drop it and reopen the index
    /// before further use.
    ///
    /// Atomicity invariant: the `delete_all_documents` and the re-adds MUST
    /// land in a SINGLE commit. Tantivy commits are atomic per generation, so
    /// a concurrent reader then sees the old-or-new full corpus and never an
    /// empty window. Do NOT add an intermediate `commit()` after the delete —
    /// see `reindex_from_store_is_atomic_for_readers`.
    pub fn reindex_from_store(&self, store: &GraphStore) -> Result<usize, TantivyError> {
        self.ensure_reopen_not_required()?;
        let writer_mutex = self
            .writer
            .as_ref()
            .ok_or(TantivyError::WriterUnavailable)?;
        let mut writer = writer_mutex
            .lock()
            .map_err(|e| TantivyError::Tantivy(format!("writer lock poisoned: {e}")))?;
        if !self.counted_search_supported {
            return self.rebuild_current_schema_atomically(store, writer);
        }
        writer.delete_all_documents()?;
        let count = self.write_full_corpus(&mut writer, store)?;
        writer.commit()?;
        // Manually reload the reader so subsequent searches see the new
        // segments without waiting for the OnCommitWithDelay tick.
        self.reader.reload()?;
        Ok(count)
    }

    fn ensure_reopen_not_required(&self) -> Result<(), TantivyError> {
        if self.migration_completed.load(Ordering::Acquire) {
            Err(TantivyError::IndexReopenRequired)
        } else {
            Ok(())
        }
    }

    fn rebuild_current_schema_atomically(
        &self,
        store: &GraphStore,
        old_writer: std::sync::MutexGuard<'_, tantivy::IndexWriter>,
    ) -> Result<usize, TantivyError> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        let staging_dir = tempfile::Builder::new()
            .prefix(".nestweaver-tantivy-reindex-")
            .tempdir_in(parent)?;
        let staging_index = Index::create_in_dir(staging_dir.path(), build_schema())?;
        let staging_schema = inspect_schema(&staging_index.schema())?;
        if !staging_schema.current {
            return Err(TantivyError::InvalidSchema(
                "newly built schema is not current".to_string(),
            ));
        }
        let mut staging_writer = staging_index.writer(50_000_000)?;
        let count = Self::write_full_corpus_with_fields(
            &mut staging_writer,
            store,
            &staging_schema.fields,
        )?;
        staging_writer.commit()?;
        drop(staging_writer);
        drop(staging_index);

        // Validate the complete replacement before touching the usable index.
        let verification = Index::open_in_dir(staging_dir.path())?;
        if !inspect_schema(&verification.schema())?.current {
            return Err(TantivyError::InvalidSchema(
                "rebuilt index did not persist the current schema".to_string(),
            ));
        }
        drop(verification);

        let backup = reindex_backup_path(&self.path);
        if backup.exists() {
            return Err(TantivyError::Io(format!(
                "refusing index migration while recovery directory exists: {}",
                backup.display()
            )));
        }
        let staging_path = staging_dir.keep();

        std::fs::rename(&self.path, &backup).map_err(|error| {
            let _ = std::fs::remove_dir_all(&staging_path);
            TantivyError::Io(format!(
                "move existing index {} to recovery directory {}: {error}",
                self.path.display(),
                backup.display()
            ))
        })?;
        if let Err(error) = crate::durable_sidecar::sync_parent_directory_durable(&self.path) {
            let rollback = std::fs::rename(&backup, &self.path);
            let _ = crate::durable_sidecar::sync_parent_directory_durable(&self.path);
            let _ = std::fs::remove_dir_all(&staging_path);
            return Err(TantivyError::Io(format!(
                "persist index recovery rename: {error}; rollback: {rollback:?}"
            )));
        }

        if let Err(error) = std::fs::rename(&staging_path, &self.path) {
            let rollback = std::fs::rename(&backup, &self.path);
            let _ = crate::durable_sidecar::sync_parent_directory_durable(&self.path);
            let _ = std::fs::remove_dir_all(&staging_path);
            return Err(TantivyError::Io(format!(
                "install rebuilt index at {}: {error}; rollback: {rollback:?}",
                self.path.display()
            )));
        }
        self.migration_completed.store(true, Ordering::Release);

        // From here onward a complete new index and the old recovery copy both
        // exist. Persist and verify the new namespace before retiring the old.
        crate::durable_sidecar::sync_parent_directory_durable(&self.path)?;
        let installed = Index::open_in_dir(&self.path)?;
        if !inspect_schema(&installed.schema())?.current {
            return Err(TantivyError::InvalidSchema(
                "installed replacement schema is not current; old index remains recoverable"
                    .to_string(),
            ));
        }
        drop(installed);
        drop(old_writer);

        std::fs::remove_dir_all(&backup)?;
        crate::durable_sidecar::sync_parent_directory_durable(&backup)?;
        Ok(count)
    }

    /// Per-note incremental update. Drops every Tantivy doc tagged with
    /// `note_uid` (the note itself + all its headings + sections) and
    /// re-indexes the supplied fresh data. Called by the file watcher
    /// after re-parsing a saved file.
    ///
    /// `sections` is a slice of `(uid, body_text, heading_title)` tuples.
    /// The heading title is indexed in the section's `title` field so that
    /// heading-name searches also surface section body content.
    #[allow(clippy::too_many_arguments)]
    pub fn update_note(
        &self,
        note_uid: &str,
        title: &str,
        vault_uid: &str,
        body_chunks: &[String],
        headings: &[(String, String)],
        sections: &[(String, String, String)],
        tags: &[String],
    ) -> Result<(), TantivyError> {
        self.ensure_reopen_not_required()?;
        let writer_mutex = self
            .writer
            .as_ref()
            .ok_or(TantivyError::WriterUnavailable)?;
        let mut writer = writer_mutex
            .lock()
            .map_err(|e| TantivyError::Tantivy(format!("writer lock poisoned: {e}")))?;
        // Remove all docs with this note_uid first.
        writer.delete_term(Term::from_field_text(self.fields.note_uid, note_uid));
        // Add fresh.
        // 1. The note itself (body = concatenated chunks for whole-note BM25).
        let combined_body = body_chunks.join("\n\n");
        writer.add_document(doc!(
            self.fields.uid => note_uid.to_string(),
            self.fields.kind => "note".to_string(),
            self.fields.title => title.to_string(),
            self.fields.body => combined_body,
            self.fields.vault_uid => vault_uid.to_string(),
            self.fields.note_uid => note_uid.to_string(),
        ))?;
        // 2. Per-heading docs.
        for (h_uid, h_text) in headings {
            writer.add_document(doc!(
                self.fields.uid => h_uid.to_string(),
                self.fields.kind => "heading".to_string(),
                self.fields.title => h_text.to_string(),
                self.fields.body => h_text.to_string(),
                self.fields.vault_uid => vault_uid.to_string(),
                self.fields.note_uid => note_uid.to_string(),
            ))?;
        }
        // 3. Per-section docs — heading text in title, body text in body.
        for (s_uid, s_body, s_heading_title) in sections {
            writer.add_document(doc!(
                self.fields.uid => s_uid.to_string(),
                self.fields.kind => "section".to_string(),
                self.fields.title => s_heading_title.to_string(),
                self.fields.body => s_body.to_string(),
                self.fields.vault_uid => vault_uid.to_string(),
                self.fields.note_uid => note_uid.to_string(),
            ))?;
        }
        // 4. Tags are indexed once globally (in reindex_from_store), not
        //    per note — multiple notes share tags. Skip here.
        let _ = tags;

        writer.commit()?;
        self.reader.reload()?;
        Ok(())
    }

    /// Batch variant of [`Self::update_note`]: processes every entry in
    /// `notes` — deleting old docs and adding fresh ones — then issues a
    /// **single** commit and reader reload at the end.
    ///
    /// Use this when updating many notes at once (e.g. an initial corpus
    /// build, a vault-wide re-parse). N individual `update_note` calls would
    /// issue N Tantivy commits (each triggering an fsync + segment merge);
    /// this method replaces that with 1 commit regardless of batch size.
    ///
    /// Each tuple in `notes` has the same shape as the parameters of
    /// `update_note`:
    /// `(note_uid, title, vault_uid, body_chunks, headings, sections, tags)`
    ///
    /// `headings` entries are `(uid, heading_text)`.
    /// `sections` entries are `(uid, body_text, heading_title)`.
    #[allow(clippy::type_complexity)]
    pub fn update_notes_batch(
        &self,
        notes: &[(
            String,
            String,
            String,
            Vec<String>,
            Vec<(String, String)>,
            Vec<(String, String, String)>,
            Vec<String>,
        )],
    ) -> Result<(), TantivyError> {
        self.ensure_reopen_not_required()?;
        let writer_mutex = self
            .writer
            .as_ref()
            .ok_or(TantivyError::WriterUnavailable)?;
        let mut writer = writer_mutex
            .lock()
            .map_err(|e| TantivyError::Tantivy(format!("writer lock poisoned: {e}")))?;

        for (note_uid, title, vault_uid, body_chunks, headings, sections, tags) in notes {
            // Remove all existing docs for this note.
            writer.delete_term(Term::from_field_text(self.fields.note_uid, note_uid));

            // 1. The note itself.
            let combined_body = body_chunks.join("\n\n");
            writer.add_document(doc!(
                self.fields.uid => note_uid.clone(),
                self.fields.kind => "note".to_string(),
                self.fields.title => title.clone(),
                self.fields.body => combined_body,
                self.fields.vault_uid => vault_uid.clone(),
                self.fields.note_uid => note_uid.clone(),
            ))?;

            // 2. Per-heading docs.
            for (h_uid, h_text) in headings {
                writer.add_document(doc!(
                    self.fields.uid => h_uid.clone(),
                    self.fields.kind => "heading".to_string(),
                    self.fields.title => h_text.clone(),
                    self.fields.body => h_text.clone(),
                    self.fields.vault_uid => vault_uid.clone(),
                    self.fields.note_uid => note_uid.clone(),
                ))?;
            }

            // 3. Per-section docs — heading text in title, body text in body.
            for (s_uid, s_body, s_heading_title) in sections {
                writer.add_document(doc!(
                    self.fields.uid => s_uid.clone(),
                    self.fields.kind => "section".to_string(),
                    self.fields.title => s_heading_title.clone(),
                    self.fields.body => s_body.clone(),
                    self.fields.vault_uid => vault_uid.clone(),
                    self.fields.note_uid => note_uid.clone(),
                ))?;
            }

            // Tags are indexed globally in reindex_from_store, not per note.
            let _ = tags;
        }

        // Single commit for the entire batch.
        writer.commit()?;
        drop(writer);
        self.reader.reload()?;
        Ok(())
    }

    /// Drop every Tantivy doc belonging to `note_uid`. Called by the
    /// watcher on file delete.
    pub fn remove_note(&self, note_uid: &str) -> Result<(), TantivyError> {
        self.ensure_reopen_not_required()?;
        let writer_mutex = self
            .writer
            .as_ref()
            .ok_or(TantivyError::WriterUnavailable)?;
        let mut writer = writer_mutex
            .lock()
            .map_err(|e| TantivyError::Tantivy(format!("writer lock poisoned: {e}")))?;
        writer.delete_term(Term::from_field_text(self.fields.note_uid, note_uid));
        writer.commit()?;
        self.reader.reload()?;
        Ok(())
    }

    /// Reload the reader so subsequent searches see any newly committed
    /// segments. This is cheap: it just checks for new segment metadata
    /// and opens any new segment readers. A no-op when nothing changed.
    pub fn reload(&self) -> Result<(), TantivyError> {
        self.ensure_reopen_not_required()?;
        self.reader.reload()?;
        Ok(())
    }

    /// BM25 search across title + body fields. Returns up to `limit`
    /// hits ranked by Tantivy's default BM25 scoring.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, TantivyError> {
        validate_presentation_limit(limit)?;
        self.ensure_reopen_not_required()?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let Some(parsed) = self.parse_query(query) else {
            return Ok(Vec::new());
        };
        let searcher = self.current_searcher();
        let top = searcher.search(
            parsed.as_ref(),
            &TopDocs::with_limit(limit).order_by_score(),
        )?;
        self.decode_hits(&searcher, top)
    }

    /// BM25 search with a logical-entity total independent of `limit`.
    ///
    /// Ranked hits intentionally remain raw Tantivy documents so existing
    /// ordering and downstream grouping do not change. The total collapses a
    /// note's whole-note, heading, and section documents by `note_uid`; other
    /// entities use `(kind, uid)`. Once [`TANTIVY_LOGICAL_COUNT_CAP`] distinct
    /// identities are observed, the total is returned as a lower bound.
    pub fn search_page(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<TantivySearchPage, TantivyError> {
        self.search_page_with_count_cap(query, limit, TANTIVY_LOGICAL_COUNT_CAP)
    }

    fn search_page_with_count_cap(
        &self,
        query: &str,
        limit: usize,
        count_cap: usize,
    ) -> Result<TantivySearchPage, TantivyError> {
        validate_presentation_limit(limit)?;
        self.ensure_reopen_not_required()?;
        if !self.counted_search_supported {
            return Err(TantivyError::CountedSearchRequiresReindex);
        }
        let Some(parsed) = self.parse_query(query) else {
            return Ok(TantivySearchPage {
                hits: Vec::new(),
                total: SearchTotal::exact(0),
            });
        };
        let searcher = self.current_searcher();
        let counter = LogicalEntityCountCollector::new(count_cap, &self.fields);
        if limit == 0 {
            let total = searcher.search(parsed.as_ref(), &counter)?;
            return Ok(TantivySearchPage {
                hits: Vec::new(),
                total,
            });
        }
        let (total, top) = searcher.search(
            parsed.as_ref(),
            &(counter, TopDocs::with_limit(limit).order_by_score()),
        )?;
        Ok(TantivySearchPage {
            hits: self.decode_hits(&searcher, top)?,
            total,
        })
    }

    /// Parse user input with the legacy lenient fallbacks shared by hit-only
    /// and counted search. `None` means the input is empty or cannot be parsed
    /// even as a literal phrase, preserving the old empty-result behavior.
    fn parse_query(&self, query: &str) -> Option<Box<dyn Query>> {
        if query.trim().is_empty() {
            return None;
        }
        let mut parser =
            QueryParser::for_index(&self.index, vec![self.fields.title, self.fields.body]);
        parser.set_field_boost(self.fields.title, 3.0);
        match parser.parse_query(query) {
            Ok(q) => q,
            Err(_) => {
                // Fall back to a "lenient" parse that escapes special
                // chars by quoting — handles common user input like
                // `auth/v2` or `path:foo` that breaks the parser.
                let escaped = escape_query(query);
                match parser.parse_query(&escaped) {
                    Ok(q) => q,
                    // Last resort: a bare boolean keyword (`a AND`), an unbalanced
                    // quote, or other residual syntax that escape_query doesn't
                    // neutralize (it only quotes tokens with special *chars*, not
                    // keywords) must not hard-error a plain search. Treat the whole
                    // input as a literal phrase; if even that won't parse, return no
                    // hits rather than an error.
                    Err(_) => {
                        let phrase = format!("\"{}\"", query.replace('"', " "));
                        match parser.parse_query(&phrase) {
                            Ok(q) => q,
                            Err(_) => return None,
                        }
                    }
                }
            }
        }
        .into()
    }

    fn current_searcher(&self) -> tantivy::Searcher {
        // Preserve the legacy best-effort reload behavior: a transient reload
        // failure does not make an otherwise readable committed generation
        // unavailable.
        let _ = self.reader.reload();
        self.reader.searcher()
    }

    fn decode_hits(
        &self,
        searcher: &tantivy::Searcher,
        top: Vec<(Score, DocAddress)>,
    ) -> Result<Vec<SearchHit>, TantivyError> {
        let mut hits = Vec::with_capacity(top.len());
        for (score, address) in top {
            let doc: TantivyDocument = searcher.doc(address)?;
            hits.push(SearchHit {
                uid: extract_text(&doc, self.fields.uid),
                kind: extract_text(&doc, self.fields.kind),
                title: extract_text(&doc, self.fields.title),
                vault_uid: extract_text(&doc, self.fields.vault_uid),
                note_uid: extract_text(&doc, self.fields.note_uid),
                score,
            });
        }
        Ok(hits)
    }

    /// Two-pass pseudo-relevance-feedback (PRF) search (Feature F7).
    ///
    /// Pass 1 runs the original `query`; the top [`PRF_TOP_K`] hits form the
    /// pseudo-relevant set. Expansion terms are mined from those hits' bodies
    /// via [`Self::prf_expand_terms`]. Pass 2 re-runs BM25 with the original
    /// query at weight 1.0 plus each expansion term boosted by
    /// [`PRF_EXPANSION_WEIGHT`], capping the combined query at
    /// [`PRF_MAX_QUERY_TERMS`] terms.
    ///
    /// `stopwords` is supplied by the caller (the engine threads its built-in
    /// `cross_domain::STOPLIST` through) so the store crate need not own a
    /// second stoplist. Entries should be lowercase.
    ///
    /// Returns the pass-2 hits and the mined expansion terms (for `--debug` /
    /// response auditing). When no expansion terms are found (empty corpus,
    /// pre-PRF index with no stored bodies, or every candidate filtered out),
    /// the pass-1 hits are returned unchanged with an empty term list — so PRF
    /// can never make results worse than the plain query.
    pub fn search_prf(
        &self,
        query: &str,
        limit: usize,
        stopwords: &[&str],
    ) -> Result<(Vec<SearchHit>, Vec<String>), TantivyError> {
        validate_presentation_limit(limit)?;
        self.ensure_reopen_not_required()?;
        if query.trim().is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let (mut pass1, terms) = self.prf_pass1_and_terms(query, limit, stopwords)?;
        if terms.is_empty() {
            pass1.truncate(limit);
            return Ok((pass1, terms));
        }
        let expanded = build_prf_query(query, &terms, PRF_EXPANSION_WEIGHT, PRF_MAX_QUERY_TERMS);
        Ok((self.search(&expanded, limit)?, terms))
    }

    /// Counted PRF search. `total` always describes the final query: the
    /// expanded second pass when expansion succeeds, otherwise the original
    /// first pass.
    pub fn search_prf_page(
        &self,
        query: &str,
        limit: usize,
        stopwords: &[&str],
    ) -> Result<TantivyPrfSearchPage, TantivyError> {
        validate_presentation_limit(limit)?;
        self.ensure_reopen_not_required()?;
        if !self.counted_search_supported {
            return Err(TantivyError::CountedSearchRequiresReindex);
        }
        if query.trim().is_empty() {
            return Ok(TantivyPrfSearchPage {
                hits: Vec::new(),
                total: SearchTotal::exact(0),
                expansion_terms: Vec::new(),
            });
        }
        let (_pass1, terms) = self.prf_pass1_and_terms(query, limit, stopwords)?;
        let final_query = if terms.is_empty() {
            query.to_string()
        } else {
            build_prf_query(query, &terms, PRF_EXPANSION_WEIGHT, PRF_MAX_QUERY_TERMS)
        };
        let page = self.search_page(&final_query, limit)?;
        Ok(TantivyPrfSearchPage {
            hits: page.hits,
            total: page.total,
            expansion_terms: terms,
        })
    }

    fn prf_pass1_and_terms(
        &self,
        query: &str,
        limit: usize,
        stopwords: &[&str],
    ) -> Result<(Vec<SearchHit>, Vec<String>), TantivyError> {
        // Pass 1: original query, take the top-K as the pseudo-relevant set.
        let pass1 = self.search(query, limit.max(PRF_TOP_K))?;
        // The full-text corpus holds several documents per note (a whole-note
        // doc plus one doc per heading and per section), and the note doc and
        // its section docs largely share the same prose. Taking the raw top-K
        // hits therefore lets a single note's near-duplicate docs crowd the
        // pseudo-relevant set, starving lower-ranked notes that carry the most
        // distinctive vocabulary. Collapse hits to one per source note before
        // selecting the top-K so K distinct notes — not K duplicate fragments —
        // seed the term mining.
        let searcher = self.reader.searcher();
        let mut seen_notes: HashSet<String> = HashSet::new();
        let mut top_k: Vec<&SearchHit> = Vec::with_capacity(PRF_TOP_K);
        for hit in &pass1 {
            // Group key: the owning note when known, else the hit's own uid
            // (tags and orphan docs have no note_uid and stand alone).
            let key = self
                .fetch_note_uid(&searcher, &hit.uid)?
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| hit.uid.clone());
            if seen_notes.insert(key) {
                top_k.push(hit);
                if top_k.len() >= PRF_TOP_K {
                    break;
                }
            }
        }
        let terms = self.prf_expand_terms(query, &top_k, stopwords)?;
        Ok((pass1, terms))
    }

    /// Mine expansion terms from a pseudo-relevant set of hits (Feature F7).
    ///
    /// Tokenizes each hit's stored `body` with the index's own `body`
    /// tokenizer (so terms match what BM25 indexed), counts term frequency
    /// across the set, computes each term's IDF from the corpus
    /// (`ln(num_docs / (1 + doc_freq))`), keeps only terms whose IDF is above
    /// the median (the high-IDF / rare-term guardrail), then ranks the
    /// survivors by `IDF × term-frequency-in-top-K` and returns the top
    /// [`PRF_EXPANSION_TERMS`]. Query terms and `stopwords` are excluded, as
    /// are pure-numeric and single-character tokens.
    pub fn prf_expand_terms(
        &self,
        query: &str,
        hits: &[&SearchHit],
        stopwords: &[&str],
    ) -> Result<Vec<String>, TantivyError> {
        use std::collections::{HashMap, HashSet};

        self.ensure_reopen_not_required()?;
        if hits.is_empty() {
            return Ok(Vec::new());
        }
        let searcher = self.reader.searcher();
        let num_docs = searcher.num_docs().max(1) as f64;

        // Query terms to exclude (lowercased, whitespace-split is sufficient —
        // the original query reaches BM25 verbatim in pass 2).
        let query_terms: HashSet<String> =
            query.split_whitespace().map(|w| w.to_lowercase()).collect();
        let stop: HashSet<&str> = stopwords.iter().copied().collect();

        // Tokenize each hit's stored body with the body field's analyzer and
        // accumulate term frequencies across the pseudo-relevant set.
        let mut analyzer = self.index.tokenizer_for_field(self.fields.body)?;
        let mut tf: HashMap<String, u64> = HashMap::new();
        for hit in hits {
            let Some(body) = self.fetch_body(&searcher, &hit.uid)? else {
                continue;
            };
            let mut stream = analyzer.token_stream(&body);
            while let Some(token) = stream.next() {
                let t = &token.text;
                if t.len() < 3 || t.chars().all(|c| c.is_ascii_digit()) {
                    continue;
                }
                if query_terms.contains(t) || stop.contains(t.as_str()) {
                    continue;
                }
                *tf.entry(t.clone()).or_insert(0) += 1;
            }
        }
        if tf.is_empty() {
            return Ok(Vec::new());
        }

        // IDF per candidate term from the corpus doc frequency.
        let mut scored: Vec<(String, f64, u64)> = Vec::with_capacity(tf.len());
        for (term, freq) in tf {
            let df = searcher
                .doc_freq(&Term::from_field_text(self.fields.body, &term))
                .unwrap_or(0);
            let idf = (num_docs / (1.0 + df as f64)).ln();
            scored.push((term, idf, freq));
        }

        // High-IDF guardrail: keep only terms whose IDF is above the median.
        // This drops common words that slipped past the stoplist and prevents
        // the query from drifting toward generic, low-information text.
        let mut idfs: Vec<f64> = scored.iter().map(|(_, idf, _)| *idf).collect();
        idfs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = idfs[idfs.len() / 2];
        scored.retain(|(_, idf, _)| *idf >= median);

        // Rank by IDF × term-frequency-in-top-K (rare AND repeated wins).
        scored.sort_by(|a, b| {
            let sa = a.1 * a.2 as f64;
            let sb = b.1 * b.2 as f64;
            sb.partial_cmp(&sa)
                .unwrap_or(std::cmp::Ordering::Equal)
                // Tie-break on term text for deterministic output.
                .then_with(|| a.0.cmp(&b.0))
        });
        scored.truncate(PRF_EXPANSION_TERMS);
        Ok(scored.into_iter().map(|(t, _, _)| t).collect())
    }

    /// Read the stored `body` field for the document with the given `uid`.
    /// Returns `None` when the doc has no stored body (pre-PRF indices) or the
    /// UID is not found.
    fn fetch_body(
        &self,
        searcher: &tantivy::Searcher,
        uid: &str,
    ) -> Result<Option<String>, TantivyError> {
        use tantivy::query::TermQuery;
        use tantivy::schema::IndexRecordOption;

        let term = Term::from_field_text(self.fields.uid, uid);
        let q = TermQuery::new(term, IndexRecordOption::Basic);
        let top = searcher.search(&q, &TopDocs::with_limit(1).order_by_score())?;
        let Some((_, address)) = top.into_iter().next() else {
            return Ok(None);
        };
        let doc: TantivyDocument = searcher.doc(address)?;
        let body = extract_text(&doc, self.fields.body);
        if body.is_empty() {
            Ok(None)
        } else {
            Ok(Some(body))
        }
    }

    /// Read the stored `note_uid` field for the document with the given `uid`.
    /// Returns `None` when the doc is not found or carries no `note_uid` (e.g.
    /// tag docs). Used by PRF to collapse a note's many fragment docs (note,
    /// heading, section) to a single entry in the pseudo-relevant set.
    fn fetch_note_uid(
        &self,
        searcher: &tantivy::Searcher,
        uid: &str,
    ) -> Result<Option<String>, TantivyError> {
        use tantivy::query::TermQuery;
        use tantivy::schema::IndexRecordOption;

        let term = Term::from_field_text(self.fields.uid, uid);
        let q = TermQuery::new(term, IndexRecordOption::Basic);
        let top = searcher.search(&q, &TopDocs::with_limit(1).order_by_score())?;
        let Some((_, address)) = top.into_iter().next() else {
            return Ok(None);
        };
        let doc: TantivyDocument = searcher.doc(address)?;
        let note_uid = extract_text(&doc, self.fields.note_uid);
        if note_uid.is_empty() {
            Ok(None)
        } else {
            Ok(Some(note_uid))
        }
    }

    /// Total doc count — useful for status / health checks.
    pub fn doc_count(&self) -> usize {
        self.reader.searcher().num_docs() as usize
    }

    /// Returns `true` if this instance has a writer (i.e. was opened via
    /// `open_or_create`). Reader-only instances return `false`.
    pub fn has_writer(&self) -> bool {
        self.writer.is_some()
    }

    fn write_full_corpus(
        &self,
        writer: &mut tantivy::IndexWriter,
        store: &GraphStore,
    ) -> Result<usize, TantivyError> {
        Self::write_full_corpus_with_fields(writer, store, &self.fields)
    }

    fn write_full_corpus_with_fields(
        writer: &mut tantivy::IndexWriter,
        store: &GraphStore,
        fields: &Fields,
    ) -> Result<usize, TantivyError> {
        use std::collections::HashMap;

        let mut count = 0usize;

        let notes = store
            .list_notes(None)
            .map_err(|e| TantivyError::Tantivy(e.to_string()))?;

        // Bulk-load all headings and sections in 2 queries instead of 2N.
        let all_headings = store
            .list_all_headings()
            .map_err(|e| TantivyError::Tantivy(e.to_string()))?;
        let all_sections = store
            .list_all_sections()
            .map_err(|e| TantivyError::Tantivy(e.to_string()))?;

        // Group by note_uid for O(1) lookup inside the per-note loop.
        let mut headings_by_note: HashMap<&str, Vec<_>> = HashMap::new();
        for h in &all_headings {
            headings_by_note
                .entry(h.note_uid.as_str())
                .or_default()
                .push(h);
        }
        let mut sections_by_note: HashMap<&str, Vec<_>> = HashMap::new();
        for s in &all_sections {
            sections_by_note
                .entry(s.note_uid.as_str())
                .or_default()
                .push(s);
        }

        for note in &notes {
            let headings = headings_by_note
                .get(note.uid.as_str())
                .map(|v| v.as_slice())
                .unwrap_or(&[]);
            let sections = sections_by_note
                .get(note.uid.as_str())
                .map(|v| v.as_slice())
                .unwrap_or(&[]);

            // Try to read the note body from disk for full-text indexing.
            let body_from_disk: Option<String> =
                store.lookup_vault(&note.vault_uid).ok().and_then(|vault| {
                    let path = std::path::Path::new(&vault.root_path).join(&note.file_path);
                    std::fs::read_to_string(path).ok()
                });

            let note_body = body_from_disk.as_deref().unwrap_or(&note.title);
            writer.add_document(doc!(
                fields.uid => note.uid.clone(),
                fields.kind => "note".to_string(),
                fields.title => note.title.clone(),
                fields.body => note_body.to_string(),
                fields.vault_uid => note.vault_uid.clone(),
                fields.note_uid => note.uid.clone(),
            ))?;
            count += 1;

            for h in headings {
                writer.add_document(doc!(
                    fields.uid => h.uid.clone(),
                    fields.kind => "heading".to_string(),
                    fields.title => h.text.clone(),
                    fields.body => h.text.clone(),
                    fields.vault_uid => note.vault_uid.clone(),
                    fields.note_uid => note.uid.clone(),
                ))?;
                count += 1;
            }

            // Section body sourcing priority:
            // 1. `Section.text_content` from the graph — canonical view of
            //    what the indexer saw, survives even if the file is later
            //    moved or deleted on disk.
            // 2. Slice from `body_from_disk` if (1) is empty (handles
            //    sections inserted by older code versions that pre-date
            //    the text_content field).
            let body_lines: Vec<&str> = body_from_disk
                .as_deref()
                .map(|b| b.lines().collect())
                .unwrap_or_default();
            for s in sections {
                let section_text = if !s.text_content.is_empty() {
                    s.text_content.clone()
                } else {
                    slice_section_lines(&body_lines, s.start_line, s.end_line)
                };
                // Resolve the section's heading text so keyword searches
                // on a heading name also surface the section's body.
                let section_title = s
                    .heading_uid
                    .as_deref()
                    .and_then(|h_uid| headings.iter().find(|h| h.uid == h_uid))
                    .map(|h| h.text.clone())
                    .unwrap_or_default();
                writer.add_document(doc!(
                    fields.uid => s.uid.clone(),
                    fields.kind => "section".to_string(),
                    fields.title => section_title,
                    fields.body => section_text,
                    fields.vault_uid => note.vault_uid.clone(),
                    fields.note_uid => note.uid.clone(),
                ))?;
                count += 1;
            }
        }

        // Tags — one doc each. The kind discriminator lets clients
        // filter `tag` hits if needed.
        let tags = store
            .list_tags(None)
            .map_err(|e| TantivyError::Tantivy(e.to_string()))?;
        for tag in &tags {
            writer.add_document(doc!(
                fields.uid => tag.uid.clone(),
                fields.kind => "tag".to_string(),
                fields.title => tag.name.clone(),
                fields.body => tag.name.clone(),
                fields.vault_uid => tag.vault_uid.clone(),
                fields.note_uid => String::new(),
            ))?;
            count += 1;
        }

        Ok(count)
    }
}

fn build_schema() -> Schema {
    let mut builder = Schema::builder();
    builder.add_text_field("uid", STRING | STORED);
    builder.add_text_field("kind", STRING | STORED);
    builder.add_text_field("title", TEXT | STORED);
    // `body` is STORED (not just TEXT) so the PRF expansion pass can read the
    // top-K hit bodies back out and mine expansion terms from them. Indices
    // created before PRF landed keep their TEXT-only `body`; on those, body
    // read-back yields empty strings and PRF simply produces no expansion
    // terms (graceful degradation — re-run `brain reindex-search` to enable).
    builder.add_text_field("body", TEXT | STORED);
    builder.add_text_field("vault_uid", STRING | STORED);
    // `note_uid` is STORED so PRF can read it back and collapse a note's many
    // fragment docs (note/heading/section) to a single pseudo-relevant entry.
    // Pre-PRF indices stored it as STRING-only; on those the read-back yields
    // empty and PRF falls back to per-doc dedup (graceful degradation).
    builder.add_text_field("note_uid", STRING | STORED);
    builder.build()
}

struct SchemaInspection {
    fields: Fields,
    current: bool,
}

fn inspect_schema(schema: &Schema) -> Result<SchemaInspection, TantivyError> {
    let field = |name: &str| {
        schema
            .get_field(name)
            .map_err(|_| TantivyError::InvalidSchema(format!("missing required field {name:?}")))
    };
    let fields = Fields {
        uid: field("uid")?,
        kind: field("kind")?,
        title: field("title")?,
        body: field("body")?,
        vault_uid: field("vault_uid")?,
        note_uid: field("note_uid")?,
    };

    for (name, field) in [
        ("uid", fields.uid),
        ("kind", fields.kind),
        ("title", fields.title),
        ("body", fields.body),
        ("vault_uid", fields.vault_uid),
        ("note_uid", fields.note_uid),
    ] {
        let entry = schema.get_field_entry(field);
        if !entry.field_type().is_str() || !entry.is_indexed() {
            return Err(TantivyError::InvalidSchema(format!(
                "required field {name:?} must be indexed text"
            )));
        }
    }
    for (name, field) in [
        ("uid", fields.uid),
        ("kind", fields.kind),
        ("title", fields.title),
        ("vault_uid", fields.vault_uid),
    ] {
        if !schema.get_field_entry(field).is_stored() {
            return Err(TantivyError::InvalidSchema(format!(
                "required field {name:?} must be stored"
            )));
        }
    }

    // Equality against the canonical entries validates all current text
    // options (tokenizer, record detail, storage), not merely field presence.
    let canonical = build_schema();
    let current = ["uid", "kind", "title", "body", "vault_uid", "note_uid"]
        .into_iter()
        .all(|name| {
            let actual = schema.get_field(name).ok();
            let expected = canonical.get_field(name).ok();
            match (actual, expected) {
                (Some(actual), Some(expected)) => {
                    schema.get_field_entry(actual) == canonical.get_field_entry(expected)
                }
                _ => false,
            }
        });

    Ok(SchemaInspection { fields, current })
}

fn validate_presentation_limit(limit: usize) -> Result<(), TantivyError> {
    if limit > SEARCH_PRESENTATION_LIMIT_MAX {
        Err(TantivyError::PresentationLimitExceeded {
            limit,
            max: SEARCH_PRESENTATION_LIMIT_MAX,
        })
    } else {
        Ok(())
    }
}

fn directory_is_empty(path: &Path) -> Result<bool, TantivyError> {
    Ok(std::fs::read_dir(path)?.next().is_none())
}

fn reindex_backup_path(path: &Path) -> PathBuf {
    let mut backup = path.as_os_str().to_os_string();
    backup.push(".reindexing");
    PathBuf::from(backup)
}

fn recover_interrupted_reindex(path: &Path) -> Result<(), TantivyError> {
    let backup = reindex_backup_path(path);
    if !backup.exists() {
        return Ok(());
    }

    let target_present = path.exists() && !directory_is_empty(path)?;
    if target_present {
        // A replacement may have been installed before the crash. Retire the
        // recovery copy only after validating it. If validation fails, first
        // validate the recovery copy, then restore it over the failed install.
        let target_valid = Index::open_in_dir(path)
            .map_err(TantivyError::from)
            .and_then(|index| inspect_schema(&index.schema()).map(|_| index));
        if let Ok(index) = target_valid {
            drop(index);
            std::fs::remove_dir_all(&backup)?;
            crate::durable_sidecar::sync_parent_directory_durable(&backup)?;
            return Ok(());
        }

        let recovery = Index::open_in_dir(&backup)?;
        inspect_schema(&recovery.schema())?;
        drop(recovery);
        std::fs::remove_dir_all(path)?;
        std::fs::rename(&backup, path)?;
        crate::durable_sidecar::sync_parent_directory_durable(path)?;
        return Ok(());
    }

    if path.exists() {
        std::fs::remove_dir(path)?;
    }
    std::fs::rename(&backup, path)?;
    crate::durable_sidecar::sync_parent_directory_durable(path)?;
    Ok(())
}

fn extract_text(doc: &TantivyDocument, field: Field) -> String {
    doc.get_first(field)
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_default()
}

fn slice_section_lines(lines: &[&str], start: u32, end: u32) -> String {
    if start == 0 || start as usize > lines.len() {
        return String::new();
    }
    let end = (end as usize).min(lines.len());
    let start = (start - 1) as usize;
    if start >= end {
        return String::new();
    }
    lines[start..end].join("\n")
}

/// Build the pass-2 PRF query string: the original query verbatim (weight
/// 1.0) followed by each expansion term boosted by `weight` (e.g. `term^0.3`).
///
/// The combined query is capped at `max_terms` whitespace-delimited terms —
/// the original query's terms are always kept; expansion terms fill the
/// remaining budget. Expansion terms are individually quoted-and-escaped via
/// [`escape_query`] so special characters can't break the parser.
fn build_prf_query(query: &str, expansion: &[String], weight: f32, max_terms: usize) -> String {
    let orig_count = query.split_whitespace().count();
    let budget = max_terms.saturating_sub(orig_count);
    let mut out = query.trim().to_string();
    for term in expansion.iter().take(budget) {
        let safe = escape_query(term);
        out.push(' ');
        out.push_str(&format!("{safe}^{weight}"));
    }
    out
}

/// Escape characters the Tantivy query parser treats specially. Used as
/// the fallback when the user's raw query fails to parse.
fn escape_query(q: &str) -> String {
    // Tantivy's query parser doesn't support backslash-escaping for all
    // special chars (notably `:` as the field separator). Split the input
    // into whitespace-delimited tokens and quote any token that contains
    // special characters. This preserves adjacency within each token
    // while avoiding parser errors.
    let specials = [
        '+', '-', '!', '(', ')', '{', '}', '[', ']', '^', '"', '~', '*', '?', ':', '\\', '/',
    ];
    let mut out = String::with_capacity(q.len() + 16);
    for (i, token) in q.split_whitespace().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        if token.chars().any(|c| specials.contains(&c)) {
            let clean: String = token.chars().filter(|c| *c != '"').collect();
            out.push('"');
            out.push_str(&clean);
            out.push('"');
        } else {
            out.push_str(token);
        }
    }
    out
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use nestweaver_schema::{Heading, Note, NoteKind, Section, Tag, Vault};
    use tempfile::tempdir;

    fn add_raw_document(idx: &TantivyIndex, uid: &str, kind: &str, note_uid: &str, text: &str) {
        let mut writer = idx.writer.as_ref().unwrap().lock().unwrap();
        writer
            .add_document(doc!(
                idx.fields.uid => uid.to_string(),
                idx.fields.kind => kind.to_string(),
                idx.fields.title => text.to_string(),
                idx.fields.body => text.to_string(),
                idx.fields.vault_uid => "vlt:t".to_string(),
                idx.fields.note_uid => note_uid.to_string(),
            ))
            .unwrap();
        writer.commit().unwrap();
        drop(writer);
        idx.reader.reload().unwrap();
    }

    fn make_store_with_notes() -> GraphStore {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_vault(&Vault {
                uid: "vlt:t".to_string(),
                name: "t".to_string(),
                root_path: "/t".to_string(),
                instance_id: "default".to_string(),
            })
            .unwrap();
        for (uid, title) in [
            ("note:1", "Auth Service Design"),
            ("note:2", "Database Migration Strategy"),
            ("note:3", "Pairing Protocol Notes"),
        ] {
            store
                .insert_note(&Note {
                    uid: uid.to_string(),
                    vault_uid: "vlt:t".to_string(),
                    file_path: format!("{title}.md"),
                    title: title.to_string(),
                    note_kind: NoteKind::Design,
                    word_count: 10,
                    content_hash: "h".to_string(),
                    frontmatter: None,
                    created_at: None,
                    modified_at: None,
                    pagerank_score: None,
                    embedding: None,
                })
                .unwrap();
        }
        store
            .insert_tag(&Tag {
                uid: "tag:1".to_string(),
                vault_uid: "vlt:t".to_string(),
                name: "auth".to_string(),
            })
            .unwrap();
        store
    }

    #[test]
    fn open_or_create_creates_empty_index() {
        let dir = tempdir().unwrap();
        let idx = TantivyIndex::open_or_create(dir.path()).unwrap();
        assert_eq!(idx.doc_count(), 0);
    }

    #[test]
    fn reindex_then_search_finds_notes_by_title() {
        let dir = tempdir().unwrap();
        let idx = TantivyIndex::open_or_create(dir.path()).unwrap();
        let store = make_store_with_notes();
        let n = idx.reindex_from_store(&store).unwrap();
        assert!(n >= 3, "expected at least 3 docs, got {n}");

        let hits = idx.search("auth", 10).unwrap();
        let titles: Vec<&str> = hits.iter().map(|h| h.title.as_str()).collect();
        assert!(
            titles.contains(&"Auth Service Design") || hits.iter().any(|h| h.kind == "tag"),
            "expected an auth hit; got {titles:?}"
        );
    }

    #[test]
    fn counted_page_totals_logical_entities_independently_of_hit_limit() {
        let dir = tempdir().unwrap();
        let idx = TantivyIndex::open_or_create(dir.path()).unwrap();
        idx.update_note(
            "note:payment-a",
            "Payment Architecture",
            "vlt:t",
            &["payment orchestration".to_string()],
            &[("head:payment-a".to_string(), "Payment flow".to_string())],
            &[(
                "sec:payment-a".to_string(),
                "payment settlement".to_string(),
                "Payment flow".to_string(),
            )],
            &[],
        )
        .unwrap();
        idx.update_note(
            "note:payment-b",
            "Payment Operations",
            "vlt:t",
            &["payment reconciliation".to_string()],
            &[],
            &[],
            &[],
        )
        .unwrap();

        let page = idx.search_page("payment", 1).unwrap();
        assert_eq!(page.hits.len(), 1);
        assert_eq!(page.total.value, 2);
        assert_eq!(page.total.relation, SearchTotalRelation::Exact);
        assert!(page.total.value > page.hits.len());

        let legacy = idx.search("payment", 1).unwrap();
        assert_eq!(
            page.hits.iter().map(|hit| &hit.uid).collect::<Vec<_>>(),
            legacy.iter().map(|hit| &hit.uid).collect::<Vec<_>>(),
            "counting must not perturb legacy ranking"
        );
    }

    #[test]
    fn counted_page_uses_uid_identity_instead_of_title_identity() {
        let dir = tempdir().unwrap();
        let idx = TantivyIndex::open_or_create(dir.path()).unwrap();
        for uid in ["note:same-title-a", "note:same-title-b"] {
            idx.update_note(
                uid,
                "Payment",
                "vlt:t",
                &["payment".to_string()],
                &[],
                &[],
                &[],
            )
            .unwrap();
        }

        let page = idx.search_page("payment", 1).unwrap();
        assert_eq!(page.total.value, 2);
        assert_eq!(page.total.relation, SearchTotalRelation::Exact);
    }

    #[test]
    fn zero_hit_limit_still_counts_logical_entities() {
        let dir = tempdir().unwrap();
        let idx = TantivyIndex::open_or_create(dir.path()).unwrap();
        idx.update_note(
            "note:zero-limit",
            "Payment",
            "vlt:t",
            &["payment".to_string()],
            &[],
            &[],
            &[],
        )
        .unwrap();

        assert!(idx.search("payment", 0).unwrap().is_empty());
        let page = idx.search_page("payment", 0).unwrap();
        assert!(page.hits.is_empty());
        assert_eq!(page.total.value, 1);
        assert_eq!(page.total.relation, SearchTotalRelation::Exact);
    }

    #[test]
    fn search_entry_points_bound_presentation_limits_before_allocation() {
        assert_eq!(SEARCH_PRESENTATION_LIMIT_MAX, 10_000);
        let dir = tempdir().unwrap();
        let idx = TantivyIndex::open_or_create(dir.path()).unwrap();
        idx.update_note(
            "note:bounded-limit",
            "Payment",
            "vlt:t",
            &["payment".to_string()],
            &[],
            &[],
            &[],
        )
        .unwrap();

        assert!(idx.search_prf("payment", 0, &[]).unwrap().0.is_empty());
        let zero_prf_page = idx.search_prf_page("payment", 0, &[]).unwrap();
        assert!(zero_prf_page.hits.is_empty());
        assert_eq!(zero_prf_page.total, SearchTotal::exact(1));
        assert!(idx.search("payment", SEARCH_PRESENTATION_LIMIT_MAX).is_ok());
        assert!(
            idx.search_page("payment", SEARCH_PRESENTATION_LIMIT_MAX)
                .is_ok()
        );
        assert!(
            idx.search_prf("payment", SEARCH_PRESENTATION_LIMIT_MAX, &[])
                .is_ok()
        );
        assert!(
            idx.search_prf_page("payment", SEARCH_PRESENTATION_LIMIT_MAX, &[])
                .is_ok()
        );

        for over_limit in [SEARCH_PRESENTATION_LIMIT_MAX + 1, usize::MAX] {
            assert!(matches!(
                idx.search("payment", over_limit),
                Err(TantivyError::PresentationLimitExceeded { .. })
            ));
            assert!(idx.search_page("payment", over_limit).is_err());
            assert!(idx.search_prf("payment", over_limit, &[]).is_err());
            assert!(idx.search_prf_page("payment", over_limit, &[]).is_err());
        }
    }

    #[test]
    fn counted_identity_is_derived_from_kind_and_rejects_malformed_documents() {
        let dir = tempdir().unwrap();
        let idx = TantivyIndex::open_or_create(dir.path()).unwrap();

        // Standalone kinds ignore stale ownership metadata and remain distinct
        // from the note whose UID happens to appear in note_uid.
        add_raw_document(&idx, "note:owner", "note", "note:owner", "payment");
        add_raw_document(&idx, "tag:payment", "tag", "note:owner", "payment");
        let page = idx.search_page("payment", 10).unwrap();
        assert_eq!(page.total, SearchTotal::exact(2));

        for (uid, kind, note_uid) in [
            ("note:missing-owner", "note", ""),
            ("", "note", "note:missing-uid"),
            ("note:mismatch", "note", "note:other"),
            ("head:missing-owner", "heading", ""),
            ("sec:missing-owner", "section", ""),
            ("tag:missing-kind", "", ""),
            ("", "tag", ""),
        ] {
            let malformed_dir = tempdir().unwrap();
            let malformed = TantivyIndex::open_or_create(malformed_dir.path()).unwrap();
            add_raw_document(&malformed, uid, kind, note_uid, "payment");
            let error = malformed.search_page("payment", 10).unwrap_err();
            assert!(
                error.to_string().contains("invalid logical identity"),
                "unexpected error for ({uid:?}, {kind:?}, {note_uid:?}): {error}"
            );
        }
    }

    #[test]
    fn logical_entity_count_reports_a_lower_bound_at_its_cap() {
        let dir = tempdir().unwrap();
        let idx = TantivyIndex::open_or_create(dir.path()).unwrap();
        for i in 0..3 {
            idx.update_note(
                &format!("note:cap-{i}"),
                "Payment",
                "vlt:t",
                &["payment".to_string()],
                &[],
                &[],
                &[],
            )
            .unwrap();
        }

        let page = idx.search_page_with_count_cap("payment", 1, 2).unwrap();
        assert_eq!(page.hits.len(), 1);
        assert_eq!(page.total.value, 2);
        assert_eq!(page.total.relation, SearchTotalRelation::LowerBound);
    }

    /// A `reindex_from_store` must rebuild the Tantivy index atomically: the
    /// `delete_all_documents` and the re-adds land in ONE commit, so a
    /// concurrent reader querying a non-empty corpus always sees the old-or-new
    /// full corpus and NEVER an empty window. A delete-commit-then-add sequence
    /// (two commits) would expose an empty index between the two commits.
    #[test]
    fn reindex_from_store_is_atomic_for_readers() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = tempdir().unwrap();
        let idx = Arc::new(TantivyIndex::open_or_create(dir.path()).unwrap());
        let store = make_store_with_notes();

        // Prime a non-empty corpus and confirm it is searchable.
        idx.reindex_from_store(&store).unwrap();
        assert!(
            !idx.search("auth", 10).unwrap().is_empty(),
            "corpus should be non-empty after the priming reindex"
        );

        // A reader hammers the index while the writer reindexes repeatedly.
        // With a single atomic commit the reader can only observe the old or
        // the new full corpus — never the empty window a two-commit rebuild
        // would expose.
        let reader_idx = Arc::clone(&idx);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_reader = Arc::clone(&stop);
        let saw_empty = Arc::new(AtomicBool::new(false));
        let saw_empty_reader = Arc::clone(&saw_empty);
        let reader = std::thread::spawn(move || {
            while !stop_reader.load(Ordering::Relaxed) {
                if reader_idx.search("auth", 10).unwrap().is_empty() {
                    saw_empty_reader.store(true, Ordering::Relaxed);
                    break;
                }
            }
        });

        for _ in 0..50 {
            idx.reindex_from_store(&store).unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        reader.join().unwrap();

        assert!(
            !saw_empty.load(Ordering::Relaxed),
            "a concurrent reader must never see an empty index during reindex"
        );
    }

    #[test]
    fn search_returns_empty_for_empty_query() {
        let dir = tempdir().unwrap();
        let idx = TantivyIndex::open_or_create(dir.path()).unwrap();
        let hits = idx.search("", 5).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn lenient_parser_handles_special_chars() {
        let dir = tempdir().unwrap();
        let idx = TantivyIndex::open_or_create(dir.path()).unwrap();
        let store = make_store_with_notes();
        idx.reindex_from_store(&store).unwrap();
        // "auth:v2" would break the strict parser (treats `auth:` as a
        // field qualifier with no matching field). Lenient escape kicks in.
        let _ = idx.search("auth:v2", 5).unwrap();
    }

    #[test]
    fn update_note_replaces_old_docs() {
        let dir = tempdir().unwrap();
        let idx = TantivyIndex::open_or_create(dir.path()).unwrap();
        idx.update_note(
            "note:x",
            "Original Title",
            "vlt:t",
            &["original body content".to_string()],
            &[],
            &[],
            &[],
        )
        .unwrap();
        let hits = idx.search("original", 5).unwrap();
        assert!(!hits.is_empty(), "should find original");

        idx.update_note(
            "note:x",
            "Renamed",
            "vlt:t",
            &["completely fresh body".to_string()],
            &[],
            &[],
            &[],
        )
        .unwrap();
        let old_hits = idx.search("original", 5).unwrap();
        assert!(old_hits.is_empty(), "old doc should be replaced");
        let new_hits = idx.search("fresh", 5).unwrap();
        assert!(!new_hits.is_empty(), "new doc should be searchable");
    }

    #[test]
    fn title_match_ranks_above_body_match() {
        let dir = tempdir().unwrap();
        let idx = TantivyIndex::open_or_create(dir.path()).unwrap();
        idx.update_note(
            "note:a",
            "Auth Service",
            "vlt:t",
            &["some unrelated body content".to_string()],
            &[],
            &[],
            &[],
        )
        .unwrap();
        idx.update_note(
            "note:b",
            "Unrelated Title",
            "vlt:t",
            &["auth is mentioned in the body".to_string()],
            &[],
            &[],
            &[],
        )
        .unwrap();
        let hits = idx.search("auth", 10).unwrap();
        assert!(
            hits.len() >= 2,
            "expected at least 2 hits, got {}",
            hits.len()
        );
        assert_eq!(hits[0].uid, "note:a", "title match should rank first");
    }

    #[test]
    fn escape_query_preserves_terms() {
        assert_eq!(escape_query("auth/v2"), "\"auth/v2\"");
        assert_eq!(escape_query("foo:bar"), "\"foo:bar\"");
        assert_eq!(escape_query("hello world"), "hello world");
        assert_eq!(
            escape_query("hello foo:bar world"),
            "hello \"foo:bar\" world"
        );
    }

    #[test]
    fn remove_note_clears_all_docs_for_that_note() {
        let dir = tempdir().unwrap();
        let idx = TantivyIndex::open_or_create(dir.path()).unwrap();
        idx.update_note(
            "note:y",
            "Doomed",
            "vlt:t",
            &["body of doomed note".to_string()],
            &[("head:y:1".to_string(), "Doomed Heading".to_string())],
            &[],
            &[],
        )
        .unwrap();
        assert!(!idx.search("doomed", 5).unwrap().is_empty());
        idx.remove_note("note:y").unwrap();
        assert!(idx.search("doomed", 5).unwrap().is_empty());
    }

    #[test]
    fn reader_only_can_search_while_writer_is_held() {
        let dir = tempdir().unwrap();
        // Open a full (writer) instance, write some data, and keep it alive.
        let writer_idx = TantivyIndex::open_or_create(dir.path()).unwrap();
        writer_idx
            .update_note(
                "note:rw",
                "Concurrent Read Test",
                "vlt:t",
                &["body for concurrent read test".to_string()],
                &[],
                &[],
                &[],
            )
            .unwrap();
        assert!(writer_idx.has_writer());

        // Open a second, reader-only instance on the same directory.
        let reader_idx = TantivyIndex::open_reader_only(dir.path()).unwrap();
        assert!(!reader_idx.has_writer());

        // The reader should be able to search the data committed by the writer.
        let hits = reader_idx.search("concurrent", 5).unwrap();
        assert!(
            !hits.is_empty(),
            "reader-only instance should find committed data"
        );
        assert_eq!(hits[0].title, "Concurrent Read Test");

        // Doc count should also work.
        assert!(reader_idx.doc_count() > 0);
    }

    #[test]
    fn reader_only_rejects_write_operations() {
        let dir = tempdir().unwrap();
        // Create the index first so reader_only can open it.
        let _writer = TantivyIndex::open_or_create(dir.path()).unwrap();
        drop(_writer);

        let reader = TantivyIndex::open_reader_only(dir.path()).unwrap();

        // update_note should fail with WriterUnavailable.
        let err = reader
            .update_note("note:x", "X", "vlt:t", &[], &[], &[], &[])
            .unwrap_err();
        assert!(
            matches!(err, TantivyError::WriterUnavailable),
            "expected WriterUnavailable, got: {err}"
        );

        // remove_note should fail with WriterUnavailable.
        let err = reader.remove_note("note:x").unwrap_err();
        assert!(
            matches!(err, TantivyError::WriterUnavailable),
            "expected WriterUnavailable, got: {err}"
        );

        // reindex_from_store should fail with WriterUnavailable.
        let store = GraphStore::in_memory().unwrap();
        let err = reader.reindex_from_store(&store).unwrap_err();
        assert!(
            matches!(err, TantivyError::WriterUnavailable),
            "expected WriterUnavailable, got: {err}"
        );
    }

    #[test]
    fn legacy_prf_search_remains_compatible_with_pre_count_indices() {
        let dir = tempdir().unwrap();
        let mut schema_builder = Schema::builder();
        // Deliberately use a different field order from the current schema:
        // open paths must derive handles from the schema on disk.
        let note_uid = schema_builder.add_text_field("note_uid", STRING);
        let body = schema_builder.add_text_field("body", TEXT);
        let title = schema_builder.add_text_field("title", TEXT | STORED);
        let uid = schema_builder.add_text_field("uid", STRING | STORED);
        let vault_uid = schema_builder.add_text_field("vault_uid", STRING | STORED);
        let kind = schema_builder.add_text_field("kind", STRING | STORED);
        let index = Index::create_in_dir(dir.path(), schema_builder.build()).unwrap();
        let mut writer = index.writer(15_000_000).unwrap();
        writer
            .add_document(doc!(
                uid => "head:legacy",
                kind => "heading",
                title => "Payment",
                body => "payment",
                vault_uid => "vlt:t",
                note_uid => "note:legacy",
            ))
            .unwrap();
        writer.commit().unwrap();
        drop(writer);
        drop(index);

        let idx = TantivyIndex::open_reader_only(dir.path()).unwrap();
        let (hits, terms) = idx.search_prf("payment", 5, &[]).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(terms.is_empty());

        let error = idx.search_page("payment", 5).unwrap_err();
        assert!(error.to_string().contains("requires reindex"));
    }

    #[test]
    fn public_reindex_migrates_pre_count_schema_for_exact_fragment_totals() {
        let dir = tempdir().unwrap();
        let mut schema_builder = Schema::builder();
        let uid = schema_builder.add_text_field("uid", STRING | STORED);
        let kind = schema_builder.add_text_field("kind", STRING | STORED);
        let title = schema_builder.add_text_field("title", TEXT | STORED);
        let body = schema_builder.add_text_field("body", TEXT);
        let vault_uid = schema_builder.add_text_field("vault_uid", STRING | STORED);
        let note_uid = schema_builder.add_text_field("note_uid", STRING);
        let index = Index::create_in_dir(dir.path(), schema_builder.build()).unwrap();
        let mut writer = index.writer(15_000_000).unwrap();
        writer
            .add_document(doc!(
                uid => "note:legacy",
                kind => "note",
                title => "Old Payment",
                body => "payment",
                vault_uid => "vlt:t",
                note_uid => "note:legacy",
            ))
            .unwrap();
        writer.commit().unwrap();
        drop(writer);
        drop(index);

        let store = GraphStore::in_memory().unwrap();
        store
            .insert_vault(&Vault {
                uid: "vlt:t".to_string(),
                name: "t".to_string(),
                root_path: dir.path().join("vault").display().to_string(),
                instance_id: "default".to_string(),
            })
            .unwrap();
        store
            .insert_note(&Note {
                uid: "note:replacement".to_string(),
                vault_uid: "vlt:t".to_string(),
                file_path: "replacement.md".to_string(),
                title: "Payment Note".to_string(),
                note_kind: NoteKind::Design,
                word_count: 3,
                content_hash: "note-hash".to_string(),
                frontmatter: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();
        store
            .insert_heading(&Heading {
                uid: "head:replacement".to_string(),
                note_uid: "note:replacement".to_string(),
                level: 1,
                text: "Payment Heading".to_string(),
                slug: "payment-heading".to_string(),
                start_line: 1,
                end_line: 1,
                content_hash: "heading-hash".to_string(),
                embedding: None,
            })
            .unwrap();
        store
            .insert_section(&Section {
                uid: "sec:replacement".to_string(),
                note_uid: "note:replacement".to_string(),
                heading_uid: Some("head:replacement".to_string()),
                start_line: 2,
                end_line: 3,
                text_hash: "section-hash".to_string(),
                text_content: "payment section".to_string(),
                word_count: 2,
                pagerank_score: None,
            })
            .unwrap();

        let legacy = TantivyIndex::open_or_create(dir.path()).unwrap();
        assert_eq!(legacy.search("payment", 10).unwrap().len(), 1);
        assert_eq!(legacy.reindex_from_store(&store).unwrap(), 3);

        macro_rules! assert_reopen_required {
            ($operation:expr) => {
                let error = $operation.unwrap_err();
                assert!(
                    matches!(error, TantivyError::IndexReopenRequired),
                    "expected IndexReopenRequired, got {error}"
                );
            };
        }
        macro_rules! assert_presentation_limit_exceeded {
            ($operation:expr) => {
                let error = $operation.unwrap_err();
                assert!(
                    matches!(error, TantivyError::PresentationLimitExceeded { .. }),
                    "expected PresentationLimitExceeded, got {error}"
                );
            };
        }

        let over_limit = SEARCH_PRESENTATION_LIMIT_MAX + 1;
        assert_presentation_limit_exceeded!(legacy.search("payment", over_limit));
        assert_presentation_limit_exceeded!(legacy.search_page("payment", over_limit));
        assert_presentation_limit_exceeded!(legacy.search_prf("payment", over_limit, &[]));
        assert_presentation_limit_exceeded!(legacy.search_prf_page("payment", over_limit, &[]));

        assert_reopen_required!(legacy.reload());
        assert_reopen_required!(legacy.search("payment", 10));
        assert_reopen_required!(legacy.search_page("payment", 10));
        assert_reopen_required!(legacy.search_prf("payment", 10, &[]));
        assert_reopen_required!(legacy.search_prf_page("payment", 10, &[]));
        assert_reopen_required!(legacy.prf_expand_terms("payment", &[], &[]));
        assert_reopen_required!(legacy.update_note(
            "note:stale",
            "Stale",
            "vlt:t",
            &[],
            &[],
            &[],
            &[],
        ));
        assert_reopen_required!(legacy.update_notes_batch(&[]));
        assert_reopen_required!(legacy.remove_note("note:stale"));
        assert_reopen_required!(legacy.reindex_from_store(&store));
        drop(legacy);

        let reopened = TantivyIndex::open_reader_only(dir.path()).unwrap();
        let page = reopened.search_page("payment", 10).unwrap();
        assert_eq!(page.hits.len(), 3);
        assert_eq!(page.total, SearchTotal::exact(1));
        let replacement_hits = reopened.search("section", 10).unwrap();
        assert_eq!(replacement_hits.len(), 1);
        assert_eq!(replacement_hits[0].uid, "sec:replacement");
        assert!(
            reopened
                .index
                .schema()
                .get_field_entry(reopened.fields.note_uid)
                .is_stored()
        );
    }

    #[test]
    fn interrupted_schema_migration_recovers_the_rename_aside_copy() {
        let root = tempdir().unwrap();
        let index_path = root.path().join("search-index");
        let idx = TantivyIndex::open_or_create(&index_path).unwrap();
        idx.update_note(
            "note:recover",
            "Payment Recovery",
            "vlt:t",
            &["payment".to_string()],
            &[],
            &[],
            &[],
        )
        .unwrap();
        drop(idx);

        let recovery_path = reindex_backup_path(&index_path);
        std::fs::rename(&index_path, &recovery_path).unwrap();
        let reopened = TantivyIndex::open_reader_only(&index_path).unwrap();
        assert_eq!(reopened.search("payment", 10).unwrap().len(), 1);
        assert!(index_path.exists());
        assert!(!recovery_path.exists());
    }

    #[test]
    fn failed_schema_migration_leaves_the_old_index_usable() {
        let root = tempdir().unwrap();
        let index_path = root.path().join("search-index");
        std::fs::create_dir(&index_path).unwrap();
        let mut schema_builder = Schema::builder();
        let uid = schema_builder.add_text_field("uid", STRING | STORED);
        let kind = schema_builder.add_text_field("kind", STRING | STORED);
        let title = schema_builder.add_text_field("title", TEXT | STORED);
        let body = schema_builder.add_text_field("body", TEXT);
        let vault_uid = schema_builder.add_text_field("vault_uid", STRING | STORED);
        let note_uid = schema_builder.add_text_field("note_uid", STRING);
        let index = Index::create_in_dir(&index_path, schema_builder.build()).unwrap();
        let mut writer = index.writer(15_000_000).unwrap();
        writer
            .add_document(doc!(
                uid => "note:old",
                kind => "note",
                title => "Payment Old",
                body => "payment",
                vault_uid => "vlt:t",
                note_uid => "note:old",
            ))
            .unwrap();
        writer.commit().unwrap();
        drop(writer);
        drop(index);

        let legacy = TantivyIndex::open_or_create(&index_path).unwrap();
        let recovery_path = reindex_backup_path(&index_path);
        std::fs::create_dir(&recovery_path).unwrap();
        let error = legacy
            .reindex_from_store(&GraphStore::in_memory().unwrap())
            .unwrap_err();
        assert!(error.to_string().contains("recovery directory exists"));
        assert_eq!(legacy.search("payment", 10).unwrap().len(), 1);
        assert!(index_path.exists());
        std::fs::remove_dir(&recovery_path).unwrap();
        drop(legacy);

        let reopened = TantivyIndex::open_reader_only(&index_path).unwrap();
        assert_eq!(reopened.search("payment", 10).unwrap().len(), 1);
    }

    // ── PRF (Feature F7) tests ──────────────────────────────────────────

    /// Build a corpus where several docs that match the query "payment" also
    /// share a distinctive term — "idempotency" — that is NOT in the query.
    /// A few unrelated docs match "payment" only weakly.
    fn index_prf_corpus(idx: &TantivyIndex) {
        // The three "good" payment docs repeatedly mention the distinctive
        // expansion term "idempotency" (rare in the corpus, frequent in the
        // pseudo-relevant set). The UI doc matches "payment" but not
        // idempotency; the filler docs supply baseline IDF statistics. Bodies
        // are kept lexically lean so the high-IDF guardrail (above-median IDF)
        // doesn't get swamped by one-off singleton words.
        let docs: &[(&str, &str, &str)] = &[
            (
                "note:p1",
                "Payment Flow",
                "payment payment idempotency idempotency idempotency",
            ),
            (
                "note:p2",
                "Payment Retries",
                "payment idempotency idempotency idempotency",
            ),
            (
                "note:p3",
                "Payment Gateway",
                "payment payment idempotency idempotency",
            ),
            // Matches "payment" but never mentions idempotency.
            ("note:u1", "Payment UI", "payment payment payment"),
            // Filler docs — unrelated, give the corpus realistic statistics.
            ("note:f1", "Cooking", "soup carrots onions celery"),
            ("note:f2", "Travel", "mountains hiking camping gear"),
            ("note:f3", "Garden", "tomatoes basil spring compost"),
        ];
        for (uid, title, body) in docs {
            idx.update_note(uid, title, "vlt:t", &[body.to_string()], &[], &[], &[])
                .unwrap();
        }
    }

    #[test]
    fn prf_surfaces_distinctive_term_and_changes_order() {
        let dir = tempdir().unwrap();
        let idx = TantivyIndex::open_or_create(dir.path()).unwrap();
        index_prf_corpus(&idx);

        // Plain search for "payment" (no PRF) — baseline ordering.
        let plain = idx.search("payment", 10).unwrap();
        let plain_order: Vec<String> = plain.iter().map(|h| h.uid.clone()).collect();
        assert!(
            !plain_order.is_empty(),
            "baseline search should return hits"
        );

        // PRF search. "idempotency" appears in the top-K payment docs but not
        // in the query, so it should be mined as an expansion term.
        let stop: &[&str] = &["the", "a", "with", "and", "of", "we"];
        let (prf_hits, terms) = idx.search_prf("payment", 10, stop).unwrap();
        assert!(
            terms.iter().any(|t| t == "idempotency"),
            "PRF should surface the distinctive term 'idempotency'; got {terms:?}"
        );

        let prf_order: Vec<String> = prf_hits.iter().map(|h| h.uid.clone()).collect();
        // PRF should change the BM25 result ordering versus the plain query —
        // the idempotency-rich docs get boosted relative to the UI-only doc.
        assert_ne!(
            plain_order, prf_order,
            "PRF should change result order vs no-PRF; plain={plain_order:?} prf={prf_order:?}"
        );
        // The idempotency doc that is NOT a strong "payment" match should still
        // surface, and an idempotency-rich doc should outrank the UI-only doc.
        let pos = |order: &[String], uid: &str| order.iter().position(|u| u == uid);
        if let (Some(p1), Some(u1)) = (pos(&prf_order, "note:p1"), pos(&prf_order, "note:u1")) {
            assert!(
                p1 < u1,
                "idempotency-rich note:p1 should outrank UI-only note:u1 under PRF; order={prf_order:?}"
            );
        }
    }

    #[test]
    fn counted_prf_page_totals_the_final_expanded_query() {
        let dir = tempdir().unwrap();
        let idx = TantivyIndex::open_or_create(dir.path()).unwrap();
        index_prf_corpus(&idx);
        idx.update_note(
            "note:idempotency-only",
            "Idempotency Guide",
            "vlt:t",
            &["idempotency idempotency safeguards".to_string()],
            &[],
            &[],
            &[],
        )
        .unwrap();

        let plain = idx.search_page("payment", 2).unwrap();
        let page = idx
            .search_prf_page("payment", 2, &["the", "a", "with", "and", "of", "we"])
            .unwrap();
        assert!(
            page.expansion_terms
                .iter()
                .any(|term| term == "idempotency")
        );
        assert!(page.total.value > plain.total.value);
        assert_eq!(page.total.relation, SearchTotalRelation::Exact);
        assert_eq!(page.hits.len(), 2);

        let (legacy_hits, legacy_terms) = idx
            .search_prf("payment", 2, &["the", "a", "with", "and", "of", "we"])
            .unwrap();
        assert_eq!(page.expansion_terms, legacy_terms);
        assert_eq!(
            page.hits.iter().map(|hit| &hit.uid).collect::<Vec<_>>(),
            legacy_hits.iter().map(|hit| &hit.uid).collect::<Vec<_>>()
        );
    }

    #[test]
    fn prf_dedupes_note_fragments_in_top_k() {
        // Regression for the F7 top-K dilution bug: each note is indexed as a
        // whole-note doc *plus* a section doc sharing the same body. With four
        // notes that is eight near-duplicate docs. Without per-note dedup the
        // raw top-K (= 5) is consumed by the high-scoring fragments of three
        // notes, pushing the fourth — the one carrying the distinctive term —
        // out of the pseudo-relevant set so its term is never mined.
        let dir = tempdir().unwrap();
        let idx = TantivyIndex::open_or_create(dir.path()).unwrap();

        // p1's distinctive word "idempotency" appears twice (high tf) and in
        // only one note (high IDF) but p1 sorts last for the bare query
        // "payment" because its body is the longest. The other three notes
        // each contribute two near-duplicate docs that crowd the top-K.
        let notes: &[(&str, &str, &str)] = &[
            (
                "note:p1",
                "P1",
                "The payment flow relies on idempotency idempotency keys to avoid double charges.",
            ),
            ("note:p2", "P2", "Refund payment handling with retries."),
            ("note:p3", "P3", "Payment checkout settlement summary."),
            ("note:p4", "P4", "Payment authorization capture pipeline."),
        ];
        for (uid, title, body) in notes {
            let sec_uid = format!("sec:{uid}");
            idx.update_note(
                uid,
                title,
                "vlt:t",
                &[body.to_string()],
                &[],
                // One section doc per note, sharing the note body — this is
                // what produces the fragment fan-out the real indexer emits.
                &[(sec_uid, body.to_string(), title.to_string())],
                &[],
            )
            .unwrap();
        }

        let stop: &[&str] = nestweaver_store_test_stoplist();
        let (_hits, terms) = idx.search_prf("payment", 10, stop).unwrap();
        assert!(
            terms.iter().any(|t| t == "idempotency"),
            "per-note dedup must keep the distinctive note in the top-K so \
             'idempotency' is mined; got {terms:?}"
        );
        // The augmented stoplist must keep common prose words out.
        assert!(
            !terms.iter().any(|t| t == "with"),
            "common stopword 'with' must not leak into expansion terms; got {terms:?}"
        );
    }

    fn nestweaver_store_test_stoplist() -> &'static [&'static str] {
        // Mirror of the prose stopwords the engine threads in (`with` is the
        // canary the BUG-1 fix protects against).
        &[
            "the", "and", "for", "with", "that", "this", "from", "are", "was", "but", "not", "you",
            "its", "into", "has", "have", "will", "can", "all", "any", "out", "use",
        ]
    }

    #[test]
    fn prf_off_matches_plain_search() {
        // The non-PRF path (`search`) must be byte-for-byte unchanged: this is
        // the "with PRF off, results are identical to before" guarantee.
        let dir = tempdir().unwrap();
        let idx = TantivyIndex::open_or_create(dir.path()).unwrap();
        index_prf_corpus(&idx);
        let plain = idx.search("payment", 10).unwrap();
        // Re-run to confirm determinism / no hidden state mutation.
        let plain2 = idx.search("payment", 10).unwrap();
        let order1: Vec<String> = plain.iter().map(|h| h.uid.clone()).collect();
        let order2: Vec<String> = plain2.iter().map(|h| h.uid.clone()).collect();
        assert_eq!(order1, order2, "plain search must be deterministic");
    }

    #[test]
    fn prf_empty_query_returns_empty() {
        let dir = tempdir().unwrap();
        let idx = TantivyIndex::open_or_create(dir.path()).unwrap();
        index_prf_corpus(&idx);
        let (hits, terms) = idx.search_prf("", 5, &[]).unwrap();
        assert!(hits.is_empty());
        assert!(terms.is_empty());
    }

    #[test]
    fn build_prf_query_caps_total_terms() {
        let expansion: Vec<String> = (0..50).map(|i| format!("term{i}")).collect();
        let q = build_prf_query("alpha beta", &expansion, 0.3, 5);
        // 2 original terms + at most 3 expansion terms = 5 total.
        assert_eq!(q.split_whitespace().count(), 5, "query: {q}");
        assert!(q.starts_with("alpha beta"));
        assert!(q.contains("^0.3"));
    }

    #[test]
    fn update_notes_batch_indexes_all_notes_with_single_commit() {
        let dir = tempdir().unwrap();
        let idx = TantivyIndex::open_or_create(dir.path()).unwrap();

        // Build a batch of three notes — each with a distinctive term.
        let notes = vec![
            (
                "note:b1".to_string(),
                "Batch Alpha".to_string(),
                "vlt:t".to_string(),
                vec!["alpha unique term here".to_string()],
                vec![],
                vec![],
                vec![],
            ),
            (
                "note:b2".to_string(),
                "Batch Beta".to_string(),
                "vlt:t".to_string(),
                vec!["beta distinctive phrase".to_string()],
                vec![("head:b2:1".to_string(), "Beta Heading".to_string())],
                vec![(
                    "sec:b2:1".to_string(),
                    "beta section body".to_string(),
                    "Beta Heading".to_string(),
                )],
                vec![],
            ),
            (
                "note:b3".to_string(),
                "Batch Gamma".to_string(),
                "vlt:t".to_string(),
                vec!["gamma exclusive content".to_string()],
                vec![],
                vec![],
                vec![],
            ),
        ];

        idx.update_notes_batch(&notes).unwrap();

        // All three notes must be searchable after the single commit.
        assert!(
            !idx.search("alpha", 5).unwrap().is_empty(),
            "note:b1 should be findable"
        );
        assert!(
            !idx.search("beta", 5).unwrap().is_empty(),
            "note:b2 should be findable"
        );
        assert!(
            !idx.search("gamma", 5).unwrap().is_empty(),
            "note:b3 should be findable"
        );

        // Re-batching replaces old docs — the old body term that does NOT
        // appear in the new title or body must vanish from the index.
        let updated = vec![(
            "note:b1".to_string(),
            "Batch Alpha Renamed".to_string(),
            "vlt:t".to_string(),
            vec!["completely different body".to_string()],
            vec![],
            vec![],
            vec![],
        )];
        idx.update_notes_batch(&updated).unwrap();
        // "peculiar" only appeared in the old body ("alpha unique term here"
        // doesn't contain it, but "term" does — use a truly absent word).
        // Search for "term" which was in the old body but not the new one.
        assert!(
            idx.search("term", 5)
                .unwrap()
                .iter()
                .all(|h| h.uid != "note:b1"),
            "stale body term should no longer hit note:b1 after re-batch"
        );
        assert!(
            !idx.search("different", 5).unwrap().is_empty(),
            "new content should be searchable after re-batch"
        );
    }

    #[test]
    fn update_notes_batch_reader_only_returns_writer_unavailable() {
        let dir = tempdir().unwrap();
        let _w = TantivyIndex::open_or_create(dir.path()).unwrap();
        drop(_w);

        let reader = TantivyIndex::open_reader_only(dir.path()).unwrap();
        let err = reader.update_notes_batch(&[]).unwrap_err();
        assert!(
            matches!(err, TantivyError::WriterUnavailable),
            "expected WriterUnavailable, got: {err}"
        );
    }

    #[test]
    fn open_reader_only_fails_on_nonexistent_dir() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let result = TantivyIndex::open_reader_only(&missing);
        assert!(result.is_err(), "should fail on missing directory");
    }
}
