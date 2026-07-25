use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Context;
use indicatif::{ProgressBar, ProgressStyle};
use nestweaver_parser::{
    AstTypeBinding, RawReference, RawSymbol, SkippedFile, detect_language, parse_source,
};
use nestweaver_resolver::{discover_workspace_context_with, resolve_references_with_context};
use nestweaver_schema::{
    File, Language, Repo, Service, Symbol, canonical_symbol_id, file_uid, repo_uid, service_uid,
    symbol_uid,
};
use nestweaver_store::GraphStore;
use serde::{Deserialize, Serialize};

/// Per-file entry accumulated during Phase 2 and consumed in Phase 3.
/// The 4th field carries the retained source string (up to 2 MB) so Phase 3
/// can build type environments without re-reading files from disk.
type ParsedFileEntry = (String, Vec<RawSymbol>, Vec<RawReference>, Option<String>);

/// F2.2: data captured per Spring/NestJS controller file so handler →
/// contract edges can be derived after the bulk symbol insert.
struct HandlerFileData {
    framework: String,
    class_signature: String,
    rel_path: String,
    /// (symbol_uid, handler-symbol view) for every symbol in the file.
    symbols: Vec<(String, crate::contracts::HandlerSymbol)>,
}

/// Result returned by the indexing functions.
pub struct IndexResult {
    pub symbols_count: usize,
    pub edges_count: usize,
    pub files_count: usize,
    pub files_unchanged: usize,
    /// Count of old File rows removed by the indexing transaction, distinct
    /// from `files_count` (files parsed) and `files_unchanged` (files skipped as
    /// identical). Forced replacement counts every old row authoritatively
    /// deleted by `bulk_reindex_write`, including same-path replacements. A
    /// delete-only re-index has `files_count == 0` but `files_deleted > 0`; the
    /// index-time PageRank guard consults this so surviving nodes' ranks are
    /// recomputed after a deletion instead of left stale.
    pub files_deleted: usize,
    /// Symbols removed by replacement/deletion transactions during this run.
    pub symbols_deleted: usize,
    pub skipped_files: Vec<SkippedFile>,
}

// ── Tiered change detection ───────────────────────────────────────────────
//
// Sidecar file: `<db>.filemeta.json`
//
// On each index run we store (mtime_secs, size_bytes, content_hash) per file.
// Before reading & hashing a file we check:
//   Tier 1 – mtime unchanged → skip (near-zero cost stat)
//   Tier 2 – mtime changed but size unchanged → skip (likely identical)
//   Tier 3 – size differs → read file, compute BLAKE3, compare hash

/// Per-file metadata cached between indexing runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedFileMeta {
    pub mtime_secs: u64,
    pub size_bytes: u64,
    pub content_hash: String,
}

/// Map from repo-relative path to cached file metadata.
pub type FileMetaCache = HashMap<String, CachedFileMeta>;

/// Sidecar format version. v2 = per-repo keying (nw-022). v1 (implicit,
/// unversioned flat map) fails deserialization and loads as empty — a
/// deliberate fail-open that costs one full re-index and can never
/// mis-classify a file.
pub const FILEMETA_VERSION: u32 = 2;

/// On-disk shape of `<db>.filemeta.json`: change-detection metadata keyed by
/// repo uid, then repo-relative path. Two repos sharing one DB can never
/// collide on a relative path (nw-022).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMetaSidecar {
    pub version: u32,
    pub repos: HashMap<String, FileMetaCache>,
}

impl Default for FileMetaSidecar {
    fn default() -> Self {
        Self {
            version: FILEMETA_VERSION,
            repos: HashMap::new(),
        }
    }
}

/// Load the sidecar. Missing, corrupt, or old-format files yield an empty
/// sidecar (every file classifies as New → full re-index, never a skip).
pub fn load_filemeta_sidecar(path: &Path) -> FileMetaSidecar {
    match std::fs::read_to_string(path) {
        Ok(data) => match serde_json::from_str::<FileMetaSidecar>(&data) {
            Ok(s) if s.version == FILEMETA_VERSION => s,
            Ok(s) => {
                tracing::debug!(
                    path = %path.display(),
                    found_version = s.version,
                    expected_version = FILEMETA_VERSION,
                    "filemeta sidecar version mismatch; discarding (full re-index)"
                );
                FileMetaSidecar::default()
            }
            Err(e) => {
                tracing::debug!(
                    path = %path.display(),
                    error = %e,
                    "filemeta sidecar corrupt or legacy format; discarding (full re-index)"
                );
                FileMetaSidecar::default()
            }
        },
        Err(e) => {
            tracing::debug!(
                path = %path.display(),
                error = %e,
                "filemeta sidecar missing or unreadable; starting empty"
            );
            FileMetaSidecar::default()
        }
    }
}

/// Save the file metadata sidecar alongside the database.
pub fn save_filemeta_sidecar(sidecar: &FileMetaSidecar, path: &Path) -> Result<(), anyhow::Error> {
    debug_assert_eq!(
        sidecar.version, FILEMETA_VERSION,
        "FileMetaSidecar::default() pins the version; never construct one by hand"
    );
    let json = serde_json::to_string(sidecar).with_context(|| "serialize filemeta sidecar")?;
    crate::manifest::atomic_replace_file(path, |file| file.write_all(json.as_bytes()))
        .with_context(|| format!("write filemeta sidecar to {}", path.display()))?;
    Ok(())
}

/// A required durable stage in post-deletion reconciliation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletionReconciliationStage {
    IndexPublicationMarker,
    IndexPublicationMarkerRetirement,
    FileMetadata,
    ResolutionDependencies,
    ManifestCache,
    EmbeddingIndex,
    LegacyRetirement,
    ClusterCache,
    GenerationPersistence,
    PersistedPageRank,
    GraphLiveness,
    ExtensionMetadata,
    PageRankCompute,
    PageRankPersistence,
    SearchIndex,
}

impl std::fmt::Display for DeletionReconciliationStage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::IndexPublicationMarker => "index-publication-marker",
            Self::IndexPublicationMarkerRetirement => "index-publication-marker-retirement",
            Self::FileMetadata => "filemeta",
            Self::ResolutionDependencies => "resolution-deps",
            Self::ManifestCache => "manifest-cache",
            Self::EmbeddingIndex => "embedding-index",
            Self::LegacyRetirement => "legacy-retirement",
            Self::ClusterCache => "cluster-cache",
            Self::GenerationPersistence => "generation-persistence",
            Self::PersistedPageRank => "persisted-pagerank",
            Self::GraphLiveness => "graph-liveness",
            Self::ExtensionMetadata => "extension-metadata",
            Self::PageRankCompute => "pagerank-compute",
            Self::PageRankPersistence => "pagerank-persistence",
            Self::SearchIndex => "search-index",
        };
        formatter.write_str(name)
    }
}

/// One failed stage from a reconciliation run. Errors are strings because the
/// finalizer crosses `anyhow`, store, filesystem, and Tantivy error domains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletionReconciliationFailure {
    pub stage: DeletionReconciliationStage,
    pub repo_uid: Option<String>,
    pub message: String,
}

/// Aggregate returned only after every safe reconciliation stage has run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletionReconciliationError {
    pub operation: String,
    pub failures: Vec<DeletionReconciliationFailure>,
}

impl DeletionReconciliationError {
    pub fn new(operation: impl Into<String>, failures: Vec<DeletionReconciliationFailure>) -> Self {
        debug_assert!(!failures.is_empty());
        Self {
            operation: operation.into(),
            failures,
        }
    }

    pub fn single(
        operation: impl Into<String>,
        stage: DeletionReconciliationStage,
        message: impl Into<String>,
    ) -> Self {
        Self::new(
            operation,
            vec![DeletionReconciliationFailure {
                stage,
                repo_uid: None,
                message: message.into(),
            }],
        )
    }
}

impl std::fmt::Display for DeletionReconciliationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} reconciliation failed in {} stage(s)",
            self.operation,
            self.failures.len()
        )?;
        for failure in &self.failures {
            write!(formatter, "; {}", failure.stage)?;
            if let Some(repo_uid) = &failure.repo_uid {
                write!(formatter, "[{repo_uid}]")?;
            }
            write!(formatter, ": {}", failure.message)?;
        }
        Ok(())
    }
}

impl std::error::Error for DeletionReconciliationError {}

trait DeletionReconciliationIo {
    fn save_filemeta(&self, sidecar: &FileMetaSidecar, path: &Path) -> Result<(), anyhow::Error>;
    fn save_resolution_deps(
        &self,
        deps: &crate::resolution_cache::ResolutionDeps,
        path: &Path,
    ) -> Result<(), anyhow::Error>;
    fn remove_file(&self, path: &Path) -> std::io::Result<()>;
}

struct FileSystemDeletionReconciliationIo;

impl DeletionReconciliationIo for FileSystemDeletionReconciliationIo {
    fn save_filemeta(&self, sidecar: &FileMetaSidecar, path: &Path) -> Result<(), anyhow::Error> {
        save_filemeta_sidecar(sidecar, path)
    }

    fn save_resolution_deps(
        &self,
        deps: &crate::resolution_cache::ResolutionDeps,
        path: &Path,
    ) -> Result<(), anyhow::Error> {
        deps.save(path)
    }

    fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        nestweaver_store::durable_sidecar::remove_file_durable(path)
    }
}

fn push_reconciliation_failure(
    failures: &mut Vec<DeletionReconciliationFailure>,
    stage: DeletionReconciliationStage,
    repo_uid: Option<&str>,
    message: impl Into<String>,
) {
    failures.push(DeletionReconciliationFailure {
        stage,
        repo_uid: repo_uid.map(str::to_owned),
        message: message.into(),
    });
}

/// Drop the removed repo's slices from the change-detection sidecars so a later
/// re-index of the SAME path re-indexes its files instead of silently skipping
/// every one as `Unchanged` (nw-048). `remove-repo` deletes the repo's graph
/// nodes/symbols but, without this, leaves the `.filemeta` slice behind — the
/// re-added repo (same path → same uid, unchanged files) then classifies every
/// file `Unchanged`, so the deleted symbols are never restored and `search`
/// finds nothing.
///
/// Contract:
/// * **uid-scoped** — ONLY `repo_uid`'s slice is removed from each sidecar;
///   other repos sharing the DB keep their slices untouched (dropping the wrong
///   slice would be the same silent-data-loss class this fixes).
/// * **fail-open input** — a missing, corrupt, or old-format sidecar is a no-op:
///   `load_filemeta_sidecar` / `ResolutionDeps::load` already fail open to empty,
///   and each sidecar is only rewritten when the slice was actually present.
/// * **required output** — once a stale slice is found, failure to durably save
///   its removal is returned. A caller must not report successful deletion while
///   stale change-detection state can survive or be reused by the same repo UID.
///
/// The `.parsed_cache` sidecar is deliberately left alone: it is content-hash
/// keyed and shared across repos (collision-safe), so an orphaned entry there is
/// harmless dead space, not a correctness hazard.
pub fn remove_repo_sidecar_slices(
    db_path: &Path,
    repo_uid: &str,
) -> Result<(), DeletionReconciliationError> {
    let mut failures = Vec::new();
    remove_repo_sidecar_slices_with_io(
        db_path,
        repo_uid,
        &FileSystemDeletionReconciliationIo,
        &mut failures,
    );
    if failures.is_empty() {
        Ok(())
    } else {
        Err(DeletionReconciliationError::new("repo sidecar", failures))
    }
}

fn remove_repo_sidecar_slices_with_io(
    db_path: &Path,
    repo_uid: &str,
    io: &dyn DeletionReconciliationIo,
    failures: &mut Vec<DeletionReconciliationFailure>,
) {
    // filemeta — the primary nw-048 cause. A stale slice makes every file of the
    // re-added repo classify `Unchanged`, so its symbols are never re-indexed.
    crate::migrate_sidecar(db_path, "filemeta.json", ".filemeta.json");
    let filemeta_path = crate::sidecar_path(db_path, ".filemeta.json");
    let mut sidecar = load_filemeta_sidecar(&filemeta_path);
    if sidecar.repos.remove(repo_uid).is_some()
        && let Err(error) = io.save_filemeta(&sidecar, &filemeta_path)
    {
        push_reconciliation_failure(
            failures,
            DeletionReconciliationStage::FileMetadata,
            Some(repo_uid),
            format!("{}: {error:#}", filemeta_path.display()),
        );
    }

    // resolution_deps — nw-045. Consumers fail open on unreadable input, but a
    // durable slice for a deleted UID can influence a later same-UID incremental
    // resolution, so a discovered slice is required output rather than hygiene.
    let resolution_deps_path = crate::sidecar_path(db_path, ".resolution_deps.bin");
    let mut deps = crate::resolution_cache::ResolutionDeps::load(&resolution_deps_path);
    if deps.remove_repo(repo_uid)
        && let Err(error) = io.save_resolution_deps(&deps, &resolution_deps_path)
    {
        push_reconciliation_failure(
            failures,
            DeletionReconciliationStage::ResolutionDependencies,
            Some(repo_uid),
            format!("{}: {error:#}", resolution_deps_path.display()),
        );
    }
}

/// Per-stage results from reconciling graph-derived sidecars after deletion.
///
/// The deletion finalizer consumes every result, continues through the other
/// safe stages, then returns all required-stage failures in one aggregate.
#[derive(Debug)]
pub struct DeletedGraphStateReconciliation {
    pub manifests_removed: Result<usize, String>,
    pub embeddings: DeletedEmbeddingStateReconciliation,
    pub clusters_invalidated: Result<bool, String>,
}

/// Typed embedding reconciliation results keep canonical persistence and
/// legacy retirement distinguishable at the aggregate error boundary.
#[derive(Debug)]
pub enum DeletedEmbeddingStateReconciliation {
    LiveSetFailed(String),
    Reconciled {
        removed: usize,
        canonical_persistence: Result<(), String>,
        legacy_retirement: Option<Result<bool, String>>,
    },
}

/// Reconcile repo/node-keyed derived state against the authoritative live
/// graph. This is safe after partial multi-statement deletion: live destination
/// or surviving rows remain in the authoritative sets and therefore retain
/// their manifest/vector data.
pub fn reconcile_deleted_graph_state(
    store: &GraphStore,
    db_path: &Path,
) -> DeletedGraphStateReconciliation {
    reconcile_deleted_graph_state_with_io(store, db_path, &FileSystemDeletionReconciliationIo)
}

fn reconcile_deleted_graph_state_with_io(
    store: &GraphStore,
    db_path: &Path,
    io: &dyn DeletionReconciliationIo,
) -> DeletedGraphStateReconciliation {
    let manifests_removed = (|| -> Result<usize, anyhow::Error> {
        let manifests_path = crate::manifest::manifest_cache_path(db_path);
        let mut manifests = crate::manifest::load_manifest_cache_for_db(db_path)?;
        if !manifests_path.exists() {
            return Ok(0);
        }
        let live_repo_uids: std::collections::HashSet<String> = store
            .list_repos(None)
            .map_err(anyhow::Error::from)?
            .into_iter()
            .map(|repo| repo.uid)
            .collect();
        let before = manifests.len();
        manifests.retain(|uid, _| live_repo_uids.contains(uid));
        let removed = before - manifests.len();
        if removed > 0 {
            crate::manifest::save_manifest_cache_for_db(&manifests, db_path)?;
        }
        Ok(removed)
    })()
    .map_err(|error| format!("manifest reconciliation failed: {error:#}"));

    let embeddings = match store.reconcile_embedding_index_stages() {
        Ok(result) => DeletedEmbeddingStateReconciliation::Reconciled {
            removed: result.removed,
            canonical_persistence: result
                .canonical_persistence
                .map_err(|error| format!("embedding persistence failed: {error:#}")),
            legacy_retirement: result.legacy_retirement.map(|retirement| {
                retirement.map_err(|error| format!("legacy embedding retirement failed: {error:#}"))
            }),
        },
        Err(error) => DeletedEmbeddingStateReconciliation::LiveSetFailed(format!(
            "embedding live-set reconciliation failed: {error:#}"
        )),
    };
    let clusters_path = crate::sidecar_path(db_path, ".clusters.json");
    let clusters_invalidated = match io.remove_file(&clusters_path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "cluster invalidation failed for {}: {error}",
            clusters_path.display()
        )),
    };

    DeletedGraphStateReconciliation {
        manifests_removed,
        embeddings,
        clusters_invalidated,
    }
}

/// Publish all cache and sidecar effects that must follow committed code graph
/// mutation, including a partially successful cascade. Both daemon RPC and
/// web-admin removals call this epilogue so they cannot diverge after the same
/// mutation. Required-stage failures are returned together only after every
/// safe stage has been attempted.
///
/// The parsed cache is intentionally not touched: unlike filemeta and
/// resolution dependencies, it is keyed by content hash and safely shared by
/// every repo in the database.
pub fn finalize_code_graph_deletion(
    store: &GraphStore,
    db_path: &Path,
    repo_uids: &[String],
    operation: &str,
) -> Result<(), DeletionReconciliationError> {
    finalize_code_graph_deletion_with_io(
        store,
        db_path,
        repo_uids,
        operation,
        &FileSystemDeletionReconciliationIo,
    )
}

fn finalize_code_graph_deletion_with_io(
    store: &GraphStore,
    db_path: &Path,
    repo_uids: &[String],
    operation: &str,
    io: &dyn DeletionReconciliationIo,
) -> Result<(), DeletionReconciliationError> {
    let mut failures = Vec::new();
    for uid in repo_uids {
        remove_repo_sidecar_slices_with_io(db_path, uid, io, &mut failures);
    }
    let reconciliation = reconcile_deleted_graph_state_with_io(store, db_path, io);
    match reconciliation.manifests_removed {
        Ok(removed) => tracing::info!(removed, operation, "manifest cache reconciled"),
        Err(error) => {
            let stage = if error.contains("legacy manifest sidecar") {
                DeletionReconciliationStage::LegacyRetirement
            } else {
                DeletionReconciliationStage::ManifestCache
            };
            push_reconciliation_failure(&mut failures, stage, None, error);
        }
    }
    match reconciliation.embeddings {
        DeletedEmbeddingStateReconciliation::LiveSetFailed(error) => push_reconciliation_failure(
            &mut failures,
            DeletionReconciliationStage::EmbeddingIndex,
            None,
            error,
        ),
        DeletedEmbeddingStateReconciliation::Reconciled {
            removed,
            canonical_persistence,
            legacy_retirement,
        } => {
            match canonical_persistence {
                Ok(()) => tracing::info!(removed, operation, "embedding index reconciled"),
                Err(error) => push_reconciliation_failure(
                    &mut failures,
                    DeletionReconciliationStage::EmbeddingIndex,
                    None,
                    error,
                ),
            }
            if let Some(Err(error)) = legacy_retirement {
                push_reconciliation_failure(
                    &mut failures,
                    DeletionReconciliationStage::LegacyRetirement,
                    None,
                    error,
                );
            }
        }
    }
    match reconciliation.clusters_invalidated {
        Ok(invalidated) => tracing::info!(invalidated, operation, "cluster cache reconciled"),
        Err(error) => push_reconciliation_failure(
            &mut failures,
            DeletionReconciliationStage::ClusterCache,
            None,
            error,
        ),
    }
    match crate::extensions::reconcile_extension_liveness(store, db_path) {
        Ok(removed) => tracing::info!(removed, operation, "extension metadata reconciled"),
        Err(error) => push_reconciliation_failure(
            &mut failures,
            DeletionReconciliationStage::ExtensionMetadata,
            None,
            format!("extension metadata liveness reconciliation failed: {error:#}"),
        ),
    }

    // Advance before its durable companion so live readers are safe even when
    // sidecar persistence fails, but never wrap an exhausted counter.
    let generation_advanced = match store.try_bump_graph_generation() {
        Ok(_) => true,
        Err(error) => {
            push_reconciliation_failure(
                &mut failures,
                DeletionReconciliationStage::GenerationPersistence,
                None,
                format!("advance graph generation: {error:#}"),
            );
            false
        }
    };
    let generation_path = crate::sidecar_path(db_path, ".generation");
    if generation_advanced && let Err(error) = store.save_graph_generation(&generation_path) {
        push_reconciliation_failure(
            &mut failures,
            DeletionReconciliationStage::GenerationPersistence,
            None,
            format!("{}: {error:#}", generation_path.display()),
        );
    }
    store.invalidate_pagerank();

    let pagerank_path = crate::sidecar_path(db_path, ".pagerank.json");
    if let Err(error) = io.remove_file(&pagerank_path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        push_reconciliation_failure(
            &mut failures,
            DeletionReconciliationStage::PersistedPageRank,
            None,
            format!("{}: {error}", pagerank_path.display()),
        );
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(DeletionReconciliationError::new(operation, failures))
    }
}

/// Mandatory cache/generation publication after a committed indexing graph
/// mutation. This runs before any later sidecar persistence or PageRank
/// recomputation so a subsequent error cannot leave the previous ranks or
/// graph generation authoritative.
pub(crate) trait IndexEpilogueIo {
    fn establish_marker(&self, path: &Path) -> Result<(), anyhow::Error>;
    fn clear_marker(&self, path: &Path) -> Result<(), anyhow::Error>;
    fn remove_file(&self, path: &Path) -> std::io::Result<()>;
    fn rename_file(&self, from: &Path, to: &Path) -> std::io::Result<()>;
    fn save_generation(
        &self,
        store: &GraphStore,
        path: &Path,
        generation: u64,
    ) -> Result<(), anyhow::Error>;
    fn compute_pagerank(
        &self,
        store: &GraphStore,
        scope: &nestweaver_store::GraphScope,
    ) -> Result<(), anyhow::Error>;
    fn save_pagerank(&self, store: &GraphStore, path: &Path) -> Result<(), anyhow::Error>;
}

pub(crate) struct FileSystemIndexEpilogueIo;

impl IndexEpilogueIo for FileSystemIndexEpilogueIo {
    fn establish_marker(&self, path: &Path) -> Result<(), anyhow::Error> {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)
            .with_context(|| format!("create index publication marker {}", path.display()))?;
        let marker = format!(
            "{}:{}\n",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        file.write_all(marker.as_bytes())
            .with_context(|| format!("write index publication marker {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("sync index publication marker {}", path.display()))?;
        sync_sidecar_parent(path)
    }

    fn clear_marker(&self, path: &Path) -> Result<(), anyhow::Error> {
        match std::fs::remove_file(path) {
            Ok(()) => sync_sidecar_parent(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error)
                .with_context(|| format!("remove index publication marker {}", path.display())),
        }
    }

    fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        nestweaver_store::durable_sidecar::remove_file_durable(path)
    }

    fn rename_file(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        std::fs::rename(from, to)?;
        sync_sidecar_parent_io(from)?;
        if from.parent() != to.parent() {
            sync_sidecar_parent_io(to)?;
        }
        Ok(())
    }

    fn save_generation(
        &self,
        store: &GraphStore,
        path: &Path,
        generation: u64,
    ) -> Result<(), anyhow::Error> {
        store.save_graph_generation_value(path, generation)?;
        Ok(())
    }

    fn compute_pagerank(
        &self,
        store: &GraphStore,
        scope: &nestweaver_store::GraphScope,
    ) -> Result<(), anyhow::Error> {
        store.compute_pagerank(0.85, 20, scope).map_err(Into::into)
    }

    fn save_pagerank(&self, store: &GraphStore, path: &Path) -> Result<(), anyhow::Error> {
        store.save_pagerank_cache(path)?;
        Ok(())
    }
}

fn sync_sidecar_parent(path: &Path) -> Result<(), anyhow::Error> {
    nestweaver_store::durable_sidecar::sync_parent_directory_durable(path)
        .with_context(|| format!("publish sidecar namespace change for {}", path.display()))
}

fn sync_sidecar_parent_io(path: &Path) -> std::io::Result<()> {
    nestweaver_store::durable_sidecar::sync_parent_directory_durable(path)
}

fn quarantine_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(".stale");
    PathBuf::from(value)
}

fn invalidate_pagerank_sidecar_with_io(
    pagerank_path: &Path,
    io: &dyn IndexEpilogueIo,
    failures: &mut Vec<DeletionReconciliationFailure>,
) -> bool {
    match io.remove_file(pagerank_path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => {
            let quarantine = quarantine_path(pagerank_path);
            let quarantine_result = io.rename_file(pagerank_path, &quarantine);
            let safe = quarantine_result.is_ok();
            push_reconciliation_failure(
                failures,
                DeletionReconciliationStage::PersistedPageRank,
                None,
                match quarantine_result {
                    Ok(()) => format!(
                        "{}: removal failed ({error}); quarantined as {}",
                        pagerank_path.display(),
                        quarantine.display()
                    ),
                    Err(quarantine_error) => format!(
                        "{}: removal failed ({error}); quarantine failed ({quarantine_error})",
                        pagerank_path.display()
                    ),
                },
            );
            safe
        }
    }
}

pub(crate) fn establish_index_publication_marker_with_io<'a>(
    store: &'a GraphStore,
    db_path: Option<&Path>,
    operation: &str,
    io: &dyn IndexEpilogueIo,
) -> Result<nestweaver_store::IndexPublicationLease<'a>, DeletionReconciliationError> {
    let lease = store.acquire_index_publication_lease().map_err(|error| {
        DeletionReconciliationError::new(
            operation,
            vec![DeletionReconciliationFailure {
                stage: DeletionReconciliationStage::IndexPublicationMarker,
                repo_uid: None,
                message: format!("acquire exclusive index publication lease: {error:#}"),
            }],
        )
    })?;
    let Some(db_path) = db_path else {
        lease.preflight_transient_generation().map_err(|error| {
            DeletionReconciliationError::new(
                operation,
                vec![DeletionReconciliationFailure {
                    stage: DeletionReconciliationStage::GenerationPersistence,
                    repo_uid: None,
                    message: format!("preflight transient index generation: {error:#}"),
                }],
            )
        })?;
        return Ok(lease);
    };
    lease.preflight_generation().map_err(|error| {
        DeletionReconciliationError::new(
            operation,
            vec![DeletionReconciliationFailure {
                stage: DeletionReconciliationStage::GenerationPersistence,
                repo_uid: None,
                message: format!("preflight index publication generation: {error:#}"),
            }],
        )
    })?;
    let marker_path = crate::sidecar_path(db_path, ".index-dirty");
    lease
        .store()
        .with_index_publication_rank_barrier(|| -> Result<(), anyhow::Error> {
            io.establish_marker(&marker_path)?;
            lease.reserve_generation()?;
            Ok(())
        })
        .map_err(|error| {
            DeletionReconciliationError::new(
                operation,
                vec![DeletionReconciliationFailure {
                    stage: DeletionReconciliationStage::IndexPublicationMarker,
                    repo_uid: None,
                    message: format!("{}: {error:#}", marker_path.display()),
                }],
            )
        })?;
    Ok(lease)
}

fn finalize_committed_index_with_io(
    lease: nestweaver_store::IndexPublicationLease<'_>,
    db_path: Option<&Path>,
    operation: &str,
    io: &dyn IndexEpilogueIo,
    refresh_pagerank: bool,
) -> Result<(), DeletionReconciliationError> {
    let scope = refresh_pagerank.then(nestweaver_store::GraphScope::code_only);
    finalize_committed_index_for_scope_with_io(lease, db_path, operation, io, scope.as_ref())
}

pub(crate) fn finalize_committed_index_for_scope_with_io(
    lease: nestweaver_store::IndexPublicationLease<'_>,
    db_path: Option<&Path>,
    operation: &str,
    io: &dyn IndexEpilogueIo,
    pagerank_scope: Option<&nestweaver_store::GraphScope>,
) -> Result<(), DeletionReconciliationError> {
    let mut failures = Vec::new();
    let store = lease.store();

    store.invalidate_pagerank();
    let pagerank_safe = if let Some(db_path) = db_path {
        invalidate_pagerank_sidecar_with_io(
            &crate::sidecar_path(db_path, ".pagerank.json"),
            io,
            &mut failures,
        )
    } else {
        true
    };

    let generation_advanced = if db_path.is_some() {
        lease.clean_generation()
    } else {
        store.try_bump_graph_generation()
    };
    let generation_durable = match generation_advanced {
        Err(error) => {
            push_reconciliation_failure(
                &mut failures,
                DeletionReconciliationStage::GenerationPersistence,
                None,
                format!("advance graph generation: {error:#}"),
            );
            false
        }
        Ok(generation) if db_path.is_some() => {
            let generation_path = crate::sidecar_path(db_path.unwrap(), ".generation");
            match io.save_generation(store, &generation_path, generation) {
                Ok(()) => true,
                Err(error) => {
                    push_reconciliation_failure(
                        &mut failures,
                        DeletionReconciliationStage::GenerationPersistence,
                        None,
                        format!("{}: {error:#}", generation_path.display()),
                    );
                    false
                }
            }
        }
        Ok(_) => true,
    };

    let mut publication_clean = db_path.is_none();
    if generation_durable
        && pagerank_safe
        && let Some(db_path) = db_path
    {
        let marker_path = crate::sidecar_path(db_path, ".index-dirty");
        let retirement =
            store.with_index_publication_rank_barrier(|| -> Result<(), anyhow::Error> {
                lease.publish_clean_generation()?;
                if let Err(error) = io.clear_marker(&marker_path) {
                    lease.fail_clean_generation()?;
                    return Err(error);
                }
                lease.complete_generation()?;
                Ok(())
            });
        if let Err(error) = retirement {
            push_reconciliation_failure(
                &mut failures,
                DeletionReconciliationStage::IndexPublicationMarkerRetirement,
                None,
                format!("{}: {error:#}", marker_path.display()),
            );
        } else {
            publication_clean = true;
        }
    }

    if let Some(scope) = pagerank_scope.filter(|_| publication_clean) {
        match io.compute_pagerank(store, scope) {
            Ok(()) => {
                if let Some(db_path) = db_path {
                    let pagerank_path = crate::sidecar_path(db_path, ".pagerank.json");
                    match io.save_pagerank(store, &pagerank_path) {
                        Ok(()) => {}
                        Err(error) => {
                            push_reconciliation_failure(
                                &mut failures,
                                DeletionReconciliationStage::PageRankPersistence,
                                None,
                                format!("{}: {error:#}", pagerank_path.display()),
                            );
                            invalidate_pagerank_sidecar_with_io(&pagerank_path, io, &mut failures);
                        }
                    }
                }
            }
            Err(error) => push_reconciliation_failure(
                &mut failures,
                DeletionReconciliationStage::PageRankCompute,
                None,
                format!("{error:#}"),
            ),
        }
    }

    if publication_clean
        && let Some(db_path) = db_path
        && let Err(error) = crate::extensions::reconcile_extension_handoffs(store, db_path)
    {
        push_reconciliation_failure(
            &mut failures,
            DeletionReconciliationStage::ExtensionMetadata,
            None,
            format!("reconcile deferred extension metadata after index publication: {error:#}"),
        );
    }

    if failures.is_empty() {
        lease.release().map_err(|error| {
            DeletionReconciliationError::new(
                operation,
                vec![DeletionReconciliationFailure {
                    stage: DeletionReconciliationStage::IndexPublicationMarkerRetirement,
                    repo_uid: None,
                    message: format!("release exclusive index publication lease: {error:#}"),
                }],
            )
        })
    } else {
        Err(DeletionReconciliationError::new(operation, failures))
    }
}

/// "Still alive" sets returned by [`merge_save_filemeta`], used to evict the
/// parsed-cache / resolution-deps sidecars. A named struct so the same-typed
/// sets can't be swapped at a call site.
struct FilemetaEvictionUnions {
    /// Cross-repo union of every repo's live content hashes. The parsed cache
    /// is content-hash keyed (collision-safe across repos), so its eviction
    /// must be union-scoped or indexing one repo would drop another's entries.
    live_hashes: std::collections::HashSet<String>,
    /// THIS repo's live repo-relative paths only. The resolution-deps tracker
    /// is now per-repo keyed (nw-045), so its eviction is scoped to the repo
    /// being indexed — never the cross-repo union (which could resurrect or
    /// wrongly retain another repo's records on a shared rel path).
    repo_live_files: std::collections::HashSet<String>,
}

/// Load-merge-save the filemeta sidecar for one repo's index run, and return
/// the cross-repo eviction unions for the parsed-cache / resolution-deps
/// sidecars. `drop_uids` removes slices for pruned or re-identified repos.
/// NEVER a blind overwrite: other repos' slices are preserved (nw-022).
fn merge_save_filemeta(
    filemeta_path: &Path,
    r_uid: &str,
    new_filemeta: FileMetaCache,
    drop_uids: &[String],
) -> Result<FilemetaEvictionUnions, anyhow::Error> {
    let mut sidecar = load_filemeta_sidecar(filemeta_path);
    for uid in drop_uids {
        sidecar.repos.remove(uid);
    }
    sidecar.repos.insert(r_uid.to_string(), new_filemeta);
    // Content-hash union across ALL repos — feeding only the current repo's
    // hashes (the old behavior) evicts every other repo's parse cache.
    let live_hashes = sidecar
        .repos
        .values()
        .flat_map(|files| files.values().map(|m| m.content_hash.clone()))
        .collect();
    // THIS repo's live rel-paths only (nw-045). The resolution-deps tracker is
    // per-repo keyed, so its retention must be scoped to r_uid — the cross-repo
    // union would preserve dead entries and defeats per-repo eviction.
    let repo_live_files = sidecar
        .repos
        .get(r_uid)
        .map(|files| files.keys().cloned().collect())
        .unwrap_or_default();
    save_filemeta_sidecar(&sidecar, filemeta_path)?;
    Ok(FilemetaEvictionUnions {
        live_hashes,
        repo_live_files,
    })
}

/// Outcome of the tiered change detection for a single file.
enum ChangeVerdict {
    /// File is unchanged — skip re-indexing it.
    Unchanged,
    /// File is new or changed — `source` contains the file content and
    /// `content_hash` is the freshly-computed BLAKE3 hex digest.
    Changed {
        source: String,
        content_hash: String,
        meta: CachedFileMeta,
    },
}

/// Run the three-tier change detection for a single file.
///
/// Returns `Unchanged` when the cached metadata proves the file has not
/// been modified, or `Changed` with the file content + new hash when it
/// has (or when no cache entry exists).
fn tiered_change_check(
    reader: &dyn crate::content_reader::ContentReader,
    rel_path: &str,
    cache: &FileMetaCache,
) -> Result<ChangeVerdict, anyhow::Error> {
    let rel = Path::new(rel_path);

    // file_meta returns None for bare-repo readers (no mtime available).
    // In that case, always fall through to read + hash.
    let (mtime_secs, size_bytes) = match reader.file_meta(rel)? {
        Some((m, s)) => (m, s),
        None => {
            // No filesystem metadata (e.g. GitBareReader) — read and hash. The
            // bare-clone reader enforces the size cap inside its own read_file
            // (oversized blobs return Err), so a huge file is skipped there.
            let source = reader
                .read_file(rel)
                .with_context(|| format!("read {rel_path}"))?;
            let content_hash = content_hash_hex(&source);
            if let Some(cached) = cache.get(rel_path)
                && content_hash == cached.content_hash
            {
                return Ok(ChangeVerdict::Unchanged);
            }
            return Ok(ChangeVerdict::Changed {
                meta: CachedFileMeta {
                    mtime_secs: 0,
                    size_bytes: source.len() as u64,
                    content_hash: content_hash.clone(),
                },
                source,
                content_hash,
            });
        }
    };

    // Enforce the oversized-file ceiling on the FILESYSTEM path too. Markdown
    // (index_md) and the bare-clone reader already skip oversized files, but the
    // local code path did not — so a single large source file (e.g. a 200 MB
    // generated bundle that minified-detection misses) was read whole and handed
    // to tree-sitter, exhausting memory. `size_bytes` is already in hand from
    // file_meta, so the guard is free; the caller turns this Err into a skip.
    if size_bytes > crate::index_md::MAX_FILE_SIZE_BYTES {
        anyhow::bail!(
            "file exceeds size limit ({size_bytes} > {} bytes), skipping",
            crate::index_md::MAX_FILE_SIZE_BYTES
        );
    }

    if let Some(cached) = cache.get(rel_path) {
        // Tier 1: mtime unchanged → skip.
        if cached.mtime_secs == mtime_secs {
            return Ok(ChangeVerdict::Unchanged);
        }

        // Tier 2: mtime changed but size unchanged → fall through to hash check.
        // Same-size edits are common, so we cannot skip based on size alone.

        // Tier 3: mtime differs → read file, compute hash, compare.
        let source = reader
            .read_file(rel)
            .with_context(|| format!("read {rel_path}"))?;
        let content_hash = content_hash_hex(&source);
        if content_hash == cached.content_hash {
            // Content identical despite mtime/size change — unchanged for the
            // graph. No need to re-parse. (The caller will carry forward the
            // cached entry.)
            return Ok(ChangeVerdict::Unchanged);
        }
        Ok(ChangeVerdict::Changed {
            meta: CachedFileMeta {
                mtime_secs,
                size_bytes,
                content_hash: content_hash.clone(),
            },
            source,
            content_hash,
        })
    } else {
        // No cache entry → file is new, read and hash it.
        let source = reader
            .read_file(rel)
            .with_context(|| format!("read {rel_path}"))?;
        let content_hash = content_hash_hex(&source);
        Ok(ChangeVerdict::Changed {
            meta: CachedFileMeta {
                mtime_secs,
                size_bytes,
                content_hash: content_hash.clone(),
            },
            source,
            content_hash,
        })
    }
}

/// Directory names to skip when walking the repository tree.
pub(crate) const SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "target",
    "__pycache__",
    "vendor",
    "dist",
    "build",
    "coverage",
    ".claude",
    ".superpowers",
    ".next",
    ".nuxt",
    ".astro",
    ".wrangler",
    "test-results",
    "playwright-report",
    ".expo",
    ".venv",
    "venv",
    ".tox",
    ".mypy_cache",
    ".ruff_cache",
    ".pytest_cache",
    "env",
    ".env",
    ".pio",
    "Pods",
    "ios",
    "android",
    ".gradle",
    "public",
    "out",
    ".output",
    "storybook-static",
];

/// Index a directory into a persistent GraphStore at `db_path`.
///
/// When `force` is false, tiered change detection is used: files whose
/// mtime, size, and content hash match the cached values from the
/// previous run are skipped entirely, avoiding expensive I/O and parsing.
/// When `force` is true, the filemeta sidecar is ignored and every file
/// is re-read, re-hashed, and re-indexed.
pub fn index_directory(
    repo_path: &Path,
    db_path: &Path,
    instance_id: &str,
    repo_url: &str,
    indexed_sha: &str,
) -> Result<IndexResult, anyhow::Error> {
    index_directory_with_options(
        repo_path,
        db_path,
        instance_id,
        repo_url,
        indexed_sha,
        false,
        None,
    )
}

/// Index a directory with explicit control over force-reindex behavior.
///
/// When `name` is `Some`, it is stored on the Repo node as a display name
/// override. This avoids basename collisions when multiple repos share a
/// generic last path segment (e.g. `client`, `server`).
pub fn index_directory_with_options(
    repo_path: &Path,
    db_path: &Path,
    instance_id: &str,
    repo_url: &str,
    indexed_sha: &str,
    force: bool,
    name: Option<&str>,
) -> Result<IndexResult, anyhow::Error> {
    let store = GraphStore::open_or_create(db_path)
        .with_context(|| format!("failed to open/create GraphStore at {}", db_path.display()))?;
    index_directory_with_store(
        &store,
        repo_path,
        db_path,
        instance_id,
        repo_url,
        indexed_sha,
        force,
        name,
    )
}

/// Index a directory using an existing GraphStore (for daemon mode where the
/// store is already open). Same as `index_directory_with_options` but skips
/// opening the database.
#[allow(clippy::too_many_arguments)]
pub fn index_directory_with_store(
    store: &GraphStore,
    repo_path: &Path,
    db_path: &Path,
    instance_id: &str,
    repo_url: &str,
    indexed_sha: &str,
    force: bool,
    name: Option<&str>,
) -> Result<IndexResult, anyhow::Error> {
    index_directory_with_store_inner(
        store,
        repo_path,
        db_path,
        instance_id,
        repo_url,
        indexed_sha,
        force,
        name,
        None,
    )
}

/// Like [`index_directory_with_store`], but observes a cooperative cancellation
/// flag. When `cancel` is set — by the daemon on an index timeout or when the
/// requesting client disconnects — the index aborts at the pre-write boundary
/// (after parse, before any graph mutation), leaving no partial write. The
/// underlying index work runs in an uncancelable `spawn_blocking` task, so this
/// flag is the only way to stop a long-running index cooperatively.
#[allow(clippy::too_many_arguments)]
pub fn index_directory_with_store_cancellable(
    store: &GraphStore,
    repo_path: &Path,
    db_path: &Path,
    instance_id: &str,
    repo_url: &str,
    indexed_sha: &str,
    force: bool,
    name: Option<&str>,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<IndexResult, anyhow::Error> {
    index_directory_with_store_inner(
        store,
        repo_path,
        db_path,
        instance_id,
        repo_url,
        indexed_sha,
        force,
        name,
        Some(cancel),
    )
}

#[allow(clippy::too_many_arguments)]
fn index_directory_with_store_inner(
    store: &GraphStore,
    repo_path: &Path,
    db_path: &Path,
    instance_id: &str,
    repo_url: &str,
    indexed_sha: &str,
    force: bool,
    name: Option<&str>,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<IndexResult, anyhow::Error> {
    let filemeta_path = crate::sidecar_path(db_path, ".filemeta.json");
    crate::migrate_sidecar(db_path, "filemeta.json", ".filemeta.json");
    let r_uid = repo_uid(instance_id, repo_url);
    let mut new_filemeta = FileMetaCache::new();

    let parsed_cache_path = crate::sidecar_path(db_path, ".parsed_cache.bin");
    let mut parsed_cache = crate::parsed_cache::ParsedCache::load(&parsed_cache_path);

    let resolution_deps_path = crate::sidecar_path(db_path, ".resolution_deps.bin");
    let mut resolution_deps = crate::resolution_cache::ResolutionDeps::load(&resolution_deps_path);

    let reader = crate::content_reader::FilesystemReader::new(repo_path);
    // Local filesystem index: the working tree location is `repo_path`.
    // Persisted as `root_path` on the Repo node so consumers never derive
    // a disk path from the identity `url`.
    let local_root = repo_path.display().to_string();
    // nw-010: when the run re-identifies a legacy file:// repo under its
    // origin remote, the old uid's filemeta slice must be dropped from the
    // sidecar alongside the graph prune.
    let mut reidentified_old_uid: Option<String> = None;
    let result = if force {
        index_into_store_with_write_gate(
            &reader,
            store,
            instance_id,
            repo_url,
            indexed_sha,
            None,
            Some(&mut new_filemeta),
            Some(&mut parsed_cache),
            Some(&mut resolution_deps),
            Some(&mut reidentified_old_uid),
            name,
            Some(&local_root),
            true,
            &FileSystemIndexEpilogueIo,
            cancel,
            || Ok::<(), anyhow::Error>(()),
        )?
    } else {
        // Only this repo's slice of the sidecar feeds change detection —
        // another repo's entry for the same rel path must never match (nw-022).
        let sidecar = load_filemeta_sidecar(&filemeta_path);
        index_into_store_with_write_gate(
            &reader,
            store,
            instance_id,
            repo_url,
            indexed_sha,
            sidecar.repos.get(&r_uid),
            Some(&mut new_filemeta),
            Some(&mut parsed_cache),
            Some(&mut resolution_deps),
            Some(&mut reidentified_old_uid),
            name,
            Some(&local_root),
            true,
            &FileSystemIndexEpilogueIo,
            cancel,
            || Ok::<(), anyhow::Error>(()),
        )?
    };

    // Merge this repo's fresh entries into the shared sidecar and evict
    // parse/resolution cache entries using cross-repo live unions.
    //
    // A sidecar write failure fails the whole run (`?`) on purpose: the graph
    // was already mutated, so a stale sidecar would silently re-enable
    // skip-classification against reality on the next run — files that
    // changed since the stale snapshot would classify Unchanged and never be
    // re-indexed. (The fallback path stays warn-only; see full_index_fallback.)
    let drop_uids: Vec<String> = reidentified_old_uid.into_iter().collect();
    let unions = merge_save_filemeta(&filemeta_path, &r_uid, new_filemeta, &drop_uids)?;
    parsed_cache.retain_hashes(&unions.live_hashes);
    resolution_deps.retain_files_for_repo(&r_uid, &unions.repo_live_files);

    if let Err(e) = parsed_cache.save(&parsed_cache_path) {
        tracing::warn!("failed to save parsed cache: {e}");
    }
    if let Err(e) = resolution_deps.save(&resolution_deps_path) {
        tracing::warn!("failed to save resolution deps: {e}");
    }

    let manifest = crate::manifest::parse_manifest(&reader);
    let mut cache = crate::manifest::load_manifest_cache_for_db(db_path).unwrap_or_default();
    cache.insert(r_uid, manifest);
    if let Err(e) = crate::manifest::save_manifest_cache_for_db(&cache, db_path) {
        tracing::warn!("failed to save manifest cache: {e}");
    }

    // nw-029: warm PageRank at index time so first queries (UI overview, impact,
    // repo-map, hubs) never pay the lazy compute. Mirrors the incremental path.
    // Release-build cost is seconds even at ~50k symbols. A failure is returned:
    // callers must not observe a successful index unless its fresh PageRank is
    // durable. nw-055 (P1b): delete-only re-indexes also need fresh surviving
    // ranks even though files_count is zero.
    Ok(result)
}

/// Index a directory into an in-memory GraphStore (for testing).
pub fn index_directory_in_memory(
    repo_path: &Path,
    instance_id: &str,
    repo_url: &str,
    indexed_sha: &str,
) -> Result<(IndexResult, GraphStore), anyhow::Error> {
    let store = GraphStore::in_memory().context("failed to create in-memory GraphStore")?;
    let reader = crate::content_reader::FilesystemReader::new(repo_path);
    let local_root = repo_path.display().to_string();
    let result = index_into_store(
        &reader,
        &store,
        instance_id,
        repo_url,
        indexed_sha,
        None,
        None,
        None,
        None,
        None,
        Some(&local_root),
    )?;
    Ok((result, store))
}

/// Index via an arbitrary [`ContentReader`] into an in-memory GraphStore.
///
/// This is the primary entry point for server-mode indexing where the caller
/// controls how files are read (e.g. via `GitBareReader` for bare clones).
pub fn index_with_reader(
    reader: &dyn crate::content_reader::ContentReader,
    store: &GraphStore,
    instance_id: &str,
    repo_url: &str,
    indexed_sha: &str,
    name: Option<&str>,
) -> Result<IndexResult, anyhow::Error> {
    index_with_reader_and_write_gate(
        reader,
        store,
        instance_id,
        repo_url,
        indexed_sha,
        name,
        None,
        || Ok::<_, anyhow::Error>(()),
    )
}

/// Index via an arbitrary [`ContentReader`] and acquire the caller-provided
/// write gate after file scan/parse/collection but before graph mutations.
#[allow(clippy::too_many_arguments)]
pub fn index_with_reader_and_write_gate<G, F>(
    reader: &dyn crate::content_reader::ContentReader,
    store: &GraphStore,
    instance_id: &str,
    repo_url: &str,
    indexed_sha: &str,
    name: Option<&str>,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    acquire_write_guard: F,
) -> Result<IndexResult, anyhow::Error>
where
    F: FnOnce() -> Result<G, anyhow::Error>,
{
    index_with_reader_and_write_gate_and_io(
        ReaderIndexRequest {
            reader,
            store,
            instance_id,
            repo_url,
            indexed_sha,
            name,
            cancel,
            epilogue_io: &FileSystemIndexEpilogueIo,
        },
        acquire_write_guard,
    )
}

struct ReaderIndexRequest<'a> {
    reader: &'a dyn crate::content_reader::ContentReader,
    store: &'a GraphStore,
    instance_id: &'a str,
    repo_url: &'a str,
    indexed_sha: &'a str,
    name: Option<&'a str>,
    cancel: Option<&'a std::sync::Arc<std::sync::atomic::AtomicBool>>,
    epilogue_io: &'a dyn IndexEpilogueIo,
}

fn index_with_reader_and_write_gate_and_io<G, F>(
    request: ReaderIndexRequest<'_>,
    acquire_write_guard: F,
) -> Result<IndexResult, anyhow::Error>
where
    F: FnOnce() -> Result<G, anyhow::Error>,
{
    let ReaderIndexRequest {
        reader,
        store,
        instance_id,
        repo_url,
        indexed_sha,
        name,
        cancel,
        epilogue_io,
    } = request;
    let result = index_into_store_with_write_gate(
        reader,
        store,
        instance_id,
        repo_url,
        indexed_sha,
        None,
        None,
        None,
        None,
        None,
        name,
        // Server-mode: the reader is backed by a bare clone with no local
        // working tree, so the Repo node carries no root_path.
        None,
        true,
        epilogue_io,
        cancel,
        acquire_write_guard,
    )?;
    Ok(result)
}

fn infer_cross_repo_call_edges(
    store: &GraphStore,
    current_repo_uid: &str,
    parsed_files: &[ParsedFileEntry],
) -> Result<Vec<nestweaver_schema::ResolvedEdge>, anyhow::Error> {
    use nestweaver_parser::ReferenceKind;
    use nestweaver_schema::{CrossRepoLinkType, EdgeEvidence, EdgeType, ResolvedEdge, Visibility};

    // A bare call name matching many public definitions store-wide carries no
    // attribution signal — `run`, `get`, `new`, `handle`, `init` are defined once
    // per type/module across every repo. Mature cross-repo indexers (SCIP/LSIF
    // monikers, Kythe VNames) resolve by package-qualified identity + import
    // corroboration and never join on a bare name, precisely because name-only
    // matching has near-zero precision on ubiquitous identifiers. We approximate
    // that cheaply: (a) a candidate-count cap drops ubiquitous names entirely, and
    // (b) import corroboration raises confidence for names the calling file
    // actually imports. Un-corroborated name matches stay at a low, sub-"breaking"
    // confidence (< the 0.5 org-severity cutoff) so they surface as hints, not as
    // breaking org-wide impact. (A stronger, import-resolved cross-repo resolver
    // already exists in `nestweaver-resolver::cross_repo`; this is the cheap
    // hypothesis layer.)
    const MAX_CROSS_REPO_NAME_CANDIDATES: usize = 3;
    const NAME_ONLY_CONFIDENCE: f32 = 0.20; // info tier (< 0.25 warning cutoff)
    const IMPORT_CORROBORATED_CONFIDENCE: f32 = 0.50; // SamePackageFallback

    let local_symbol_names: std::collections::HashSet<String> = parsed_files
        .iter()
        .flat_map(|(_, symbols, _, _)| symbols.iter().map(|s| s.name.clone()))
        .collect();
    let mut edges = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (rel_path, symbols, references, _) in parsed_files {
        // Names this file imports — corroborates a same-named cross-repo call.
        let imported_names: std::collections::HashSet<&str> = references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Import)
            .map(|r| r.name.as_str())
            .collect();

        for reference in references {
            if reference.kind != ReferenceKind::Call || reference.receiver.is_some() {
                continue;
            }
            if local_symbol_names.contains(&reference.name) {
                continue;
            }

            let Some(source_symbol) = containing_symbol_for_line(symbols, reference.start_line)
            else {
                continue;
            };
            let source_uid = symbol_uid(
                current_repo_uid,
                rel_path,
                &source_symbol.name,
                source_symbol.start_line,
            );

            // Collect eligible cross-repo candidates, then apply the ubiquity cap.
            let candidates: Vec<_> = store
                .lookup_symbols_by_name(&reference.name)
                .map_err(|e| anyhow::anyhow!(e))?
                .into_iter()
                .filter(|t| {
                    t.repo_uid != current_repo_uid
                        && t.uid != source_uid
                        && t.visibility != Visibility::Private
                })
                .collect();
            if candidates.is_empty() || candidates.len() > MAX_CROSS_REPO_NAME_CANDIDATES {
                continue;
            }

            let import_corroborated = imported_names.contains(reference.name.as_str());
            let confidence = if import_corroborated {
                IMPORT_CORROBORATED_CONFIDENCE
            } else {
                NAME_ONLY_CONFIDENCE
            };
            let evidence_kind = if import_corroborated {
                "cross_repo_name_import_corroborated"
            } else {
                "cross_repo_name_match"
            };

            for target in candidates {
                if seen.insert((source_uid.clone(), target.uid.clone())) {
                    edges.push(ResolvedEdge {
                        source_uid: source_uid.clone(),
                        target_uid: target.uid,
                        edge_type: EdgeType::CrossRepoLink,
                        confidence,
                        link_type: Some(CrossRepoLinkType::SharedImport),
                        evidence: vec![EdgeEvidence {
                            kind: evidence_kind.to_string(),
                            weight: confidence,
                            note: Some(format!(
                                "cross-repo call '{}' matched by name{}",
                                reference.name,
                                if import_corroborated {
                                    " (import-corroborated)"
                                } else {
                                    ""
                                }
                            )),
                        }],
                    });
                }
            }
        }
    }
    Ok(edges)
}

fn containing_symbol_for_line(symbols: &[RawSymbol], line: u32) -> Option<&RawSymbol> {
    symbols
        .iter()
        .filter(|s| s.start_line <= line && line <= s.end_line)
        .max_by_key(|s| s.start_line)
        .or_else(|| {
            // Fallback for symbols with a degenerate/unknown span (end_line
            // never advanced past start_line). A symbol with a REAL span that
            // ends before `line` must NOT claim module-level code that comes
            // after it (same misattribution class as the resolver's
            // find_enclosing_symbol fix).
            symbols
                .iter()
                .filter(|s| s.start_line <= line && s.end_line <= s.start_line)
                .max_by_key(|s| s.start_line)
        })
}

/// Core indexing logic shared by both public functions.
///
/// When `filemeta_cache` is `Some`, tiered change detection skips files
/// whose mtime and/or size match the cached values (avoiding expensive
/// BLAKE3 hashing and re-parsing for unchanged files). Entries for all
/// processed files are written to `new_filemeta` so the caller can
/// persist the updated sidecar after indexing completes.
///
/// When `parsed_cache` is provided, unchanged files whose content hash
/// matches a cache entry will return their symbols/references from the
/// cache instead of being skipped. Newly parsed files are inserted into
/// the cache so callers can persist it after indexing.
#[allow(clippy::too_many_arguments)]
fn index_into_store(
    reader: &dyn crate::content_reader::ContentReader,
    store: &GraphStore,
    instance_id: &str,
    repo_url: &str,
    indexed_sha: &str,
    filemeta_cache: Option<&FileMetaCache>,
    new_filemeta: Option<&mut FileMetaCache>,
    parsed_cache: Option<&mut crate::parsed_cache::ParsedCache>,
    resolution_deps: Option<&mut crate::resolution_cache::ResolutionDeps>,
    name: Option<&str>,
    root_path: Option<&str>,
) -> Result<IndexResult, anyhow::Error> {
    index_into_store_with_write_gate(
        reader,
        store,
        instance_id,
        repo_url,
        indexed_sha,
        filemeta_cache,
        new_filemeta,
        parsed_cache,
        resolution_deps,
        None,
        name,
        root_path,
        true,
        &FileSystemIndexEpilogueIo,
        None,
        || Ok::<_, anyhow::Error>(()),
    )
}

#[allow(clippy::too_many_arguments)]
fn index_into_store_with_write_gate<G, F>(
    reader: &dyn crate::content_reader::ContentReader,
    store: &GraphStore,
    instance_id: &str,
    repo_url: &str,
    indexed_sha: &str,
    filemeta_cache: Option<&FileMetaCache>,
    mut new_filemeta: Option<&mut FileMetaCache>,
    mut parsed_cache: Option<&mut crate::parsed_cache::ParsedCache>,
    mut resolution_deps: Option<&mut crate::resolution_cache::ResolutionDeps>,
    reidentified_old_uid_out: Option<&mut Option<String>>,
    name: Option<&str>,
    root_path: Option<&str>,
    bump_generation_after_write: bool,
    epilogue_io: &dyn IndexEpilogueIo,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    acquire_write_guard: F,
) -> Result<IndexResult, anyhow::Error>
where
    F: FnOnce() -> Result<G, anyhow::Error>,
{
    let started = Instant::now();

    // 1. Compute the Repo UID. Graph mutations are deferred until after file
    // scan/parse/collection so worker threads can fan out expensive parsing and
    // only serialize LadybugDB writes.
    let r_uid = repo_uid(instance_id, repo_url);

    // Re-identify detection MUST happen before the scan/parse phases: the
    // filemeta sidecar was recorded when this working tree's graph rows
    // lived under the legacy file:// uid. Trusting it now would classify
    // every file as Unchanged and skip its writes under the NEW uid, while
    // the prune below deletes the only copy of that data under the old uid
    // — silently emptying the repo graph. A re-identified index is a true
    // cold index for the new uid, so the tiered-change cache is bypassed
    // for this pass (the fresh sidecar is still written afterwards).
    let reidentify_old_uid: Option<String> = match root_path {
        Some(rp) => reidentified_legacy_uid(store, instance_id, rp, &r_uid)?,
        None => None,
    };
    // Report the re-identified legacy uid so the caller can drop its filemeta
    // slice when merge-saving the sidecar (nw-022).
    if let Some(out) = reidentified_old_uid_out {
        *out = reidentify_old_uid.clone();
    }
    let filemeta_cache = if reidentify_old_uid.is_some() {
        None
    } else {
        filemeta_cache
    };

    // ── Phase 1: Scan files ───────────────────────────────────────────────
    let _phase1_span = tracing::info_span!("index_phase_scan").entered();
    let scan_pb = ProgressBar::new_spinner();
    scan_pb.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner()),
    );
    scan_pb.set_message("Scanning files...");

    let mut file_entries: Vec<(PathBuf, Language)> = Vec::new();
    // F2.1: spec files (OpenAPI/Swagger/proto/GraphQL) collected separately —
    // most have no detected source language so they fall outside `file_entries`.
    let mut spec_files: Vec<PathBuf> = Vec::new();

    let repo_path = reader.root();
    let discovered_files = reader
        .list_files()
        .context("ContentReader::list_files failed")?;

    for rel_path in &discovered_files {
        let path = repo_path.join(rel_path);

        // F2.1: collect API spec files regardless of source language.
        if crate::contracts::is_spec_file(&path.to_string_lossy()) {
            spec_files.push(path.clone());
        }

        // Only process files with a supported language extension.
        let lang = match detect_language(&path) {
            Some(l) => l,
            None => continue,
        };

        // Skip minified/bundled files — they produce noise in the graph.
        if is_minified_or_bundled(&path) {
            continue;
        }

        file_entries.push((path, lang));
        scan_pb.set_message(format!("Scanning files... {}", file_entries.len()));
        scan_pb.tick();
    }

    scan_pb.finish_with_message(format!("Scanned {} files", file_entries.len()));
    tracing::info!(
        files_found = file_entries.len(),
        spec_files_found = spec_files.len(),
        "phase scan complete"
    );
    drop(_phase1_span);

    // ── Phase 2: Parse files (parallelised with rayon) ─────────────────
    let _phase2_span = tracing::info_span!("index_phase_parse").entered();
    let total_files = file_entries.len() as u64;
    let parse_pb = ProgressBar::new(total_files);
    parse_pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.cyan} Parsing [{bar:30.cyan/dim}] {pos}/{len} {wide_msg}",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("━╸─"),
    );

    // Empty cache used as fallback when no sidecar was provided.
    let empty_cache = FileMetaCache::new();
    let cache = filemeta_cache.unwrap_or(&empty_cache);

    // Per-file outcome from the parallel phase. Each entry is either
    // Unchanged (carry forward cached meta), Skipped (error), Parsed
    // (freshly parsed), or CachedParsed (symbols loaded from the durable
    // parsed cache — counts as unchanged for reporting, but symbols are
    // available for resolution).
    enum ParseOutcome {
        Unchanged {
            rel_path: String,
        },
        Skipped(SkippedFile),
        Parsed {
            rel_path: String,
            lang: Language,
            file_meta: CachedFileMeta,
            content_hash: String,
            symbols: Vec<RawSymbol>,
            references: Vec<RawReference>,
            type_bindings: Vec<AstTypeBinding>,
            /// Full file source — retained only for languages whose parser does
            /// not fold class/route decorators into symbol signatures (e.g.
            /// TypeScript/NestJS), so framework detection can scan it.
            source: Option<String>,
        },
        /// Unchanged file whose symbols were loaded from the durable parsed
        /// cache. Treated as unchanged for filemeta carry-forward and
        /// reporting, but symbols/references are fed into the resolver.
        CachedParsed {
            rel_path: String,
            symbols: Vec<RawSymbol>,
            references: Vec<RawReference>,
            type_bindings: Vec<AstTypeBinding>,
        },
    }

    // Run change detection + parsing in parallel. Each file is independent:
    // stat, read, hash, and tree-sitter parse are all CPU/IO-bound work
    // that benefits from multi-core execution.
    use rayon::prelude::*;

    // Immutable borrow of the parsed cache for the parallel phase.
    let pc_ref: Option<&crate::parsed_cache::ParsedCache> = parsed_cache.as_deref();

    // Duty-cycle throttle for the CPU-bound phases below (parse + type-env
    // build) — keeps an indexing daemon under runningboardd's CPU limits.
    let cpu_throttle = crate::cpu_throttle::CpuThrottle::from_env();

    let parse_one = |(path, lang): &(PathBuf, Language)| -> ParseOutcome {
        let display_name = path
            .strip_prefix(repo_path)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();

        // Cooperative cancellation: once the daemon trips the flag (index
        // timeout or client disconnect), skip the expensive read+parse for
        // every remaining file so all cores are freed promptly. The index
        // then bails before any graph mutation (checked right after the
        // collect), so no partial/empty graph is ever persisted.
        if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed)) {
            parse_pb.inc(1);
            return ParseOutcome::Skipped(SkippedFile {
                path: display_name,
                reason: "index cancelled".to_string(),
            });
        }

        // Tiered change detection.
        let (source, content_hash, file_meta) =
            match tiered_change_check(reader, &display_name, cache) {
                Ok(ChangeVerdict::Unchanged) => {
                    parse_pb.inc(1);
                    // Check parsed cache: if we have cached symbols for this
                    // file's content hash, return CachedParsed so symbols are
                    // available for resolution while still counting as unchanged.
                    if let Some(cached_meta) = cache.get(&display_name)
                        && let Some(pc) = pc_ref
                        && let Some(cached_parse) = pc.get(&cached_meta.content_hash)
                    {
                        return ParseOutcome::CachedParsed {
                            rel_path: display_name,
                            symbols: cached_parse.symbols.clone(),
                            references: cached_parse.references.clone(),
                            type_bindings: cached_parse.type_bindings.clone(),
                        };
                    }
                    return ParseOutcome::Unchanged {
                        rel_path: display_name,
                    };
                }
                Ok(ChangeVerdict::Changed {
                    source,
                    content_hash,
                    meta,
                }) => (source, content_hash, meta),
                Err(err) => {
                    parse_pb.inc(1);
                    return ParseOutcome::Skipped(SkippedFile {
                        path: path.to_string_lossy().into_owned(),
                        reason: format!("stat/read error: {err}"),
                    });
                }
            };

        // Parse the file (CPU-bound tree-sitter work). Throttle first so
        // a saturated daemon yields often enough to stay under its CPU
        // budget (mirrors the cancellation check above).
        cpu_throttle.check();
        match parse_source(path, &source) {
            Ok(parsed) => {
                parse_pb.inc(1);
                // Retain source for all languages up to 2 MB so Phase 3
                // (type env build) can skip redundant disk re-reads.
                // TS/JS must clone because `source` is still needed below
                // for NestJS controller detection; other languages move it.
                const SOURCE_RETENTION_CAP: usize = 2 * 1024 * 1024;
                let retained_source =
                    if matches!(*lang, Language::TypeScript | Language::JavaScript) {
                        Some(source.clone())
                    } else if source.len() <= SOURCE_RETENTION_CAP {
                        Some(source)
                    } else {
                        None
                    };
                ParseOutcome::Parsed {
                    rel_path: display_name,
                    lang: *lang,
                    file_meta,
                    content_hash,
                    symbols: parsed.symbols,
                    references: parsed.references,
                    type_bindings: parsed.type_bindings,
                    source: retained_source,
                }
            }
            Err(err) => {
                parse_pb.inc(1);
                ParseOutcome::Skipped(SkippedFile {
                    path: path.to_string_lossy().into_owned(),
                    reason: err.to_string(),
                })
            }
        }
    };

    // Parse on the dedicated low-priority pool (utility QoS on macOS) so the
    // daemon's query serving on the global pool stays responsive; falls back
    // to the global pool if the dedicated one cannot be built.
    let outcomes: Vec<ParseOutcome> =
        crate::parse_pool::install_parse_pool(|| file_entries.par_iter().map(parse_one).collect());

    parse_pb.finish_and_clear();
    drop(_phase2_span);

    // Cooperative cancellation: if the flag tripped during the parallel parse,
    // bail now — BEFORE collection, resolution, and any graph mutation — so a
    // cancelled index never persists a partial/empty graph. The `?` returns
    // ahead of the write gate below, preserving the no-partial-write invariant.
    if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Acquire)) {
        anyhow::bail!("index cancelled");
    }

    // ── Sequential collection of parallel results ────────────────────────
    let _phase_collect_span = tracing::info_span!("index_phase_collect").entered();
    let mut all_files: Vec<File> = Vec::new();
    let mut all_symbols: Vec<Symbol> = Vec::new();
    let mut repo_file_edge_pairs: Vec<(String, String)> = Vec::new();
    let mut file_symbol_edge_pairs: Vec<(String, String)> = Vec::new();
    let mut parsed_files_for_resolver: Vec<ParsedFileEntry> = Vec::new();
    let mut ast_bindings_by_file: HashMap<String, Vec<AstTypeBinding>> = HashMap::new();
    let mut detected_languages: Vec<Language> = Vec::new();
    // F2.2: per framework file, the controller class signature + the (uid,
    // HandlerSymbol) of every symbol in the file, so handler detection can
    // map matches back to symbol UIDs after the bulk symbol insert.
    let mut handler_files: Vec<HandlerFileData> = Vec::new();
    let mut files_count = 0usize;
    let mut files_unchanged = 0usize;
    let mut files_deleted = 0usize;
    let mut symbols_deleted = 0usize;
    let mut symbols_count = 0usize;
    let mut skipped_files: Vec<SkippedFile> = Vec::new();
    let mut actually_changed_files: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    // Every file still present in the working tree this index, regardless of
    // whether it was Unchanged, CachedParsed, or Parsed. Used after the
    // re-insert cleanup to prune File/Symbol nodes for files that were removed
    // since the last index (e.g. a force-push that drops a file). Must NOT be
    // derived from `all_files`, which excludes Unchanged/CachedParsed files and
    // would wrongly delete still-present files.
    let mut present_files: std::collections::HashSet<String> = std::collections::HashSet::new();

    for outcome in outcomes {
        match outcome {
            ParseOutcome::Unchanged { rel_path } => {
                present_files.insert(rel_path.clone());
                // Carry forward the existing cache entry.
                if let (Some(ref mut new_cache), Some(cached)) =
                    (new_filemeta.as_deref_mut(), cache.get(&rel_path))
                {
                    new_cache.insert(rel_path, cached.clone());
                }
                files_unchanged += 1;
            }
            ParseOutcome::CachedParsed {
                rel_path,
                symbols: raw_symbols,
                references: raw_references,
                type_bindings: raw_type_bindings,
            } => {
                present_files.insert(rel_path.clone());
                // Carry forward the existing filemeta cache entry (file is unchanged).
                if let (Some(ref mut new_cache), Some(cached)) =
                    (new_filemeta.as_deref_mut(), cache.get(&rel_path))
                {
                    new_cache.insert(rel_path.clone(), cached.clone());
                }
                files_unchanged += 1;
                // Feed cached symbols/references into the resolver so cross-file
                // resolution works even when no files changed.
                symbols_count += raw_symbols.len();
                if !raw_type_bindings.is_empty() {
                    ast_bindings_by_file.insert(rel_path.clone(), raw_type_bindings);
                }
                parsed_files_for_resolver.push((rel_path, raw_symbols, raw_references, None));
            }
            ParseOutcome::Skipped(sf) => {
                skipped_files.push(sf);
            }
            ParseOutcome::Parsed {
                rel_path,
                lang,
                file_meta,
                content_hash,
                symbols: raw_symbols,
                references: raw_references,
                type_bindings: raw_type_bindings,
                source,
            } => {
                present_files.insert(rel_path.clone());
                actually_changed_files.insert(rel_path.clone());

                // Record in the new filemeta cache.
                if let Some(ref mut new_cache) = new_filemeta.as_deref_mut() {
                    new_cache.insert(rel_path.clone(), file_meta);
                }

                let f_uid = file_uid(&r_uid, &rel_path);

                all_files.push(File {
                    uid: f_uid.clone(),
                    path: rel_path.clone(),
                    repo_uid: r_uid.clone(),
                    content_hash,
                });

                repo_file_edge_pairs.push((r_uid.clone(), f_uid.clone()));
                files_count += 1;
                detected_languages.push(lang);

                // F2.0: run the (previously dormant) framework detector and
                // attach hints to the symbols it identifies. The detector keys
                // off the lowercase language string + per-symbol signatures.
                let mut hint_by_index: HashMap<usize, nestweaver_schema::FrameworkHint> =
                    HashMap::new();
                if let Some(lang_str) = crate::contracts::framework_language_str(lang) {
                    for (sym_idx, hint) in
                        nestweaver_parser::detect_frameworks(&raw_symbols, &rel_path, lang_str)
                    {
                        hint_by_index.insert(sym_idx, hint);
                    }
                }

                // F2.2: NestJS controllers carry `@Controller(...)` as a
                // decorator on the line *above* the class, which the TS parser
                // does not fold into the class signature, so `detect_frameworks`
                // misses it. Recover the controller from the retained source.
                let class_starts: Vec<(usize, u32)> = raw_symbols
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.kind == nestweaver_schema::SymbolKind::Class)
                    .map(|(i, s)| (i, s.start_line))
                    .collect();
                if let Some(src) = source.as_deref()
                    && let Some(ctrl_idx) =
                        crate::contracts::detect_nestjs_controller_index(src, &class_starts)
                {
                    hint_by_index.entry(ctrl_idx).or_insert_with(|| {
                        nestweaver_schema::FrameworkHint {
                            framework: "nestjs".into(),
                            role: "controller".into(),
                        }
                    });
                }

                // F2.2: if this file has a Spring/NestJS controller, capture the
                // data needed to derive IMPLEMENTS_CONTRACT edges later.
                let controller_framework = hint_by_index
                    .values()
                    .find(|h| h.role == "controller")
                    .map(|h| h.framework.clone());
                let mut handler_file = controller_framework.map(|framework| {
                    let class_signature = raw_symbols
                        .iter()
                        .zip(0..)
                        .find(|(_, i)| hint_by_index.get(i).is_some_and(|h| h.role == "controller"))
                        .map(|(s, _)| s.signature.clone())
                        .unwrap_or_default();
                    HandlerFileData {
                        framework,
                        class_signature,
                        rel_path: rel_path.clone(),
                        symbols: Vec::new(),
                    }
                });

                for (sym_idx, raw_sym) in raw_symbols.iter().enumerate() {
                    let s_uid = symbol_uid(&r_uid, &rel_path, &raw_sym.name, raw_sym.start_line);

                    if let Some(hf) = handler_file.as_mut() {
                        hf.symbols.push((
                            s_uid.clone(),
                            crate::contracts::HandlerSymbol {
                                name: raw_sym.name.clone(),
                                signature: raw_sym.signature.clone(),
                                start_line: raw_sym.start_line,
                            },
                        ));
                    }

                    let scope_str = raw_sym.scope_chain.as_deref().unwrap_or("");
                    let canonical =
                        canonical_symbol_id(repo_url, &rel_path, &raw_sym.name, scope_str);

                    all_symbols.push(Symbol {
                        uid: s_uid.clone(),
                        name: raw_sym.name.clone(),
                        kind: raw_sym.kind,
                        repo_uid: r_uid.clone(),
                        file_path: rel_path.clone(),
                        start_line: raw_sym.start_line,
                        end_line: raw_sym.end_line,
                        signature: raw_sym.signature.clone(),
                        summary: None,
                        content_hash: raw_sym.content_hash.clone(),
                        embedding: None,
                        pagerank_score: None,
                        is_entry_point: raw_sym.is_entry_point,
                        entry_point_kind: raw_sym.entry_point_kind,
                        visibility: raw_sym.visibility,
                        type_info: raw_sym.type_info.clone(),
                        framework_hint: hint_by_index.remove(&sym_idx),
                        canonical_id: Some(canonical),
                    });

                    file_symbol_edge_pairs.push((f_uid.clone(), s_uid.clone()));
                    symbols_count += 1;
                }

                if let Some(hf) = handler_file.take() {
                    handler_files.push(hf);
                }

                if !raw_type_bindings.is_empty() {
                    ast_bindings_by_file.insert(rel_path.clone(), raw_type_bindings);
                }
                parsed_files_for_resolver.push((rel_path, raw_symbols, raw_references, source));
            }
        }
    }

    tracing::info!(
        files_parsed = files_count,
        files_unchanged = files_unchanged,
        files_skipped = skipped_files.len(),
        symbols_collected = symbols_count,
        "phase collect complete"
    );

    // Populate the parsed cache with newly parsed files so future warm runs
    // can retrieve their symbols without re-parsing.
    if let Some(ref mut pc) = parsed_cache {
        for (rel_path, raw_symbols, raw_references, _source) in &parsed_files_for_resolver {
            // Only insert newly-parsed files, not ones loaded from cache.
            if !actually_changed_files.contains(rel_path.as_str()) {
                continue;
            }
            // Look up the content hash from the new filemeta cache.
            let content_hash = new_filemeta
                .as_deref()
                .and_then(|fm| fm.get(rel_path))
                .map(|m| m.content_hash.clone());
            if let Some(hash) = content_hash {
                pc.insert(
                    hash,
                    crate::parsed_cache::CachedParseResult {
                        symbols: raw_symbols.clone(),
                        references: raw_references.clone(),
                        type_bindings: ast_bindings_by_file
                            .get(rel_path.as_str())
                            .cloned()
                            .unwrap_or_default(),
                    },
                );
            }
        }
        tracing::debug!(parsed_cache_entries = pc.len(), "parsed cache updated");
    }

    drop(_phase_collect_span);

    let _write_guard = acquire_write_guard()?;
    let publication = establish_index_publication_marker_with_io(
        store,
        store.db_path(),
        "index graph write",
        epilogue_io,
    )?;
    let graph_mutation_attempted = std::cell::Cell::new(false);
    let graph_result = (|| -> Result<IndexResult, anyhow::Error> {
        // Re-identify prune: when a local repo previously indexed under a
        // `file://<root_path>` identity is now indexed under a different
        // identity (its git origin remote), the old file:// node is a stale
        // duplicate of the same working tree. Prune it STRICTLY by uid — never
        // by disk path — so unrelated repos can never be caught by this delete.
        // Detected before the parse phase (see above) so the filemeta cache was
        // already bypassed for this pass.
        if let Some(old_uid) = &reidentify_old_uid {
            graph_mutation_attempted.set(true);
            tracing::info!(
                old_uid,
                new_uid = %r_uid,
                root_path = root_path.unwrap_or(""),
                url = repo_url,
                "repo re-identified under its origin remote; pruning old file:// node by uid"
            );
            let (deleted_files, deleted_symbols) = delete_repo_all_data(store, old_uid)
                .context("delete_repo_all_data (re-identify prune)")?;
            files_deleted += deleted_files;
            symbols_deleted += deleted_symbols;
        }

        // Insert the Repo node if it doesn't exist yet. The target SHA is recorded
        // only after every required graph write succeeds, so a later write failure
        // cannot make retry preparation think this commit is already indexed.
        let existing_repo = store.lookup_repo(&r_uid).context("lookup_repo")?;
        // Every successful path below performs at least one graph write. Mark the
        // attempt before the first one so even a partially committed store error
        // is conservatively finalized.
        graph_mutation_attempted.set(true);
        if existing_repo.is_none() {
            let repo = Repo {
                uid: r_uid.clone(),
                url: repo_url.trim_end_matches('/').to_string(),
                indexed_sha: String::new(),
                staleness_commits_behind: 0,
                instance_id: instance_id.to_string(),
                name: name.map(String::from),
                root_path: root_path.map(String::from),
            };
            store.insert_repo(&repo).context("insert_repo")?;
        } else if let (Some(rp), Some(existing)) = (root_path, existing_repo.as_ref())
            && existing.root_path.as_deref() != Some(rp)
        {
            // Keep the on-disk location current for pre-existing rows (old DBs
            // that predate root_path, or a working tree that moved).
            store
                .update_repo_root_path(&r_uid, rp)
                .context("update_repo_root_path")?;
        }

        // 2b. When re-indexing over an existing store (tiered detection is active
        //     and some files changed), clean up old File nodes and their symbols
        //     for files we are about to re-insert.
        //
        //     For the incremental path, per-file deletes happen here (the window
        //     is tiny per-file). For the force re-index path, the bulk delete is
        //     deferred to step 3 and runs inside the same transaction as the
        //     insert — see `bulk_reindex_write` — to prevent concurrent readers
        //     from seeing zero symbols while the CPU-heavy service-grouping work
        //     runs between delete and insert.
        let force_reindex = existing_repo.is_some() && files_unchanged == 0;
        if !force_reindex && existing_repo.is_some() {
            // Incremental: only delete the specific files we're about to re-insert.
            for file in &all_files {
                // Remove old symbols belonging to this file.
                let _ = store.delete_symbols_in_file(&r_uid, &file.path);
                // Remove old File node.
                let _ = store.delete_file_node(&file.uid);
            }
            // Prune File/Symbol nodes for files that vanished since the last
            // index (e.g. a force-push that removed a file). The incremental
            // branch above only deletes files being re-inserted, so without
            // this pass removed files would linger. `present_files` covers
            // Unchanged/CachedParsed/Parsed files — anything in the store but
            // not present anymore is stale and gets dropped.
            if let Ok(stored_files) = store.list_files_by_repo(&r_uid) {
                for (f_uid, path) in &stored_files {
                    if !present_files.contains(path) {
                        let _ = store.delete_symbols_in_file(&r_uid, path);
                        let _ = store.delete_file_node(f_uid);
                        // nw-055 (P1b): a vanished file is a genuine deletion. Count
                        // it so the index-time PageRank guard fires on a delete-only
                        // re-index (files_count == 0) instead of leaving surviving
                        // nodes' ranks stale.
                        files_deleted += 1;
                    }
                }
            }
            // Clear repo-scoped derived nodes (Service, Contract) before
            // re-insert. `bulk_index_write` plain-CREATEs Service nodes whose UID
            // is derived deterministically from repo_uid + directory, so an
            // incremental re-index would otherwise collide on the primary key.
            let _ = store.clear_repo_derived_nodes(&r_uid);
        }

        // 3-7. Build service groupings and perform all bulk inserts in a single transaction.
        let _phase_write_span = tracing::info_span!("index_phase_write").entered();
        let mut dir_symbols: HashMap<String, Vec<String>> = HashMap::new();
        for sym in &all_symbols {
            let dir = sym
                .file_path
                .rsplit_once('/')
                .map(|(d, _)| d.to_string())
                .unwrap_or_default();
            if !dir.is_empty() {
                dir_symbols.entry(dir).or_default().push(sym.uid.clone());
            }
        }

        let mut all_services: Vec<Service> = Vec::new();
        let mut service_symbol_pairs: Vec<(String, String)> = Vec::new();
        for (dir, sym_uids) in &dir_symbols {
            let svc_uid = service_uid(&r_uid, dir);
            all_services.push(Service {
                uid: svc_uid.clone(),
                name: dir.clone(),
                repo_uid: r_uid.clone(),
                summary: None,
                summary_hash: None,
                embedding: None,
            });
            for sym_uid in sym_uids {
                service_symbol_pairs.push((svc_uid.clone(), sym_uid.clone()));
            }
        }

        let repo_file_refs: Vec<(&str, &str)> = repo_file_edge_pairs
            .iter()
            .map(|(r, f)| (r.as_str(), f.as_str()))
            .collect();
        let file_sym_refs: Vec<(&str, &str)> = file_symbol_edge_pairs
            .iter()
            .map(|(f, s)| (f.as_str(), s.as_str()))
            .collect();
        let svc_sym_refs: Vec<(&str, &str)> = service_symbol_pairs
            .iter()
            .map(|(s, sym)| (s.as_str(), sym.as_str()))
            .collect();

        if force_reindex {
            // Atomic delete+insert: old data is only removed within the same
            // transaction that inserts the replacement, so concurrent readers
            // never see an empty repo.
            let (deleted_files, deleted_symbols) = store
                .bulk_reindex_write(
                    &r_uid,
                    &all_files,
                    &all_symbols,
                    &repo_file_refs,
                    &file_sym_refs,
                    &all_services,
                    &svc_sym_refs,
                )
                .context("bulk_reindex_write")?;
            files_deleted += deleted_files;
            symbols_deleted += deleted_symbols;
        } else {
            store
                .bulk_index_write(
                    &all_files,
                    &all_symbols,
                    &repo_file_refs,
                    &file_sym_refs,
                    &all_services,
                    &svc_sym_refs,
                )
                .context("bulk_index_write")?;
        }
        tracing::info!(
            files_written = all_files.len(),
            symbols_written = all_symbols.len(),
            services_written = all_services.len(),
            "phase write complete"
        );
        drop(_phase_write_span);

        // ── Phase 3: Resolve cross-file references ────────────────────────────
        let _phase_resolve_span = tracing::info_span!("index_phase_resolve").entered();
        let resolve_pb = ProgressBar::new_spinner();
        resolve_pb.set_style(
            ProgressStyle::with_template("{spinner:.cyan} {msg}")
                .unwrap_or_else(|_| ProgressStyle::default_spinner()),
        );
        resolve_pb.set_message("Resolving cross-file references...");
        resolve_pb.enable_steady_tick(std::time::Duration::from_millis(100));

        // 8. Run full cross-file resolution via the resolver.
        //    Use the most common language detected across files; fall back to JavaScript.
        let language = {
            let mut counts: HashMap<Language, usize> = HashMap::new();
            for l in &detected_languages {
                *counts.entry(*l).or_insert(0) += 1;
            }
            counts
                .into_iter()
                .max_by_key(|(_, c)| *c)
                .map(|(l, _)| l)
                .unwrap_or(Language::JavaScript)
        };

        // Load workspace context (monorepo packages + tsconfig aliases) for JS/TS resolution.
        // Uses the ContentReader so this works with both filesystem and bare-repo readers.
        let workspace_ctx = if matches!(
            language,
            Language::JavaScript
                | Language::TypeScript
                | Language::Vue
                | Language::Svelte
                | Language::Astro
        ) {
            discover_workspace_context_with(|rel_path| {
                reader
                    .read_file(rel_path)
                    .map_err(|e| std::io::Error::other(e.to_string()))
            })
        } else {
            Default::default()
        };

        // Build type environments per file for type-aware resolution.
        // Each file's type env is independent, so we build them in parallel.
        let mut type_envs: HashMap<String, nestweaver_resolver::types::TypeEnvironment> =
            parsed_files_for_resolver
                .par_iter()
                .filter_map(|(file_path, symbols, _references, source_opt)| {
                    // Same CPU budget as the parse phase above.
                    cpu_throttle.check();
                    let source_owned;
                    let source: &str = if let Some(s) = source_opt.as_deref() {
                        s
                    } else {
                        source_owned = reader.read_file(Path::new(file_path.as_str())).ok()?;
                        &source_owned
                    };

                    let empty_bindings = Vec::new();
                    let file_ast_bindings = ast_bindings_by_file
                        .get(file_path.as_str())
                        .unwrap_or(&empty_bindings);

                    let env = nestweaver_resolver::types::TypeEnvironment::build(
                        source,
                        language,
                        symbols,
                        file_ast_bindings,
                    );

                    if env.binding_count() > 0 {
                        Some((file_path.clone(), env))
                    } else {
                        None
                    }
                })
                .collect();
        tracing::info!(
            files_with_bindings = type_envs.len(),
            total_bindings = type_envs.values().map(|e| e.binding_count()).sum::<usize>(),
            "type environments built"
        );

        // Cross-file return type propagation: seed bindings from known function return types
        {
            let all_symbols_with_returns: std::collections::HashMap<
                &str,
                &nestweaver_parser::RawSymbol,
            > = parsed_files_for_resolver
                .iter()
                .flat_map(|(_, syms, _, _)| syms.iter())
                .filter(|s| {
                    s.type_info
                        .as_ref()
                        .and_then(|ti| ti.return_type.as_ref())
                        .is_some()
                })
                .map(|s| (s.name.as_str(), s))
                .collect();

            if !all_symbols_with_returns.is_empty() {
                let mut seeded = 0usize;
                for (file_path, _symbols, _refs, source_opt) in &parsed_files_for_resolver {
                    if let Some(env) = type_envs.get_mut(file_path) {
                        let source_str = match source_opt {
                            Some(s) => s.clone(),
                            None => match reader.read_file(Path::new(file_path.as_str())) {
                                Ok(s) => s,
                                Err(_) => continue,
                            },
                        };
                        let before = env.binding_count();
                        env.seed_return_types(&source_str, &all_symbols_with_returns);
                        seeded += env.binding_count() - before;
                    }
                }
                if seeded > 0 {
                    tracing::info!(new_bindings = seeded, "cross-file return type propagation");
                }
            }
        }

        // Build a 3-tuple view for the resolver (it does not need source strings).
        let resolver_view: Vec<(String, Vec<RawSymbol>, Vec<RawReference>)> =
            parsed_files_for_resolver
                .iter()
                .map(|(path, syms, refs, _)| (path.clone(), syms.clone(), refs.clone()))
                .collect();

        // Compute the incremental resolution filter: only re-resolve files that
        // changed plus files that depend on changed files.
        // When no files changed and we have prior resolution data, skip resolution
        // entirely — edges from the previous run are still valid in the DB.
        let skip_resolution = actually_changed_files.is_empty()
            && resolution_deps
                .as_ref()
                .is_some_and(|rd| !rd.is_empty_for_repo(&r_uid));

        let resolve_filter = if !skip_resolution
            && !actually_changed_files.is_empty()
            && files_unchanged > 0
            && resolution_deps
                .as_ref()
                .is_some_and(|rd| !rd.is_empty_for_repo(&r_uid))
        {
            let affected = resolution_deps
                .as_ref()
                .unwrap()
                .affected_files_for_repo(&r_uid, &actually_changed_files);
            tracing::info!(
                changed = actually_changed_files.len(),
                affected = affected.len(),
                total = resolver_view.len(),
                "incremental resolution"
            );
            Some(affected)
        } else {
            None
        };

        if skip_resolution {
            tracing::info!("no files changed, skipping resolution");
        }

        let resolved_edges = if skip_resolution {
            Vec::new()
        } else {
            resolve_references_with_context(
                &resolver_view,
                language,
                &r_uid,
                &workspace_ctx,
                Some(&type_envs),
                resolve_filter.as_ref(),
            )
        };

        // Filter out unresolved edges whose target doesn't exist in the DB.
        let insertable_edges: Vec<_> = resolved_edges
            .into_iter()
            .filter(|e| !e.target_uid.starts_with("unresolved:"))
            .collect();

        // When doing incremental resolution, delete old resolved edges for
        // affected files before inserting the new ones.
        if let Some(ref filter) = resolve_filter {
            for file_path in filter {
                let _ = store.delete_resolved_edges_for_file(&r_uid, file_path);
            }
        }

        let mut edges_count = insertable_edges.len();
        store
            .batch_insert_edges(&insertable_edges)
            .context("batch_insert_edges (resolved)")?;

        let inferred_cross_repo_edges =
            infer_cross_repo_call_edges(store, &r_uid, &parsed_files_for_resolver)?;
        if !inferred_cross_repo_edges.is_empty() {
            edges_count += inferred_cross_repo_edges.len();
            store
                .batch_insert_edges(&inferred_cross_repo_edges)
                .context("batch_insert_edges (inferred cross-repo calls)")?;
            tracing::debug!(
                count = inferred_cross_repo_edges.len(),
                "emitted inferred CROSS_REPO_LINK edges"
            );
        }

        // Record file-level dependency information for future incremental runs.
        if let Some(ref mut rd) = resolution_deps {
            // Build symbol UID → file path map from ALL files (including cached)
            // so incremental runs don't lose edges from CachedParsed files.
            let symbol_file_index: HashMap<String, String> = parsed_files_for_resolver
                .iter()
                .flat_map(|(path, syms, _, _)| {
                    let r_uid_ref = &r_uid;
                    syms.iter().map(move |s| {
                        let uid = symbol_uid(r_uid_ref, path, &s.name, s.start_line);
                        (uid, path.clone())
                    })
                })
                .collect();
            let mut file_deps: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
            for edge in &insertable_edges {
                if let (Some(src_file), Some(tgt_file)) = (
                    symbol_file_index.get(&edge.source_uid),
                    symbol_file_index.get(&edge.target_uid),
                ) && src_file != tgt_file
                {
                    file_deps
                        .entry(src_file.clone())
                        .or_default()
                        .insert(tgt_file.clone());
                }
            }
            for (file, deps) in file_deps {
                rd.set_deps_for_repo(&r_uid, file, deps);
            }
        }

        // ── Structural MEMBER_OF edges ────────────────────────────────────────
        // Build a lookup: (file_path, type_name) → type_symbol_uid for all
        // container symbols (Class / Interface / Enum / Trait).  Then for every
        // raw symbol that carries a parent_name, emit a MEMBER_OF edge from the
        // member to its parent container.
        {
            use nestweaver_schema::{EdgeType, ResolvedEdge};

            let container_kinds = [
                nestweaver_schema::SymbolKind::Class,
                nestweaver_schema::SymbolKind::Interface,
                nestweaver_schema::SymbolKind::Enum,
                nestweaver_schema::SymbolKind::Trait,
            ];

            // (file_path, type_name) → uid — built from ALL files (including cached)
            // so incremental runs don't lose MEMBER_OF edges for CachedParsed files.
            let mut container_map: HashMap<(String, String), String> = HashMap::new();
            for (rel_path, raw_symbols, _, _) in &parsed_files_for_resolver {
                for raw_sym in raw_symbols {
                    if container_kinds.contains(&raw_sym.kind) {
                        let uid = symbol_uid(&r_uid, rel_path, &raw_sym.name, raw_sym.start_line);
                        container_map.insert((rel_path.clone(), raw_sym.name.clone()), uid);
                    }
                }
            }

            let mut member_of_edges: Vec<ResolvedEdge> = Vec::new();
            for (rel_path, raw_symbols, _, _) in &parsed_files_for_resolver {
                for raw_sym in raw_symbols {
                    if let Some(parent_name) = &raw_sym.parent_name {
                        let key = (rel_path.clone(), parent_name.clone());
                        if let Some(parent_uid) = container_map.get(&key) {
                            let child_uid =
                                symbol_uid(&r_uid, rel_path, &raw_sym.name, raw_sym.start_line);
                            member_of_edges.push(ResolvedEdge {
                                source_uid: child_uid,
                                target_uid: parent_uid.clone(),
                                edge_type: EdgeType::MemberOf,
                                confidence: 1.0,
                                link_type: None,
                                evidence: Vec::new(),
                            });
                        }
                    }
                }
            }

            if !member_of_edges.is_empty() {
                edges_count += member_of_edges.len();
                store
                    .batch_insert_edges(&member_of_edges)
                    .context("batch_insert_edges (member_of)")?;
                tracing::debug!(count = member_of_edges.len(), "emitted MEMBER_OF edges");
            }
        }

        resolve_pb.finish_and_clear();
        tracing::info!(edges_resolved = edges_count, "phase resolve complete");
        drop(_phase_resolve_span);

        // ── Phase 4 (F2-core): derive the API contract graph ──────────────────
        let _phase_contracts_span = tracing::info_span!("index_phase_contracts").entered();
        // Best-effort: a malformed spec or unexpected store error here must not
        // fail the whole index. Contracts are hypotheses layered on top of the
        // code graph.
        if let Err(e) = derive_contracts(store, reader, &r_uid, &spec_files, &handler_files) {
            tracing::warn!("contract derivation failed (non-fatal): {e}");
        }
        tracing::info!(
            spec_files = spec_files.len(),
            handler_files = handler_files.len(),
            "phase contracts complete"
        );
        drop(_phase_contracts_span);

        store
            .update_repo_sha(&r_uid, indexed_sha)
            .context("update_repo_sha")?;

        // ── Summary ───────────────────────────────────────────────────────────
        let elapsed = started.elapsed();
        if files_unchanged > 0 {
            tracing::info!(
                files = files_count,
                files_unchanged,
                symbols = symbols_count,
                edges = edges_count,
                elapsed_secs = format!("{:.1}", elapsed.as_secs_f64()),
                "Done: {} files ({} unchanged, skipped), {} symbols, {} edges ({:.1}s)",
                files_count,
                files_unchanged,
                symbols_count,
                edges_count,
                elapsed.as_secs_f64(),
            );
        } else {
            tracing::info!(
                files = files_count,
                symbols = symbols_count,
                edges = edges_count,
                elapsed_secs = format!("{:.1}", elapsed.as_secs_f64()),
                "Done: {} files, {} symbols, {} edges ({:.1}s)",
                files_count,
                symbols_count,
                edges_count,
                elapsed.as_secs_f64(),
            );
        }

        tracing::info!(
            total_files = files_count,
            files_unchanged = files_unchanged,
            symbols = symbols_count,
            "indexing complete"
        );

        let result = IndexResult {
            symbols_count,
            edges_count,
            files_count,
            files_unchanged,
            files_deleted,
            symbols_deleted,
            skipped_files,
        };

        Ok(result)
    })();

    if !graph_mutation_attempted.get() {
        if publication.is_recovered() {
            return match graph_result {
                // A recovered owner cannot prove that the preceding owner made
                // no graph changes. A successful no-op therefore heals that
                // unknown committed graph as one unified publication.
                Ok(result) => finalize_committed_index_for_scope_with_io(
                    publication,
                    store.db_path(),
                    "recovered index graph write",
                    epilogue_io,
                    Some(&nestweaver_store::GraphScope::unified()),
                )
                .map(|()| result)
                .map_err(anyhow::Error::from),
                // On an early error there is no safe finalization point. Drop
                // only the live lease; keep the inherited marker/reservation
                // so the next open continues to fail closed.
                Err(primary) => Err(primary),
            };
        }
        if let Some(db_path) = store.db_path() {
            let marker_path = crate::sidecar_path(db_path, ".index-dirty");
            let cancellation = store.with_index_publication_rank_barrier(|| {
                publication.cancel_generation()?;
                epilogue_io.clear_marker(&marker_path)?;
                Ok::<(), anyhow::Error>(())
            });
            if let Err(marker_error) = cancellation {
                return match graph_result {
                    Ok(_) => Err(marker_error),
                    Err(primary) => {
                        let primary_message = format!("{primary:#}");
                        Err(primary.context(format!(
                            "{primary_message}; additionally, failed to retire uncommitted index marker: {marker_error:#}"
                        )))
                    }
                };
            }
        }
        publication.release()?;
        return graph_result;
    }

    // The write guard stays alive through this mandatory epilogue. Publish
    // invalidation/generation/PageRank on success and every later graph error.
    let finalization = finalize_committed_index_with_io(
        publication,
        store.db_path(),
        "index graph write",
        epilogue_io,
        bump_generation_after_write,
    );
    match (graph_result, finalization) {
        (Ok(result), Ok(())) => Ok(result),
        (Ok(_), Err(finalization)) => Err(finalization.into()),
        (Err(primary), Ok(())) => Err(primary),
        (Err(primary), Err(finalization)) => {
            let primary_message = format!("{primary:#}");
            Err(primary.context(format!(
                "{primary_message}; additionally, mandatory index finalization failed: {finalization}"
            )))
        }
    }
}

/// Returns true if the given path has a supported language extension.
fn is_parseable(path: &Path) -> bool {
    detect_language(path).is_some()
}

/// F2-core: build the API contract graph for one repo.
///
/// 1. Parse spec files into **declared** [`nestweaver_schema::Contract`] nodes
///    (confidence 1.0).
/// 2. For each Spring/NestJS controller file, match handlers to contracts. When
///    a spec already declares the route, link to it (exact-match confidence);
///    otherwise mint a **code-derived** contract and link to that.
/// 3. Emit `IMPLEMENTS_CONTRACT` edges (handler Symbol → Contract) carrying the
///    match confidence (1.0 exact verb+path, 0.8 base-path-inferred).
fn derive_contracts(
    store: &GraphStore,
    reader: &dyn crate::content_reader::ContentReader,
    r_uid: &str,
    spec_files: &[PathBuf],
    handler_files: &[HandlerFileData],
) -> Result<(), anyhow::Error> {
    use nestweaver_schema::{EdgeType, ResolvedEdge};
    use std::collections::HashSet;

    // Bulk delete existing contracts for this repo in one query.
    store.clear_repo_contracts(r_uid)?;

    // 1. Declared contracts from specs.
    let mut all_contracts: Vec<nestweaver_schema::Contract> = Vec::new();
    let mut declared_uids: HashSet<String> = HashSet::new();
    let repo_path = reader.root();
    for spec_path in spec_files {
        let rel = spec_path
            .strip_prefix(repo_path)
            .unwrap_or(spec_path)
            .to_string_lossy()
            .into_owned();
        let source = match reader.read_file(Path::new(&rel)) {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!("skip unreadable spec {rel}: {e}");
                continue;
            }
        };
        for sc in crate::contracts::parse_spec_file(&rel, &source) {
            let contract = sc.into_contract(r_uid, &rel, 1.0);
            declared_uids.insert(contract.uid.clone());
            all_contracts.push(contract);
        }
    }

    // 2 + 3. Handler matches → contracts + IMPLEMENTS_CONTRACT edges.
    let mut edges: Vec<ResolvedEdge> = Vec::new();
    for hf in handler_files {
        let handler_syms: Vec<crate::contracts::HandlerSymbol> =
            hf.symbols.iter().map(|(_, hs)| hs.clone()).collect();
        // The parsed class signature only keeps the first annotation, so the
        // class-level base path (@RequestMapping / @Controller) is usually
        // dropped. Read the raw source to recover it; fall back to the
        // truncated signature if the file is unreadable.
        let base_source = reader
            .read_file(Path::new(&hf.rel_path))
            .unwrap_or_else(|_| hf.class_signature.clone());
        let matches = crate::contracts::detect_handlers(&hf.framework, &base_source, &handler_syms);
        for m in matches {
            let contract_uid = m.contract.uid();
            // Mint a code-derived contract only when no spec declared this UID.
            if !declared_uids.contains(&contract_uid) {
                let contract = m
                    .contract
                    .clone()
                    .into_contract(r_uid, &hf.rel_path, m.confidence);
                all_contracts.push(contract);
            }
            if let Some((sym_uid, _)) = hf.symbols.get(m.symbol_index) {
                edges.push(ResolvedEdge {
                    source_uid: sym_uid.clone(),
                    target_uid: contract_uid,
                    edge_type: EdgeType::ImplementsContract,
                    confidence: m.confidence,
                    link_type: None,
                    evidence: Vec::new(),
                });
            }
        }
    }

    // Batch insert all contracts at once via COPY FROM CSV.
    store.batch_insert_contracts(&all_contracts)?;

    if !edges.is_empty() {
        store.batch_insert_edges(&edges)?;
    }
    Ok(())
}

/// Returns true if the file looks like a minified bundle, webpack output,
/// or other generated artifact that would produce noise in the graph.
pub(crate) fn is_minified_or_bundled(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name.ends_with(".min.js")
        || name.ends_with(".min.ts")
        || name.ends_with(".bundle.js")
        || name.ends_with(".chunk.js")
    {
        return true;
    }
    // Webpack/Vite hashed filenames: app.a1b2c3d4.js, chunk-HASH.js
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
        let parts: Vec<&str> = stem.split('.').collect();
        if parts.len() >= 2 {
            let last = parts.last().unwrap_or(&"");
            if last.len() >= 8 && last.chars().all(|c| c.is_ascii_hexdigit()) {
                return true;
            }
        }
    }
    false
}

/// Returns true if any component of `path` is in `SKIP_DIRS`.
pub(crate) fn path_in_skip_dir(path: &Path) -> bool {
    path.components().any(|c| {
        c.as_os_str()
            .to_str()
            .is_some_and(|name| SKIP_DIRS.contains(&name))
    })
}

/// Result returned by `incremental_index`.
#[derive(Debug, Default)]
pub struct IncrementalResult {
    pub files_added: usize,
    pub files_modified: usize,
    pub files_deleted: usize,
    pub files_renamed: usize,
    pub files_skipped: usize,
    pub symbols_added: usize,
    pub symbols_removed: usize,
    pub fell_back_to_full: bool,
}

/// Incrementally re-index a repository using git diff.
///
/// Opens the store at `db_path`, looks up the previously indexed SHA, and
/// then only processes files that changed between that SHA and the current
/// HEAD.  Falls back to a full `index_directory` run when:
/// - No Repo node exists in the store yet.
/// - The previously indexed SHA is not an ancestor of the current HEAD
///   (e.g. force-push / rebase).
///
/// When `name` is `Some`, it is forwarded to the full-index fallback path
/// so the Repo node is created with the display name override.
pub fn incremental_index(
    repo_path: &Path,
    db_path: &Path,
    instance_id: &str,
    repo_url: &str,
) -> Result<IncrementalResult, anyhow::Error> {
    incremental_index_with_name(repo_path, db_path, instance_id, repo_url, None)
}

/// Like [`incremental_index`] but accepts an optional display name override.
pub fn incremental_index_with_name(
    repo_path: &Path,
    db_path: &Path,
    instance_id: &str,
    repo_url: &str,
    name: Option<&str>,
) -> Result<IncrementalResult, anyhow::Error> {
    incremental_index_with_name_and_io(
        repo_path,
        db_path,
        instance_id,
        repo_url,
        name,
        &FileSystemIndexEpilogueIo,
    )
}

fn incremental_index_with_name_and_io(
    repo_path: &Path,
    db_path: &Path,
    instance_id: &str,
    repo_url: &str,
    name: Option<&str>,
    epilogue_io: &dyn IndexEpilogueIo,
) -> Result<IncrementalResult, anyhow::Error> {
    let store = nestweaver_store::GraphStore::open_or_create(db_path)
        .with_context(|| format!("open/create store at {}", db_path.display()))?;

    let r_uid = nestweaver_schema::repo_uid(instance_id, repo_url);

    // 1. Look up existing Repo.
    let existing_repo = store
        .lookup_repo(&r_uid)
        .with_context(|| "lookup_repo failed")?;

    let new_sha = match crate::git_diff::current_head_sha(repo_path) {
        Ok(sha) => sha,
        Err(_) => {
            tracing::info!("not a git repo; falling back to full index");
            return full_index_fallback(
                &store,
                FullIndexFallback {
                    repo_path,
                    db_path,
                    instance_id,
                    repo_url,
                    new_sha: "local",
                    name,
                    force: false,
                    epilogue_io,
                },
            );
        }
    };

    // 2. If no existing Repo → full index.
    let old_sha = match existing_repo {
        None => {
            tracing::info!("no existing repo found; falling back to full index");
            return full_index_fallback(
                &store,
                FullIndexFallback {
                    repo_path,
                    db_path,
                    instance_id,
                    repo_url,
                    new_sha: &new_sha,
                    name,
                    force: false,
                    epilogue_io,
                },
            );
        }
        Some(r) => r.indexed_sha,
    };

    // 2b. Self-heal an incomplete index BEFORE the up-to-date shortcut below:
    // an empty indexed_sha (Repo row created but SHA never committed — today
    // only handled implicitly via `is_ancestor("")` → false) or a committed
    // SHA with zero symbols (crash between the SHA write and content landing)
    // can never be repaired incrementally, and would otherwise self-perpetuate
    // through the `old_sha == new_sha` skip.
    let index_incomplete = old_sha.is_empty()
        || !store
            .repo_has_symbols(&r_uid)
            .with_context(|| "repo_has_symbols failed")?;
    if index_incomplete {
        tracing::warn!(
            old_sha,
            "index is incomplete (empty SHA or no symbols); forcing full re-index"
        );
        return full_index_fallback(
            &store,
            FullIndexFallback {
                repo_path,
                db_path,
                instance_id,
                repo_url,
                new_sha: &new_sha,
                name,
                // Force the core path so bulk_reindex_write deletes the old
                // graph and installs its replacement in one transaction.
                force: true,
                epilogue_io,
            },
        );
    }

    // 3. Verify old_sha is an ancestor of new_sha.
    if !crate::git_diff::is_ancestor(repo_path, &old_sha, &new_sha) {
        tracing::warn!(
            old_sha,
            new_sha,
            "old SHA is not an ancestor of HEAD; falling back to full re-index"
        );
        return full_index_fallback(
            &store,
            FullIndexFallback {
                repo_path,
                db_path,
                instance_id,
                repo_url,
                new_sha: &new_sha,
                name,
                // Force the core path so bulk_reindex_write deletes the old
                // graph and installs its replacement in one transaction.
                force: true,
                epilogue_io,
            },
        );
    }

    // 4. Nothing changed.
    if old_sha == new_sha {
        tracing::debug!(sha = old_sha, "repo is already up to date; skipping");
        return Ok(IncrementalResult::default());
    }

    // 5. Detect file-level changes.
    let changes = crate::git_diff::detect_changes(repo_path, &old_sha, &new_sha)
        .with_context(|| "detect_changes")?;

    tracing::info!(
        count = changes.len(),
        old_sha,
        new_sha,
        "processing incremental changes"
    );

    let reader = crate::content_reader::FilesystemReader::new(repo_path);
    let mut result = IncrementalResult::default();

    // nw-008 Phase 0 — transitive reverse-dependents from the LIVE graph, BEFORE
    // any mutation (the per-file `DETACH DELETE` destroys the edges we walk).
    let (changed_files, removed_files) = partition_changed_removed(&changes);
    let rdeps = collect_reverse_dep_files(&store, &r_uid, &changed_files, &removed_files);

    let publication = establish_index_publication_marker_with_io(
        &store,
        Some(db_path),
        "incremental index",
        epilogue_io,
    )?;

    // Wrap the entire incremental update in a single transaction so that a
    // crash mid-index doesn't leave partial data in the store. The indexed
    // SHA is updated inside the transaction — if we crash before commit, the
    // next run replays from the old SHA.
    let txn = store
        .begin_transaction()
        .with_context(|| "begin incremental transaction")?;

    for change in &changes {
        match change {
            crate::git_diff::FileChange::Added(rel_path) => {
                if path_in_skip_dir(rel_path) || !is_parseable(rel_path) {
                    result.files_skipped += 1;
                    continue;
                }
                let added = process_added_or_modified_file_txn(
                    &reader, rel_path, &r_uid, repo_url, &store, &txn,
                )?;
                result.symbols_added += added;
                result.files_added += 1;
            }
            crate::git_diff::FileChange::Modified(rel_path) => {
                if path_in_skip_dir(rel_path) || !is_parseable(rel_path) {
                    result.files_skipped += 1;
                    continue;
                }
                // Remove old symbols first.
                let rel_str = rel_path.to_string_lossy();
                let removed =
                    nestweaver_store::GraphStore::delete_symbols_in_file_on(&txn, &r_uid, &rel_str)
                        .with_context(|| format!("delete_symbols_in_file {}", rel_str))?;
                result.symbols_removed += removed;

                // Re-parse and insert.
                let added = process_added_or_modified_file_txn(
                    &reader, rel_path, &r_uid, repo_url, &store, &txn,
                )?;
                result.symbols_added += added;
                result.files_modified += 1;
            }
            crate::git_diff::FileChange::Deleted(rel_path) => {
                let rel_str = rel_path.to_string_lossy();
                let removed =
                    nestweaver_store::GraphStore::delete_symbols_in_file_on(&txn, &r_uid, &rel_str)
                        .with_context(|| format!("delete_symbols_in_file {}", rel_str))?;
                result.symbols_removed += removed;

                let f_uid = nestweaver_schema::file_uid(&r_uid, &rel_str);
                nestweaver_store::GraphStore::delete_file_node_on(&txn, &f_uid)
                    .with_context(|| format!("delete_file_node {}", rel_str))?;
                result.files_deleted += 1;
            }
            crate::git_diff::FileChange::Renamed { from, to } => {
                let from_str = from.to_string_lossy();
                let to_str = to.to_string_lossy();

                if is_parseable(to) && !path_in_skip_dir(to) {
                    // Update symbol file_path references.
                    nestweaver_store::GraphStore::update_symbol_file_paths_on(
                        &txn, &r_uid, &from_str, &to_str,
                    )
                    .with_context(|| {
                        format!("update_symbol_file_paths {} -> {}", from_str, to_str)
                    })?;
                } else {
                    // Destination is not parseable — just delete the old symbols.
                    let removed = nestweaver_store::GraphStore::delete_symbols_in_file_on(
                        &txn, &r_uid, &from_str,
                    )
                    .with_context(|| format!("delete_symbols_in_file {}", from_str))?;
                    result.symbols_removed += removed;
                }

                // Re-key the File node: delete old, insert new.
                let old_f_uid = nestweaver_schema::file_uid(&r_uid, &from_str);
                nestweaver_store::GraphStore::delete_file_node_on(&txn, &old_f_uid)
                    .with_context(|| format!("delete_file_node (rename from) {}", from_str))?;

                if is_parseable(to) && !path_in_skip_dir(to) {
                    // Re-read from disk and re-insert the file + symbols under the new path.
                    let removed2 = nestweaver_store::GraphStore::delete_symbols_in_file_on(
                        &txn, &r_uid, &to_str,
                    )
                    .with_context(|| "delete_symbols_in_file (rename to)")?;
                    result.symbols_removed += removed2;

                    let added = process_added_or_modified_file_txn(
                        &reader, to, &r_uid, repo_url, &store, &txn,
                    )?;
                    result.symbols_added += added;
                }

                result.files_renamed += 1;
            }
        }
    }

    // nw-008 Phase 2 — re-resolve reverse-dependents and surgically restore the
    // cross-file edges the per-file `DETACH DELETE` removed (same transaction).
    let reresolved = reresolve_affected_dependents(&reader, &txn, &r_uid, &changed_files, &rdeps)?;
    if reresolved > 0 {
        tracing::info!(
            edges = reresolved,
            rdeps = rdeps.len(),
            "restored cross-file edges via transitive re-resolution"
        );
    }

    // 6. Update the stored SHA inside the transaction, then commit.
    // If we crash before commit, the next run replays from the old SHA.
    nestweaver_store::GraphStore::update_repo_sha_on(&txn, &r_uid, &new_sha)
        .with_context(|| "update_repo_sha")?;

    store
        .commit_transaction(&txn)
        .with_context(|| "commit incremental transaction")?;
    drop(txn);

    finalize_committed_index_with_io(
        publication,
        Some(db_path),
        "incremental index",
        epilogue_io,
        true,
    )?;

    Ok(result)
}

/// Incrementally re-index a repository using an arbitrary content reader.
///
/// Server mode keeps blobless bare clones rather than checked-out worktrees, so
/// this entry point uses `git_repo_path` for diff/ancestor checks and `reader`
/// for file contents at `new_sha`.
pub(crate) fn incremental_index_with_reader_and_write_gate<G, F>(
    reader: &dyn crate::content_reader::ContentReader,
    git_repo_path: &Path,
    store: &nestweaver_store::GraphStore,
    instance_id: &str,
    repo_url: &str,
    new_sha: &str,
    acquire_write_guard: F,
) -> Result<IncrementalResult, anyhow::Error>
where
    F: FnOnce() -> Result<G, anyhow::Error>,
{
    let r_uid = nestweaver_schema::repo_uid(instance_id, repo_url);
    let old_sha = store
        .lookup_repo(&r_uid)
        .with_context(|| "lookup_repo failed")?
        .map(|r| r.indexed_sha)
        .filter(|sha| !sha.is_empty())
        .ok_or_else(|| anyhow::anyhow!("incremental index requires an existing indexed repo"))?;

    // A committed SHA with zero symbols (crash between the SHA write and
    // content landing) must not take the up-to-date skip below — report the
    // full-index fallback so the caller rebuilds the repo from scratch.
    if !store
        .repo_has_symbols(&r_uid)
        .with_context(|| "repo_has_symbols failed")?
    {
        tracing::warn!(
            old_sha,
            "index is incomplete (SHA set but no symbols); falling back to full re-index"
        );
        return Ok(IncrementalResult {
            fell_back_to_full: true,
            ..IncrementalResult::default()
        });
    }

    if old_sha == new_sha {
        tracing::debug!(sha = old_sha, "repo is already up to date; skipping");
        return Ok(IncrementalResult::default());
    }

    if !crate::git_diff::is_ancestor(git_repo_path, &old_sha, new_sha) {
        return Ok(IncrementalResult {
            fell_back_to_full: true,
            ..IncrementalResult::default()
        });
    }

    let changes = crate::git_diff::detect_changes(git_repo_path, &old_sha, new_sha)
        .with_context(|| "detect_changes")?;

    tracing::info!(
        count = changes.len(),
        old_sha,
        new_sha,
        "processing server incremental changes"
    );

    // nw-008 Phase 0 — compute transitive reverse-dependents from the LIVE
    // graph BEFORE any mutation. The per-file `DETACH DELETE` below destroys the
    // edges we walk here, so this ordering is correctness-critical.
    let (changed_files, removed_files) = partition_changed_removed(&changes);
    let rdeps = collect_reverse_dep_files(store, &r_uid, &changed_files, &removed_files);

    let _write_guard = acquire_write_guard()?;
    let publication = establish_index_publication_marker_with_io(
        store,
        store.db_path(),
        "server incremental index",
        &FileSystemIndexEpilogueIo,
    )?;
    let txn = store
        .begin_transaction()
        .with_context(|| "begin incremental transaction")?;
    let mut result = IncrementalResult::default();

    for change in &changes {
        match change {
            crate::git_diff::FileChange::Added(rel_path) => {
                if path_in_skip_dir(rel_path) || !is_parseable(rel_path) {
                    result.files_skipped += 1;
                    continue;
                }
                let added = process_added_or_modified_file_txn(
                    reader, rel_path, &r_uid, repo_url, store, &txn,
                )?;
                result.symbols_added += added;
                result.files_added += 1;
            }
            crate::git_diff::FileChange::Modified(rel_path) => {
                if path_in_skip_dir(rel_path) || !is_parseable(rel_path) {
                    result.files_skipped += 1;
                    continue;
                }
                let rel_str = rel_path.to_string_lossy();
                let removed =
                    nestweaver_store::GraphStore::delete_symbols_in_file_on(&txn, &r_uid, &rel_str)
                        .with_context(|| format!("delete_symbols_in_file {}", rel_str))?;
                result.symbols_removed += removed;

                let added = process_added_or_modified_file_txn(
                    reader, rel_path, &r_uid, repo_url, store, &txn,
                )?;
                result.symbols_added += added;
                result.files_modified += 1;
            }
            crate::git_diff::FileChange::Deleted(rel_path) => {
                let rel_str = rel_path.to_string_lossy();
                let removed =
                    nestweaver_store::GraphStore::delete_symbols_in_file_on(&txn, &r_uid, &rel_str)
                        .with_context(|| format!("delete_symbols_in_file {}", rel_str))?;
                result.symbols_removed += removed;

                let f_uid = nestweaver_schema::file_uid(&r_uid, &rel_str);
                nestweaver_store::GraphStore::delete_file_node_on(&txn, &f_uid)
                    .with_context(|| format!("delete_file_node {}", rel_str))?;
                result.files_deleted += 1;
            }
            crate::git_diff::FileChange::Renamed { from, to } => {
                let from_str = from.to_string_lossy();
                let to_str = to.to_string_lossy();

                if is_parseable(to) && !path_in_skip_dir(to) {
                    nestweaver_store::GraphStore::update_symbol_file_paths_on(
                        &txn, &r_uid, &from_str, &to_str,
                    )
                    .with_context(|| {
                        format!("update_symbol_file_paths {} -> {}", from_str, to_str)
                    })?;
                } else {
                    let removed = nestweaver_store::GraphStore::delete_symbols_in_file_on(
                        &txn, &r_uid, &from_str,
                    )
                    .with_context(|| format!("delete_symbols_in_file {}", from_str))?;
                    result.symbols_removed += removed;
                }

                let old_f_uid = nestweaver_schema::file_uid(&r_uid, &from_str);
                nestweaver_store::GraphStore::delete_file_node_on(&txn, &old_f_uid)
                    .with_context(|| format!("delete_file_node (rename from) {}", from_str))?;

                if is_parseable(to) && !path_in_skip_dir(to) {
                    let removed2 = nestweaver_store::GraphStore::delete_symbols_in_file_on(
                        &txn, &r_uid, &to_str,
                    )
                    .with_context(|| "delete_symbols_in_file (rename to)")?;
                    result.symbols_removed += removed2;

                    let added = process_added_or_modified_file_txn(
                        reader, to, &r_uid, repo_url, store, &txn,
                    )?;
                    result.symbols_added += added;
                }

                result.files_renamed += 1;
            }
        }
    }

    // nw-008 Phase 2 — re-resolve the reverse-dependents computed in Phase 0
    // and surgically restore the cross-file edges that the per-file
    // `DETACH DELETE` removed (same transaction).
    let reresolved = reresolve_affected_dependents(reader, &txn, &r_uid, &changed_files, &rdeps)?;
    if reresolved > 0 {
        tracing::info!(
            edges = reresolved,
            rdeps = rdeps.len(),
            "restored cross-file edges via transitive re-resolution"
        );
    }

    nestweaver_store::GraphStore::update_repo_sha_on(&txn, &r_uid, new_sha)
        .with_context(|| "update_repo_sha")?;
    store
        .commit_transaction(&txn)
        .with_context(|| "commit incremental transaction")?;
    drop(txn);

    finalize_committed_index_with_io(
        publication,
        store.db_path(),
        "server incremental index",
        &FileSystemIndexEpilogueIo,
        true,
    )?;

    Ok(result)
}

/// Like [`process_added_or_modified_file`] but uses an externally-provided
/// transaction connection for all store writes, ensuring atomicity.
fn process_added_or_modified_file_txn(
    reader: &dyn crate::content_reader::ContentReader,
    rel_path: &std::path::Path,
    r_uid: &str,
    repo_url: &str,
    _store: &nestweaver_store::GraphStore,
    conn: &nestweaver_store::DbConnection<'_>,
) -> Result<usize, anyhow::Error> {
    use nestweaver_parser::{RawReference, RawSymbol};
    use nestweaver_resolver::{discover_workspace_context_with, resolve_references_with_context};
    use nestweaver_schema::{File, Symbol, canonical_symbol_id, file_uid, symbol_uid};

    let abs_path = reader.root().join(rel_path);
    let rel_str = rel_path.to_string_lossy().into_owned();

    let source = match reader.read_file(rel_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(path = %abs_path.display(), "read error: {e}; skipping");
            return Ok(0);
        }
    };

    let parsed = match nestweaver_parser::parse_source(&abs_path, &source) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(path = %abs_path.display(), "parse error: {e}; skipping");
            return Ok(0);
        }
    };

    let content_hash = content_hash_hex(&source);
    let f_uid = file_uid(r_uid, &rel_str);

    // Insert the File node via the transaction connection.
    let file = File {
        uid: f_uid.clone(),
        path: rel_str.clone(),
        repo_uid: r_uid.to_string(),
        content_hash,
    };
    nestweaver_store::GraphStore::insert_file_on(conn, &file)
        .with_context(|| format!("insert_file {}", rel_str))?;
    nestweaver_store::GraphStore::insert_repo_file_edge_on(conn, r_uid, &f_uid)
        .with_context(|| format!("insert_repo_file_edge {}", rel_str))?;

    let mut symbols: Vec<nestweaver_schema::Symbol> = Vec::new();
    let mut file_sym_pairs: Vec<(String, String)> = Vec::new();

    for raw_sym in &parsed.symbols {
        let s_uid = symbol_uid(r_uid, &rel_str, &raw_sym.name, raw_sym.start_line);
        let scope_str = raw_sym.scope_chain.as_deref().unwrap_or("");
        let canonical = canonical_symbol_id(repo_url, &rel_str, &raw_sym.name, scope_str);
        let sym = Symbol {
            uid: s_uid.clone(),
            name: raw_sym.name.clone(),
            kind: raw_sym.kind,
            repo_uid: r_uid.to_string(),
            file_path: rel_str.clone(),
            start_line: raw_sym.start_line,
            end_line: raw_sym.end_line,
            signature: raw_sym.signature.clone(),
            summary: None,
            content_hash: raw_sym.content_hash.clone(),
            embedding: None,
            pagerank_score: None,
            is_entry_point: raw_sym.is_entry_point,
            entry_point_kind: raw_sym.entry_point_kind,
            visibility: raw_sym.visibility,
            type_info: raw_sym.type_info.clone(),
            framework_hint: None,
            canonical_id: Some(canonical),
        };
        symbols.push(sym);
        file_sym_pairs.push((f_uid.clone(), s_uid));
    }

    let sym_count = symbols.len();

    nestweaver_store::GraphStore::batch_insert_symbols_on(conn, &symbols)
        .with_context(|| format!("batch_insert_symbols {}", rel_str))?;

    let file_sym_refs: Vec<(&str, &str)> = file_sym_pairs
        .iter()
        .map(|(f, s)| (f.as_str(), s.as_str()))
        .collect();
    nestweaver_store::GraphStore::batch_insert_file_symbol_edges_on(conn, &file_sym_refs)
        .with_context(|| format!("batch_insert_file_symbol_edges {}", rel_str))?;

    // Resolve cross-file edges within this file only (single-file scope).
    let lang = nestweaver_parser::detect_language(&abs_path)
        .unwrap_or(nestweaver_schema::Language::JavaScript);

    let workspace_ctx = if matches!(
        lang,
        nestweaver_schema::Language::JavaScript
            | nestweaver_schema::Language::TypeScript
            | nestweaver_schema::Language::Vue
            | nestweaver_schema::Language::Svelte
            | nestweaver_schema::Language::Astro
    ) {
        discover_workspace_context_with(|p| {
            reader
                .read_file(p)
                .map_err(|e| std::io::Error::other(e.to_string()))
        })
    } else {
        Default::default()
    };

    let file_data: Vec<(String, Vec<RawSymbol>, Vec<RawReference>)> = vec![(
        rel_str.clone(),
        parsed.symbols.clone(),
        parsed.references.clone(),
    )];
    let resolved_edges =
        resolve_references_with_context(&file_data, lang, r_uid, &workspace_ctx, None, None);
    let insertable_edges: Vec<_> = resolved_edges
        .into_iter()
        .filter(|e| !e.target_uid.starts_with("unresolved:"))
        .collect();
    if !insertable_edges.is_empty() {
        nestweaver_store::GraphStore::batch_insert_edges_on(conn, &insertable_edges)
            .with_context(|| format!("batch_insert_edges {}", rel_str))?;
    }

    Ok(sym_count)
}

/// Split a set of file changes into the parseable files that still exist after
/// the change (`changed`: Added / Modified / Renamed.to) and the files that no
/// longer exist (`removed`: Deleted / Renamed.from). Used to seed the
/// transitive re-resolution pass (nw-008).
fn partition_changed_removed(
    changes: &[crate::git_diff::FileChange],
) -> (
    std::collections::HashSet<String>,
    std::collections::HashSet<String>,
) {
    use crate::git_diff::FileChange;
    let mut changed = std::collections::HashSet::new();
    let mut removed = std::collections::HashSet::new();
    for change in changes {
        match change {
            FileChange::Added(p) | FileChange::Modified(p) => {
                if !path_in_skip_dir(p) && is_parseable(p) {
                    changed.insert(p.to_string_lossy().into_owned());
                }
            }
            FileChange::Deleted(p) => {
                removed.insert(p.to_string_lossy().into_owned());
            }
            FileChange::Renamed { from, to } => {
                removed.insert(from.to_string_lossy().into_owned());
                if !path_in_skip_dir(to) && is_parseable(to) {
                    changed.insert(to.to_string_lossy().into_owned());
                }
            }
        }
    }
    (changed, removed)
}

/// Phase 0 of transitive re-resolution (nw-008). Starting from the
/// changed/removed files, walk reverse-dependency edges in the **live** graph
/// to find files whose resolved cross-file edges must be rebuilt after the
/// changed files' symbols are deleted and re-inserted. Re-inserting a changed
/// file shifts its symbols' UIDs and the per-file `DETACH DELETE` destroys all
/// edges incident to the old symbols — including the `A→C` edges that
/// *dependents* own. Those dependents must be re-resolved so the edges come
/// back pointing at the new UIDs.
///
/// `changed` are the files that still exist after the change (Added / Modified
/// / Renamed.to). `removed` are gone (Deleted / Renamed.from) but remain useful
/// BFS seeds: files that imported a now-deleted file are reverse-dependents too.
///
/// Returns the reverse-dependents to re-resolve (`affected \ changed \
/// removed`), bounded by [`crate::resolution_cache::MAX_HOPS`] hops and a
/// `MAX_AFFECTED_FILES` total cap. On exceeding the total cap the pass is
/// skipped (empty set returned) — a periodic full re-index is the backstop for
/// hub/widely-imported files.
///
/// **CRITICAL ORDERING:** this must run BEFORE `begin_transaction`; the
/// per-file `DETACH DELETE` in the mutation loop destroys the very edges this
/// query reads.
///
/// `pub(crate)` so the live code watcher (`watch_code.rs`) can run the same
/// nw-008 Phase 0 — without it a watcher reindex loses every cross-file edge
/// incident to the re-indexed file.
pub(crate) fn collect_reverse_dep_files(
    store: &nestweaver_store::GraphStore,
    r_uid: &str,
    changed: &std::collections::HashSet<String>,
    removed: &std::collections::HashSet<String>,
) -> std::collections::HashSet<String> {
    use std::collections::HashSet;

    /// Upper bound on total files touched by the transitive re-resolution pass.
    /// Beyond this the pass is skipped (full re-index is the backstop) so a
    /// change to a widely-imported hub file does not cascade unbounded.
    const MAX_AFFECTED_FILES: usize = 200;

    let mut seeds: HashSet<String> = changed.clone();
    seeds.extend(removed.iter().cloned());
    if seeds.is_empty() {
        return HashSet::new();
    }

    let mut affected = seeds.clone();
    let mut frontier = seeds;
    for _ in 0..crate::resolution_cache::MAX_HOPS {
        let mut next: HashSet<String> = HashSet::new();
        for file in &frontier {
            match store.files_referencing_file(r_uid, file) {
                Ok(rdeps) => {
                    for dep in rdeps {
                        if !affected.contains(&dep) {
                            next.insert(dep);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(file = %file, "files_referencing_file failed: {e}");
                }
            }
        }
        if next.is_empty() {
            break; // fixed point — no more transitive reverse-dependents
        }
        if affected.len() + next.len() > MAX_AFFECTED_FILES {
            tracing::warn!(
                affected = affected.len(),
                next = next.len(),
                cap = MAX_AFFECTED_FILES,
                "transitive re-resolution affected-file cap exceeded; skipping pass \
                 (full re-index is the backstop)"
            );
            return HashSet::new();
        }
        affected.extend(next.iter().cloned());
        frontier = next;
    }

    // rdeps = affected \ changed \ removed: only the genuine reverse-dependents
    // that still exist and were not themselves directly changed.
    affected
        .into_iter()
        .filter(|f| !changed.contains(f) && !removed.contains(f))
        .collect()
}

fn reresolve_affected_dependents(
    reader: &dyn crate::content_reader::ContentReader,
    conn: &nestweaver_store::DbConnection<'_>,
    r_uid: &str,
    changed: &std::collections::HashSet<String>,
    rdeps: &std::collections::HashSet<String>,
) -> Result<usize, anyhow::Error> {
    if changed.is_empty() {
        return Ok(0);
    }

    let db_symbols = nestweaver_store::GraphStore::lookup_symbols_by_repo_on(conn, r_uid)
        .with_context(|| "lookup_symbols_by_repo_on for forward edge resolution")?;
    let insertable = build_reresolve_edges(reader, r_uid, changed, rdeps, &db_symbols)?;

    // Runs inside the same transaction as the mutation loop.
    let count = insertable.len();
    if count > 0 {
        nestweaver_store::GraphStore::batch_insert_edges_on(conn, &insertable)
            .with_context(|| "batch_insert_edges (transitive re-resolution)")?;
    }
    Ok(count)
}

/// Non-transactional variant of [`reresolve_affected_dependents`] for the
/// live code watcher (`watch_code.rs`), which interleaves its mutations with
/// store-level (non-txn) calls. Same nw-008 Phase 2 semantics: re-insert ONLY
/// the cross-file edges the per-file `DETACH DELETE` destroyed.
pub(crate) fn reresolve_affected_dependents_on_store(
    reader: &dyn crate::content_reader::ContentReader,
    store: &nestweaver_store::GraphStore,
    r_uid: &str,
    changed: &std::collections::HashSet<String>,
    rdeps: &std::collections::HashSet<String>,
) -> Result<usize, anyhow::Error> {
    if changed.is_empty() {
        return Ok(0);
    }

    let db_symbols = store
        .lookup_symbols_by_repo(r_uid)
        .with_context(|| "lookup_symbols_by_repo for forward edge resolution")?;
    let insertable = build_reresolve_edges(reader, r_uid, changed, rdeps, &db_symbols)?;

    let count = insertable.len();
    if count > 0 {
        store
            .batch_insert_edges(&insertable)
            .with_context(|| "batch_insert_edges (transitive re-resolution)")?;
    }
    Ok(count)
}

/// Shared core of nw-008 Phase 2. Re-parse `S = changed ∪ rdeps` from
/// `reader`, resolve cross-file references with the full symbol map across
/// `S`, and return ONLY the edges the per-file `DETACH DELETE` removed: those
/// whose SOURCE or TARGET lives in a changed file, with both endpoints in
/// different files. Intra-file edges and edges between files that were both
/// untouched were never deleted (or were re-created by single-file resolution
/// in the mutation loop), so re-inserting them would duplicate (edge insert
/// is `CREATE`, not `MERGE`) — the `source_file != target_file` and
/// `source ∈ changed OR target ∈ changed` filters keep the insert
/// duplicate-free without a `delete_resolved_edges_for_file` pass.
///
/// `db_symbols` must be the repo's live symbol set (post-mutation), used to
/// give the resolver visibility into unchanged files' symbols as targets.
fn build_reresolve_edges(
    reader: &dyn crate::content_reader::ContentReader,
    r_uid: &str,
    changed: &std::collections::HashSet<String>,
    rdeps: &std::collections::HashSet<String>,
    db_symbols: &[nestweaver_schema::Symbol],
) -> Result<Vec<nestweaver_schema::ResolvedEdge>, anyhow::Error> {
    // S = changed ∪ rdeps — files whose references need re-resolution.
    let mut scope: std::collections::HashSet<String> = changed.clone();
    scope.extend(rdeps.iter().cloned());

    let mut file_data: Vec<(String, Vec<RawSymbol>, Vec<RawReference>)> = Vec::new();
    let mut uid_to_file: HashMap<String, String> = HashMap::new();
    let mut lang_counts: HashMap<Language, usize> = HashMap::new();

    for rel_str in &scope {
        let rel_path = Path::new(rel_str.as_str());
        let abs_path = reader.root().join(rel_path);
        let source = match reader.read_file(rel_path) {
            Ok(s) => s,
            Err(_) => continue, // deleted/unreadable — nothing to re-resolve from
        };
        let parsed = match parse_source(&abs_path, &source) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if let Some(lang) = detect_language(&abs_path) {
            *lang_counts.entry(lang).or_insert(0) += 1;
        }
        for raw_sym in &parsed.symbols {
            let s_uid = symbol_uid(r_uid, rel_str, &raw_sym.name, raw_sym.start_line);
            uid_to_file.insert(s_uid, rel_str.clone());
        }
        file_data.push((rel_str.clone(), parsed.symbols, parsed.references));
    }

    if file_data.is_empty() {
        return Ok(Vec::new());
    }

    // ── Include unchanged files' symbols as resolution targets ──────────
    //
    // The per-file `DETACH DELETE` destroys outgoing edges from changed
    // files to unchanged files.  The single-file resolution pass in the
    // mutation loop can only recreate intra-file edges because it has no
    // visibility into other files' symbols.  To restore those
    // changed→unchanged edges we must include the unchanged files' symbols
    // in the resolver's symbol map so the resolver can find them as
    // targets.  We add them to `file_data` with empty references (we
    // don't need to resolve their references — those edges were never
    // deleted) and populate `uid_to_file` so the edge filter can look up
    // their file paths.  `db_symbols` is the repo's live (post-mutation)
    // symbol set, fetched by the caller.
    let mut unchanged_by_file: HashMap<String, Vec<RawSymbol>> = HashMap::new();
    for sym in db_symbols {
        if scope.contains(&sym.file_path) {
            // Already in file_data from re-parsing above; just ensure
            // uid_to_file has the DB uid (the re-parsed uid should match,
            // but belt-and-suspenders).
            uid_to_file
                .entry(sym.uid.clone())
                .or_insert_with(|| sym.file_path.clone());
            continue;
        }
        uid_to_file.insert(sym.uid.clone(), sym.file_path.clone());
        unchanged_by_file
            .entry(sym.file_path.clone())
            .or_default()
            .push(RawSymbol {
                name: sym.name.clone(),
                kind: sym.kind,
                start_line: sym.start_line,
                end_line: sym.end_line,
                signature: sym.signature.clone(),
                content_hash: sym.content_hash.clone(),
                is_entry_point: sym.is_entry_point,
                entry_point_kind: sym.entry_point_kind,
                visibility: sym.visibility,
                type_info: None,
                parent_name: None,
                scope_chain: None,
            });
    }
    for (file_path, symbols) in unchanged_by_file {
        // Empty references: we only need these files as resolution
        // targets, not as resolution sources.
        file_data.push((file_path, symbols, Vec::new()));
    }

    // Resolve with the most-common language across S (matches the full-index
    // batch heuristic). Mixed-language scopes are rare; cross-language edges are
    // not resolved anyway.
    let language = lang_counts
        .into_iter()
        .max_by_key(|(_, c)| *c)
        .map(|(l, _)| l)
        .unwrap_or(Language::JavaScript);

    let workspace_ctx = if matches!(
        language,
        Language::JavaScript
            | Language::TypeScript
            | Language::Vue
            | Language::Svelte
            | Language::Astro
    ) {
        discover_workspace_context_with(|p| {
            reader
                .read_file(p)
                .map_err(|e| std::io::Error::other(e.to_string()))
        })
    } else {
        Default::default()
    };

    let resolved_edges = resolve_references_with_context(
        &file_data,
        language,
        r_uid,
        &workspace_ctx,
        None,
        Some(&scope),
    );

    // Keep only the cross-file edges that the `DETACH DELETE` destroyed:
    //
    //  • target in changed  — the target symbol was deleted and re-inserted
    //    with new UIDs, so any edge pointing at it (from rdeps or other
    //    changed files) must be recreated.
    //  • source in changed  — the source symbol was deleted and re-inserted,
    //    so its outgoing edges (to unchanged files, rdeps, or other changed
    //    files) must be recreated.
    //
    // Edges where NEITHER endpoint is in a changed file (e.g. rdep→rdep or
    // rdep→unchanged) were never deleted and must NOT be re-inserted
    // (edge insert is CREATE, not MERGE — duplicates would result).
    let insertable: Vec<_> = resolved_edges
        .into_iter()
        .filter(|e| {
            if e.target_uid.starts_with("unresolved:") {
                return false;
            }
            let (Some(tf), Some(sf)) = (
                uid_to_file.get(&e.target_uid),
                uid_to_file.get(&e.source_uid),
            ) else {
                return false;
            };
            sf != tf && (changed.contains(tf.as_str()) || changed.contains(sf.as_str()))
        })
        .collect();

    Ok(insertable)
}

/// Delete all File nodes (and their symbols) that belong to a repo,
/// then delete the Repo node itself.  Used before a forced full re-index.
/// Re-identify detection, shared by the indexer and the code watcher so the
/// two sites cannot drift: when a working tree at `root_path` is now being
/// indexed under `new_uid` but a Repo row still exists under its legacy
/// `file://<root_path>` identity, returns that old uid (the caller prunes it
/// via [`delete_repo_all_data`] — strictly uid-keyed, never by disk path).
/// Returns `None` when the identities coincide or no legacy row exists.
pub(crate) fn reidentified_legacy_uid(
    store: &nestweaver_store::GraphStore,
    instance_id: &str,
    root_path: &str,
    new_uid: &str,
) -> Result<Option<String>, anyhow::Error> {
    let old_uid = repo_uid(instance_id, &format!("file://{root_path}"));
    if old_uid != new_uid
        && store
            .lookup_repo(&old_uid)
            .context("lookup_repo (re-identify detection)")?
            .is_some()
    {
        Ok(Some(old_uid))
    } else {
        Ok(None)
    }
}

/// Delete every graph row belonging to `r_uid`.
///
/// Uses two bulk DETACH DELETE queries (one for Symbol, one for File)
/// instead of the previous per-file loop that issued O(2N) queries.
pub(crate) fn delete_repo_all_data(
    store: &nestweaver_store::GraphStore,
    r_uid: &str,
) -> Result<(usize, usize), anyhow::Error> {
    let (file_count, sym_count) = store
        .bulk_delete_repo_files_and_symbols(r_uid)
        .with_context(|| "bulk_delete_repo_files_and_symbols")?;

    tracing::debug!(
        "delete_repo_all_data: removed {sym_count} symbols and {file_count} files for repo {r_uid}"
    );

    // Clear repo-scoped derived nodes (Service, Contract) so a forced full
    // re-index does not collide on their deterministic primary keys.
    store
        .clear_repo_derived_nodes(r_uid)
        .with_context(|| "clear_repo_derived_nodes")?;

    store
        .delete_repo_node(r_uid)
        .with_context(|| "delete_repo_node")?;

    Ok((file_count, sym_count))
}

/// Full index fallback — uses the already-open store to avoid double-
/// opening the LadybugDB file (which corrupts it).
struct FullIndexFallback<'a> {
    repo_path: &'a Path,
    db_path: &'a Path,
    instance_id: &'a str,
    repo_url: &'a str,
    new_sha: &'a str,
    name: Option<&'a str>,
    force: bool,
    epilogue_io: &'a dyn IndexEpilogueIo,
}

fn full_index_fallback(
    store: &GraphStore,
    request: FullIndexFallback<'_>,
) -> Result<IncrementalResult, anyhow::Error> {
    let FullIndexFallback {
        repo_path,
        db_path,
        instance_id,
        repo_url,
        new_sha,
        name,
        force,
        epilogue_io,
    } = request;
    // Load filemeta sidecar for tiered change detection even in fallback.
    // Only this repo's slice feeds change detection — another repo's entry
    // for the same rel path must never match (nw-022).
    crate::migrate_sidecar(db_path, "filemeta.json", ".filemeta.json");
    let filemeta_path = crate::sidecar_path(db_path, ".filemeta.json");
    let r_uid = nestweaver_schema::repo_uid(instance_id, repo_url);
    let filemeta_cache = load_filemeta_sidecar(&filemeta_path)
        .repos
        .get(&r_uid)
        .cloned()
        .unwrap_or_default();
    let mut new_filemeta = FileMetaCache::new();

    let parsed_cache_path = crate::sidecar_path(db_path, ".parsed_cache.bin");
    let mut parsed_cache = crate::parsed_cache::ParsedCache::load(&parsed_cache_path);

    let resolution_deps_path = crate::sidecar_path(db_path, ".resolution_deps.bin");
    let mut resolution_deps = crate::resolution_cache::ResolutionDeps::load(&resolution_deps_path);

    let reader = crate::content_reader::FilesystemReader::new(repo_path);
    let local_root = repo_path.display().to_string();
    // nw-022: capture a re-identified legacy file:// uid so its filemeta
    // slice is dropped below, mirroring index_directory_with_store_inner.
    let mut reidentified_old_uid: Option<String> = None;
    let result = index_into_store_with_write_gate(
        &reader,
        store,
        instance_id,
        repo_url,
        new_sha,
        (!force).then_some(&filemeta_cache),
        Some(&mut new_filemeta),
        Some(&mut parsed_cache),
        Some(&mut resolution_deps),
        Some(&mut reidentified_old_uid),
        name,
        Some(&local_root),
        true,
        epilogue_io,
        None,
        || Ok::<(), anyhow::Error>(()),
    )?;

    // Merge this repo's fresh entries into the shared sidecar (preserving
    // other repos' slices) and evict parse/resolution cache entries using the
    // cross-repo live unions.
    //
    // Deliberately warn-only, unlike the primary path's `?`: this preserves
    // the legacy incremental-entry behavior where sidecar persistence is
    // best-effort, and a stale slice here self-heals — the next full pass
    // falls through Tier 1/2 to Tier 3's content-hash comparison and
    // re-indexes anything the stale snapshot would have mis-skipped.
    let drop_uids: Vec<String> = reidentified_old_uid.into_iter().collect();
    match merge_save_filemeta(&filemeta_path, &r_uid, new_filemeta, &drop_uids) {
        Ok(unions) => {
            parsed_cache.retain_hashes(&unions.live_hashes);
            resolution_deps.retain_files_for_repo(&r_uid, &unions.repo_live_files);
        }
        Err(e) => tracing::warn!("failed to save filemeta sidecar: {e}"),
    }
    if let Err(e) = parsed_cache.save(&parsed_cache_path) {
        tracing::warn!("failed to save parsed cache: {e}");
    }
    if let Err(e) = resolution_deps.save(&resolution_deps_path) {
        tracing::warn!("failed to save resolution deps: {e}");
    }

    // Update the manifest cache sidecar (same as index_directory does).
    let manifest = crate::manifest::parse_manifest(&reader);
    let mut cache = crate::manifest::load_manifest_cache_for_db(db_path).unwrap_or_default();
    cache.insert(r_uid, manifest);
    if let Err(e) = crate::manifest::save_manifest_cache_for_db(&cache, db_path) {
        tracing::warn!("failed to save manifest cache: {e}");
    }

    // nw-029: warm PageRank at index time so first queries (UI overview, impact,
    // repo-map, hubs) never pay the lazy compute. This is the first-index-of-a-
    // new-repo path (`nestweaver index` with no prior index / non-git dir), the
    // most common case — without this it stayed sidecar-less. Mirrors the full
    // and incremental paths. Release-build cost is seconds even at ~50k symbols.
    // A failure is returned so the fallback cannot report success without a
    // durable fresh cache. nw-055 (P1b): delete-only re-indexes also refresh the
    // surviving nodes' ranks even though files_count is zero.
    Ok(IncrementalResult {
        fell_back_to_full: true,
        symbols_added: result.symbols_count,
        files_deleted: result.files_deleted,
        symbols_removed: result.symbols_deleted,
        ..Default::default()
    })
}

fn content_hash_hex(s: &str) -> String {
    crate::hash::blake3_hex(s)
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn insert_publication_graph(store: &GraphStore, publisher: &str) {
        let repo_uid = format!("repo:publisher-{publisher}");
        let file_uid = format!("file:publisher-{publisher}");
        let source_uid = format!("sym:publisher-{publisher}:source");
        let target_uid = format!("sym:publisher-{publisher}:target");
        let file_path = format!("src/{publisher}.rs");
        store
            .insert_repo(&nestweaver_schema::Repo {
                uid: repo_uid.clone(),
                url: format!("https://example.test/publisher-{publisher}"),
                indexed_sha: publisher.into(),
                staleness_commits_behind: 0,
                instance_id: "test".into(),
                name: None,
                root_path: None,
            })
            .unwrap();
        store
            .insert_file(&nestweaver_schema::File {
                uid: file_uid.clone(),
                path: file_path.clone(),
                repo_uid: repo_uid.clone(),
                content_hash: format!("hash-{publisher}"),
            })
            .unwrap();
        store.insert_repo_file_edge(&repo_uid, &file_uid).unwrap();
        for (uid, name) in [
            (&source_uid, format!("publisher_{publisher}_source")),
            (&target_uid, format!("publisher_{publisher}_target")),
        ] {
            store
                .insert_symbol(&nestweaver_schema::Symbol {
                    uid: uid.clone(),
                    name,
                    kind: nestweaver_schema::SymbolKind::Function,
                    repo_uid: repo_uid.clone(),
                    file_path: file_path.clone(),
                    start_line: 1,
                    end_line: 2,
                    signature: format!("fn {publisher}()"),
                    summary: None,
                    content_hash: format!("hash-{uid}"),
                    embedding: None,
                    pagerank_score: None,
                    is_entry_point: false,
                    entry_point_kind: None,
                    visibility: nestweaver_schema::Visibility::Inferred,
                    type_info: None,
                    framework_hint: None,
                    canonical_id: None,
                })
                .unwrap();
            store.insert_file_symbol_edge(&file_uid, uid).unwrap();
        }
        store
            .insert_edge(&nestweaver_schema::ResolvedEdge {
                source_uid,
                target_uid,
                edge_type: nestweaver_schema::EdgeType::Calls,
                confidence: 1.0,
                link_type: None,
                evidence: Vec::new(),
            })
            .unwrap();
    }

    #[test]
    fn overlapping_publications_serialize_before_the_second_mutation() {
        use std::sync::{Arc, mpsc};
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let generation_path = crate::sidecar_path(&db_path, ".generation");
        let pagerank_path = crate::sidecar_path(&db_path, ".pagerank.json");
        let store = Arc::new(GraphStore::open_or_create(&db_path).unwrap());

        let publication_a = establish_index_publication_marker_with_io(
            &store,
            Some(&db_path),
            "publisher A",
            &FileSystemIndexEpilogueIo,
        )
        .unwrap();

        let (established_tx, established_rx) = mpsc::channel();
        let (continue_tx, continue_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let b_store = Arc::clone(&store);
        let b_db_path = db_path.clone();
        let publisher_b = std::thread::spawn(move || {
            let publication_b = establish_index_publication_marker_with_io(
                &b_store,
                Some(&b_db_path),
                "publisher B",
                &FileSystemIndexEpilogueIo,
            );
            established_tx
                .send(
                    publication_b
                        .as_ref()
                        .map(|_| ())
                        .map_err(ToString::to_string),
                )
                .unwrap();
            continue_rx.recv().unwrap();
            let publication_b = publication_b.unwrap();
            insert_publication_graph(&b_store, "b");
            let result = finalize_committed_index_with_io(
                publication_b,
                Some(&b_db_path),
                "publisher B",
                &FileSystemIndexEpilogueIo,
                true,
            );
            done_tx.send(result).unwrap();
        });

        assert!(
            store.wait_for_index_publication_waiters(1, Duration::from_secs(2)),
            "publisher B must register as waiting on A's publication lease"
        );
        assert!(
            matches!(established_rx.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "publisher B must not establish while A owns the lease"
        );
        assert!(
            store.lookup_repo("repo:publisher-b").unwrap().is_none(),
            "publisher B must not mutate before it exclusively establishes"
        );
        insert_publication_graph(&store, "a");
        finalize_committed_index_with_io(
            publication_a,
            Some(&db_path),
            "publisher A",
            &FileSystemIndexEpilogueIo,
            true,
        )
        .unwrap();
        let generation_after_a = store.graph_generation();

        let b_established = established_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("publisher B must establish after A finalizes");
        b_established.unwrap();
        continue_tx.send(()).unwrap();
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("publisher B must finish")
            .unwrap();
        publisher_b.join().unwrap();

        assert!(store.graph_generation() > generation_after_a);
        assert_eq!(
            fs::read_to_string(&generation_path)
                .unwrap()
                .trim()
                .parse::<u64>()
                .unwrap(),
            store.graph_generation()
        );
        assert!(pagerank_path.exists());
        let expected_scores = store.pagerank_scores();
        assert_eq!(expected_scores.len(), 4);
        for uid in [
            "sym:publisher-a:source",
            "sym:publisher-a:target",
            "sym:publisher-b:source",
            "sym:publisher-b:target",
        ] {
            assert!(
                expected_scores.contains_key(uid),
                "missing PageRank for {uid}"
            );
        }
        drop(store);

        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        assert!(reopened.lookup_repo("repo:publisher-a").unwrap().is_some());
        assert!(reopened.lookup_repo("repo:publisher-b").unwrap().is_some());
        assert_eq!(
            reopened
                .list_files_by_repo("repo:publisher-a")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            reopened
                .list_files_by_repo("repo:publisher-b")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            reopened
                .callees_of("sym:publisher-a:source")
                .unwrap()
                .into_iter()
                .map(|symbol| symbol.uid)
                .collect::<Vec<_>>(),
            vec!["sym:publisher-a:target".to_string()]
        );
        assert_eq!(
            reopened
                .callees_of("sym:publisher-b:source")
                .unwrap()
                .into_iter()
                .map(|symbol| symbol.uid)
                .collect::<Vec<_>>(),
            vec!["sym:publisher-b:target".to_string()]
        );
        reopened.load_pagerank_cache(&pagerank_path).unwrap();
        assert_eq!(reopened.pagerank_scores(), expected_scores);
        assert_eq!(reopened.graph_generation(), generation_after_a + 2);
    }

    #[test]
    fn dropped_publication_owner_leaves_dirty_state_for_next_owner_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let marker_path = crate::sidecar_path(&db_path, ".index-dirty");
        let generation_path = crate::sidecar_path(&db_path, ".generation");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        store.bump_graph_generation();
        store.save_graph_generation(&generation_path).unwrap();
        let canonical_generation = store.graph_generation();

        let abandoned = establish_index_publication_marker_with_io(
            &store,
            Some(&db_path),
            "abandoned publisher",
            &FileSystemIndexEpilogueIo,
        )
        .unwrap();
        let dirty_generation = store.graph_generation();
        assert_eq!(dirty_generation, canonical_generation + 1);
        drop(abandoned);

        assert!(marker_path.exists());
        assert_eq!(store.graph_generation(), dirty_generation);
        let recovery = establish_index_publication_marker_with_io(
            &store,
            Some(&db_path),
            "recovery publisher",
            &FileSystemIndexEpilogueIo,
        )
        .unwrap();
        assert_eq!(
            store.graph_generation(),
            dirty_generation,
            "recovery must retain the abandoned N+1 reservation"
        );
        store
            .insert_repo(&nestweaver_schema::Repo {
                uid: "repo:recovery-owner".into(),
                url: "https://example.test/recovery-owner".into(),
                indexed_sha: "recovered".into(),
                staleness_commits_behind: 0,
                instance_id: "test".into(),
                name: None,
                root_path: None,
            })
            .unwrap();
        finalize_committed_index_with_io(
            recovery,
            Some(&db_path),
            "recovery publisher",
            &FileSystemIndexEpilogueIo,
            true,
        )
        .unwrap();

        assert!(!marker_path.exists());
        assert_eq!(store.graph_generation(), canonical_generation + 2);
        drop(store);
        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        assert_eq!(reopened.graph_generation(), canonical_generation + 2);
        assert!(
            reopened
                .lookup_repo("repo:recovery-owner")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn recovered_owner_early_noop_cannot_cancel_prior_unknown_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let marker_path = crate::sidecar_path(&db_path, ".index-dirty");
        let generation_path = crate::sidecar_path(&db_path, ".generation");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        store.bump_graph_generation();
        store.save_graph_generation(&generation_path).unwrap();

        let abandoned = establish_index_publication_marker_with_io(
            &store,
            Some(&db_path),
            "publisher with unknown committed work",
            &FileSystemIndexEpilogueIo,
        )
        .unwrap();
        insert_publication_graph(&store, "abandoned");
        drop(abandoned);

        let recovered = establish_index_publication_marker_with_io(
            &store,
            Some(&db_path),
            "early no-op recovery",
            &FileSystemIndexEpilogueIo,
        )
        .unwrap();
        assert!(recovered.is_recovered());
        assert!(
            store
                .lookup_repo("repo:early-lookup-miss")
                .unwrap()
                .is_none(),
            "exercise an early lookup/no-op before this owner mutates"
        );
        assert!(
            recovered.cancel_generation().is_err(),
            "a recovered owner must not cancel the prior owner's unknown mutation"
        );
        drop(recovered);

        assert!(marker_path.exists());
        drop(store);
        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        assert!(reopened.is_index_publication_dirty());
        assert!(
            reopened
                .lookup_repo("repo:publisher-abandoned")
                .unwrap()
                .is_some()
        );
        assert!(reopened.pagerank_scores().is_empty());
    }

    #[test]
    fn recovered_owner_can_heal_prior_unknown_mutation_without_own_write() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let marker_path = crate::sidecar_path(&db_path, ".index-dirty");
        let generation_path = crate::sidecar_path(&db_path, ".generation");
        let pagerank_path = crate::sidecar_path(&db_path, ".pagerank.json");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        store.bump_graph_generation();
        store.save_graph_generation(&generation_path).unwrap();
        let canonical_generation = store.graph_generation();

        let abandoned = establish_index_publication_marker_with_io(
            &store,
            Some(&db_path),
            "publisher with unknown committed work",
            &FileSystemIndexEpilogueIo,
        )
        .unwrap();
        insert_publication_graph(&store, "healed");
        drop(abandoned);

        let recovered = establish_index_publication_marker_with_io(
            &store,
            Some(&db_path),
            "successful no-op recovery",
            &FileSystemIndexEpilogueIo,
        )
        .unwrap();
        assert!(recovered.is_recovered());
        finalize_committed_index_for_scope_with_io(
            recovered,
            Some(&db_path),
            "successful no-op recovery",
            &FileSystemIndexEpilogueIo,
            Some(&nestweaver_store::GraphScope::unified()),
        )
        .unwrap();

        assert!(!marker_path.exists());
        assert_eq!(store.graph_generation(), canonical_generation + 2);
        let expected_scores = store.pagerank_scores();
        assert_eq!(expected_scores.len(), 2);
        drop(store);

        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        assert!(!reopened.is_index_publication_dirty());
        assert!(
            reopened
                .lookup_symbol("sym:publisher-healed:source")
                .is_ok()
        );
        reopened.load_pagerank_cache(&pagerank_path).unwrap();
        assert_eq!(reopened.pagerank_scores(), expected_scores);
    }

    #[test]
    fn index_js_directory_extracts_symbols() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("main.js"),
            r#"
function greet(name) { return hello(name); }
function hello(name) { return "Hello " + name; }
        "#,
        )
        .unwrap();

        let (result, _store) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();
        assert!(
            result.symbols_count >= 2,
            "expected >= 2 symbols, got {}",
            result.symbols_count
        );
        assert_eq!(result.files_count, 1);
        assert!(result.skipped_files.is_empty());
    }

    /// A plain local index persists the working-tree location as
    /// `root_path` on the Repo node, independent of the identity url.
    #[test]
    fn index_persists_root_path_on_repo_node() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("main.js"), "function a() { return 1; }\n").unwrap();

        let (_result, store) =
            index_directory_in_memory(&src, "test", "https://example.com/acme/demo", "abc123")
                .unwrap();

        let repos = store.list_repos(None).unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].url, "https://example.com/acme/demo");
        assert_eq!(
            repos[0].root_path.as_deref(),
            Some(src.display().to_string().as_str()),
            "root_path must record the on-disk working tree"
        );
    }

    /// Re-identify prune: a repo first indexed under its `file://` identity
    /// and later re-indexed under its git-origin identity must end up as a
    /// single Repo node — the old file:// node (same working tree, same
    /// instance) is pruned strictly by uid.
    #[test]
    fn reindex_under_origin_identity_prunes_old_file_url_node() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("main.js"), "function a() { return 1; }\n").unwrap();

        let store = GraphStore::in_memory().unwrap();
        let reader = crate::content_reader::FilesystemReader::new(&src);
        let local_root = src.display().to_string();
        let file_url = format!("file://{local_root}");

        // 1. First index under the file:// identity (pre-origin behavior),
        //    capturing the filemeta sidecar exactly like the CLI does.
        let mut first_filemeta = FileMetaCache::new();
        index_into_store(
            &reader,
            &store,
            "test",
            &file_url,
            "sha-1",
            None,
            Some(&mut first_filemeta),
            None,
            None,
            None,
            Some(&local_root),
        )
        .unwrap();
        let old_uid = repo_uid("test", &file_url);
        assert!(store.lookup_repo(&old_uid).unwrap().is_some());
        assert!(
            !first_filemeta.is_empty(),
            "first pass must record filemeta entries"
        );

        // 2. Re-index the same working tree under its origin identity,
        //    passing the sidecar from the first pass — the regression path:
        //    files are unchanged on disk, so a trusted cache would skip all
        //    writes under the new uid while the prune deletes the old copy.
        let origin_url = "https://example.com/acme/demo.git";
        index_into_store(
            &reader,
            &store,
            "test",
            origin_url,
            "sha-1",
            Some(&first_filemeta),
            None,
            None,
            None,
            None,
            Some(&local_root),
        )
        .unwrap();

        let new_uid = repo_uid("test", origin_url);
        assert_ne!(new_uid, old_uid);
        assert!(
            store.lookup_repo(&old_uid).unwrap().is_none(),
            "old file:// node must be pruned by uid"
        );
        let repos = store.list_repos(None).unwrap();
        assert_eq!(repos.len(), 1, "exactly one Repo node must remain");
        assert_eq!(repos[0].uid, new_uid);
        assert_eq!(repos[0].url, origin_url);
        assert_eq!(repos[0].root_path.as_deref(), Some(local_root.as_str()));

        // The re-identified index is a cold index for the new uid: files and
        // symbols MUST exist under it (the filemeta cache is bypassed).
        let files = store.list_files_by_repo(&new_uid).unwrap();
        assert!(
            !files.is_empty(),
            "files must be re-inserted under the new uid"
        );
        let symbols = store.symbol_names_by_repo(&new_uid).unwrap();
        assert!(
            symbols.iter().any(|n| n == "a"),
            "symbol `a` must exist under the new uid, got {symbols:?}"
        );
    }

    /// An unrelated repo (different working tree) must never be caught by
    /// the re-identify prune, and a pre-existing row without `root_path`
    /// gets it backfilled on the next index.
    #[test]
    fn reidentify_prune_leaves_unrelated_repos_and_backfills_root_path() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("main.js"), "function a() { return 1; }\n").unwrap();

        let store = GraphStore::in_memory().unwrap();

        // Unrelated remote repo with no working tree.
        store
            .insert_repo(&Repo {
                uid: repo_uid("test", "https://example.com/other/unrelated"),
                url: "https://example.com/other/unrelated".to_string(),
                indexed_sha: "zzz".to_string(),
                staleness_commits_behind: 0,
                instance_id: "test".to_string(),
                name: None,
                root_path: None,
            })
            .unwrap();

        // Pre-migration row for THIS repo: origin identity, no root_path.
        let origin_url = "https://example.com/acme/demo";
        let r_uid = repo_uid("test", origin_url);
        store
            .insert_repo(&Repo {
                uid: r_uid.clone(),
                url: origin_url.to_string(),
                indexed_sha: String::new(),
                staleness_commits_behind: 0,
                instance_id: "test".to_string(),
                name: None,
                root_path: None,
            })
            .unwrap();

        let reader = crate::content_reader::FilesystemReader::new(&src);
        let local_root = src.display().to_string();
        index_into_store(
            &reader,
            &store,
            "test",
            origin_url,
            "sha-1",
            None,
            None,
            None,
            None,
            None,
            Some(&local_root),
        )
        .unwrap();

        let repos = store.list_repos(None).unwrap();
        assert_eq!(repos.len(), 2, "unrelated repo must survive the prune");
        let this = store.lookup_repo(&r_uid).unwrap().unwrap();
        assert_eq!(
            this.root_path.as_deref(),
            Some(local_root.as_str()),
            "root_path must be backfilled on an existing row"
        );
        assert!(
            store
                .lookup_repo(&repo_uid("test", "https://example.com/other/unrelated"))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn index_creates_call_edges_for_same_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("main.js"),
            r#"
function greet(name) { return hello(name); }
function hello(name) { return "Hello " + name; }
        "#,
        )
        .unwrap();

        let (result, _store) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();
        assert!(result.edges_count > 0, "expected CALLS edges, got 0");
    }

    #[test]
    fn index_infers_cross_repo_call_edges_for_exported_symbol_names() {
        let dir = tempfile::tempdir().unwrap();
        let api = dir.path().join("api");
        let web = dir.path().join("web");
        fs::create_dir_all(&api).unwrap();
        fs::create_dir_all(&web).unwrap();
        fs::write(
            api.join("payment.js"),
            "export function processPayment(amount, currency) {\n  return { amount, currency };\n}\n",
        )
        .unwrap();
        fs::write(
            web.join("checkout.js"),
            "export function WebCheckoutSymbol(cart) {\n  return processPayment(cart.total, 'USD');\n}\n",
        )
        .unwrap();

        let store = GraphStore::in_memory().unwrap();
        let api_reader = crate::content_reader::FilesystemReader::new(&api);
        let web_reader = crate::content_reader::FilesystemReader::new(&web);
        index_into_store(
            &api_reader,
            &store,
            "test",
            "https://example.com/api",
            "api-sha",
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        index_into_store(
            &web_reader,
            &store,
            "test",
            "https://example.com/web",
            "web-sha",
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();

        let process_payment = store
            .lookup_symbols_by_name("processPayment")
            .unwrap()
            .into_iter()
            .next()
            .expect("processPayment indexed");
        let impacted = store.impact(&process_payment.uid, 3, 0.0).unwrap();
        assert!(
            impacted.iter().any(|s| s.name == "WebCheckoutSymbol"),
            "expected WebCheckoutSymbol in impact set, got {impacted:#?}"
        );
    }

    #[test]
    fn cross_repo_edges_cap_ubiquitous_names_and_keep_distinctive() {
        // A bare call to a ubiquitous name (`run`, defined publicly in 4 repos)
        // must NOT fan out into cross-repo edges; a distinctive name (1 def) must
        // still link. Guards the frequency-cap precision fix.
        let dir = tempfile::tempdir().unwrap();
        let store = GraphStore::in_memory().unwrap();

        // Index the definitions FIRST so the caller can resolve them at index time.
        for i in 0..4 {
            let repo = dir.path().join(format!("svc{i}"));
            fs::create_dir_all(&repo).unwrap();
            let body = if i == 0 {
                "export function run() { return 1; }\nexport function veryDistinctiveHandler42() { return 2; }\n"
            } else {
                "export function run() { return 1; }\n"
            };
            fs::write(repo.join("lib.js"), body).unwrap();
            let reader = crate::content_reader::FilesystemReader::new(&repo);
            index_into_store(
                &reader,
                &store,
                "test",
                &format!("https://example.com/svc{i}"),
                &format!("svc{i}-sha"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        }

        // Caller makes bare calls to both a ubiquitous and a distinctive name.
        let caller = dir.path().join("caller");
        fs::create_dir_all(&caller).unwrap();
        fs::write(
            caller.join("app.js"),
            "export function Caller() { run(); return veryDistinctiveHandler42(); }\n",
        )
        .unwrap();
        let reader = crate::content_reader::FilesystemReader::new(&caller);
        index_into_store(
            &reader,
            &store,
            "test",
            "https://example.com/caller",
            "caller-sha",
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();

        // `run` has 4 public defs store-wide → exceeds the cap → no cross-repo edge.
        let run_def = store
            .lookup_symbols_by_name("run")
            .unwrap()
            .into_iter()
            .next()
            .expect("run indexed");
        let run_impact = store.impact(&run_def.uid, 3, 0.0).unwrap();
        assert!(
            !run_impact.iter().any(|s| s.name == "Caller"),
            "ubiquitous name 'run' must NOT create a cross-repo edge, got {run_impact:#?}"
        );

        // The distinctive name has exactly one candidate → still linked.
        let dist = store
            .lookup_symbols_by_name("veryDistinctiveHandler42")
            .unwrap()
            .into_iter()
            .next()
            .expect("distinctive indexed");
        let dist_impact = store.impact(&dist.uid, 3, 0.0).unwrap();
        assert!(
            dist_impact.iter().any(|s| s.name == "Caller"),
            "distinctive cross-repo call must still link, got {dist_impact:#?}"
        );
    }

    #[test]
    fn index_populates_framework_hint_for_spring_controller() {
        // F2.0: detect_frameworks is wired into the pipeline, so a Spring
        // @RestController class gets a framework_hint persisted on its Symbol.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("UserController.java"),
            "@RestController\npublic class UserController {\n  public void get() {}\n}\n",
        )
        .unwrap();

        let (_result, store) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();

        let matches = store.lookup_symbols_by_name("UserController").unwrap();
        let ctrl = matches
            .iter()
            .find(|s| s.name == "UserController")
            .expect("UserController symbol present");
        let hint = ctrl
            .framework_hint
            .as_ref()
            .expect("framework_hint should be populated for @RestController");
        assert_eq!(hint.framework, "spring");
        assert_eq!(hint.role, "controller");
    }

    #[test]
    fn index_derives_contracts_and_implements_edges() {
        // F2.1 + F2.2: a repo with an OpenAPI spec and a matching Spring
        // controller produces declared Contract nodes plus an
        // IMPLEMENTS_CONTRACT edge from the handler to the contract.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("openapi.yaml"),
            "openapi: 3.0.0\n\
             info: { title: t, version: \"1.0\" }\n\
             paths:\n  \
             /v1/approvals:\n    \
             post:\n      \
             operationId: createApproval\n      \
             responses: { \"200\": { description: ok } }\n",
        )
        .unwrap();
        fs::write(
            src.join("ApprovalsController.java"),
            "@RestController\n\
             @RequestMapping(\"/v1/approvals\")\n\
             public class ApprovalsController {\n  \
             @PostMapping\n  \
             public void create() {}\n\
             }\n",
        )
        .unwrap();

        let (_result, store) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();

        // Declared contract from the spec exists.
        let contracts = store.list_contracts(None).unwrap();
        assert!(
            contracts
                .iter()
                .any(|c| c.uid == "contract:http:POST:/v1/approvals"),
            "expected declared contract; got {:?}",
            contracts.iter().map(|c| &c.uid).collect::<Vec<_>>()
        );

        // The handler implements it (base-path-inferred since @PostMapping has
        // no sub-path; UID still matches the spec's POST /v1/approvals).
        let implemented = store.list_implemented_contract_uids().unwrap();
        assert!(
            implemented.contains(&"contract:http:POST:/v1/approvals".to_string()),
            "expected IMPLEMENTS_CONTRACT edge; implemented: {implemented:?}"
        );
    }

    #[test]
    fn index_drift_flags_declared_not_implemented() {
        // F2.4: a spec declares GET /v1/items but no handler implements it.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("openapi.yaml"),
            "openapi: 3.0.0\n\
             info: { title: t, version: \"1.0\" }\n\
             paths:\n  \
             /v1/items:\n    \
             get:\n      \
             responses: { \"200\": { description: ok } }\n",
        )
        .unwrap();

        let (_result, store) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();

        let report = crate::contracts::drift_for_store(&store, None).unwrap();
        assert_eq!(report.declared_not_implemented.len(), 1);
        assert_eq!(
            report.declared_not_implemented[0].uid,
            "contract:http:GET:/v1/items"
        );
        assert!(report.implemented_not_declared.is_empty());
    }

    #[test]
    fn index_links_nestjs_decorator_on_own_line() {
        // F2-core correctness gap: with the decorator on the line ABOVE the
        // method (`@Post('approvals')` over `createApproval()`), the parsed
        // signature lacks the decorator. The handler must still link to the
        // declared contract, and drift must NOT flag it as
        // declared-not-implemented.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(src.join("src")).unwrap();
        fs::write(
            src.join("openapi.yaml"),
            "openapi: 3.0.0\n\
             info: { title: A, version: \"1.0\" }\n\
             paths:\n  \
             /v1/approvals:\n    \
             post:\n      \
             operationId: createApproval\n      \
             responses: { \"200\": { description: ok } }\n",
        )
        .unwrap();
        fs::write(
            src.join("src/approvals.controller.ts"),
            "@Controller('v1')\n\
             export class ApprovalsController {\n  \
             @Post('approvals')\n  \
             createApproval() { return {}; }\n\
             }\n",
        )
        .unwrap();

        let (_result, store) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();

        // The handler implements the spec-declared contract (exact verb+path).
        let implemented = store.list_implemented_contract_uids().unwrap();
        assert!(
            implemented.contains(&"contract:http:POST:/v1/approvals".to_string()),
            "expected IMPLEMENTS_CONTRACT edge; implemented: {implemented:?}"
        );

        // Drift must be clean — POST /v1/approvals is implemented.
        let report = crate::contracts::drift_for_store(&store, None).unwrap();
        assert!(
            report.declared_not_implemented.is_empty(),
            "POST /v1/approvals must not be declared-not-implemented; got {:?}",
            report.declared_not_implemented
        );

        // The controller class carries a NestJS framework_hint.
        let matches = store.lookup_symbols_by_name("ApprovalsController").unwrap();
        let ctrl = matches
            .iter()
            .find(|s| s.name == "ApprovalsController")
            .expect("ApprovalsController symbol present");
        let hint = ctrl
            .framework_hint
            .as_ref()
            .expect("framework_hint should be populated for @Controller");
        assert_eq!(hint.framework, "nestjs");
    }

    #[test]
    fn index_links_all_nestjs_handlers_in_multi_method_controller() {
        // QA bug A: a NestJS controller with MULTIPLE route methods, each
        // decorator on its own line, must produce IMPLEMENTS_CONTRACT edges for
        // EVERY handler — not just the first. The spec declares both routes;
        // drift must flag neither as declared-not-implemented.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(src.join("src")).unwrap();
        fs::write(
            src.join("openapi.yaml"),
            "openapi: 3.0.0\n\
             info: { title: A, version: \"1.0\" }\n\
             paths:\n  \
             /v1/health:\n    \
             get:\n      \
             responses: { \"200\": { description: ok } }\n  \
             /v1/users:\n    \
             post:\n      \
             responses: { \"200\": { description: ok } }\n",
        )
        .unwrap();
        fs::write(
            src.join("src/api.controller.ts"),
            "@Controller('v1')\n\
             export class Api {\n  \
             @Get('health')\n  \
             health(): object {\n    \
             return {};\n  \
             }\n\n  \
             @Post('users')\n  \
             createUser(\n    \
             @Body() body: CreateUserDto,\n  \
             ): object {\n    \
             return {};\n  \
             }\n\
             }\n",
        )
        .unwrap();

        let (_result, store) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();

        let implemented = store.list_implemented_contract_uids().unwrap();
        assert!(
            implemented.contains(&"contract:http:GET:/v1/health".to_string()),
            "GET /v1/health must be implemented; implemented: {implemented:?}"
        );
        assert!(
            implemented.contains(&"contract:http:POST:/v1/users".to_string()),
            "POST /v1/users must be implemented; implemented: {implemented:?}"
        );

        let report = crate::contracts::drift_for_store(&store, None).unwrap();
        assert!(
            report.declared_not_implemented.is_empty(),
            "neither route may be declared-not-implemented; got {:?}",
            report.declared_not_implemented
        );
    }

    #[test]
    fn index_skips_node_modules() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(src.join("node_modules/pkg")).unwrap();
        fs::write(
            src.join("node_modules/pkg/index.js"),
            "function hidden() {}",
        )
        .unwrap();
        fs::write(src.join("main.js"), "function visible() {}").unwrap();

        let (result, _) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();
        assert_eq!(result.files_count, 1, "only main.js should be indexed");
    }

    #[test]
    fn index_handles_multiple_languages() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("app.js"), "function jsFunc() {}").unwrap();
        fs::write(src.join("app.py"), "def py_func():\n    pass").unwrap();
        fs::write(src.join("style.css"), "body {}").unwrap(); // unsupported, skip

        let (result, _) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();
        assert_eq!(result.files_count, 2, "expected js + py, not css");
    }

    #[test]
    fn index_to_file_creates_db() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        let db_path = dir.path().join("test.lbug");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("main.js"), "function test() {}").unwrap();

        let result =
            index_directory(&src, &db_path, "test", "https://example.com/repo", "abc123").unwrap();
        assert!(result.symbols_count >= 1, "expected >= 1 symbol");
        assert!(db_path.exists(), "db file should exist");
    }

    #[test]
    fn force_reindex_is_idempotent_for_service_nodes() {
        // BUG repro: a repo with a src/ dir yields a Service node. The first
        // force-index succeeds; the second force-index must NOT crash with a
        // duplicated primary key on the deterministic svc: UID.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        let db_path = dir.path().join("test.lbug");
        fs::create_dir_all(src.join("src")).unwrap();
        fs::write(
            src.join("src").join("main.js"),
            "function greet(n) { return hello(n); } function hello(n) { return n; }",
        )
        .unwrap();

        let first = index_directory_with_options(
            &src,
            &db_path,
            "test",
            "https://example.com/repo",
            "abc123",
            true,
            None,
        )
        .expect("first force index");
        assert!(first.symbols_count >= 2);

        // Second force index over the same DB — previously this tripped
        // "Found duplicated primary key value svc:...".
        let second = index_directory_with_options(
            &src,
            &db_path,
            "test",
            "https://example.com/repo",
            "abc123",
            true,
            None,
        )
        .expect("second force index must be idempotent (no duplicate-PK crash)");
        assert!(second.symbols_count >= 2);

        // Queries still work after the re-index.
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let services = store.list_services(None).unwrap();
        assert!(
            !services.is_empty(),
            "expected at least one Service node after re-index"
        );
    }

    // ── Tiered change detection tests ─────────────────────────────────────

    #[test]
    fn force_reindex_reports_transactional_deletion_count() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        let db_path = dir.path().join("test.lbug");
        fs::create_dir_all(&src).unwrap();
        let source_path = src.join("main.js");
        fs::write(&source_path, "function before() { return 1; }").unwrap();

        let first = index_directory_with_options(
            &src,
            &db_path,
            "test",
            "https://example.com/repo",
            "sha-1",
            true,
            None,
        )
        .unwrap();
        assert_eq!(first.files_count, 1);

        // The path is still present, so the old pre-count classified it as
        // "not deleted" even though bulk_reindex_write transactionally deletes
        // the old File row before inserting its replacement.
        fs::write(&source_path, "function after() { return 2; }").unwrap();
        let second = index_directory_with_options(
            &src,
            &db_path,
            "test",
            "https://example.com/repo",
            "sha-2",
            true,
            None,
        )
        .unwrap();

        assert_eq!(
            second.files_deleted, 1,
            "the result must use bulk_reindex_write's authoritative deletion count"
        );
    }

    #[test]
    fn deletion_finalizer_removes_phantom_manifest_suggestions_and_preserves_survivors() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let repos = [
            (
                "app",
                "https://example.com/app",
                r#"{"name":"@acme/app","dependencies":{"@acme/removed":"1","@acme/survivor":"1"}}"#,
            ),
            (
                "removed",
                "https://example.com/removed",
                r#"{"name":"@acme/removed"}"#,
            ),
            (
                "survivor",
                "https://example.com/survivor",
                r#"{"name":"@acme/survivor"}"#,
            ),
        ];
        for (name, url, manifest) in repos {
            let root = dir.path().join(name);
            fs::create_dir_all(&root).unwrap();
            fs::write(
                root.join(format!("{name}.js")),
                format!("function {name}() {{ return '{name}'; }}"),
            )
            .unwrap();
            fs::write(root.join("package.json"), manifest).unwrap();
            index_directory(&root, &db_path, "test", url, "sha").unwrap();
        }

        let removed_uid = nestweaver_schema::repo_uid("test", "https://example.com/removed");
        let survivor_uid = nestweaver_schema::repo_uid("test", "https://example.com/survivor");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        store
            .bulk_delete_repo_files_and_symbols(&removed_uid)
            .unwrap();
        store.clear_repo_derived_nodes(&removed_uid).unwrap();
        store.delete_repo_node(&removed_uid).unwrap();
        let clusters_path = crate::sidecar_path(&db_path, ".clusters.json");
        fs::write(&clusters_path, r#"{"communities":[]}"#).unwrap();

        finalize_code_graph_deletion(
            &store,
            &db_path,
            std::slice::from_ref(&removed_uid),
            "manifest regression",
        )
        .unwrap();

        let manifests_path = crate::sidecar_path(&db_path, ".manifests.json");
        let manifests = crate::load_manifest_cache(&manifests_path).unwrap();
        let suggestions = crate::suggest_links(&store, &manifests).unwrap();
        assert!(
            suggestions.links.iter().all(|link| {
                link.to != removed_uid && !link.description.contains("@acme/removed")
            }),
            "removed repo manifest produced a phantom suggestion: {:?}",
            suggestions.links
        );
        assert!(!manifests.contains_key(&removed_uid));
        assert!(manifests.contains_key(&survivor_uid));
        assert!(suggestions.links.iter().any(|link| {
            link.link_type == "package-dependency" && link.description.contains("@acme/survivor")
        }));
        assert!(
            !clusters_path.exists(),
            "node-UID-keyed cluster output must be invalidated after deletion"
        );
    }

    #[test]
    fn deletion_finalizer_prunes_stale_embeddings_before_vector_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        for (name, url) in [
            ("removed", "https://example.com/removed-vector"),
            ("survivor", "https://example.com/survivor-vector"),
        ] {
            let root = dir.path().join(name);
            fs::create_dir_all(&root).unwrap();
            fs::write(
                root.join(format!("{name}.js")),
                format!("function {name}() {{ return '{name}'; }}"),
            )
            .unwrap();
            index_directory(&root, &db_path, "test", url, "sha").unwrap();
        }

        let removed_repo_uid =
            nestweaver_schema::repo_uid("test", "https://example.com/removed-vector");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let removed_symbol_uid = store
            .symbols_in_file("removed.js")
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
            .uid;
        let survivor_symbol_uid = store
            .symbols_in_file("survivor.js")
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
            .uid;
        assert!(store.add_embedding(&removed_symbol_uid, vec![1.0, 0.0]));
        assert!(store.add_embedding(&survivor_symbol_uid, vec![0.8, 0.6]));
        store.flush_embedding_index().unwrap();
        assert_eq!(
            store.vector_search(&[1.0, 0.0], 1)[0].0,
            removed_symbol_uid,
            "precondition: stale vector must displace the live result"
        );

        store
            .bulk_delete_repo_files_and_symbols(&removed_repo_uid)
            .unwrap();
        store.clear_repo_derived_nodes(&removed_repo_uid).unwrap();
        store.delete_repo_node(&removed_repo_uid).unwrap();
        finalize_code_graph_deletion(
            &store,
            &db_path,
            std::slice::from_ref(&removed_repo_uid),
            "embedding regression",
        )
        .unwrap();

        let live_results = store.vector_search(&[1.0, 0.0], 1);
        assert_eq!(live_results[0].0, survivor_symbol_uid);
        assert!(!store.has_embedding(&removed_symbol_uid));
        assert!(store.has_embedding(&survivor_symbol_uid));

        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        let persisted_results = reopened.vector_search(&[1.0, 0.0], 1);
        assert_eq!(persisted_results[0].0, survivor_symbol_uid);
        assert!(!reopened.has_embedding(&removed_symbol_uid));
        assert!(reopened.has_embedding(&survivor_symbol_uid));
    }

    struct InjectedDeletionIo {
        fail_save: PathBuf,
        fail_remove: PathBuf,
    }

    impl DeletionReconciliationIo for InjectedDeletionIo {
        fn save_filemeta(
            &self,
            sidecar: &FileMetaSidecar,
            path: &Path,
        ) -> Result<(), anyhow::Error> {
            if path == self.fail_save {
                anyhow::bail!("injected filemeta save failure");
            }
            save_filemeta_sidecar(sidecar, path)
        }

        fn save_resolution_deps(
            &self,
            deps: &crate::resolution_cache::ResolutionDeps,
            path: &Path,
        ) -> Result<(), anyhow::Error> {
            deps.save(path)
        }

        fn remove_file(&self, path: &Path) -> std::io::Result<()> {
            if path == self.fail_remove {
                return Err(std::io::Error::other("injected sidecar removal failure"));
            }
            std::fs::remove_file(path)
        }
    }

    #[test]
    fn deletion_finalizer_aggregates_failures_and_runs_every_later_stage() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let repo_uid = "repo:test:aggregate-reconciliation".to_string();

        let filemeta_path = crate::sidecar_path(&db_path, ".filemeta.json");
        let mut filemeta = FileMetaSidecar::default();
        filemeta.repos.entry(repo_uid.clone()).or_default();
        save_filemeta_sidecar(&filemeta, &filemeta_path).unwrap();

        let clusters_path = crate::sidecar_path(&db_path, ".clusters.json");
        fs::write(&clusters_path, r#"{"communities":[]}"#).unwrap();
        let generation_path = crate::sidecar_path(&db_path, ".generation");
        fs::create_dir(&generation_path).unwrap();
        let pagerank_path = crate::sidecar_path(&db_path, ".pagerank.json");
        fs::write(&pagerank_path, r#"{"deleted":1.0}"#).unwrap();
        store.load_pagerank_cache(&pagerank_path).unwrap();
        let pagerank_generation = store.pagerank_generation();

        let io = InjectedDeletionIo {
            fail_save: filemeta_path.clone(),
            fail_remove: clusters_path.clone(),
        };

        let error = finalize_code_graph_deletion_with_io(
            &store,
            &db_path,
            std::slice::from_ref(&repo_uid),
            "aggregate regression",
            &io,
        )
        .unwrap_err();

        assert_eq!(
            error
                .failures
                .iter()
                .map(|failure| failure.stage)
                .collect::<Vec<_>>(),
            vec![
                DeletionReconciliationStage::FileMetadata,
                DeletionReconciliationStage::ClusterCache,
                DeletionReconciliationStage::GenerationPersistence,
            ]
        );
        assert!(
            load_filemeta_sidecar(&filemeta_path)
                .repos
                .contains_key(&repo_uid),
            "failed filemeta save must leave the durable stale slice visible"
        );
        assert!(
            clusters_path.exists(),
            "injected removal failure must be real"
        );
        assert!(
            generation_path.is_dir(),
            "injected generation persistence failure must remain observable"
        );
        assert!(
            !pagerank_path.exists(),
            "persisted PageRank invalidation must run after earlier failures"
        );
        assert!(
            !store.pagerank_scores().contains_key("deleted"),
            "live PageRank invalidation must discard the primed stale score"
        );
        assert!(store.pagerank_generation() > pagerank_generation);
    }

    #[test]
    fn deletion_finalizer_classifies_legacy_retirement_failure() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let repo_uid = "repo:test:legacy-retirement".to_string();
        let manifests_path = crate::manifest::manifest_cache_path(&db_path);
        let manifests =
            HashMap::from([(repo_uid.clone(), crate::manifest::ManifestInfo::default())]);
        crate::manifest::save_manifest_cache(&manifests, &manifests_path).unwrap();
        let legacy_path = db_path.with_extension("manifests.json");
        fs::create_dir(&legacy_path).unwrap();

        let error = finalize_code_graph_deletion(
            &store,
            &db_path,
            &[repo_uid],
            "legacy retirement regression",
        )
        .unwrap_err();

        assert_eq!(error.failures.len(), 1);
        assert_eq!(
            error.failures[0].stage,
            DeletionReconciliationStage::LegacyRetirement
        );
        assert!(
            error.failures[0]
                .message
                .contains("legacy manifest sidecar")
        );
        assert!(
            crate::sidecar_path(&db_path, ".generation").exists(),
            "later generation persistence must still run"
        );
    }

    #[test]
    fn deletion_finalizer_classifies_embedding_legacy_retirement_failure() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let mut legacy_path = db_path.as_os_str().to_owned();
        legacy_path.push(".embeddings");
        fs::create_dir(std::path::PathBuf::from(legacy_path)).unwrap();

        let error = finalize_code_graph_deletion(
            &store,
            &db_path,
            &[],
            "embedding legacy retirement regression",
        )
        .unwrap_err();

        assert_eq!(error.failures.len(), 1);
        assert_eq!(
            error.failures[0].stage,
            DeletionReconciliationStage::LegacyRetirement
        );
        assert!(
            error.failures[0]
                .message
                .contains("legacy embedding retirement")
        );
    }

    #[test]
    fn full_index_filemeta_failure_still_finalizes_committed_graph() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let db_path = dir.path().join("test.lbug");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("old.js"), "function oldVersion() { return 1; }").unwrap();
        index_directory(
            &repo,
            &db_path,
            "test",
            "https://example.com/filemeta-epilogue",
            "sha-1",
        )
        .unwrap();

        let store = GraphStore::open_or_create(&db_path).unwrap();
        let pagerank_path = crate::sidecar_path(&db_path, ".pagerank.json");
        fs::write(&pagerank_path, r#"{"stale":1.0}"#).unwrap();
        store.load_pagerank_cache(&pagerank_path).unwrap();
        let generation_before = store.graph_generation();

        fs::remove_file(repo.join("old.js")).unwrap();
        fs::write(repo.join("new.js"), "function newVersion() { return 2; }").unwrap();
        let filemeta_path = crate::sidecar_path(&db_path, ".filemeta.json");
        fs::remove_file(&filemeta_path).unwrap();
        fs::create_dir(&filemeta_path).unwrap();

        let error = match index_directory_with_store(
            &store,
            &repo,
            &db_path,
            "test",
            "https://example.com/filemeta-epilogue",
            "sha-2",
            true,
            None,
        ) {
            Ok(_) => panic!("injected filemeta persistence failure must be returned"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("filemeta sidecar"));
        assert!(
            store
                .symbols_in_file("new.js")
                .unwrap()
                .iter()
                .any(|symbol| symbol.name == "newVersion"),
            "precondition: the replacement graph transaction must commit before filemeta fails"
        );
        assert!(
            !store.pagerank_scores().contains_key("stale"),
            "the committed graph must invalidate the live stale PageRank cache"
        );
        let persisted: HashMap<String, f64> =
            serde_json::from_slice(&fs::read(&pagerank_path).unwrap()).unwrap();
        assert!(
            !persisted.contains_key("stale"),
            "the committed graph may publish fresh PageRank but must not retain the stale score"
        );
        assert!(
            !crate::sidecar_path(&db_path, ".index-dirty").exists(),
            "durable fresh publication should retire the fail-closed marker"
        );
        assert!(store.graph_generation() > generation_before);
        assert_eq!(
            fs::read_to_string(crate::sidecar_path(&db_path, ".generation"))
                .unwrap()
                .trim()
                .parse::<u64>()
                .unwrap(),
            store.graph_generation(),
            "the bumped graph generation must be durable before returning the filemeta error"
        );
    }

    #[derive(Default)]
    struct InjectedIndexEpilogueIo {
        fail_establish: bool,
        fail_remove: bool,
        fail_rename: bool,
        fail_generation: bool,
        fail_compute: bool,
        fail_save: bool,
    }

    impl IndexEpilogueIo for InjectedIndexEpilogueIo {
        fn establish_marker(&self, path: &Path) -> Result<(), anyhow::Error> {
            if self.fail_establish {
                anyhow::bail!("injected marker establishment failure");
            }
            FileSystemIndexEpilogueIo.establish_marker(path)
        }

        fn clear_marker(&self, path: &Path) -> Result<(), anyhow::Error> {
            FileSystemIndexEpilogueIo.clear_marker(path)
        }

        fn remove_file(&self, path: &Path) -> std::io::Result<()> {
            if self.fail_remove {
                return Err(std::io::Error::other("injected PageRank removal failure"));
            }
            std::fs::remove_file(path)
        }

        fn rename_file(&self, from: &Path, to: &Path) -> std::io::Result<()> {
            if self.fail_rename {
                return Err(std::io::Error::other(
                    "injected PageRank quarantine failure",
                ));
            }
            std::fs::rename(from, to)
        }

        fn save_generation(
            &self,
            store: &GraphStore,
            path: &Path,
            generation: u64,
        ) -> Result<(), anyhow::Error> {
            if self.fail_generation {
                anyhow::bail!("injected generation save failure");
            }
            store
                .save_graph_generation_value(path, generation)
                .map_err(Into::into)
        }

        fn compute_pagerank(
            &self,
            store: &GraphStore,
            scope: &nestweaver_store::GraphScope,
        ) -> Result<(), anyhow::Error> {
            if self.fail_compute {
                anyhow::bail!("injected PageRank compute failure");
            }
            FileSystemIndexEpilogueIo.compute_pagerank(store, scope)
        }

        fn save_pagerank(&self, store: &GraphStore, path: &Path) -> Result<(), anyhow::Error> {
            if self.fail_save {
                anyhow::bail!("injected PageRank save failure");
            }
            FileSystemIndexEpilogueIo.save_pagerank(store, path)
        }
    }

    #[test]
    fn pagerank_compute_failure_is_returned_after_mandatory_commit_publication() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let pagerank_path = crate::sidecar_path(&db_path, ".pagerank.json");
        fs::write(&pagerank_path, r#"{"stale":1.0}"#).unwrap();
        store.load_pagerank_cache(&pagerank_path).unwrap();
        let generation_before = store.graph_generation();

        let publication = establish_index_publication_marker_with_io(
            &store,
            Some(&db_path),
            "compute failure regression",
            &FileSystemIndexEpilogueIo,
        )
        .unwrap();
        let error = finalize_committed_index_with_io(
            publication,
            Some(&db_path),
            "compute failure regression",
            &InjectedIndexEpilogueIo {
                fail_compute: true,
                ..Default::default()
            },
            true,
        )
        .expect_err("a post-commit PageRank compute failure must be returned");

        assert_eq!(
            error.failures[0].stage,
            DeletionReconciliationStage::PageRankCompute
        );
        assert!(!store.pagerank_scores().contains_key("stale"));
        assert!(!pagerank_path.exists());
        assert!(store.graph_generation() > generation_before);
        assert_eq!(
            fs::read_to_string(crate::sidecar_path(&db_path, ".generation"))
                .unwrap()
                .trim()
                .parse::<u64>()
                .unwrap(),
            store.graph_generation()
        );
    }

    #[test]
    fn pagerank_save_failure_is_returned_without_restoring_stale_persisted_ranks() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let pagerank_path = crate::sidecar_path(&db_path, ".pagerank.json");
        fs::write(&pagerank_path, r#"{"stale":1.0}"#).unwrap();
        store.load_pagerank_cache(&pagerank_path).unwrap();
        let generation_before = store.graph_generation();

        let publication = establish_index_publication_marker_with_io(
            &store,
            Some(&db_path),
            "save failure regression",
            &FileSystemIndexEpilogueIo,
        )
        .unwrap();
        let error = finalize_committed_index_with_io(
            publication,
            Some(&db_path),
            "save failure regression",
            &InjectedIndexEpilogueIo {
                fail_save: true,
                ..Default::default()
            },
            true,
        )
        .expect_err("a post-commit PageRank save failure must be returned");

        assert_eq!(
            error.failures[0].stage,
            DeletionReconciliationStage::PageRankPersistence
        );
        assert!(!store.pagerank_scores().contains_key("stale"));
        assert!(!pagerank_path.exists());
        assert!(store.graph_generation() > generation_before);
    }

    #[test]
    fn pagerank_removal_failure_quarantines_stale_ranks_and_publishes_generation() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let pagerank_path = crate::sidecar_path(&db_path, ".pagerank.json");
        fs::write(&pagerank_path, r#"{"stale":1.0}"#).unwrap();
        store.load_pagerank_cache(&pagerank_path).unwrap();
        let generation_before = store.graph_generation();

        let publication = establish_index_publication_marker_with_io(
            &store,
            Some(&db_path),
            "removal failure regression",
            &FileSystemIndexEpilogueIo,
        )
        .unwrap();
        let error = finalize_committed_index_with_io(
            publication,
            Some(&db_path),
            "removal failure regression",
            &InjectedIndexEpilogueIo {
                fail_remove: true,
                ..Default::default()
            },
            false,
        )
        .expect_err("the injected durable removal failure must be returned");

        assert_eq!(
            error.failures[0].stage,
            DeletionReconciliationStage::PersistedPageRank
        );
        assert!(!store.pagerank_scores().contains_key("stale"));
        assert!(!pagerank_path.exists());
        assert!(quarantine_path(&pagerank_path).exists());
        assert!(store.graph_generation() > generation_before);
        assert_eq!(
            fs::read_to_string(crate::sidecar_path(&db_path, ".generation"))
                .unwrap()
                .trim()
                .parse::<u64>()
                .unwrap(),
            store.graph_generation()
        );
    }

    #[test]
    fn unlink_and_quarantine_failure_keeps_dirty_marker_fail_closed_on_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let pagerank_path = crate::sidecar_path(&db_path, ".pagerank.json");
        let generation_path = crate::sidecar_path(&db_path, ".generation");
        let marker_path = crate::sidecar_path(&db_path, ".index-dirty");
        fs::write(&pagerank_path, r#"{"stale":1.0}"#).unwrap();
        store.load_pagerank_cache(&pagerank_path).unwrap();
        store.bump_graph_generation();
        store.save_graph_generation(&generation_path).unwrap();

        let publication = establish_index_publication_marker_with_io(
            &store,
            Some(&db_path),
            "unlink and quarantine regression",
            &FileSystemIndexEpilogueIo,
        )
        .unwrap();
        let error = finalize_committed_index_with_io(
            publication,
            Some(&db_path),
            "unlink and quarantine regression",
            &InjectedIndexEpilogueIo {
                fail_remove: true,
                fail_rename: true,
                fail_compute: true,
                ..Default::default()
            },
            true,
        )
        .expect_err("unsafe persisted PageRank must fail publication");

        assert!(error.to_string().contains("persisted-pagerank"));
        assert!(
            !error.to_string().contains("pagerank-compute"),
            "PageRank must not run while unsafe publication remains dirty"
        );
        assert!(marker_path.exists(), "unsafe publication must remain dirty");
        assert!(
            pagerank_path.exists(),
            "both injected retirement paths failed"
        );
        let canonical_generation = fs::read_to_string(&generation_path)
            .unwrap()
            .trim()
            .parse::<u64>()
            .unwrap();
        drop(store);

        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        assert_ne!(reopened.graph_generation(), canonical_generation);
        reopened.load_pagerank_cache(&pagerank_path).unwrap();
        assert!(!reopened.pagerank_scores().contains_key("stale"));
    }

    #[test]
    fn generation_save_failure_keeps_dirty_marker_fail_closed_on_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let pagerank_path = crate::sidecar_path(&db_path, ".pagerank.json");
        let generation_path = crate::sidecar_path(&db_path, ".generation");
        let marker_path = crate::sidecar_path(&db_path, ".index-dirty");
        fs::write(&pagerank_path, r#"{"stale":1.0}"#).unwrap();
        store.load_pagerank_cache(&pagerank_path).unwrap();
        store.bump_graph_generation();
        store.save_graph_generation(&generation_path).unwrap();
        let stale_generation = store.graph_generation();

        let publication = establish_index_publication_marker_with_io(
            &store,
            Some(&db_path),
            "generation regression",
            &FileSystemIndexEpilogueIo,
        )
        .unwrap();
        let error = finalize_committed_index_with_io(
            publication,
            Some(&db_path),
            "generation regression",
            &InjectedIndexEpilogueIo {
                fail_generation: true,
                fail_compute: true,
                ..Default::default()
            },
            true,
        )
        .expect_err("unsafe generation persistence must fail publication");

        let message = error.to_string();
        assert!(message.contains("generation-persistence"));
        assert!(
            !message.contains("pagerank-compute"),
            "PageRank must not run before generation publication is clean"
        );
        assert!(marker_path.exists(), "unsafe publication must remain dirty");
        drop(store);

        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        assert_ne!(reopened.graph_generation(), stale_generation);
        reopened.load_pagerank_cache(&pagerank_path).unwrap();
        assert!(!reopened.pagerank_scores().contains_key("stale"));
    }

    #[test]
    fn unreadable_marker_recovery_publishes_monotonic_generation_and_rejects_stale_caches() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let generation_path = crate::sidecar_path(&db_path, ".generation");
        let pagerank_path = crate::sidecar_path(&db_path, ".pagerank.json");
        let marker_path = crate::sidecar_path(&db_path, ".index-dirty");
        let cache_key = nestweaver_store::cache::ResponseCache::key(
            "brain_search",
            &serde_json::json!({"query": "historical"}),
        );
        let scope_digest = 41;

        {
            let store = GraphStore::open_or_create(&db_path).unwrap();
            for _ in 0..7 {
                store.bump_graph_generation();
            }
            store.save_graph_generation(&generation_path).unwrap();
            let mut cache = nestweaver_store::cache::ResponseCache::open(&db_path, 1);
            cache.insert(
                cache_key,
                "brain_search",
                br#"{"stale":true}"#,
                store.graph_generation(),
                scope_digest,
            );
            cache.save();
        }
        fs::write(&pagerank_path, r#"{"stale":1.0}"#).unwrap();
        fs::create_dir(&marker_path).unwrap();

        let recovering = GraphStore::open_or_create(&db_path).unwrap();
        assert_eq!(
            recovering.graph_generation(),
            8,
            "dirty recovery must reserve canonical generation 7's successor"
        );
        recovering.load_pagerank_cache(&pagerank_path).unwrap();
        assert!(!recovering.pagerank_scores().contains_key("stale"));
        let mut cache = nestweaver_store::cache::ResponseCache::open(&db_path, 1);
        assert!(
            cache
                .get(cache_key, recovering.graph_generation(), scope_digest)
                .is_none(),
            "the reserved recovery generation must reject generation-7 cache entries"
        );
        cache.insert(
            cache_key,
            "brain_search",
            br#"{"dirty":true}"#,
            recovering.graph_generation(),
            scope_digest,
        );
        cache.save();

        fs::remove_dir(&marker_path).unwrap();
        let publication = establish_index_publication_marker_with_io(
            &recovering,
            Some(&db_path),
            "unreadable marker recovery",
            &FileSystemIndexEpilogueIo,
        )
        .unwrap();
        finalize_committed_index_with_io(
            publication,
            Some(&db_path),
            "unreadable marker recovery",
            &FileSystemIndexEpilogueIo,
            false,
        )
        .unwrap();
        assert!(!marker_path.exists());
        assert_eq!(
            recovering.graph_generation(),
            9,
            "clean publication must not reuse dirty reservation 8"
        );
        drop(recovering);

        let clean = GraphStore::open_or_create(&db_path).unwrap();
        assert_eq!(clean.graph_generation(), 9);
        assert!(clean.graph_generation() > 8);
        clean.load_pagerank_cache(&pagerank_path).unwrap();
        assert!(!clean.pagerank_scores().contains_key("stale"));
        let mut cache = nestweaver_store::cache::ResponseCache::open(&db_path, 1);
        assert!(
            cache
                .get(cache_key, clean.graph_generation(), scope_digest)
                .is_none(),
            "clean reopen must reject the dirty-generation cache entry"
        );
    }

    #[test]
    fn successor_exhaustion_aborts_before_marker_establishment() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let generation_path = crate::sidecar_path(&db_path, ".generation");
        let marker_path = crate::sidecar_path(&db_path, ".index-dirty");

        let recovering = GraphStore::open_or_create(&db_path).unwrap();
        fs::write(&generation_path, (u64::MAX - 1).to_string()).unwrap();
        recovering.load_graph_generation(&generation_path);
        assert_eq!(recovering.graph_generation(), u64::MAX - 1);

        let error = establish_index_publication_marker_with_io(
            &recovering,
            Some(&db_path),
            "exhausted generation",
            &FileSystemIndexEpilogueIo,
        )
        .expect_err("generation exhaustion must abort before graph publication");

        assert!(error.to_string().contains("generation"));
        assert!(
            !marker_path.exists(),
            "successor exhaustion must fail before establishing the marker"
        );
        assert_eq!(recovering.graph_generation(), u64::MAX - 1);
        assert_eq!(
            fs::read_to_string(&generation_path).unwrap(),
            (u64::MAX - 1).to_string()
        );
    }

    struct EmptyReader {
        root: PathBuf,
    }

    impl crate::content_reader::ContentReader for EmptyReader {
        fn read_file(&self, _rel_path: &Path) -> Result<String, anyhow::Error> {
            // Manifest discovery probes well-known paths even when list_files
            // is empty; model those probes as absent/empty content.
            Ok(String::new())
        }

        fn list_files(&self) -> Result<Vec<PathBuf>, anyhow::Error> {
            Ok(Vec::new())
        }

        fn file_meta(&self, _rel_path: &Path) -> Result<Option<(u64, u64)>, anyhow::Error> {
            Ok(None)
        }

        fn root(&self) -> &Path {
            &self.root
        }

        fn version_id(&self) -> &str {
            "empty"
        }
    }

    #[test]
    fn server_full_delete_only_replacement_refreshes_pagerank() {
        let dir = tempfile::tempdir().unwrap();
        let removed_repo = dir.path().join("removed-repo");
        let surviving_repo = dir.path().join("surviving-repo");
        let db_path = dir.path().join("test.lbug");
        fs::create_dir_all(&removed_repo).unwrap();
        fs::create_dir_all(&surviving_repo).unwrap();
        fs::write(
            removed_repo.join("removed.js"),
            "function removed() { return 1; }",
        )
        .unwrap();
        fs::write(
            surviving_repo.join("survivor.js"),
            "function survivor() { return 2; }",
        )
        .unwrap();

        let removed_url = "https://example.com/removed";
        index_directory(&removed_repo, &db_path, "test", removed_url, "sha-1").unwrap();
        index_directory(
            &surviving_repo,
            &db_path,
            "test",
            "https://example.com/surviving",
            "sha-1",
        )
        .unwrap();

        let pagerank_path = crate::sidecar_path(&db_path, ".pagerank.json");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        store.load_pagerank_cache(&pagerank_path).unwrap();
        let removed_uid = store
            .symbols_in_file("removed.js")
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
            .uid;
        assert!(store.pagerank_scores().contains_key(&removed_uid));

        let reader = EmptyReader {
            root: dir.path().join("empty-reader"),
        };
        let result = index_with_reader_and_write_gate(
            &reader,
            &store,
            "test",
            removed_url,
            "sha-2",
            None,
            None,
            || Ok::<_, anyhow::Error>(()),
        )
        .unwrap();

        assert_eq!(result.files_deleted, 1);
        assert!(
            !store.pagerank_scores().contains_key(&removed_uid),
            "the live server store must not serve the deleted symbol's stale score"
        );
        let persisted: HashMap<String, f64> =
            serde_json::from_slice(&fs::read(&pagerank_path).unwrap()).unwrap();
        assert!(
            !persisted.contains_key(&removed_uid),
            "a daemon restart must not reload the deleted symbol from the PageRank sidecar"
        );
        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        reopened.load_pagerank_cache(&pagerank_path).unwrap();
        assert!(
            !reopened.pagerank_scores().contains_key(&removed_uid),
            "a reopened store must not reload the deleted symbol's stale score"
        );
    }

    #[test]
    fn server_full_compute_failure_finalizes_before_releasing_write_gate() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        struct PublicationGuard<'a> {
            store: &'a GraphStore,
            pagerank_path: PathBuf,
            generation_path: PathBuf,
            generation_before: u64,
            dropped: Arc<AtomicBool>,
        }

        impl Drop for PublicationGuard<'_> {
            fn drop(&mut self) {
                assert!(
                    !self.pagerank_path.exists(),
                    "the stale PageRank sidecar must be retired before the write gate is released"
                );
                assert!(self.store.graph_generation() > self.generation_before);
                assert_eq!(
                    fs::read_to_string(&self.generation_path)
                        .unwrap()
                        .trim()
                        .parse::<u64>()
                        .unwrap(),
                    self.store.graph_generation(),
                    "the graph generation must be durable before the write gate is released"
                );
                self.dropped.store(true, Ordering::SeqCst);
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let db_path = dir.path().join("test.lbug");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("old.js"), "function oldVersion() { return 1; }").unwrap();

        let repo_url = "https://example.com/server-write-gate-epilogue";
        index_directory(&repo, &db_path, "test", repo_url, "sha-1").unwrap();
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let pagerank_path = crate::sidecar_path(&db_path, ".pagerank.json");
        fs::write(&pagerank_path, r#"{"stale":1.0}"#).unwrap();
        store.load_pagerank_cache(&pagerank_path).unwrap();
        let generation_path = crate::sidecar_path(&db_path, ".generation");
        let generation_before = store.graph_generation();
        let dropped = Arc::new(AtomicBool::new(false));
        let reader = EmptyReader {
            root: dir.path().join("empty-reader"),
        };

        let error = match index_with_reader_and_write_gate_and_io(
            ReaderIndexRequest {
                reader: &reader,
                store: &store,
                instance_id: "test",
                repo_url,
                indexed_sha: "sha-2",
                name: None,
                cancel: None,
                epilogue_io: &InjectedIndexEpilogueIo {
                    fail_compute: true,
                    ..Default::default()
                },
            },
            || {
                Ok::<_, anyhow::Error>(PublicationGuard {
                    store: &store,
                    pagerank_path: pagerank_path.clone(),
                    generation_path: generation_path.clone(),
                    generation_before,
                    dropped: Arc::clone(&dropped),
                })
            },
        ) {
            Ok(_) => panic!("the injected server PageRank compute failure must be returned"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("pagerank-compute"));
        assert!(
            store.symbols_in_file("old.js").unwrap().is_empty(),
            "the server replacement transaction must commit before PageRank fails"
        );
        assert!(!store.pagerank_scores().contains_key("stale"));
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn full_generation_failure_skips_dirty_pagerank_compute() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let db_path = dir.path().join("test.lbug");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("old.js"), "function oldVersion() { return 1; }").unwrap();
        let repo_url = "https://example.com/full-dual-failure";
        index_directory(&repo, &db_path, "test", repo_url, "sha-1").unwrap();
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let reader = EmptyReader {
            root: dir.path().join("empty-reader"),
        };

        let error = match index_with_reader_and_write_gate_and_io(
            ReaderIndexRequest {
                reader: &reader,
                store: &store,
                instance_id: "test",
                repo_url,
                indexed_sha: "sha-2",
                name: None,
                cancel: None,
                epilogue_io: &InjectedIndexEpilogueIo {
                    fail_generation: true,
                    fail_compute: true,
                    ..Default::default()
                },
            },
            || Ok::<_, anyhow::Error>(()),
        ) {
            Ok(_) => panic!("dual post-commit failure must be returned"),
            Err(error) => error,
        };

        let message = error.to_string();
        assert!(message.contains("generation-persistence"));
        assert!(
            !message.contains("pagerank-compute"),
            "PageRank must not run before generation publication is clean"
        );
        assert!(crate::sidecar_path(&db_path, ".index-dirty").exists());
        assert!(store.symbols_in_file("old.js").unwrap().is_empty());
    }

    #[test]
    fn marker_establishment_failure_aborts_before_full_graph_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let db_path = dir.path().join("test.lbug");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("old.js"), "function oldVersion() { return 1; }").unwrap();
        let repo_url = "https://example.com/marker-precondition";
        index_directory(&repo, &db_path, "test", repo_url, "sha-1").unwrap();
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let reader = EmptyReader {
            root: dir.path().join("empty-reader"),
        };

        let error = match index_with_reader_and_write_gate_and_io(
            ReaderIndexRequest {
                reader: &reader,
                store: &store,
                instance_id: "test",
                repo_url,
                indexed_sha: "sha-2",
                name: None,
                cancel: None,
                epilogue_io: &InjectedIndexEpilogueIo {
                    fail_establish: true,
                    ..Default::default()
                },
            },
            || Ok::<_, anyhow::Error>(()),
        ) {
            Ok(_) => panic!("marker establishment failure must abort indexing"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("index-publication-marker"));
        assert!(
            store
                .symbols_in_file("old.js")
                .unwrap()
                .iter()
                .any(|symbol| symbol.name == "oldVersion"),
            "no graph replacement may commit without the durable marker"
        );
    }

    #[test]
    fn deleting_last_parseable_file_refreshes_pagerank() {
        let dir = tempfile::tempdir().unwrap();
        let removed_repo = dir.path().join("removed-repo");
        let surviving_repo = dir.path().join("surviving-repo");
        let db_path = dir.path().join("test.lbug");
        fs::create_dir_all(&removed_repo).unwrap();
        fs::create_dir_all(&surviving_repo).unwrap();
        let removed_file = removed_repo.join("removed.js");
        fs::write(&removed_file, "function removed() { return 1; }").unwrap();
        fs::write(
            surviving_repo.join("survivor.js"),
            "function survivor() { return 2; }",
        )
        .unwrap();

        index_directory(
            &removed_repo,
            &db_path,
            "test",
            "https://example.com/removed",
            "abc123",
        )
        .unwrap();
        index_directory(
            &surviving_repo,
            &db_path,
            "test",
            "https://example.com/surviving",
            "abc123",
        )
        .unwrap();

        let pagerank_path = crate::sidecar_path(&db_path, ".pagerank.json");
        let pagerank_before: HashMap<String, f64> =
            serde_json::from_slice(&fs::read(&pagerank_path).unwrap()).unwrap();
        fs::remove_file(&removed_file).unwrap();

        let result = index_directory(
            &removed_repo,
            &db_path,
            "test",
            "https://example.com/removed",
            "abc123",
        )
        .unwrap();

        assert_eq!(
            result.files_deleted, 1,
            "the force-reindex path must report deletion of the repo's last parseable file"
        );
        let pagerank_after: HashMap<String, f64> =
            serde_json::from_slice(&fs::read(&pagerank_path).unwrap()).unwrap();
        assert!(
            pagerank_after.len() < pagerank_before.len(),
            "PageRank sidecar must drop the deleted repo's symbols"
        );
    }

    #[test]
    fn reidentify_delete_only_refreshes_pagerank() {
        let dir = tempfile::tempdir().unwrap();
        let removed_repo = dir.path().join("removed-repo");
        let surviving_repo = dir.path().join("surviving-repo");
        let db_path = dir.path().join("test.lbug");
        fs::create_dir_all(&removed_repo).unwrap();
        fs::create_dir_all(&surviving_repo).unwrap();
        let removed_file = removed_repo.join("removed.js");
        fs::write(&removed_file, "function removed() { return 1; }").unwrap();
        fs::write(
            surviving_repo.join("survivor.js"),
            "function survivor() { return 2; }",
        )
        .unwrap();

        let local_url = format!("file://{}", removed_repo.display());
        index_directory(&removed_repo, &db_path, "test", &local_url, "abc123").unwrap();
        index_directory(
            &surviving_repo,
            &db_path,
            "test",
            "https://example.com/surviving",
            "abc123",
        )
        .unwrap();

        let pagerank_path = crate::sidecar_path(&db_path, ".pagerank.json");
        let pagerank_before: HashMap<String, f64> =
            serde_json::from_slice(&fs::read(&pagerank_path).unwrap()).unwrap();
        fs::remove_file(&removed_file).unwrap();

        let result = index_directory(
            &removed_repo,
            &db_path,
            "test",
            "https://example.com/reidentified",
            "abc123",
        )
        .unwrap();

        assert_eq!(
            result.files_deleted, 1,
            "re-identification must report files deleted with the old repo uid"
        );
        let pagerank_after: HashMap<String, f64> =
            serde_json::from_slice(&fs::read(&pagerank_path).unwrap()).unwrap();
        assert!(
            pagerank_after.len() < pagerank_before.len(),
            "PageRank sidecar must drop symbols deleted during re-identification"
        );
    }

    #[test]
    fn non_ancestor_delete_only_fallback_refreshes_pagerank() {
        let dir = tempfile::tempdir().unwrap();
        let removed_repo = dir.path().join("removed-repo");
        let surviving_repo = dir.path().join("surviving-repo");
        let db_path = dir.path().join("test.lbug");
        fs::create_dir_all(&removed_repo).unwrap();
        fs::create_dir_all(&surviving_repo).unwrap();
        fs::write(
            removed_repo.join("removed.js"),
            "function removed() { return 1; }",
        )
        .unwrap();
        fs::write(
            surviving_repo.join("survivor.js"),
            "function survivor() { return 2; }",
        )
        .unwrap();

        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&removed_repo)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "NestWeaver Test"]);
        git(&["add", "removed.js"]);
        git(&["commit", "-q", "-m", "initial"]);
        let old_sha = git(&["rev-parse", "HEAD"]);

        let repo_url = "https://example.com/removed";
        index_directory(&removed_repo, &db_path, "test", repo_url, &old_sha).unwrap();
        index_directory(
            &surviving_repo,
            &db_path,
            "test",
            "https://example.com/surviving",
            "abc123",
        )
        .unwrap();

        let pagerank_path = crate::sidecar_path(&db_path, ".pagerank.json");
        let pagerank_before: HashMap<String, f64> =
            serde_json::from_slice(&fs::read(&pagerank_path).unwrap()).unwrap();
        fs::remove_file(removed_repo.join("removed.js")).unwrap();
        git(&["add", "-A"]);
        git(&["commit", "--amend", "--no-edit", "--allow-empty", "-q"]);

        let result = incremental_index(&removed_repo, &db_path, "test", repo_url).unwrap();

        assert!(result.fell_back_to_full);
        assert_eq!(
            result.files_deleted, 1,
            "non-ancestor fallback must report files deleted before the full index"
        );
        let pagerank_after: HashMap<String, f64> =
            serde_json::from_slice(&fs::read(&pagerank_path).unwrap()).unwrap();
        assert!(
            pagerank_after.len() < pagerank_before.len(),
            "PageRank sidecar must drop symbols deleted before non-ancestor fallback"
        );
    }

    /// Regression: a crash between the SHA commit and content landing leaves a
    /// Repo row whose indexed_sha matches HEAD but owns zero symbols. The
    /// `old_sha == new_sha` skip must not self-perpetuate that empty state —
    /// incremental_index must force a full re-index.
    #[test]
    fn sha_set_but_no_symbols_forces_full_reindex() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let db_path = dir.path().join("test.lbug");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("main.js"), "function healed() { return 1; }").unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "NestWeaver Test"]);
        git(&["add", "main.js"]);
        git(&["commit", "-q", "-m", "initial"]);
        let head = git(&["rev-parse", "HEAD"]);

        let repo_url = "https://example.com/sha-set-but-empty";
        let r_uid = nestweaver_schema::repo_uid("test", repo_url);
        {
            let store = GraphStore::open_or_create(&db_path).unwrap();
            store
                .insert_repo(&nestweaver_schema::Repo {
                    uid: r_uid.clone(),
                    url: repo_url.into(),
                    // SHA matches HEAD but no symbols were ever indexed.
                    indexed_sha: head,
                    staleness_commits_behind: 0,
                    instance_id: "test".into(),
                    name: None,
                    root_path: None,
                })
                .unwrap();
        }

        let result = incremental_index(&repo, &db_path, "test", repo_url).unwrap();

        assert!(
            result.fell_back_to_full,
            "SHA-set-but-empty repo must force a full re-index"
        );
        let store = GraphStore::open_or_create(&db_path).unwrap();
        assert!(
            store.repo_has_symbols(&r_uid).unwrap(),
            "full re-index must land symbols for the repo"
        );
    }

    /// An empty indexed_sha (Repo row created but SHA never committed) must
    /// explicitly take the full-index path rather than relying on
    /// `is_ancestor("")` returning false downstream.
    #[test]
    fn empty_indexed_sha_forces_full_reindex() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let db_path = dir.path().join("test.lbug");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("main.js"), "function healed() { return 1; }").unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "NestWeaver Test"]);
        git(&["add", "main.js"]);
        git(&["commit", "-q", "-m", "initial"]);

        let repo_url = "https://example.com/empty-sha";
        let r_uid = nestweaver_schema::repo_uid("test", repo_url);
        {
            let store = GraphStore::open_or_create(&db_path).unwrap();
            store
                .insert_repo(&nestweaver_schema::Repo {
                    uid: r_uid.clone(),
                    url: repo_url.into(),
                    indexed_sha: String::new(),
                    staleness_commits_behind: 0,
                    instance_id: "test".into(),
                    name: None,
                    root_path: None,
                })
                .unwrap();
        }

        let result = incremental_index(&repo, &db_path, "test", repo_url).unwrap();

        assert!(
            result.fell_back_to_full,
            "empty indexed_sha must force a full re-index"
        );
        let store = GraphStore::open_or_create(&db_path).unwrap();
        assert!(
            store.repo_has_symbols(&r_uid).unwrap(),
            "full re-index must land symbols for the repo"
        );
    }

    #[test]
    fn incremental_generation_failure_skips_dirty_pagerank_compute() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let db_path = dir.path().join("test.lbug");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("main.js"), "function before() { return 1; }").unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?} failed");
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "NestWeaver Test"]);
        git(&["add", "main.js"]);
        git(&["commit", "-q", "-m", "initial"]);
        let old_sha = git(&["rev-parse", "HEAD"]);
        let repo_url = "https://example.com/incremental-epilogue";
        index_directory(&repo, &db_path, "test", repo_url, &old_sha).unwrap();

        fs::write(repo.join("main.js"), "function after() { return 2; }").unwrap();
        git(&["add", "main.js"]);
        git(&["commit", "-q", "-m", "update"]);
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let pagerank_path = crate::sidecar_path(&db_path, ".pagerank.json");
        fs::write(&pagerank_path, r#"{"stale":1.0}"#).unwrap();
        store.load_pagerank_cache(&pagerank_path).unwrap();
        let generation_before = store.graph_generation();
        drop(store);

        let error = incremental_index_with_name_and_io(
            &repo,
            &db_path,
            "test",
            repo_url,
            None,
            &InjectedIndexEpilogueIo {
                fail_generation: true,
                fail_compute: true,
                ..Default::default()
            },
        )
        .expect_err("the injected incremental compute failure must be returned");

        let message = error.to_string();
        assert!(message.contains("generation-persistence"));
        assert!(
            !message.contains("pagerank-compute"),
            "PageRank must not run before generation publication is clean"
        );
        let store = GraphStore::open_or_create(&db_path).unwrap();
        assert!(
            store
                .symbols_in_file("main.js")
                .unwrap()
                .iter()
                .any(|symbol| symbol.name == "after"),
            "the incremental transaction must be committed before PageRank fails"
        );
        assert!(!store.pagerank_scores().contains_key("stale"));
        assert!(!pagerank_path.exists());
        assert!(store.graph_generation() > generation_before);
    }

    #[test]
    fn fallback_generation_failure_skips_dirty_pagerank_compute() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let db_path = dir.path().join("test.lbug");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("old.js"), "function oldVersion() { return 1; }").unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?} failed");
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "NestWeaver Test"]);
        git(&["add", "old.js"]);
        git(&["commit", "-q", "-m", "initial"]);
        let old_sha = git(&["rev-parse", "HEAD"]);
        let repo_url = "https://example.com/fallback-epilogue";
        index_directory(&repo, &db_path, "test", repo_url, &old_sha).unwrap();

        fs::remove_file(repo.join("old.js")).unwrap();
        fs::write(repo.join("new.js"), "function replacement() { return 2; }").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "--amend", "--no-edit", "-q"]);
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let pagerank_path = crate::sidecar_path(&db_path, ".pagerank.json");
        fs::write(&pagerank_path, r#"{"stale":1.0}"#).unwrap();
        store.load_pagerank_cache(&pagerank_path).unwrap();
        let generation_before = store.graph_generation();
        drop(store);

        let error = incremental_index_with_name_and_io(
            &repo,
            &db_path,
            "test",
            repo_url,
            None,
            &InjectedIndexEpilogueIo {
                fail_generation: true,
                fail_compute: true,
                ..Default::default()
            },
        )
        .expect_err("the injected fallback compute failure must be returned");

        let message = error.to_string();
        assert!(message.contains("generation-persistence"));
        assert!(
            !message.contains("pagerank-compute"),
            "PageRank must not run before generation publication is clean"
        );
        let store = GraphStore::open_or_create(&db_path).unwrap();
        assert!(store.symbols_in_file("old.js").unwrap().is_empty());
        assert!(
            store
                .symbols_in_file("new.js")
                .unwrap()
                .iter()
                .any(|symbol| symbol.name == "replacement"),
            "the forced fallback must atomically install the replacement graph"
        );
        assert!(!store.pagerank_scores().contains_key("stale"));
        assert!(!pagerank_path.exists());
        assert!(store.graph_generation() > generation_before);
    }

    #[test]
    fn tiered_check_new_file_returns_changed() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("hello.js");
        fs::write(&file_path, "function hello() {}").unwrap();

        let reader = crate::content_reader::FilesystemReader::new(dir.path());
        let cache = FileMetaCache::new();
        match tiered_change_check(&reader, "hello.js", &cache).unwrap() {
            ChangeVerdict::Changed {
                source,
                content_hash,
                meta,
            } => {
                assert!(source.contains("function hello"));
                assert!(!content_hash.is_empty());
                assert!(meta.size_bytes > 0);
                assert!(meta.mtime_secs > 0);
            }
            ChangeVerdict::Unchanged => panic!("expected Changed for new file"),
        }
    }

    #[test]
    fn tiered_check_unchanged_mtime_returns_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("hello.js");
        let content = "function hello() {}";
        fs::write(&file_path, content).unwrap();

        let fs_meta = fs::metadata(&file_path).unwrap();
        let mtime_secs = fs_meta
            .modified()
            .unwrap()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let mut cache = FileMetaCache::new();
        cache.insert(
            "hello.js".to_string(),
            CachedFileMeta {
                mtime_secs,
                size_bytes: fs_meta.len(),
                content_hash: content_hash_hex(content),
            },
        );

        let reader = crate::content_reader::FilesystemReader::new(dir.path());
        match tiered_change_check(&reader, "hello.js", &cache).unwrap() {
            ChangeVerdict::Unchanged => {} // expected
            ChangeVerdict::Changed { .. } => panic!("expected Unchanged for same mtime"),
        }
    }

    #[test]
    fn tiered_check_same_size_different_mtime_falls_through_to_hash() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("hello.js");
        let content = "function hello() {}";
        fs::write(&file_path, content).unwrap();

        let fs_meta = fs::metadata(&file_path).unwrap();

        // Cache has a different mtime but same size and same hash.
        // Tier 2 falls through to Tier 3 hash check, which finds content unchanged.
        let mut cache = FileMetaCache::new();
        cache.insert(
            "hello.js".to_string(),
            CachedFileMeta {
                mtime_secs: 1, // different from actual mtime
                size_bytes: fs_meta.len(),
                content_hash: content_hash_hex(content),
            },
        );

        let reader = crate::content_reader::FilesystemReader::new(dir.path());
        match tiered_change_check(&reader, "hello.js", &cache).unwrap() {
            ChangeVerdict::Unchanged => {} // expected — hash matches, so unchanged
            ChangeVerdict::Changed { .. } => panic!("expected Unchanged when hash matches"),
        }

        // Now test with same size but different content hash — should be Changed.
        let mut cache2 = FileMetaCache::new();
        cache2.insert(
            "hello.js".to_string(),
            CachedFileMeta {
                mtime_secs: 1,
                size_bytes: fs_meta.len(),
                content_hash: content_hash_hex("different content!"),
            },
        );

        match tiered_change_check(&reader, "hello.js", &cache2).unwrap() {
            ChangeVerdict::Changed { .. } => {} // expected — hash differs
            ChangeVerdict::Unchanged => {
                panic!("expected Changed when hash differs despite same size")
            }
        }
    }

    #[test]
    fn tiered_check_different_size_different_hash_returns_changed() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("hello.js");
        let new_content = "function hello() { return 42; }";
        fs::write(&file_path, new_content).unwrap();

        // Cache has a different size and different content hash.
        let mut cache = FileMetaCache::new();
        cache.insert(
            "hello.js".to_string(),
            CachedFileMeta {
                mtime_secs: 1,
                size_bytes: 5, // clearly different from actual file
                content_hash: content_hash_hex("old content"),
            },
        );

        let reader = crate::content_reader::FilesystemReader::new(dir.path());
        match tiered_change_check(&reader, "hello.js", &cache).unwrap() {
            ChangeVerdict::Changed {
                source,
                content_hash,
                ..
            } => {
                assert!(source.contains("return 42"));
                assert_eq!(content_hash, content_hash_hex(new_content));
            }
            ChangeVerdict::Unchanged => panic!("expected Changed for different-size file"),
        }
    }

    #[test]
    fn filemeta_sidecar_v2_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.lbug.filemeta.json");
        let mut sidecar = FileMetaSidecar::default();
        sidecar
            .repos
            .entry("repo:test:aaaa".into())
            .or_default()
            .insert(
                "main.js".into(),
                CachedFileMeta {
                    mtime_secs: 1,
                    size_bytes: 2,
                    content_hash: "h1".into(),
                },
            );
        save_filemeta_sidecar(&sidecar, &path).unwrap();
        let loaded = load_filemeta_sidecar(&path);
        assert_eq!(loaded.version, FILEMETA_VERSION);
        assert_eq!(loaded.repos["repo:test:aaaa"]["main.js"].content_hash, "h1");
    }

    #[test]
    fn filemeta_sidecar_legacy_flat_format_loads_empty() {
        // Old flat format must fail-open to empty → one-time full re-index.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.lbug.filemeta.json");
        fs::write(
            &path,
            r#"{"main.js":{"mtime_secs":5,"size_bytes":10,"content_hash":"abc"}}"#,
        )
        .unwrap();
        let loaded = load_filemeta_sidecar(&path);
        assert!(
            loaded.repos.is_empty(),
            "legacy format must load as empty, got {loaded:?}"
        );
    }

    #[test]
    fn filemeta_sidecar_corrupt_and_missing_load_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.lbug.filemeta.json");
        assert!(load_filemeta_sidecar(&path).repos.is_empty()); // missing
        fs::write(&path, "not json").unwrap();
        assert!(load_filemeta_sidecar(&path).repos.is_empty()); // corrupt
    }

    #[test]
    fn filemeta_sidecar_future_version_loads_empty() {
        // A version we don't know (e.g. written by a newer binary) must
        // fail-open to empty — full re-index, never a mis-classification.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.lbug.filemeta.json");
        fs::write(
            &path,
            r#"{"version":3,"repos":{"repo:test:aaaa":{"main.js":{"mtime_secs":1,"size_bytes":2,"content_hash":"h1"}}}}"#,
        )
        .unwrap();
        let loaded = load_filemeta_sidecar(&path);
        assert!(
            loaded.repos.is_empty(),
            "future version must load as empty, got {loaded:?}"
        );
    }

    #[test]
    fn filemeta_sidecar_empty_repos_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.lbug.filemeta.json");
        let sidecar = FileMetaSidecar::default();
        save_filemeta_sidecar(&sidecar, &path).unwrap();
        // Assert on the serialized bytes, not just the round-tripped struct:
        // without the `impl Default` version pin, save would write
        // `"version":0`, load would see the mismatch and fail-open to
        // default() — silently satisfying the version/empty asserts below.
        // Pinning the check to the on-disk bytes catches that regression.
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains(r#""version":2"#),
            "a default-constructed sidecar must serialize with the current version"
        );
        let loaded = load_filemeta_sidecar(&path);
        assert_eq!(loaded.version, FILEMETA_VERSION);
        assert!(loaded.repos.is_empty());
    }

    struct GateOrderReader {
        root: PathBuf,
        read_seen: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl crate::content_reader::ContentReader for GateOrderReader {
        fn read_file(&self, _rel_path: &Path) -> Result<String, anyhow::Error> {
            self.read_seen
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok("pub fn gate_order() {}".to_string())
        }

        fn list_files(&self) -> Result<Vec<PathBuf>, anyhow::Error> {
            Ok(vec![PathBuf::from("src/lib.rs")])
        }

        fn file_meta(&self, _rel_path: &Path) -> Result<Option<(u64, u64)>, anyhow::Error> {
            Ok(None)
        }

        fn root(&self) -> &Path {
            &self.root
        }

        fn version_id(&self) -> &str {
            "test"
        }
    }

    #[test]
    fn reader_write_gate_is_acquired_after_parse_phase() {
        let tmp = tempfile::tempdir().unwrap();
        let read_seen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader = GateOrderReader {
            root: tmp.path().to_path_buf(),
            read_seen: read_seen.clone(),
        };
        let store = GraphStore::in_memory().unwrap();

        index_with_reader_and_write_gate(
            &reader,
            &store,
            "test",
            "file://gate-order",
            "abc",
            None,
            None,
            || {
                assert!(
                    read_seen.load(std::sync::atomic::Ordering::SeqCst),
                    "write gate should not be acquired until after files are read and parsed"
                );
                Ok::<_, anyhow::Error>(())
            },
        )
        .unwrap();
    }

    #[test]
    fn index_directory_creates_filemeta_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        let db_path = dir.path().join("test.lbug");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("main.js"), "function test() {}").unwrap();

        index_directory(&src, &db_path, "test", "https://example.com/repo", "abc123").unwrap();

        let filemeta_path = crate::sidecar_path(&db_path, ".filemeta.json");
        assert!(filemeta_path.exists(), "filemeta sidecar should be created");
        let sidecar = load_filemeta_sidecar(&filemeta_path);
        let uid = repo_uid("test", "https://example.com/repo");
        assert_eq!(sidecar.repos.len(), 1, "one repo slice should exist");
        assert!(
            sidecar.repos[&uid].contains_key("main.js"),
            "repo slice should contain main.js, got {sidecar:?}"
        );
    }

    #[test]
    fn second_index_skips_unchanged_files() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        let db_path = dir.path().join("test.lbug");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("main.js"), "function test() {}").unwrap();

        // First index — all files are new.
        let result1 =
            index_directory(&src, &db_path, "test", "https://example.com/repo", "abc123").unwrap();
        assert_eq!(result1.files_count, 1);
        assert_eq!(result1.files_unchanged, 0);

        // Second index — file is unchanged, should be skipped.
        let result2 =
            index_directory(&src, &db_path, "test", "https://example.com/repo", "abc123").unwrap();
        assert_eq!(
            result2.files_unchanged, 1,
            "unchanged file should be skipped"
        );
        // files_count tracks only files that were actually processed (parsed).
        assert_eq!(result2.files_count, 0, "no files should be re-indexed");
    }

    #[test]
    fn full_index_writes_pagerank_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("t.lbug");
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        fs::write(
            repo.join("main.js"),
            "function f() { g(); }\nfunction g() {}",
        )
        .unwrap();
        index_directory(&repo, &db_path, "test", "https://example.com/a", "sha").unwrap();
        let sidecar = dir.path().join("t.lbug.pagerank.json");
        assert!(sidecar.exists(), "full index must compute+save pagerank");
        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&sidecar).unwrap()).unwrap();
        assert!(
            json.as_object().map(|o| !o.is_empty()).unwrap_or(false),
            "sidecar must contain scores"
        );
    }

    #[test]
    fn first_index_of_new_repo_via_fallback_writes_pagerank_sidecar() {
        // nw-029: the first `nestweaver index` of a new repo routes through
        // full_index_fallback, which (pre-fix) never computed/saved the pagerank
        // sidecar — leaving the most common case cold.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("t.lbug");
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        fs::write(
            repo.join("main.js"),
            "function f() { g(); }\nfunction g() {}",
        )
        .unwrap();
        // drive the plain/incremental entry that falls back to full_index_fallback
        // for a repo with no prior index (non-git dir → full fallback).
        incremental_index_with_name(&repo, &db_path, "test", "https://example.com/a", None)
            .unwrap();
        let sidecar = dir.path().join("t.lbug.pagerank.json");
        assert!(
            sidecar.exists(),
            "first index via fallback must compute+save pagerank"
        );
        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&sidecar).unwrap()).unwrap();
        assert!(
            json.as_object().map(|o| !o.is_empty()).unwrap_or(false),
            "sidecar must contain scores"
        );
    }

    #[test]
    fn second_repo_with_colliding_rel_path_and_mtime_is_indexed() {
        // nw-022 repro: two repos share ONE db. Their files share a rel path
        // ("main.js") and an mtime second. Tier 1 (tiered_change_check) matches
        // repo A's sidecar entry when repo B is first indexed → repo B's file
        // is classified Unchanged and its symbols are never written.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("shared.lbug");
        let repo_a = dir.path().join("repo_a");
        let repo_b = dir.path().join("repo_b");
        fs::create_dir_all(&repo_a).unwrap();
        fs::create_dir_all(&repo_b).unwrap();
        fs::write(repo_a.join("main.js"), "function alpha() {}").unwrap();
        fs::write(repo_b.join("main.js"), "function beta() {}").unwrap();

        // Pin identical mtimes (don't rely on same-second scheduling).
        let t = std::time::SystemTime::now() - std::time::Duration::from_secs(10);
        fs::File::options()
            .write(true)
            .open(repo_a.join("main.js"))
            .unwrap()
            .set_modified(t)
            .unwrap();
        fs::File::options()
            .write(true)
            .open(repo_b.join("main.js"))
            .unwrap()
            .set_modified(t)
            .unwrap();

        let r1 =
            index_directory(&repo_a, &db_path, "test", "https://example.com/a", "sha").unwrap();
        assert_eq!(r1.files_count, 1);

        let r2 =
            index_directory(&repo_b, &db_path, "test", "https://example.com/b", "sha").unwrap();
        assert_eq!(
            r2.files_unchanged, 0,
            "repo B must not inherit repo A's filemeta entry"
        );
        assert_eq!(r2.files_count, 1);

        let store = GraphStore::open_or_create(&db_path).unwrap();
        let uid_b = repo_uid("test", "https://example.com/b");
        let symbols = store.symbol_names_by_repo(&uid_b).unwrap();
        assert!(
            symbols.iter().any(|n| n == "beta"),
            "repo B's symbol must exist in the shared DB, got {symbols:?}"
        );
        assert!(
            !store.list_files_by_repo(&uid_b).unwrap().is_empty(),
            "repo B must have File nodes"
        );
    }

    #[test]
    fn indexing_second_repo_preserves_first_repos_filemeta() {
        // nw-022 second defect: the save site overwrites the whole sidecar with
        // only the current repo's entries, destroying repo A's warm cache.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("shared.lbug");
        let repo_a = dir.path().join("repo_a");
        let repo_b = dir.path().join("repo_b");
        fs::create_dir_all(&repo_a).unwrap();
        fs::create_dir_all(&repo_b).unwrap();
        fs::write(repo_a.join("alpha.js"), "function alpha() {}").unwrap();
        fs::write(repo_b.join("beta.js"), "function beta() {}").unwrap();

        index_directory(&repo_a, &db_path, "test", "https://example.com/a", "sha").unwrap();
        index_directory(&repo_b, &db_path, "test", "https://example.com/b", "sha").unwrap();

        // Re-index repo A: it must still see its own warm entries.
        let r3 =
            index_directory(&repo_a, &db_path, "test", "https://example.com/a", "sha").unwrap();
        assert_eq!(
            r3.files_unchanged, 1,
            "repo A's warm cache must survive repo B's index (sidecar must not be overwritten)"
        );
        assert_eq!(r3.files_count, 0);
    }

    #[test]
    fn full_index_fallback_uses_per_repo_slice_and_merge_saves() {
        // nw-022 T4: same two-repo collision shape as
        // `second_repo_with_colliding_rel_path_and_mtime_is_indexed`, but driven
        // through the incremental entry point — a non-git dir routes to
        // full_index_fallback, which must also key change detection per repo.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("shared.lbug");
        let repo_a = dir.path().join("repo_a");
        let repo_b = dir.path().join("repo_b");
        fs::create_dir_all(&repo_a).unwrap();
        fs::create_dir_all(&repo_b).unwrap();
        fs::write(repo_a.join("main.js"), "function alpha() {}").unwrap();
        fs::write(repo_b.join("main.js"), "function beta() {}").unwrap();

        // Pin identical mtimes (don't rely on same-second scheduling).
        let t = std::time::SystemTime::now() - std::time::Duration::from_secs(10);
        for p in [repo_a.join("main.js"), repo_b.join("main.js")] {
            fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(t)
                .unwrap();
        }

        let r1 =
            incremental_index_with_name(&repo_a, &db_path, "test", "https://example.com/a", None)
                .unwrap();
        assert!(r1.fell_back_to_full, "non-git dir must fall back to full");
        assert!(r1.symbols_added >= 1, "repo A must be indexed");

        // IncrementalResult carries no files_unchanged; a cross-match would
        // classify repo B's only file Unchanged and never write its symbols,
        // so assert on symbols_added + the store contents instead.
        let r2 =
            incremental_index_with_name(&repo_b, &db_path, "test", "https://example.com/b", None)
                .unwrap();
        assert!(r2.fell_back_to_full);
        assert!(
            r2.symbols_added >= 1,
            "fallback path must not cross-match repo A's filemeta entries"
        );

        let store = GraphStore::open_or_create(&db_path).unwrap();
        let uid_b = repo_uid("test", "https://example.com/b");
        let symbols = store.symbol_names_by_repo(&uid_b).unwrap();
        assert!(
            symbols.iter().any(|n| n == "beta"),
            "repo B's symbol must exist in the shared DB, got {symbols:?}"
        );
        assert!(
            !store.list_files_by_repo(&uid_b).unwrap().is_empty(),
            "repo B must have File nodes"
        );

        // Merge-save: repo A's slice must survive repo B's fallback run.
        let filemeta_path = crate::sidecar_path(&db_path, ".filemeta.json");
        let sidecar = load_filemeta_sidecar(&filemeta_path);
        let uid_a = repo_uid("test", "https://example.com/a");
        assert!(
            sidecar.repos.contains_key(&uid_a) && sidecar.repos.contains_key(&uid_b),
            "both repo slices must be present after the fallback merge-save, got {:?}",
            sidecar.repos.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn full_index_fallback_eviction_uses_cross_repo_unions() {
        // nw-022 T4, the eviction property (the graph-loss / nw-010 class):
        // full_index_fallback feeds `parsed_cache.retain_hashes(...)` the
        // cross-repo union of live hashes from merge_save_filemeta. If it were
        // fed only THIS run's hashes (the pre-fix behavior), indexing repo B
        // through the fallback path would evict repo A's parsed-cache entries.
        // This test reverts to RED if the eviction block is fed current-run-only
        // hashes/files instead of `unions.live_hashes` / `unions.live_files`.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("shared.lbug");
        let repo_a = dir.path().join("repo_a");
        let repo_b = dir.path().join("repo_b");
        fs::create_dir_all(&repo_a).unwrap();
        fs::create_dir_all(&repo_b).unwrap();
        // Distinct files + distinct content → distinct content hashes.
        fs::write(repo_a.join("alpha.js"), "function alpha() { return 1; }").unwrap();
        fs::write(repo_b.join("beta.js"), "function beta() { return 2; }").unwrap();

        // Index repo A through the incremental entry point (non-git → fallback).
        let ra =
            incremental_index_with_name(&repo_a, &db_path, "test", "https://example.com/a", None)
                .unwrap();
        assert!(ra.fell_back_to_full, "non-git dir must fall back to full");

        // Recover repo A's live content hash from its filemeta slice (the same
        // hash the fallback path keys the parsed cache by) and confirm the
        // parsed-cache entry exists after A's run.
        let filemeta_path = crate::sidecar_path(&db_path, ".filemeta.json");
        let uid_a = repo_uid("test", "https://example.com/a");
        let hash_a = load_filemeta_sidecar(&filemeta_path)
            .repos
            .get(&uid_a)
            .and_then(|slice| slice.get("alpha.js"))
            .map(|m| m.content_hash.clone())
            .expect("repo A's filemeta slice must record alpha.js");

        let parsed_cache_path = crate::sidecar_path(&db_path, ".parsed_cache.bin");
        assert!(
            crate::parsed_cache::ParsedCache::load(&parsed_cache_path)
                .get(&hash_a)
                .is_some(),
            "repo A's parsed-cache entry must exist after its own fallback run"
        );

        // Index repo B into the SAME db through the fallback path.
        let rb =
            incremental_index_with_name(&repo_b, &db_path, "test", "https://example.com/b", None)
                .unwrap();
        assert!(rb.fell_back_to_full);

        // The eviction must be union-scoped: repo A's entry survives.
        assert!(
            crate::parsed_cache::ParsedCache::load(&parsed_cache_path)
                .get(&hash_a)
                .is_some(),
            "repo A's parsed-cache entry must survive repo B's fallback run"
        );
    }

    #[test]
    fn reidentify_drops_old_uid_filemeta_slice() {
        // nw-022 T6, PRIMARY path (index_directory): re-indexing a working
        // tree under its origin identity must drop the legacy file:// uid's
        // filemeta slice from the sidecar alongside the graph prune.
        //
        // Naming note: this is the spec-named test, but the primary-path
        // drop_uids threading it guards already shipped in the PARENT commit
        // (b7a0661e). Reverting THIS commit's fallback threading does NOT turn
        // this test red — the fallback deliverable is guarded by
        // `reidentify_drops_old_uid_filemeta_slice_fallback` below.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        let db_path = dir.path().join("test.lbug");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("main.js"), "function a() { return 1; }\n").unwrap();

        let local_root = src.display().to_string();
        let file_url = format!("file://{local_root}");
        index_directory(&src, &db_path, "test", &file_url, "sha-1").unwrap();

        let old_uid = repo_uid("test", &file_url);
        let filemeta_path = crate::sidecar_path(&db_path, ".filemeta.json");
        assert!(
            load_filemeta_sidecar(&filemeta_path)
                .repos
                .contains_key(&old_uid),
            "first pass must record the file:// slice"
        );

        let origin_url = "https://example.com/acme/demo.git";
        index_directory(&src, &db_path, "test", origin_url, "sha-1").unwrap();

        let new_uid = repo_uid("test", origin_url);
        let sidecar = load_filemeta_sidecar(&filemeta_path);
        assert!(
            !sidecar.repos.contains_key(&old_uid),
            "re-identify must drop the legacy uid's slice, got {:?}",
            sidecar.repos.keys().collect::<Vec<_>>()
        );
        assert!(
            sidecar.repos.contains_key(&new_uid),
            "the new origin uid must have a slice"
        );
    }

    #[test]
    fn reidentify_drops_old_uid_filemeta_slice_fallback() {
        // nw-022 T6, FALLBACK path: the same re-identify hand-off driven
        // through the incremental entry point (non-git dir →
        // full_index_fallback) must also drop the legacy uid's slice.
        //
        // Naming note: this is the real T6 deliverable of THIS commit. It is
        // the ONLY test that turns red if full_index_fallback's `drop_uids`
        // threading (reidentified_old_uid → merge_save_filemeta) is reverted;
        // the spec-named `reidentify_drops_old_uid_filemeta_slice` above only
        // exercises the parent commit's already-fixed primary path.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        let db_path = dir.path().join("test.lbug");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("main.js"), "function a() { return 1; }\n").unwrap();

        let local_root = src.display().to_string();
        let file_url = format!("file://{local_root}");
        let r1 = incremental_index_with_name(&src, &db_path, "test", &file_url, None).unwrap();
        assert!(r1.fell_back_to_full, "non-git dir must fall back to full");

        let old_uid = repo_uid("test", &file_url);
        let filemeta_path = crate::sidecar_path(&db_path, ".filemeta.json");
        assert!(
            load_filemeta_sidecar(&filemeta_path)
                .repos
                .contains_key(&old_uid),
            "first pass must record the file:// slice"
        );

        let origin_url = "https://example.com/acme/demo.git";
        let r2 = incremental_index_with_name(&src, &db_path, "test", origin_url, None).unwrap();
        assert!(r2.fell_back_to_full);

        let new_uid = repo_uid("test", origin_url);
        let sidecar = load_filemeta_sidecar(&filemeta_path);
        assert!(
            !sidecar.repos.contains_key(&old_uid),
            "fallback re-identify must drop the legacy uid's slice, got {:?}",
            sidecar.repos.keys().collect::<Vec<_>>()
        );
        assert!(
            sidecar.repos.contains_key(&new_uid),
            "the new origin uid must have a slice"
        );
    }

    #[test]
    fn reindex_prunes_removed_files() {
        // nw-009 Fix #1 regression: when a file is removed between indexes (e.g.
        // a force-push that drops it), the incremental cleanup branch only
        // deletes files being re-inserted. Without the present_files prune pass,
        // the removed file's File/Symbol nodes linger. This test exercises the
        // local full path (db_path → filemeta sidecar → files_unchanged > 0),
        // which is the actively-buggy path pre-Fix#1.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        let db_path = dir.path().join("test.lbug");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("main.js"), "function main() {}").unwrap();
        fs::write(src.join("helper.js"), "function helper() {}").unwrap();

        let url = "https://example.com/repo";
        let r_uid = repo_uid("test", url);

        // First index — both files present.
        let result1 = index_directory(&src, &db_path, "test", url, "abc123").unwrap();
        assert_eq!(result1.files_count, 2);

        {
            let store = GraphStore::open_or_create(&db_path).unwrap();
            let files = store.list_files_by_repo(&r_uid).unwrap();
            assert_eq!(files.len(), 2, "both files should be indexed");
            assert!(
                !store.symbols_in_file("helper.js").unwrap().is_empty(),
                "helper.js should have symbols after first index"
            );
        }

        // Remove helper.js, then re-index. main.js is unchanged, so the
        // incremental cleanup branch runs (files_unchanged > 0).
        fs::remove_file(src.join("helper.js")).unwrap();
        let _result2 = index_directory(&src, &db_path, "test", url, "def456").unwrap();

        let store = GraphStore::open_or_create(&db_path).unwrap();
        let files = store.list_files_by_repo(&r_uid).unwrap();
        let paths: Vec<&str> = files.iter().map(|(_, p)| p.as_str()).collect();
        assert!(
            !paths.contains(&"helper.js"),
            "removed helper.js File node should be pruned, got {paths:?}"
        );
        assert!(
            paths.contains(&"main.js"),
            "still-present main.js File node must remain, got {paths:?}"
        );
        assert!(
            store.symbols_in_file("helper.js").unwrap().is_empty(),
            "removed helper.js symbols should be pruned"
        );
    }

    #[test]
    fn index_emits_member_of_edges_for_rust_struct_methods() {
        // Task 4: MEMBER_OF edges must be emitted from methods to their parent
        // struct (which the parser classifies as SymbolKind::Class via the impl
        // block).
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("lib.rs"),
            r#"
pub struct Counter {
    value: u32,
}

impl Counter {
    pub fn new() -> Self {
        Counter { value: 0 }
    }

    pub fn increment(&mut self) {
        self.value += 1;
    }
}
"#,
        )
        .unwrap();

        let (_result, store) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();

        let all_edges = store.load_typed_edges().unwrap();
        let member_of: Vec<_> = all_edges
            .iter()
            .filter(|(_, _, edge_type, _, _)| edge_type == "MEMBER_OF")
            .collect();

        assert!(
            member_of.len() >= 2,
            "expected at least 2 MEMBER_OF edges (new + increment), got {}: {member_of:?}",
            member_of.len()
        );
    }

    #[test]
    fn parsed_cache_avoids_reparse() {
        // Cold index: parses all files and populates the parsed cache sidecar.
        // Warm index: files are unchanged, but their symbols should still be
        // available from the parsed cache (no re-parse needed).
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        fs::write(
            repo.join("main.ts"),
            "export function hello() { return 1; }\nexport function world() { return 2; }\n",
        )
        .unwrap();
        let db = tmp.path().join("test.lbug");

        // Cold index — symbols should be parsed.
        let r1 =
            index_directory_with_options(&repo, &db, "t", "file:///t", "HEAD", true, None).unwrap();
        assert!(
            r1.symbols_count > 0,
            "cold index should find symbols, got {}",
            r1.symbols_count
        );

        // Verify the parsed cache sidecar was created.
        let parsed_cache_path = crate::sidecar_path(&db, ".parsed_cache.bin");
        assert!(
            parsed_cache_path.exists(),
            "parsed cache sidecar should exist after cold index"
        );

        // Warm index — all files unchanged, symbols loaded from parsed cache.
        let r2 = index_directory_with_options(&repo, &db, "t", "file:///t", "HEAD", false, None)
            .unwrap();
        assert_eq!(
            r2.symbols_count, r1.symbols_count,
            "warm index should have same symbol count as cold index (from parsed cache)"
        );
    }
}
