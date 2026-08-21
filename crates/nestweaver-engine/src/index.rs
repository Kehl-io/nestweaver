use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Context;
use indicatif::{ProgressBar, ProgressStyle};
use nestweaver_parser::{
    AstTypeBinding, RawReference, RawSymbol, SkipReasonCode, SkippedFile, detect_language,
    parse_source,
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
type ContractDerivationInputs = (
    Vec<PathBuf>,
    Vec<HandlerFileData>,
    Vec<nestweaver_schema::Symbol>,
    Vec<SkippedFile>,
);
pub(crate) type PreparedFileData = HashMap<String, (Vec<RawSymbol>, Vec<RawReference>)>;

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
///
/// Not `Serialize`: no surface emits an `IndexResult` as JSON — the CLI and the
/// daemon both destructure it into their own progress/report types — so a serde
/// derive here would be published API surface nobody consumes. `Debug` is
/// derived because the contract-status fields are asserted on in tests.
#[derive(Debug, Clone)]
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
    /// Contract nodes written by the contract phase. `0` on a repo with no
    /// specs and no route handlers — and also `0` when derivation failed, which
    /// is exactly why `contracts_status` exists alongside it.
    pub contracts_derived: usize,
    /// Whether the contract phase ran to completion. Reuses the blast-radius
    /// trust vocabulary: [`AnalysisStatus::Complete`] or
    /// [`AnalysisStatus::Degraded`]. Contract derivation is best-effort and
    /// never fails the index, so this is the ONLY signal a caller has that the
    /// repo's contract graph is missing rather than empty.
    pub contracts_status: crate::blast_radius::AnalysisStatus,
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
///
/// Paired with `resolution_cache::CACHE_VERSION`: bumping one requires
/// bumping the other (both sidecars must be invalidated together), enforced
/// by `resolution_cache::tests::cache_version_moves_with_filemeta_version`.
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
    reconcile_deleted_graph_state_with_io(store, db_path, &FileSystemDeletionReconciliationIo, None)
}

fn reconcile_deleted_graph_state_with_io(
    store: &GraphStore,
    db_path: &Path,
    io: &dyn DeletionReconciliationIo,
    manifests_before_generation_advance: Option<
        Result<std::collections::HashMap<String, crate::manifest::ManifestInfo>, anyhow::Error>,
    >,
) -> DeletedGraphStateReconciliation {
    let manifests_removed = (|| -> Result<usize, anyhow::Error> {
        let manifests_path = crate::manifest::manifest_cache_path(db_path);
        let mut manifests = match manifests_before_generation_advance {
            Some(loaded) => loaded?,
            None => crate::manifest::load_manifest_cache_for_db(store, db_path)?,
        };
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
        // The envelope is graph-generation-bound. Even when every Repo row
        // survives a partial child deletion, republish the unchanged payload
        // at the new generation or the next reader must reject it as stale.
        crate::manifest::save_manifest_cache_for_db(&manifests, store, db_path)?;
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

    // Read the incumbent manifest while it still matches generation N. The
    // graph mutation is already committed, but the generation transition is
    // the boundary that makes the old envelope stale; reconciliation below
    // filters this payload against the authoritative post-mutation live set
    // and republishes it at N+1.
    let manifests_before_generation_advance =
        crate::manifest::load_manifest_cache_for_db(store, db_path);

    // Establish the generation that every reconciled artifact describes
    // BEFORE writing those artifacts. The previous ordering saved manifests
    // at N and then advanced the graph to N+1, making a freshly written,
    // identity-bound artifact stale immediately. Advancing in memory first is
    // also the existing fail-safe rule: if later persistence fails, live
    // readers see N+1 and a reopen sees the older durable generation, so the
    // N+1 artifact is rejected rather than trusted against an unknown graph.
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

    let reconciliation = reconcile_deleted_graph_state_with_io(
        store,
        db_path,
        io,
        Some(manifests_before_generation_advance),
    );
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
    /// Rewrite an already-established marker with a reason field, so a
    /// publication left dirty ON PURPOSE is distinguishable from one abandoned
    /// by a crash. Defaulted so alternate `IndexEpilogueIo` implementations
    /// (test doubles) inherit the behaviour of the one they wrap.
    fn stamp_marker_reason(&self, path: &Path, reason: &str) -> Result<(), anyhow::Error> {
        FileSystemIndexEpilogueIo.stamp_marker_reason_impl(path, reason)
    }
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
        lease: &nestweaver_store::IndexPublicationLease<'_>,
        scope: &nestweaver_store::GraphScope,
    ) -> Result<(), anyhow::Error>;
    fn save_pagerank(
        &self,
        lease: &nestweaver_store::IndexPublicationLease<'_>,
        path: &Path,
    ) -> Result<(), anyhow::Error>;
}

pub(crate) struct FileSystemIndexEpilogueIo;

impl FileSystemIndexEpilogueIo {
    /// Durably (re)write the marker payload. Shared by `establish_marker` and
    /// `stamp_marker_reason`: the ordering — write, `sync_all`, fsync the
    /// parent directory — is what makes the marker survive process death, and
    /// it must not be duplicated with drift.
    fn write_marker_payload(&self, path: &Path, reason: Option<&str>) -> Result<(), anyhow::Error> {
        let marker = nestweaver_store::index_publication::format_marker_payload(
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            reason,
        );
        // Temp-file + rename, NOT truncate-in-place. A truncating rewrite makes
        // the marker briefly zero-byte, and a reader landing in that window
        // parses no pid and no timestamp — which the wedged predicate now reads
        // as "unattributable, therefore WEDGED", telling an operator to run
        // `repair --force` against a publication that is perfectly healthy and
        // in flight. The window is microseconds and cannot cause data loss
        // (lbug's write lock stops any forced repair from opening the store),
        // but a rename removes it entirely: a reader sees the old payload or the
        // new one, never a partial one.
        //
        // `atomic_replace_file` keeps the durability this marker depends on —
        // it syncs the temp file and the parent directory, so the marker still
        // survives process death, which is the whole reason it exists.
        nestweaver_store::durable_sidecar::atomic_replace_file(path, |file| {
            file.write_all(marker.as_bytes())
        })
        .with_context(|| format!("publish index publication marker {}", path.display()))
    }

    fn stamp_marker_reason_impl(&self, path: &Path, reason: &str) -> Result<(), anyhow::Error> {
        self.write_marker_payload(path, Some(reason))
    }
}

impl IndexEpilogueIo for FileSystemIndexEpilogueIo {
    fn establish_marker(&self, path: &Path) -> Result<(), anyhow::Error> {
        self.write_marker_payload(path, None)
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
        lease: &nestweaver_store::IndexPublicationLease<'_>,
        scope: &nestweaver_store::GraphScope,
    ) -> Result<(), anyhow::Error> {
        lease.compute_pagerank(0.85, 20, scope).map_err(Into::into)
    }

    fn save_pagerank(
        &self,
        lease: &nestweaver_store::IndexPublicationLease<'_>,
        path: &Path,
    ) -> Result<(), anyhow::Error> {
        lease.save_pagerank(path)?;
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
    establish_index_publication_marker_on_lease(lease, db_path, operation, io)
}

/// The marker-establishment half of [`establish_index_publication_marker_with_io`],
/// for callers that already hold the lease.
///
/// Abandoned-publication recovery acquires the lease NON-blockingly (a lease
/// already owned in-process means the publication is not abandoned) and must
/// repair a fail-closed generation base before preflight can succeed, so it
/// cannot use the acquire-then-establish entry point.
pub(crate) fn establish_index_publication_marker_on_lease<'a>(
    lease: nestweaver_store::IndexPublicationLease<'a>,
    db_path: Option<&Path>,
    operation: &str,
    io: &dyn IndexEpilogueIo,
) -> Result<nestweaver_store::IndexPublicationLease<'a>, DeletionReconciliationError> {
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
    finalize_committed_index_for_scope_with_io(lease, db_path, operation, io, scope.as_ref(), true)
}

/// `publish_clean`: when false (a run that committed AFTER cancellation was
/// requested), the file-backed generation advance, `.index-dirty` retirement,
/// and the scoped PageRank refresh are skipped so the publication stays dirty
/// and the next open reconciles it instead of trusting generation/PageRank
/// state that predates this commit. In-memory stores are the exception: they
/// have no `.index-dirty` marker to reconcile on a later open, so their
/// generation bump still runs (see below).
pub(crate) fn finalize_committed_index_for_scope_with_io(
    lease: nestweaver_store::IndexPublicationLease<'_>,
    db_path: Option<&Path>,
    operation: &str,
    io: &dyn IndexEpilogueIo,
    pagerank_scope: Option<&nestweaver_store::GraphScope>,
    publish_clean: bool,
) -> Result<(), DeletionReconciliationError> {
    let mut failures = Vec::new();
    let store = lease.store();

    // ORDERING, deliberate: when this publication is being left dirty ON
    // PURPOSE, record that in the marker payload BEFORE any other finalize
    // I/O, and durably (`sync_all` + parent fsync). This is the earliest point
    // in the shared finalizer, so the window in which a death leaves an
    // unlabelled marker is as narrow as the code permits.
    //
    // THE WINDOW CANNOT BE CLOSED COMPLETELY, and that is not a defect to fix
    // later: a run does not LEARN it was cancelled until it polls the flag,
    // which happens after the commit. There is no earlier instant at which
    // anything could be stamped, because before that poll the run has nothing
    // to stamp.
    //
    // Why abandoned-publication recovery still auto-heals a deliberately-dirty
    // publication, rather than refusing to touch anything that might have been
    // cancelled (nw-C1 / the cancelled-index item's `publish_clean: false`
    // path). Four points, the last decisive:
    //
    //   1. Recovery reconciles ONLY when the recorded writer process is dead,
    //      so it can never act on a live run.
    //   2. It asserts only that the SIDECARS NOW MATCH WHAT WAS COMMITTED. It
    //      never claims the graph is complete. That assertion is equally true
    //      of a crashed run and a cancelled one.
    //   3. The stamp therefore changes the MESSAGE, not the ACTION. A run
    //      killed inside the residual window is reported as a crash — which is
    //      precisely what it is indistinguishable from at every layer,
    //      including before this stamp existed.
    //   4. Refusing to auto-heal "possibly cancelled" publications means
    //      refusing to auto-heal ANY of them, because we cannot tell them
    //      apart. That is exactly the wedge this work exists to end: one
    //      abandoned marker failing every ranked query in the database
    //      forever, with no way out.
    //
    // So the stamp buys honest reporting (a recovered cancelled run repeats
    // the `index --force` guidance, because its graph really may be
    // incomplete) without gating recovery on a distinction the system cannot
    // reliably make.
    //
    // Best-effort by construction: a stamp failure leaves the ordinary
    // `{pid}:{nanos}` payload, which still fails closed and is still
    // recoverable. Turning a labelling failure into a publication failure
    // would be strictly worse.
    if !publish_clean && let Some(db_path) = db_path {
        let marker_path = crate::sidecar_path(db_path, ".index-dirty");
        if let Err(error) = io.stamp_marker_reason(
            &marker_path,
            nestweaver_store::index_publication::MARKER_REASON_CANCELLED,
        ) {
            tracing::warn!(
                "could not record the cancellation reason in {}: {error:#}; \
                 the publication stays dirty and recoverable regardless",
                marker_path.display()
            );
        }
    }

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

    // A cancelled-but-committed run skips the generation advance and the
    // clean-publish/marker retirement below: `.index-dirty` and the reserved
    // generation survive so the next open reconciles this publication as
    // dirty (fail-closed) rather than treating it as a clean commit.
    let mut publication_clean = false;
    if publish_clean {
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

        publication_clean = db_path.is_none();

        // ORDERING, deliberate: when a PageRank refresh is requested
        // (`pagerank_scope.is_some()` — every production caller), the fresh
        // PageRank is computed and its sidecar persisted BEFORE the marker
        // retires, gated on the same preconditions that gate retirement
        // (generation durable, stale sidecar gone). A kill anywhere after
        // this point then leaves either the marker still set — dirty, which
        // the next open reconciles — or a clean publication WITH a sidecar
        // matching the committed graph. The state that must never occur is
        // marker retired + generation advanced + no sidecar: it reports CLEAN
        // while the note side of the graph has no ranks, and the lazy
        // fallback (`ranking.rs` `ensure_pagerank_loaded`) recomputes only
        // `code_only()` and only in memory, so nothing would ever say so.
        //
        // Scope of that guarantee: a refresh-LESS clean publish
        // (`pagerank_scope: None`, `publish_clean: true`) still deletes the
        // stale sidecar and retires the marker with no replacement. No
        // production caller does this (only tests); if one ever does, the
        // clean-without-sidecar state returns by intent, not by race.
        //
        // A refresh failure therefore also blocks retirement — including the
        // owner save failing closed when a reader wiped the fresh cache
        // mid-window (`ranking.rs` `save_pagerank_cache_for_publication_owner`):
        // the publication stays dirty and recoverable instead of reporting
        // clean without ranks. The failure is still returned to the caller
        // either way.
        //
        // The cost is a longer dirty window: ranked queries fail closed for
        // the duration of the compute rather than resuming just before it,
        // which the bounded MCP wait absorbs. In-memory stores have no marker
        // or sidecar to order, so their compute is unchanged.
        let mut pagerank_persisted = true;
        if let Some(scope) =
            pagerank_scope.filter(|_| db_path.is_none() || (generation_durable && pagerank_safe))
        {
            match io.compute_pagerank(&lease, scope) {
                Ok(()) => {
                    if let Some(db_path) = db_path {
                        let pagerank_path = crate::sidecar_path(db_path, ".pagerank.json");
                        match io.save_pagerank(&lease, &pagerank_path) {
                            Ok(()) => {}
                            Err(error) => {
                                // Known residual: under sustained ranked-query
                                // traffic a reader blocked on
                                // `pagerank_compute_lock` during the compute
                                // typically acquires it in the compute→save gap
                                // and wipes the fresh cache, so this fail-closed
                                // save can fail on every retry and keep the
                                // publication dirty (queries error) until
                                // traffic pauses or a restart heals it via
                                // recovery.
                                push_reconciliation_failure(
                                    &mut failures,
                                    DeletionReconciliationStage::PageRankPersistence,
                                    None,
                                    format!("{}: {error:#}", pagerank_path.display()),
                                );
                                invalidate_pagerank_sidecar_with_io(
                                    &pagerank_path,
                                    io,
                                    &mut failures,
                                );
                                pagerank_persisted = false;
                            }
                        }
                    }
                }
                Err(error) => {
                    push_reconciliation_failure(
                        &mut failures,
                        DeletionReconciliationStage::PageRankCompute,
                        None,
                        format!("{error:#}"),
                    );
                    pagerank_persisted = false;
                }
            }
        }

        if generation_durable
            && pagerank_safe
            && pagerank_persisted
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
                // The retirement barrier discards ALL ranking caches so no
                // dirty-window state survives publication — including the
                // ranks this finalizer just computed and persisted. Reload
                // them from the committed sidecar so in-process readers keep
                // the scope the disk now holds; a lazy refill would be
                // `code_only()`, narrower than a unified publication.
                // nw-147: this caller KNOWS the parameters it just computed
                // with (compute_pagerank above uses 0.85 / 20 over this exact
                // scope), so it can verify the sidecar's algorithm fingerprint
                // instead of letting the artifact vouch for itself.
                if let Some(scope) = pagerank_scope
                    && let Err(error) = store.load_pagerank_cache_expecting(
                        &crate::sidecar_path(db_path, ".pagerank.json"),
                        Some(&nestweaver_store::ranking::pagerank_algorithm_fingerprint(
                            0.85, 20, scope,
                        )),
                    )
                {
                    tracing::warn!(
                        "fresh PageRank sidecar could not be reloaded after marker retirement: \
                         {error:#}; the next ranked query recomputes lazily"
                    );
                }
            }
        }
    } else if db_path.is_none() {
        // In-memory stores have no `.index-dirty` marker for a later open to
        // reconcile, so a cancelled-but-committed run would otherwise be
        // invisible to generation-keyed snapshot readers. Bump the in-memory
        // generation even though the file-backed clean-publish steps above
        // stay skipped.
        if let Err(error) = store.try_bump_graph_generation() {
            push_reconciliation_failure(
                &mut failures,
                DeletionReconciliationStage::GenerationPersistence,
                None,
                format!("advance graph generation: {error:#}"),
            );
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

// ── Abandoned-publication recovery (nw-C1) ──────────────────────────────────

/// Why recovery did or did not run. Every "did not" arm names its reason: the
/// operator escape hatch prints these verbatim, and a silent no-op is exactly
/// the failure mode this work exists to remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexPublicationRecovery {
    /// No marker; nothing to do.
    Clean,
    /// The store is in-memory, so there is no marker to reconcile.
    NotFileBacked,
    /// The marker's state could not be determined (permissions, I/O error).
    /// Fails closed: "cannot tell" is not "abandoned".
    Undeterminable { detail: String },
    /// The marker records a writer that is still alive. A live publication is
    /// never recovered out from under its writer.
    WriterAlive { pid: i32 },
    /// The marker records no pid we can attribute (an older binary, a
    /// truncated write, or a hand-created marker). Auto-heal declines; the
    /// operator escape hatch can still be pointed at it explicitly.
    WriterUnattributed,
    /// A live in-process publisher holds the lease, so this publication is
    /// in flight rather than abandoned.
    LeaseHeld,
    /// The publication IS abandoned, but this store is open read-only, so this
    /// caller must report the condition rather than clear it.
    ReadOnlyStore { abandoned_writer_pid: i32 },
    /// Recovery completed: PageRank was recomputed against the committed
    /// graph, `.pagerank.json` and `.generation` were persisted, and only then
    /// was the marker cleared.
    Recovered {
        /// The pid of the writer that abandoned the publication. `None` when
        /// the marker carried no attributable writer and an operator forced
        /// the repair.
        abandoned_writer_pid: Option<i32>,
        /// The canonical generation now persisted.
        generation: u64,
        /// True when the abandoned publication was left dirty deliberately by
        /// a committed-after-cancellation run. The graph may be incomplete;
        /// `nestweaver index --repo <path> --force` is the repair for that,
        /// and it is a different question from this reconciliation.
        was_cancelled_run: bool,
    },
}

impl IndexPublicationRecovery {
    /// True when the marker was cleared.
    pub fn recovered(&self) -> bool {
        matches!(self, IndexPublicationRecovery::Recovered { .. })
    }

    /// A single line suitable for a log or for `nestweaver repair` output.
    pub fn describe(&self) -> String {
        match self {
            IndexPublicationRecovery::Clean => {
                "index publication is clean; nothing to recover".to_string()
            }
            IndexPublicationRecovery::NotFileBacked => {
                "in-memory store has no publication marker to recover".to_string()
            }
            IndexPublicationRecovery::Undeterminable { detail } => format!(
                "index publication marker state could not be determined ({detail}); \
                 failing closed rather than assuming it was abandoned"
            ),
            IndexPublicationRecovery::WriterAlive { pid } => format!(
                "index publication is in flight; writer pid {pid} is alive — not recovering"
            ),
            IndexPublicationRecovery::WriterUnattributed => {
                "index publication marker records no writer pid; not recovering automatically"
                    .to_string()
            }
            IndexPublicationRecovery::LeaseHeld => {
                "another publisher in this process holds the publication lease — not recovering"
                    .to_string()
            }
            IndexPublicationRecovery::ReadOnlyStore {
                abandoned_writer_pid,
            } => format!(
                "index publication was abandoned by dead writer pid {abandoned_writer_pid}, but \
                 this store is open read-only; a writer must perform the repair"
            ),
            IndexPublicationRecovery::Recovered {
                abandoned_writer_pid,
                generation,
                was_cancelled_run,
            } => {
                let who = match abandoned_writer_pid {
                    Some(pid) => format!("dead writer pid {pid}"),
                    None => "an unattributable writer (forced repair)".to_string(),
                };
                let base = format!(
                    "recovered an abandoned index publication left by {who}: stale PageRank \
                     sidecar removed, graph generation {generation} persisted, and PageRank \
                     recomputed against the committed graph and saved, all before the marker \
                     was cleared"
                );
                if *was_cancelled_run {
                    format!(
                        "{base}. That publication was left dirty deliberately by a run that \
                         committed AFTER cancellation, so the graph itself may be incomplete — \
                         run `nestweaver index --repo <path> --force` to rebuild it"
                    )
                } else {
                    base
                }
            }
        }
    }
}

/// Reconcile an abandoned index publication, in the WRITER only.
///
/// The `<db>.index-dirty` marker is durable by design so it survives process
/// death, and while it exists ranked queries fail closed — correctly, since
/// `.pagerank.json` and `.generation` may predate the committed graph. What was
/// missing is the way out. `finalize_committed_index_for_scope_with_io` already
/// documents the cancelled-commit path as leaving the publication dirty "so the
/// next open reconciles it"; this function is that reconciliation.
///
/// It is a finalize **epilogue, not an `rm`**. Deleting the marker would make
/// the stale sidecars authoritative again — precisely the silent-wrong-ranks
/// outcome the guard exists to prevent. Instead it takes the publication lease,
/// re-establishes ownership of the marker, and routes through
/// `finalize_committed_index_for_scope_with_io`.
///
/// The ordering that function actually guarantees, stated precisely because it
/// is easy to overclaim: when a PageRank refresh is requested (recovery always
/// requests `unified()`), the **stale `.pagerank.json` is removed**, `.generation`
/// advanced and persisted, and the fresh PageRank computed and saved BEFORE the
/// marker is cleared — and the owner save fails closed rather than persist
/// nothing if the fresh cache was wiped mid-window, so a failed or raced
/// refresh blocks retirement instead of publishing clean without ranks. A kill
/// anywhere inside such a finalize therefore leaves either the marker still
/// set — dirty, which the next open reconciles — or a clean publication WITH a
/// sidecar matching the committed graph. Never clean with an advanced
/// generation and no sidecar: that state reports CLEAN while the note side of
/// the graph has no ranks, and the lazy fallback (`ranking.rs`,
/// `ensure_pagerank_loaded` → `compute_pagerank_warm_locked`) ranks
/// `GraphScope::code_only()` and only in memory — it never re-persists, and on
/// a brain database it never covers the note side at all.
///
/// The guarantee does NOT extend to a refresh-less clean publish
/// (`pagerank_scope: None`, `publish_clean: true`), which deletes the stale
/// sidecar and retires with no replacement by intent. No production caller
/// does that today.
///
/// The cost is a longer dirty window: ranked queries fail closed for the
/// duration of the PageRank compute rather than resuming just before it, which
/// the bounded MCP wait absorbs. Not duplicated here: the ordering is the
/// entire point of the guard and lives in one place.
///
/// `read_write` gates recovery exactly as it gates the orphaned-WAL arm of
/// `open_lbug_with_recovery`: a read-only caller must report the condition, not
/// mutate the directory to clear it. This is concrete, not theoretical — the
/// MCP server commonly runs as a separate process from the daemon and opens the
/// store read-only, so it must never be the process performing repair.
pub fn recover_abandoned_index_publication(
    store: &GraphStore,
    read_write: bool,
) -> Result<IndexPublicationRecovery, DeletionReconciliationError> {
    recover_abandoned_index_publication_with_io(
        store,
        read_write,
        false,
        &FileSystemIndexEpilogueIo,
    )
}

/// Operator-forced recovery: proceeds on the two cases the automatic predicate
/// must decline — a marker with no attributable writer, and one whose state
/// cannot be read.
///
/// `force` never overrides a writer we can prove is ALIVE, and never overrides
/// an in-process lease holder; those remain hard declines. What it overrides is
/// only "cannot prove dead", which is the automatic predicate's conservatism,
/// not a safety property. The real protection against clobbering a live writer
/// is that every recovery path holds a read-write `GraphStore`, and lbug's
/// exclusive write lock means no other process can hold one at the same time;
/// the pid and lease checks are defence in depth on top of that.
pub fn force_recover_index_publication(
    store: &GraphStore,
) -> Result<IndexPublicationRecovery, DeletionReconciliationError> {
    recover_abandoned_index_publication_with_io(store, true, true, &FileSystemIndexEpilogueIo)
}

pub(crate) fn recover_abandoned_index_publication_with_io(
    store: &GraphStore,
    read_write: bool,
    force: bool,
    io: &dyn IndexEpilogueIo,
) -> Result<IndexPublicationRecovery, DeletionReconciliationError> {
    const OPERATION: &str = "recover abandoned index publication";

    let Some(db_path) = store.db_path().map(Path::to_path_buf) else {
        return Ok(IndexPublicationRecovery::NotFileBacked);
    };

    // Cheap file-derived triage BEFORE touching the lease, so the common
    // (clean) case costs one `read_to_string` and nothing else.
    let state = nestweaver_store::index_publication::read_marker(&db_path);
    match &state {
        nestweaver_store::index_publication::MarkerState::Absent => {
            return Ok(IndexPublicationRecovery::Clean);
        }
        nestweaver_store::index_publication::MarkerState::Undeterminable(detail) => {
            // "Cannot tell" is not "abandoned" — never automatically. An
            // operator who has looked at the directory can still override.
            if !(force && read_write) {
                return Ok(IndexPublicationRecovery::Undeterminable {
                    detail: detail.clone(),
                });
            }
            tracing::warn!(
                "forced recovery of an index publication whose marker state could not be \
                 determined ({detail}); proceeding on explicit operator instruction"
            );
        }
        nestweaver_store::index_publication::MarkerState::Present(_) => {}
    }
    let record = state.record();
    let was_cancelled_run = record.is_some_and(|r| r.is_deliberately_dirty());
    // `pid` is `None` for an unattributed or undeterminable marker. Automatic
    // recovery declines; a forced one proceeds.
    let pid = match record.and_then(|r| r.writer_pid) {
        Some(pid) => {
            if crate::index_publication::process_is_alive(pid) {
                // Never overridden, not even by `force`.
                return Ok(IndexPublicationRecovery::WriterAlive { pid });
            }
            Some(pid)
        }
        None => {
            if !(force && read_write) {
                return Ok(IndexPublicationRecovery::WriterUnattributed);
            }
            None
        }
    };
    if !read_write {
        // Report, never clear. Same rule, and the same reason, as the
        // read-only arm of `open_lbug_with_recovery`: "a read-only caller
        // quarantining a log out from under a live writer would be a genuine
        // hazard". The MCP server opens the store read-only and can be a
        // different process from the daemon, so it lands here. A read-only
        // handle also does not hold lbug's exclusive write lock, which is what
        // actually keeps a live writer safe.
        return Ok(match pid {
            Some(pid) => IndexPublicationRecovery::ReadOnlyStore {
                abandoned_writer_pid: pid,
            },
            // Unreachable in practice: a pid-less marker only gets past the
            // match above when `force && read_write`, and `read_write` is false
            // here. Written totally rather than unwrapped so it cannot become a
            // panic if that gating ever changes.
            None => IndexPublicationRecovery::WriterUnattributed,
        });
    }

    // Second half of the abandoned predicate: no in-process publisher owns the
    // lease. Acquired non-blockingly — queueing behind a live publisher would
    // mean the publication was never abandoned in the first place.
    let lease = store
        .try_acquire_index_publication_lease()
        .map_err(|error| {
            DeletionReconciliationError::new(
                OPERATION,
                vec![DeletionReconciliationFailure {
                    stage: DeletionReconciliationStage::IndexPublicationMarker,
                    repo_uid: None,
                    message: format!("acquire exclusive index publication lease: {error:#}"),
                }],
            )
        })?;
    let Some(lease) = lease else {
        return Ok(IndexPublicationRecovery::LeaseHeld);
    };

    // Re-read under the lease. Between the triage read and here, a fresh
    // publisher in another process could have established a new marker with a
    // live pid; recovering that would clear a marker out from under its writer.
    let confirmed = nestweaver_store::index_publication::read_marker(&db_path);
    match (confirmed.record().and_then(|r| r.writer_pid), pid) {
        // A different pid appeared: a fresh publisher established a new marker
        // between the triage read and here. Recovering it would clear a marker
        // out from under its writer.
        (Some(current), Some(original)) if current != original => {
            return Ok(IndexPublicationRecovery::WriterAlive { pid: current });
        }
        // A forced recovery of a pid-less marker that has since GAINED a pid is
        // likewise a new publisher; decline if that publisher is alive.
        (Some(current), None) if crate::index_publication::process_is_alive(current) => {
            return Ok(IndexPublicationRecovery::WriterAlive { pid: current });
        }
        (Some(_), _) => {}
        (None, Some(_)) => return Ok(IndexPublicationRecovery::WriterUnattributed),
        (None, None) => {}
    }

    // The `u64::MAX` arm. When `.generation` is missing or unparseable while
    // the marker is present, the fail-closed load takes `canonical = u64::MAX`,
    // so `checked_add(2)` overflows in preflight and the publication can NEVER
    // complete — surfacing as `graph generation exhausted during index
    // publication`, a DIFFERENT error string. Recovery that ignored this arm
    // would appear to fix nothing. Re-derive instead of adding to `MAX`.
    let generation_path = crate::sidecar_path(&db_path, ".generation");
    if let Some(rederived) = lease
        .rederive_unavailable_generation_base(&generation_path)
        .map_err(|error| {
            DeletionReconciliationError::new(
                OPERATION,
                vec![DeletionReconciliationFailure {
                    stage: DeletionReconciliationStage::GenerationPersistence,
                    repo_uid: None,
                    message: format!("re-derive fail-closed generation base: {error:#}"),
                }],
            )
        })?
    {
        tracing::warn!(
            "index publication recovery re-derived an unavailable generation base to \
             {rederived} because {} was missing or unparseable while the marker was set",
            generation_path.display()
        );
    }

    // Take ownership of the marker (it is rewritten with THIS process's pid and
    // timestamp), then run the ordinary committed-index epilogue.
    let lease = establish_index_publication_marker_on_lease(lease, Some(&db_path), OPERATION, io)?;

    // Re-apply the cancellation reason that `establish_marker` just overwrote.
    // Without this, a crash DURING recovery loses the attribution: the next
    // recovery would report an ordinary crash and drop the `index --force`
    // guidance, which is the exact outcome the stamp exists to preserve.
    if was_cancelled_run
        && let Err(error) = io.stamp_marker_reason(
            &crate::sidecar_path(&db_path, ".index-dirty"),
            nestweaver_store::index_publication::MARKER_REASON_CANCELLED,
        )
    {
        tracing::warn!(
            "could not carry the cancellation reason through recovery: {error:#}; \
             a crash before this recovery completes would report it as a plain crash"
        );
    }

    // SCOPE: `unified()`, never `code_only()`.
    //
    // `compute_pagerank` REPLACES the whole score map rather than merging into
    // it, so the scope chosen here decides which node kinds survive recovery.
    //
    // Which scope the canonical sidecar currently holds is NOT knowable from
    // here, and it is not always unified: whichever publisher ran last wins.
    // `nestweaver index` and the code watcher publish `code_only()`; the vault
    // watcher publishes `unified()`. A fresh `index --repo` followed by
    // `brain add` leaves a sym-only sidecar on disk.
    //
    // `unified()` is right precisely because it is a strict SUPERSET: it can
    // only ever widen what was published, never narrow it. `code_only()` could
    // narrow it, and on a database holding both a repo and a vault that means
    // silently deleting every Note/Section/Heading/Tag rank — a recovery that
    // destroys data. This matches the recovered-owner arm in
    // `index_with_reader_and_write_gate`, which heals "that unknown committed
    // graph as one unified publication" for the same reason: a recovering owner
    // cannot prove which slices of the graph the dead owner had touched.
    finalize_committed_index_for_scope_with_io(
        lease,
        Some(&db_path),
        OPERATION,
        io,
        Some(&nestweaver_store::GraphScope::unified()),
        true,
    )?;

    Ok(IndexPublicationRecovery::Recovered {
        abandoned_writer_pid: pid,
        generation: store.graph_generation(),
        was_cancelled_run,
    })
}

/// Open a store READ-WRITE and reconcile any abandoned index publication
/// before handing it back.
///
/// This is the writer-side funnel for auto-heal. Every production writer opens
/// through it (`index_repo`, the incremental path, the code and vault
/// watchers); the daemon reconciles separately at startup. Read-only openers —
/// notably the MCP server, which commonly runs in a different process from the
/// daemon — deliberately do NOT have an equivalent and must never repair.
pub fn open_store_for_writing_with_recovery(db_path: &Path) -> Result<GraphStore, anyhow::Error> {
    let store = GraphStore::open_or_create(db_path)
        .with_context(|| format!("open/create store at {}", db_path.display()))?;
    recover_abandoned_index_publication_best_effort(&store, true);
    Ok(store)
}

/// Run [`recover_abandoned_index_publication`] and log the outcome, swallowing
/// errors. For call sites (daemon startup, writer opens) where recovery is a
/// best-effort improvement and must never be able to fail the caller.
pub fn recover_abandoned_index_publication_best_effort(
    store: &GraphStore,
    read_write: bool,
) -> Option<IndexPublicationRecovery> {
    match recover_abandoned_index_publication(store, read_write) {
        Ok(outcome) => {
            if outcome.recovered() {
                tracing::warn!("{}", outcome.describe());
            } else if !matches!(
                outcome,
                IndexPublicationRecovery::Clean | IndexPublicationRecovery::NotFileBacked
            ) {
                tracing::info!("{}", outcome.describe());
            }
            Some(outcome)
        }
        Err(error) => {
            tracing::warn!("index publication recovery failed: {error:#}");
            None
        }
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
    let enforce_actual_size = |source: String| -> Result<String, anyhow::Error> {
        let observed_bytes = source.len() as u64;
        let limit_bytes = reader.max_source_file_bytes();
        if observed_bytes > limit_bytes {
            return Err(crate::content_reader::SourceTooLarge {
                path: rel_path.to_string(),
                observed_bytes,
                limit_bytes,
            }
            .into());
        }
        Ok(source)
    };

    // file_meta returns None for bare-repo readers (no mtime available).
    // In that case, always fall through to read + hash.
    let (mtime_secs, size_bytes) = match reader.file_meta(rel)? {
        Some((m, s)) => (m, s),
        None => {
            // No filesystem metadata (e.g. GitBareReader) — read and hash. The
            // bare-clone reader enforces the size cap inside its own read_file
            // (oversized blobs return Err), so a huge file is skipped there.
            let source = enforce_actual_size(
                reader
                    .read_file(rel)
                    .with_context(|| format!("read {rel_path}"))?,
            )?;
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
    let max_source_file_bytes = reader.max_source_file_bytes();
    if size_bytes > max_source_file_bytes {
        return Err(crate::content_reader::SourceTooLarge {
            path: rel_path.to_string(),
            observed_bytes: size_bytes,
            limit_bytes: max_source_file_bytes,
        }
        .into());
    }

    if let Some(cached) = cache.get(rel_path) {
        // Tier 1: mtime unchanged → skip.
        if cached.mtime_secs == mtime_secs {
            return Ok(ChangeVerdict::Unchanged);
        }

        // Tier 2: mtime changed but size unchanged → fall through to hash check.
        // Same-size edits are common, so we cannot skip based on size alone.

        // Tier 3: mtime differs → read file, compute hash, compare.
        let source = enforce_actual_size(
            reader
                .read_file(rel)
                .with_context(|| format!("read {rel_path}"))?,
        )?;
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
        let source = enforce_actual_size(
            reader
                .read_file(rel)
                .with_context(|| format!("read {rel_path}"))?,
        )?;
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
    index_directory_with_options_and_limits(
        repo_path,
        db_path,
        instance_id,
        repo_url,
        indexed_sha,
        force,
        name,
        crate::index_limits::IndexLimits::default(),
    )
}

/// Index a directory with an explicit validated source-input ceiling.
#[allow(clippy::too_many_arguments)]
pub fn index_directory_with_options_and_limits(
    repo_path: &Path,
    db_path: &Path,
    instance_id: &str,
    repo_url: &str,
    indexed_sha: &str,
    force: bool,
    name: Option<&str>,
    limits: crate::index_limits::IndexLimits,
) -> Result<IndexResult, anyhow::Error> {
    // nw-C1: reconcile an abandoned publication before indexing. A crashed
    // predecessor's `.index-dirty` otherwise wedges every ranked query, and the
    // fail-closed `u64::MAX` generation base can block this run's own preflight.
    let store = open_store_for_writing_with_recovery(db_path)?;
    index_directory_with_store_and_limits(
        &store,
        repo_path,
        db_path,
        instance_id,
        repo_url,
        indexed_sha,
        force,
        name,
        limits,
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
    index_directory_with_store_and_limits(
        store,
        repo_path,
        db_path,
        instance_id,
        repo_url,
        indexed_sha,
        force,
        name,
        crate::index_limits::IndexLimits::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn index_directory_with_store_and_limits(
    store: &GraphStore,
    repo_path: &Path,
    db_path: &Path,
    instance_id: &str,
    repo_url: &str,
    indexed_sha: &str,
    force: bool,
    name: Option<&str>,
    limits: crate::index_limits::IndexLimits,
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
        limits,
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
    index_directory_with_store_cancellable_and_limits(
        store,
        repo_path,
        db_path,
        instance_id,
        repo_url,
        indexed_sha,
        force,
        name,
        cancel,
        crate::index_limits::IndexLimits::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn index_directory_with_store_cancellable_and_limits(
    store: &GraphStore,
    repo_path: &Path,
    db_path: &Path,
    instance_id: &str,
    repo_url: &str,
    indexed_sha: &str,
    force: bool,
    name: Option<&str>,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    limits: crate::index_limits::IndexLimits,
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
        limits,
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
    limits: crate::index_limits::IndexLimits,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<IndexResult, anyhow::Error> {
    let filemeta_path = crate::sidecar_path(db_path, ".filemeta.json");
    crate::migrate_sidecar(db_path, "filemeta.json", ".filemeta.json");
    let r_uid = repo_uid(instance_id, repo_url);
    // Capture generation-N derived state before graph publication advances to
    // N+2. Loading it after the graph commit would correctly reject it as
    // stale and, historically, `unwrap_or_default` then discarded every other
    // repository's manifest entry.
    let mut manifest_cache =
        crate::manifest::load_manifest_cache_for_db(store, db_path).unwrap_or_default();
    let mut new_filemeta = FileMetaCache::new();

    let parsed_cache_path = crate::sidecar_path(db_path, ".parsed_cache.bin");
    let mut parsed_cache = crate::parsed_cache::ParsedCache::load(&parsed_cache_path);

    let resolution_deps_path = crate::sidecar_path(db_path, ".resolution_deps.bin");
    let mut resolution_deps = crate::resolution_cache::ResolutionDeps::load(&resolution_deps_path);

    let reader = crate::content_reader::FilesystemReader::with_limits(repo_path, limits);
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
    manifest_cache.insert(r_uid, manifest);
    if let Err(e) = crate::manifest::save_manifest_cache_for_db(&manifest_cache, store, db_path) {
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
    // name -> store hits, memoised for this call (nw-127).
    let mut name_lookup_cache: std::collections::HashMap<String, Vec<nestweaver_schema::Symbol>> =
        std::collections::HashMap::new();
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
            //
            // nw-127: this is one store round-trip PER CALL SITE, and call names
            // repeat heavily across a repo, so the same name was looked up
            // thousands of times per run. The store is not mutated inside this
            // loop (symbol writes happen earlier in the phase), so memoising the
            // lookup for the duration of the call is a pure win.
            let by_name = match name_lookup_cache.get(reference.name.as_str()) {
                Some(hits) => hits,
                None => {
                    let hits = store
                        .lookup_symbols_by_name(&reference.name)
                        .map_err(|e| anyhow::anyhow!(e))?;
                    name_lookup_cache
                        .entry(reference.name.clone())
                        .or_insert(hits)
                }
            };
            let candidates: Vec<_> = by_name
                .iter()
                .filter(|t| {
                    t.repo_uid != current_repo_uid
                        && t.uid != source_uid
                        && t.visibility != Visibility::Private
                })
                .cloned()
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
    // An empty resolution-deps slice for this repo (missing/corrupt/stale
    // sidecar — the loader fails open to empty) is unsafe to combine with a
    // valid filemeta cache: unchanged files would stay classified Unchanged
    // while full resolution runs with no stale-edge clear and no bulk
    // delete, re-creating (duplicating) every resolved edge. Bypass the
    // cache so every file classifies Parsed, `force_reindex` falls out
    // below (`files_unchanged == 0`), and the atomic `bulk_reindex_write`
    // replaces the repo's graph instead of accumulating edges on top of it.
    let deps_empty_for_repo = resolution_deps
        .as_ref()
        .is_some_and(|rd| rd.is_empty_for_repo(&r_uid));
    if deps_empty_for_repo && reidentify_old_uid.is_none() && filemeta_cache.is_some() {
        tracing::warn!(
            repo_uid = %r_uid,
            "resolution deps empty for repo; bypassing filemeta cache to force a full replacement"
        );
    }
    let filemeta_cache = if reidentify_old_uid.is_some() || deps_empty_for_repo {
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
    let mut scan_skipped_files: Vec<SkippedFile> = Vec::new();
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
            scan_skipped_files.push(SkippedFile::new(
                rel_path.to_string_lossy().into_owned(),
                SkipReasonCode::Ignored,
                "minified or generated bundle policy",
            ));
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
        /// Transient I/O/parser failure. The full publication must abort;
        /// treating this as a policy skip could replace a complete incumbent
        /// graph with an incomplete one.
        Failed(String),
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
        // every remaining file so all cores are freed promptly. Cancellation
        // is observed here per-file during parse, at the post-parse barrier
        // below, and once more at the pre-write boundary after the write
        // guard is acquired — all before any graph mutation, so an index
        // cancelled at those points never persists a partial/empty graph.
        // A cancel that lands after the pre-write boundary still commits;
        // the daemon reports that as committed-after-cancellation and names
        // `index --force` as the repair.
        if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed)) {
            parse_pb.inc(1);
            return ParseOutcome::Skipped(SkippedFile::new(
                display_name,
                SkipReasonCode::Cancelled,
                "index cancelled",
            ));
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
                    if let Some(oversized) =
                        err.downcast_ref::<crate::content_reader::SourceTooLarge>()
                    {
                        return ParseOutcome::Skipped(SkippedFile::oversized(
                            display_name,
                            oversized.observed_bytes,
                            oversized.limit_bytes,
                        ));
                    }
                    return ParseOutcome::Failed(format!("stat/read {}: {err}", path.display()));
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
                ParseOutcome::Failed(format!("parse {}: {err}", path.display()))
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

    // Cooperative cancellation, post-parse barrier: if the flag tripped
    // during the parallel parse, bail now — BEFORE collection, resolution,
    // and any graph mutation. This is one of three observation points
    // (per-file during parse above, here, and at the pre-write boundary
    // after the write guard is acquired); an index cancelled at any of them
    // persists nothing. The no-partial-write invariant does NOT extend past
    // the pre-write boundary: a cancel landing after it still commits, the
    // publication is left dirty for the next open to reconcile, and the
    // daemon reports the run as committed-after-cancellation, naming
    // `index --force` as the repair.
    if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Acquire)) {
        anyhow::bail!("index cancelled");
    }
    if let Some(error) = outcomes.iter().find_map(|outcome| match outcome {
        ParseOutcome::Failed(error) => Some(error),
        _ => None,
    }) {
        anyhow::bail!("source indexing failed before publication: {error}");
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
    let mut skipped_files: Vec<SkippedFile> = scan_skipped_files;
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
            ParseOutcome::Failed(_) => unreachable!("failures are rejected before collection"),
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
                    .map(|h| h.framework.clone())
                    .or_else(|| {
                        // nw-160: Express and Fastify have no controller CLASS —
                        // routes are registered inside function bodies, so their
                        // hints carry role "handler". Requiring "controller"
                        // meant those files never became handler files and no
                        // HTTP contract was ever minted from them.
                        hint_by_index
                            .values()
                            .find(|h| {
                                h.role == "handler"
                                    && matches!(h.framework.as_str(), "express" | "fastify")
                            })
                            .map(|h| h.framework.clone())
                            .or_else(|| {
                                // The signature-based hint cannot see a route
                                // registered inside a function BODY, so recover
                                // it from the retained source.
                                source
                                    .as_deref()
                                    .and_then(crate::contracts::detect_node_route_framework)
                                    .map(str::to_string)
                            })
                    });
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

    // Contract specifications are part of the publication input, not a
    // best-effort decoration applied after the code graph has changed. Parse
    // every recognized spec before the write boundary so malformed or
    // unreadable input cannot advance the graph, SHA, generation, or caches.
    // A full parse already accumulated complete handler/symbol inputs; a warm
    // pass rebuilds the whole-repo view because unchanged files are absent
    // from those collections.
    spec_files.sort();
    handler_files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    let contract_plan_result = if files_unchanged == 0 {
        prepare_contract_derivation(
            reader,
            &r_uid,
            &spec_files,
            &handler_files,
            &all_symbols,
            true,
        )
    } else {
        prepare_incremental_contract_derivation(reader, &r_uid, repo_url)
    };

    let _write_guard = acquire_write_guard()?;

    let contract_plan = match contract_plan_result {
        Ok(plan) => plan,
        Err(error) => {
            if let Err(marker_error) =
                store.set_contract_derivation_failed(&r_uid, &error.to_string())
            {
                tracing::warn!("recording contract derivation failure failed: {marker_error}");
            }
            return Err(error).context("prepare strict contract derivation");
        }
    };
    skipped_files.extend(contract_plan.skipped_files.iter().cloned());
    skipped_files.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.reason_code.cmp(&right.reason_code))
    });
    skipped_files
        .dedup_by(|left, right| left.path == right.path && left.reason_code == right.reason_code);

    // Last cancellation observation point. Bailing in this window needs no
    // teardown: the marker call below is what creates `.index-dirty` and
    // reserves the generation, so nothing owned by this run exists yet and
    // any pre-existing `.index-dirty` (a prior interrupted publication)
    // stays untouched for its own recovery. A cancel that lands AFTER this
    // poll still commits; the committed finalizer then keeps the
    // publication dirty and the daemon reports committed-after-cancellation.
    // The incremental path (`incremental_index_with_reader_and_write_gate`)
    // takes no cancel token by design this cycle.
    if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Acquire)) {
        anyhow::bail!("index cancelled");
    }

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
        let build_env = |(file_path, symbols, _references, source_opt): &ParsedFileEntry| {
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
        };
        // Same dedicated low-priority pool as the parse phase.
        let mut type_envs: HashMap<String, nestweaver_resolver::types::TypeEnvironment> =
            crate::parse_pool::install_parse_pool(|| {
                parsed_files_for_resolver
                    .par_iter()
                    .filter_map(build_env)
                    .collect()
            });
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

        // Delete old resolved edges before inserting the new ones, or every
        // re-resolution accumulates duplicates. Incremental resolution clears
        // per affected file; a full (unfiltered) run re-creates every
        // resolved edge in the repo, so it must clear them repo-wide. When
        // `skip_resolution` holds, nothing is re-created and nothing is
        // cleared.
        if let Some(ref filter) = resolve_filter {
            for file_path in filter {
                let _ = store.delete_resolved_edges_for_file(&r_uid, file_path);
            }
        } else if !skip_resolution {
            let _ = store.delete_resolved_edges_for_repo(&r_uid);
        }

        let mut edges_count = insertable_edges.len();
        store
            .batch_insert_edges(&insertable_edges)
            .context("batch_insert_edges (resolved)")?;

        // nw-127: this walks EVERY parsed file — including unchanged ones, which
        // are deliberately fed to the resolver — and issues a store lookup per
        // call site. It ran unconditionally, so an index with zero changed files
        // still paid for it: 57 minutes on a 755-file repo against a 130k-symbol
        // graph, which is what pushed large repos past the 1800s ceiling and made
        // them report failure for work that had actually succeeded.
        //
        // When nothing changed, this repo's call sites are identical to the last
        // run and the edges it would infer are already in the database, so the
        // whole pass is recomputing a known answer. Skipping matches what
        // `skip_resolution` already does for ordinary resolution one block above,
        // and for the same reason.
        let inferred_cross_repo_edges = if skip_resolution {
            Vec::new()
        } else {
            infer_cross_repo_call_edges(store, &r_uid, &parsed_files_for_resolver)?
        };
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
            // Record the dep set for every file this run actually resolved —
            // including files that resolve to ZERO outbound edges, recorded as
            // an empty set. Without those entries a repo whose files have no
            // cross-file edges would look identical to a missing/corrupt
            // sidecar (`is_empty_for_repo`), and the empty-deps cache bypass
            // above would force a full replacement on every index. On an
            // incremental run only the affected files were re-resolved, so
            // only their entries are refreshed (files in the resolve filter
            // that were not actually fed to the resolver keep their previous
            // records); other files' records carry over. Nothing is recorded
            // when resolution was skipped: the previous records are still
            // accurate.
            if !skip_resolution {
                match &resolve_filter {
                    Some(filter) => {
                        // Only files actually fed to the resolver may have
                        // their records refreshed: a filter member that was
                        // NOT re-resolved (e.g. an unchanged dependent on a
                        // cold parsed cache) must keep its previous record
                        // rather than be clobbered with an empty set.
                        let resolved: std::collections::HashSet<_> = parsed_files_for_resolver
                            .iter()
                            .map(|(p, _, _, _)| p)
                            .collect();
                        for file in filter {
                            if resolved.contains(file) {
                                let deps = file_deps.remove(file).unwrap_or_default();
                                rd.set_deps_for_repo(&r_uid, file.clone(), deps);
                            }
                        }
                    }
                    None => {
                        for (path, _, _, _) in &parsed_files_for_resolver {
                            let deps = file_deps.remove(path).unwrap_or_default();
                            rd.set_deps_for_repo(&r_uid, path.clone(), deps);
                        }
                    }
                }
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
        // The complete plan above is immutable across the write boundary. Its
        // replacement, generation migration, and current-repo debt clear are
        // one transaction; an invalid write plan therefore retains the prior
        // derived graph.
        let contract_txn = store
            .begin_transaction()
            .context("begin full contract derivation transaction")?;
        // This transaction opens AFTER symbols, edges, and MEMBER_OF have
        // already committed in their own transactions. Returning Err from here
        // therefore reported a failed index over a fully written graph — the
        // same "committed work reported as a failure" defect as a cancelled
        // index, reached by a different door.
        //
        // Degrade instead of failing the run. The preflight parse phase
        // (`prepare_contract_derivation`, before the write guard) still rejects
        // a malformed spec with no graph mutation at all, so this path is only
        // reached when the graph is already durable and the *contracts* alone
        // could not be applied. Dropping the transaction leaves the previously
        // derived contract graph intact, exactly as split 1 guaranteed.
        //
        // `set_contract_derivation_failed` already marks the repo and split 2
        // already taught every consumer to read it, so a degraded-but-successful
        // result reuses that vocabulary end to end rather than inventing a
        // second way to say the same thing.
        //
        // The two incremental paths deliberately keep returning Err: their
        // contract apply shares one transaction with their symbol writes and
        // SHA update, so a failure there rolls the entire change back and
        // commits nothing. Failing is honest for them; it is not honest here.
        let contract_apply =
            apply_contract_derivation_checked(&contract_txn, &r_uid, &contract_plan).and_then(
                |count| {
                    store
                        .commit_transaction(&contract_txn)
                        .context("commit full contract derivation transaction")?;
                    Ok(count)
                },
            );
        drop(contract_txn);
        let (contracts_derived, contracts_status) = match contract_apply {
            Ok(count) => {
                if let Err(error) = store.clear_contract_derivation_failed(&r_uid) {
                    tracing::warn!("clearing contract derivation marker failed: {error}");
                }
                (count, crate::blast_radius::AnalysisStatus::Complete)
            }
            Err(error) => {
                if let Err(marker_error) =
                    store.set_contract_derivation_failed(&r_uid, &error.to_string())
                {
                    tracing::warn!("recording contract derivation failure failed: {marker_error}");
                }
                // error!, not warn!: the index succeeds, so this line is the
                // only place the failure is loud. The previous behavior at
                // least surfaced a non-zero exit.
                tracing::error!(
                    error = %format!("{error:#}"),
                    repo = %r_uid,
                    "contract derivation failed; the graph is committed and this \
                     repository is reported as degraded rather than failing the index"
                );
                (0, crate::blast_radius::AnalysisStatus::Degraded)
            }
        };
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
            contracts_derived,
            contracts_status,
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
                    true,
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
    //
    // Cancellation observed here arrived past every pre-write observation
    // point, so the graph committed anyway. Do NOT run the clean publish:
    // `.index-dirty` and the reserved generation must survive so the next
    // open reconciles this publication as dirty (fail-closed) instead of
    // trusting a generation/PageRank that predates the commit. The daemon
    // reports the outcome as committed-after-cancellation. The rest of the
    // finalizer (pagerank invalidation, failure aggregation, lease release)
    // is unchanged.
    let finalization = if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Acquire)) {
        finalize_committed_index_for_scope_with_io(
            publication,
            store.db_path(),
            "index graph write (committed after cancellation)",
            epilogue_io,
            // The scoped PageRank refresh is gated on `publication_clean`,
            // which stays false for a cancelled commit — pass `None` so the
            // skip is explicit rather than implied by a dead scope.
            None,
            false,
        )
    } else {
        finalize_committed_index_with_io(
            publication,
            store.db_path(),
            "index graph write",
            epilogue_io,
            bump_generation_after_write,
        )
    };
    match (graph_result, finalization) {
        (Ok(result), Ok(())) => {
            // nw-124: stamp WHICH resolver produced this repo's edges. Some
            // resolver fixes change edge shape rather than query behaviour
            // (nw-103's import fan-out), so upgrading the binary leaves already
            // indexed repos wrong and nothing said so. Recorded only on a fully
            // successful, finalized index — a failed run must not claim the
            // repo is current. Best-effort: a sidecar write failure must never
            // fail an index that already committed.
            if let Some(db_path) = store.db_path()
                && let Err(e) = crate::resolver_generation::record(db_path, &r_uid)
            {
                tracing::warn!(
                    repo = %r_uid,
                    error = %e,
                    "indexed successfully but could not record the resolver generation; \
                     ranking staleness for this repo will be over-reported"
                );
            }
            Ok(result)
        }
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

/// Where a candidate [`nestweaver_schema::Contract`] came from. Ordered worst
/// to best: a spec declaration always outranks a route inferred from handler
/// source, which is the preference the old `declared_uids` guard encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ContractOrigin {
    CodeDerived,
    Declared,
}

/// Order-preserving, UID-keyed accumulator for Contract nodes.
///
/// `Contract.uid` is the primary key of the `Contract` node table, so the bulk
/// COPY in `batch_insert_contracts` aborts the *whole* batch the moment a UID
/// repeats — and repeats are normal, not pathological:
///
/// - [`nestweaver_schema::uid::normalize_http_path`] deliberately discards the
///   *name* of a path parameter, so `GET /users/{id}` and `GET /users/{userId}`
///   mint one UID.
/// - [`nestweaver_schema::uid::contract_uid`] never mixes in the spec's
///   location, so one spec vendored at two paths mints its routes twice.
/// - `.proto` / `.graphql` operations carry no cross-file uniqueness check, so
///   two files declaring the same RPC or `Query` field collide too.
///
/// This accumulator is the only place that sees every candidate, so it is where
/// the collapse belongs.
struct ContractSet {
    /// UIDs in first-sighting order, so the COPY input stays reproducible.
    order: Vec<String>,
    by_uid: HashMap<String, (ContractOrigin, nestweaver_schema::Contract)>,
}

impl ContractSet {
    fn new() -> Self {
        Self {
            order: Vec::new(),
            by_uid: HashMap::new(),
        }
    }

    /// True once any candidate has claimed `uid`.
    fn contains(&self, uid: &str) -> bool {
        self.by_uid.contains_key(uid)
    }

    /// True when a *spec* declared `uid`. Replaces the old `declared_uids`
    /// set: the declared loop populates it by inserting, so the guard on the
    /// code-derived path can no longer fall out of sync with what was stored.
    fn is_declared(&self, uid: &str) -> bool {
        matches!(self.by_uid.get(uid), Some((ContractOrigin::Declared, _)))
    }

    /// Record `contract`, keeping the better row when its UID is already held.
    ///
    /// The winner is chosen by a **total, order-independent** order:
    ///
    /// 1. spec-declared beats code-derived (the `declared_uids` semantics);
    /// 2. then higher `confidence` — `f32` has no `Ord`, so `total_cmp`;
    /// 3. then the lexicographically smallest `source_path`.
    ///
    /// Collection order deliberately breaks no tie. `FilesystemReader` walks in
    /// readdir order while `GitReader` walks git-sorted, so "first one wins"
    /// would pick a different survivor on two machines and make the collapse
    /// untestable. Rows that tie on all three differ at most in `operation_id`;
    /// kind, verb and path are already pinned by the shared UID, so they are
    /// equivalent for the graph and the incumbent stays.
    fn insert(&mut self, origin: ContractOrigin, contract: nestweaver_schema::Contract) {
        match self.by_uid.entry(contract.uid.clone()) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                self.order.push(contract.uid.clone());
                slot.insert((origin, contract));
            }
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                let (held_origin, held) = slot.get();
                let wins = origin
                    .cmp(held_origin)
                    .then_with(|| contract.confidence.total_cmp(&held.confidence))
                    .then_with(|| held.source_path.cmp(&contract.source_path))
                    .is_gt();
                if wins {
                    slot.insert((origin, contract));
                }
            }
        }
    }

    /// The deduplicated rows, first-sighting order preserved.
    fn into_contracts(self) -> Vec<nestweaver_schema::Contract> {
        let Self { order, mut by_uid } = self;
        order
            .into_iter()
            .filter_map(|uid| by_uid.remove(&uid).map(|(_, c)| c))
            .collect()
    }
}

/// Rebuild the whole-repo inputs consumed by contract derivation.
///
/// Full indexing accumulates these while parsing every file. Incremental
/// indexing parses only changed files, but contracts are a derived whole-repo
/// view: a renamed spec or an unchanged controller can affect the same route.
/// Rescanning the lightweight contract inputs keeps incremental semantics
/// identical to force/full indexing without replacing the whole code graph.
fn collect_contract_derivation_inputs(
    reader: &dyn crate::content_reader::ContentReader,
    r_uid: &str,
    repo_url: &str,
    strict: bool,
) -> Result<ContractDerivationInputs, anyhow::Error> {
    let mut spec_files = Vec::new();
    let mut handler_files = Vec::new();
    let mut all_symbols = Vec::new();
    let mut skipped_files = Vec::new();
    let repo_path = reader.root();
    let discovered_files = reader
        .list_files()
        .context("list files for incremental contract derivation")?;
    for rel_path in &discovered_files {
        let abs_path = repo_path.join(rel_path);
        if crate::contracts::is_spec_file(&abs_path.to_string_lossy()) {
            spec_files.push(abs_path);
        }
    }
    let has_grpc_specs = spec_files.iter().any(|path| {
        path.extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("proto"))
    });

    for rel_path in discovered_files {
        let abs_path = repo_path.join(&rel_path);

        let Some(lang) = detect_language(&abs_path) else {
            continue;
        };
        if is_minified_or_bundled(&abs_path) {
            continue;
        }
        // Only these languages can contribute handler-derived contracts.
        // Filtering before metadata/read is important in strict mode: an
        // unreadable unrelated Python/Go/etc. file must not abort otherwise
        // valid contract publication.
        let eligible_handler_language = matches!(
            lang,
            nestweaver_schema::Language::Java
                | nestweaver_schema::Language::Kotlin
                | nestweaver_schema::Language::JavaScript
                | nestweaver_schema::Language::TypeScript
        ) || (has_grpc_specs
            && lang == nestweaver_schema::Language::Rust);
        if !eligible_handler_language {
            continue;
        }
        if reader
            .file_meta(&rel_path)
            .context("read contract input metadata")?
            .is_some_and(|(_, size)| size > reader.max_source_file_bytes())
        {
            tracing::debug!(path = %rel_path.display(), "skip oversized contract input before read");
            let observed_bytes = reader
                .file_meta(&rel_path)
                .ok()
                .flatten()
                .map(|(_, size)| size)
                .unwrap_or(reader.max_source_file_bytes() + 1);
            skipped_files.push(SkippedFile::oversized(
                rel_path.to_string_lossy().into_owned(),
                observed_bytes,
                reader.max_source_file_bytes(),
            ));
            continue;
        }
        let source = match reader.read_file(&rel_path) {
            Ok(source) => source,
            Err(error)
                if error
                    .downcast_ref::<crate::content_reader::SourceTooLarge>()
                    .is_some() =>
            {
                let oversized = error
                    .downcast_ref::<crate::content_reader::SourceTooLarge>()
                    .expect("guarded above");
                skipped_files.push(SkippedFile::oversized(
                    rel_path.to_string_lossy().into_owned(),
                    oversized.observed_bytes,
                    oversized.limit_bytes,
                ));
                continue;
            }
            Err(error) if strict => {
                return Err(error).with_context(|| {
                    format!("read contract handler candidate {}", rel_path.display())
                });
            }
            Err(error) => {
                tracing::debug!(path = %rel_path.display(), "skip unreadable handler candidate: {error}");
                continue;
            }
        };
        if source.len() as u64 > reader.max_source_file_bytes() {
            tracing::debug!(path = %rel_path.display(), "skip oversized contract input");
            skipped_files.push(SkippedFile::oversized(
                rel_path.to_string_lossy().into_owned(),
                source.len() as u64,
                reader.max_source_file_bytes(),
            ));
            continue;
        }
        let controller_candidate = match lang {
            nestweaver_schema::Language::Java | nestweaver_schema::Language::Kotlin => {
                source.contains("@RestController") || source.contains("@Controller")
            }
            nestweaver_schema::Language::JavaScript | nestweaver_schema::Language::TypeScript => {
                source.contains("@Controller")
            }
            _ => false,
        };
        let grpc_candidate =
            has_grpc_specs && lang == nestweaver_schema::Language::Rust && source.contains("impl ");
        if !controller_candidate && !grpc_candidate {
            continue;
        }
        let parsed = match parse_source(&abs_path, &source) {
            Ok(parsed) => parsed,
            Err(error) if strict => {
                return Err(error).with_context(|| {
                    format!("parse contract handler candidate {}", rel_path.display())
                });
            }
            Err(error) => {
                tracing::debug!(path = %rel_path.display(), "skip unparseable handler candidate: {error}");
                continue;
            }
        };
        let rel_path_string = rel_path.to_string_lossy().into_owned();
        if grpc_candidate {
            all_symbols.extend(parsed.symbols.iter().map(|symbol| {
                let scope = symbol.scope_chain.as_deref().unwrap_or("");
                nestweaver_schema::Symbol {
                    uid: symbol_uid(r_uid, &rel_path_string, &symbol.name, symbol.start_line),
                    name: symbol.name.clone(),
                    kind: symbol.kind,
                    repo_uid: r_uid.to_string(),
                    file_path: rel_path_string.clone(),
                    start_line: symbol.start_line,
                    end_line: symbol.end_line,
                    signature: symbol.signature.clone(),
                    summary: None,
                    content_hash: symbol.content_hash.clone(),
                    embedding: None,
                    pagerank_score: None,
                    is_entry_point: symbol.is_entry_point,
                    entry_point_kind: symbol.entry_point_kind,
                    visibility: symbol.visibility,
                    type_info: symbol.type_info.clone(),
                    framework_hint: None,
                    canonical_id: Some(canonical_symbol_id(
                        repo_url,
                        &rel_path_string,
                        &symbol.name,
                        scope,
                    )),
                }
            }));
        }
        if !controller_candidate {
            continue;
        }

        let Some(lang_str) = crate::contracts::framework_language_str(lang) else {
            continue;
        };

        let mut hint_by_index: HashMap<usize, nestweaver_schema::FrameworkHint> =
            nestweaver_parser::detect_frameworks(&parsed.symbols, &rel_path_string, lang_str)
                .into_iter()
                .collect();
        let class_starts: Vec<(usize, u32)> = parsed
            .symbols
            .iter()
            .enumerate()
            .filter(|(_, symbol)| symbol.kind == nestweaver_schema::SymbolKind::Class)
            .map(|(index, symbol)| (index, symbol.start_line))
            .collect();
        if let Some(controller_index) =
            crate::contracts::detect_nestjs_controller_index(&source, &class_starts)
        {
            hint_by_index.entry(controller_index).or_insert_with(|| {
                nestweaver_schema::FrameworkHint {
                    framework: "nestjs".into(),
                    role: "controller".into(),
                }
            });
        }

        let Some((controller_index, framework)) = hint_by_index.iter().find_map(|(index, hint)| {
            (hint.role == "controller").then(|| (*index, hint.framework.clone()))
        }) else {
            continue;
        };
        let class_signature = parsed
            .symbols
            .get(controller_index)
            .map(|symbol| symbol.signature.clone())
            .unwrap_or_default();
        let symbols = parsed
            .symbols
            .iter()
            .map(|symbol| {
                (
                    symbol_uid(r_uid, &rel_path_string, &symbol.name, symbol.start_line),
                    crate::contracts::HandlerSymbol {
                        name: symbol.name.clone(),
                        signature: symbol.signature.clone(),
                        start_line: symbol.start_line,
                    },
                )
            })
            .collect();
        handler_files.push(HandlerFileData {
            framework,
            class_signature,
            rel_path: rel_path_string,
            symbols,
        });
    }

    spec_files.sort();
    handler_files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok((spec_files, handler_files, all_symbols, skipped_files))
}

fn prepare_incremental_contract_derivation(
    reader: &dyn crate::content_reader::ContentReader,
    r_uid: &str,
    repo_url: &str,
) -> Result<ContractDerivationPlan, anyhow::Error> {
    let (spec_files, handler_files, all_symbols, skipped_files) =
        collect_contract_derivation_inputs(reader, r_uid, repo_url, true)?;
    let mut plan = prepare_contract_derivation(
        reader,
        r_uid,
        &spec_files,
        &handler_files,
        &all_symbols,
        true,
    )?;
    plan.skipped_files.extend(skipped_files);
    Ok(plan)
}

pub(crate) fn prepare_watcher_contract_derivation(
    reader: &dyn crate::content_reader::ContentReader,
    r_uid: &str,
    repo_url: &str,
) -> Result<ContractDerivationPlan, anyhow::Error> {
    prepare_watcher_contract_derivation_with_hooks(reader, r_uid, repo_url, || {}, || {})
}

pub(crate) fn watcher_contract_input_snapshot(
    reader: &dyn crate::content_reader::ContentReader,
) -> Result<std::collections::BTreeMap<String, String>, anyhow::Error> {
    let files = reader
        .list_files()
        .context("list files for watcher contract snapshot")?;
    let has_grpc_specs = files.iter().any(|path| {
        crate::contracts::is_spec_file(&path.to_string_lossy())
            && path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("proto"))
    });
    let mut snapshot = std::collections::BTreeMap::new();
    for rel_path in files {
        let abs_path = reader.root().join(&rel_path);
        let is_spec = crate::contracts::is_spec_file(&abs_path.to_string_lossy());
        let language = detect_language(&abs_path);
        if !is_spec && language.is_none() {
            continue;
        }
        if !is_spec && is_minified_or_bundled(&abs_path) {
            continue;
        }
        if reader
            .file_meta(&rel_path)
            .with_context(|| format!("read watcher contract metadata {}", rel_path.display()))?
            .is_some_and(|(_, size)| size > reader.max_source_file_bytes())
        {
            continue;
        }
        let source = reader
            .read_file(&rel_path)
            .with_context(|| format!("read watcher contract input {}", rel_path.display()))?;
        if source.len() as u64 > reader.max_source_file_bytes() {
            continue;
        }
        let candidate = if is_spec {
            true
        } else {
            match language.expect("checked above") {
                nestweaver_schema::Language::Java | nestweaver_schema::Language::Kotlin => {
                    source.contains("@RestController") || source.contains("@Controller")
                }
                nestweaver_schema::Language::JavaScript
                | nestweaver_schema::Language::TypeScript => source.contains("@Controller"),
                nestweaver_schema::Language::Rust => has_grpc_specs && source.contains("impl "),
                _ => false,
            }
        };
        if candidate {
            snapshot.insert(
                rel_path.to_string_lossy().into_owned(),
                crate::hash::blake3_hex(&source),
            );
        }
    }
    Ok(snapshot)
}

fn prepare_watcher_contract_derivation_with_hooks<F, G>(
    reader: &dyn crate::content_reader::ContentReader,
    r_uid: &str,
    repo_url: &str,
    before_plan: F,
    after_plan: G,
) -> Result<ContractDerivationPlan, anyhow::Error>
where
    F: FnOnce(),
    G: FnOnce(),
{
    let before = watcher_contract_input_snapshot(reader)?;
    before_plan();
    let observed = watcher_contract_input_snapshot(reader)?;
    let (spec_files, handler_files, all_symbols, skipped_files) =
        collect_contract_derivation_inputs(reader, r_uid, repo_url, true)?;
    let mut plan = prepare_contract_derivation(
        reader,
        r_uid,
        &spec_files,
        &handler_files,
        &all_symbols,
        true,
    )?;
    plan.skipped_files.extend(skipped_files);
    after_plan();
    let after = watcher_contract_input_snapshot(reader)?;
    let plan_reads_match_observed = plan
        .input_hashes
        .iter()
        .all(|(path, hash)| observed.get(path) == Some(hash));
    if before != observed || observed != after || !plan_reads_match_observed {
        let changed: Vec<_> = before
            .keys()
            .chain(observed.keys())
            .chain(after.keys())
            .chain(plan.input_hashes.keys())
            .filter(|path| {
                before.get(*path) != observed.get(*path)
                    || observed.get(*path) != after.get(*path)
                    || plan
                        .input_hashes
                        .get(*path)
                        .is_some_and(|hash| observed.get(*path) != Some(hash))
            })
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        anyhow::bail!(
            "contract inputs changed while watcher plan was prepared: {}",
            changed.join(", ")
        );
    }
    plan.observed_input_hashes = observed;
    Ok(plan)
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
///
pub(crate) struct ContractDerivationPlan {
    contracts: Vec<nestweaver_schema::Contract>,
    edges: Vec<nestweaver_schema::ResolvedEdge>,
    pub(crate) input_hashes: std::collections::BTreeMap<String, String>,
    pub(crate) observed_input_hashes: std::collections::BTreeMap<String, String>,
    pub(crate) skipped_files: Vec<SkippedFile>,
}

/// Prepare contract rows and implementation edges without mutating the graph.
/// All source reads and parsing finish before an incremental transaction opens.
fn prepare_contract_derivation(
    reader: &dyn crate::content_reader::ContentReader,
    r_uid: &str,
    spec_files: &[PathBuf],
    handler_files: &[HandlerFileData],
    all_symbols: &[nestweaver_schema::Symbol],
    strict: bool,
) -> Result<ContractDerivationPlan, anyhow::Error> {
    use nestweaver_schema::{EdgeType, ResolvedEdge};

    // 1. Declared contracts from specs.
    let mut all_contracts = ContractSet::new();
    let mut input_hashes = std::collections::BTreeMap::new();
    let mut skipped_files = Vec::new();
    // (contract_uid, "<package>.<Service>/<Rpc>") for every declared gRPC method.
    let mut declared_grpc: Vec<(String, String)> = Vec::new();
    let repo_path = reader.root();
    for spec_path in spec_files {
        let rel = spec_path
            .strip_prefix(repo_path)
            .unwrap_or(spec_path)
            .to_string_lossy()
            .into_owned();
        let source = match reader.read_file(Path::new(&rel)) {
            Ok(s) => s,
            Err(error)
                if error
                    .downcast_ref::<crate::content_reader::SourceTooLarge>()
                    .is_some() =>
            {
                let oversized = error
                    .downcast_ref::<crate::content_reader::SourceTooLarge>()
                    .expect("guarded above");
                skipped_files.push(SkippedFile::oversized(
                    rel,
                    oversized.observed_bytes,
                    oversized.limit_bytes,
                ));
                continue;
            }
            Err(error) if strict => {
                return Err(error).with_context(|| format!("read watched contract spec {rel}"));
            }
            Err(e) => {
                tracing::debug!("skip unreadable spec {rel}: {e}");
                continue;
            }
        };
        if source.len() as u64 > reader.max_source_file_bytes() {
            skipped_files.push(SkippedFile::oversized(
                rel,
                source.len() as u64,
                reader.max_source_file_bytes(),
            ));
            continue;
        }
        let parsed_specs = if strict {
            input_hashes.insert(rel.clone(), crate::hash::blake3_hex(&source));
            crate::contracts::parse_spec_file_strict(&rel, &source)
                .map_err(anyhow::Error::msg)
                .with_context(|| format!("parse watched contract spec {rel}"))?
        } else {
            crate::contracts::parse_spec_file(&rel, &source)
        };
        for sc in parsed_specs {
            // Keep gRPC operations so implementations can be matched against the
            // DECLARED contract rather than minting a UID from source (nw-104).
            let grpc_operation = (sc.kind == "grpc")
                .then(|| sc.operation_id.clone())
                .flatten();
            let contract = sc.into_contract(r_uid, &rel, 1.0);
            // One entry per declared RPC: `detect_grpc_impls` emits a match per
            // entry, so an RPC declared in two .proto files would otherwise
            // produce two identical IMPLEMENTS_CONTRACT edges.
            if let Some(operation) = grpc_operation
                && !all_contracts.contains(&contract.uid)
            {
                declared_grpc.push((contract.uid.clone(), operation));
            }
            all_contracts.insert(ContractOrigin::Declared, contract);
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
        let base_source = match reader.read_file(Path::new(&hf.rel_path)) {
            Ok(source) => source,
            Err(error) if strict => {
                return Err(error).with_context(|| format!("read watched handler {}", hf.rel_path));
            }
            Err(_) => hf.class_signature.clone(),
        };
        if strict {
            input_hashes.insert(hf.rel_path.clone(), crate::hash::blake3_hex(&base_source));
        }
        let matches = crate::contracts::detect_handlers(&hf.framework, &base_source, &handler_syms);
        for m in matches {
            let contract_uid = m.contract.uid(r_uid);
            // Mint a code-derived contract only when no spec declared this UID.
            // The set is authoritative either way — inserting a code-derived row
            // over a declared one loses on provenance — but skipping the work is
            // free, and the edge below still points at the declared node.
            if !all_contracts.is_declared(&contract_uid) {
                let contract = m
                    .contract
                    .clone()
                    .into_contract(r_uid, &hf.rel_path, m.confidence);
                all_contracts.insert(ContractOrigin::CodeDerived, contract);
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

    // 3b. gRPC: link declared contracts to their Rust/tonic implementations
    //     (nw-104).
    //
    // This does NOT go through `detect_handlers`. That path needs a framework
    // hint, which `detect_frameworks` never produces for Rust, which in turn
    // means no `HandlerFileData` is built — three gates that all had to be
    // opened to reach a detector that also did not exist. Matching declared
    // contracts against symbols here needs none of them, and the edge adopts the
    // DECLARED uid so it provably points at a real contract.
    if strict {
        for rel_path in all_symbols
            .iter()
            .map(|symbol| symbol.file_path.as_str())
            .collect::<std::collections::BTreeSet<_>>()
        {
            if input_hashes.contains_key(rel_path) {
                continue;
            }
            let source = reader
                .read_file(Path::new(rel_path))
                .with_context(|| format!("read watched gRPC candidate {rel_path}"))?;
            input_hashes.insert(rel_path.to_string(), crate::hash::blake3_hex(&source));
        }
    }
    if !declared_grpc.is_empty() {
        // Group symbols by file so each candidate file is read once.
        let mut by_file: std::collections::BTreeMap<&str, Vec<(String, String, u32)>> =
            std::collections::BTreeMap::new();
        for sym in all_symbols {
            by_file.entry(sym.file_path.as_str()).or_default().push((
                sym.uid.clone(),
                sym.name.clone(),
                sym.start_line,
            ));
        }

        let mut linked = 0usize;
        for (rel_path, symbols) in by_file {
            // Cheap pre-filter: a tonic server implementation is a trait impl, so
            // a file with no `impl ` at all cannot contain one. Avoids reading
            // every source file in the repo.
            let source = match reader.read_file(Path::new(rel_path)) {
                Ok(source) => source,
                Err(error) if strict => {
                    return Err(error)
                        .with_context(|| format!("read watched gRPC handler {rel_path}"));
                }
                Err(_) => continue,
            };
            if strict {
                input_hashes.insert(rel_path.to_string(), crate::hash::blake3_hex(&source));
            }
            if !source.contains("impl ") {
                continue;
            }
            for m in crate::contracts::detect_grpc_impls(&source, &declared_grpc, &symbols) {
                edges.push(ResolvedEdge {
                    source_uid: m.symbol_uid,
                    target_uid: m.contract_uid,
                    edge_type: EdgeType::ImplementsContract,
                    // Exact service AND method match against a declared contract.
                    confidence: 1.0,
                    link_type: None,
                    evidence: Vec::new(),
                });
                linked += 1;
            }
        }
        if linked > 0 {
            tracing::debug!(
                linked,
                declared = declared_grpc.len(),
                "linked gRPC contracts to tonic implementations"
            );
        }
    }

    // Batch insert all contracts at once via COPY FROM CSV. `Contract.uid` is
    // the node table's primary key, so the rows must already be unique by UID —
    // `ContractSet` guarantees that. Note the collapse is intentionally visible
    // in edge cardinality: two handlers whose routes normalize to one UID both
    // keep their IMPLEMENTS_CONTRACT edge onto the surviving contract.
    let contracts = all_contracts.into_contracts();

    Ok(ContractDerivationPlan {
        contracts,
        edges,
        input_hashes,
        observed_input_hashes: std::collections::BTreeMap::new(),
        skipped_files,
    })
}

// Test seam for the post-write contract-apply failure path.
//
// Both production triggers were fixed (UID dedup and CSV dialect, #229) and the
// strict preflight now rejects a malformed spec before any write, so there is no
// longer a way to reach the degradation branch from a fixture. Without a seam
// that branch would ship untested, which is the situation this whole item exists
// to complain about.
#[cfg(test)]
thread_local! {
    static FAIL_CONTRACT_APPLY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Arm a one-shot injected failure for the next post-write contract apply.
#[cfg(test)]
pub(crate) fn fail_next_contract_apply() {
    FAIL_CONTRACT_APPLY.with(|cell| cell.set(true));
}

/// [`apply_contract_derivation_on`], consulting the test seam first. A plain
/// passthrough in non-test builds.
fn apply_contract_derivation_checked(
    conn: &nestweaver_store::DbConnection<'_>,
    r_uid: &str,
    plan: &ContractDerivationPlan,
) -> Result<usize, anyhow::Error> {
    #[cfg(test)]
    if FAIL_CONTRACT_APPLY.with(|cell| cell.replace(false)) {
        anyhow::bail!("injected post-write contract apply failure");
    }
    apply_contract_derivation_on(conn, r_uid, plan)
}

/// Replace one repo's derived contract graph on an existing transaction.
///
/// Incremental indexing uses this seam so changed symbols, source paths,
/// contracts, implementation edges, and the indexed SHA publish as one unit.
/// Any contract write failure therefore rolls the whole incremental mutation
/// back, retaining the previously committed graph rather than leaving new
/// symbols paired with stale or missing derived edges.
pub(crate) fn apply_contract_derivation_on(
    conn: &nestweaver_store::DbConnection<'_>,
    r_uid: &str,
    plan: &ContractDerivationPlan,
) -> Result<usize, anyhow::Error> {
    // Clear + insert + edges must be ONE transaction. Previously the clear ran
    // on its own connection ahead of the inserts, so any failure in between
    // (e.g. a single row the COPY rejects) left the repo with zero contracts
    // and zero IMPLEMENTS_CONTRACT edges — and the caller only warns, so the
    // index still reported success. The transaction opens here, as late as
    // possible: all the spec/handler parsing above is expensive and holding a
    // write transaction across it would serialise writers for no benefit.
    //
    // No explicit rollback on the error paths, matching `bulk_reindex_write`:
    // a statement that throws inside an explicit transaction already rolls it
    // back, and dropping the connection rolls back anything still open.
    GraphStore::ensure_contract_derivation_v2_on(conn)?;
    GraphStore::clear_repo_contracts_on(conn, r_uid)?;
    GraphStore::batch_insert_contracts_on(conn, &plan.contracts)?;
    if !plan.edges.is_empty() {
        GraphStore::batch_insert_edges_on(conn, &plan.edges)?;
    }
    GraphStore::clear_contract_derivation_debt_on(conn, r_uid)?;
    GraphStore::clear_contract_derivation_failed_on(conn, r_uid)?;
    Ok(plan.contracts.len())
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
    pub skipped_files: Vec<SkippedFile>,
    pub symbols_added: usize,
    pub symbols_removed: usize,
    pub fell_back_to_full: bool,
}

enum IncrementalFileOutcome {
    Indexed(usize),
    PolicySkipped(SkippedFile),
}

fn record_incremental_file_outcome(
    result: &mut IncrementalResult,
    outcome: IncrementalFileOutcome,
) -> bool {
    match outcome {
        IncrementalFileOutcome::Indexed(symbols) => {
            result.symbols_added += symbols;
            true
        }
        IncrementalFileOutcome::PolicySkipped(skipped) => {
            if !result.skipped_files.iter().any(|existing| {
                existing.path == skipped.path && existing.reason_code == skipped.reason_code
            }) {
                result.files_skipped += 1;
                result.skipped_files.push(skipped);
            }
            false
        }
    }
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
    incremental_index_with_name_and_limits(
        repo_path,
        db_path,
        instance_id,
        repo_url,
        name,
        crate::index_limits::IndexLimits::default(),
    )
}

pub fn incremental_index_with_name_and_limits(
    repo_path: &Path,
    db_path: &Path,
    instance_id: &str,
    repo_url: &str,
    name: Option<&str>,
    limits: crate::index_limits::IndexLimits,
) -> Result<IncrementalResult, anyhow::Error> {
    incremental_index_with_name_and_io(
        repo_path,
        db_path,
        instance_id,
        repo_url,
        name,
        limits,
        &FileSystemIndexEpilogueIo,
    )
}

fn incremental_index_with_name_and_io(
    repo_path: &Path,
    db_path: &Path,
    instance_id: &str,
    repo_url: &str,
    name: Option<&str>,
    limits: crate::index_limits::IndexLimits,
    epilogue_io: &dyn IndexEpilogueIo,
) -> Result<IncrementalResult, anyhow::Error> {
    // nw-C1: reconcile BEFORE the `old_sha == new_sha` short-circuit below,
    // which returns early without ever establishing a marker. Without this, an
    // idle repo could never clear an abandoned publication however often it was
    // re-indexed.
    let store = open_store_for_writing_with_recovery(db_path)?;

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
                    limits,
                    epilogue_io,
                },
            );
        }
    };

    // 2. If no existing Repo → full index.
    let existing_repo = match existing_repo {
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
                    limits,
                    epilogue_io,
                },
            );
        }
        Some(r) => r,
    };
    let old_sha = existing_repo.indexed_sha.clone();

    // 2b. Self-heal an incomplete index BEFORE the up-to-date shortcut below:
    // an empty indexed_sha (Repo row created but SHA never committed — today
    // only handled implicitly via `is_ancestor("")` → false) or a committed
    // SHA with no content (crash between the SHA write and content landing)
    // can never be repaired incrementally, and would otherwise self-perpetuate
    // through the `old_sha == new_sha` skip.
    let index_incomplete = old_sha.is_empty()
        || store
            .repo_index_incomplete(&existing_repo)
            .with_context(|| "repo_index_incomplete failed")?;
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
                limits,
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
                limits,
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

    let reader = crate::content_reader::FilesystemReader::with_limits(repo_path, limits);
    let mut result = IncrementalResult::default();

    // nw-008 Phase 0 — transitive reverse-dependents from the LIVE graph, BEFORE
    // any mutation (the per-file `DETACH DELETE` destroys the edges we walk).
    let (changed_files, removed_files) = partition_changed_removed(&changes);
    let rdeps = collect_reverse_dep_files(&store, &r_uid, &changed_files, &removed_files);
    let contract_plan = match prepare_incremental_contract_derivation(&reader, &r_uid, repo_url) {
        Ok(plan) => plan,
        Err(error) => {
            if let Err(marker_error) =
                store.set_contract_derivation_failed(&r_uid, &error.to_string())
            {
                tracing::warn!("recording contract derivation failure failed: {marker_error}");
            }
            return Err(error).context("prepare incremental contract derivation");
        }
    };
    result
        .skipped_files
        .extend(contract_plan.skipped_files.iter().cloned());
    result.files_skipped += contract_plan.skipped_files.len();
    let prepared_files =
        prepare_incremental_files(&reader, &changes).context("prepare incremental source files")?;

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
                    continue;
                }
                match prepared_files
                    .get(rel_path.to_string_lossy().as_ref())
                    .expect("parseable added file was prepared")
                {
                    PreparedIncrementalOutcome::Ready(prepared) => {
                        let outcome = write_prepared_incremental_file_txn(
                            &reader, prepared, &r_uid, repo_url, &store, &txn,
                        )?;
                        if record_incremental_file_outcome(&mut result, outcome) {
                            result.files_added += 1;
                        }
                    }
                    PreparedIncrementalOutcome::PolicySkipped(skipped) => {
                        record_incremental_file_outcome(
                            &mut result,
                            IncrementalFileOutcome::PolicySkipped(skipped.clone()),
                        );
                    }
                }
            }
            crate::git_diff::FileChange::Modified(rel_path) => {
                if path_in_skip_dir(rel_path) || !is_parseable(rel_path) {
                    continue;
                }
                let prepared = prepared_files
                    .get(rel_path.to_string_lossy().as_ref())
                    .expect("parseable modified file was prepared");
                // A policy exclusion intentionally removes stale incumbent
                // coverage. Transient failures never reach this transaction.
                let rel_str = rel_path.to_string_lossy();
                let removed =
                    nestweaver_store::GraphStore::delete_symbols_in_file_on(&txn, &r_uid, &rel_str)
                        .with_context(|| format!("delete_symbols_in_file {}", rel_str))?;
                result.symbols_removed += removed;
                match prepared {
                    PreparedIncrementalOutcome::Ready(prepared) => {
                        let outcome = write_prepared_incremental_file_txn(
                            &reader, prepared, &r_uid, repo_url, &store, &txn,
                        )?;
                        if record_incremental_file_outcome(&mut result, outcome) {
                            result.files_modified += 1;
                        }
                    }
                    PreparedIncrementalOutcome::PolicySkipped(skipped) => {
                        let file_uid = nestweaver_schema::file_uid(&r_uid, &rel_str);
                        nestweaver_store::GraphStore::delete_file_node_on(&txn, &file_uid)
                            .with_context(|| format!("delete policy-skipped file {rel_str}"))?;
                        record_incremental_file_outcome(
                            &mut result,
                            IncrementalFileOutcome::PolicySkipped(skipped.clone()),
                        );
                    }
                }
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

                    match prepared_files
                        .get(to_str.as_ref())
                        .expect("parseable renamed file was prepared")
                    {
                        PreparedIncrementalOutcome::Ready(prepared) => {
                            let outcome = write_prepared_incremental_file_txn(
                                &reader, prepared, &r_uid, repo_url, &store, &txn,
                            )?;
                            record_incremental_file_outcome(&mut result, outcome);
                        }
                        PreparedIncrementalOutcome::PolicySkipped(skipped) => {
                            record_incremental_file_outcome(
                                &mut result,
                                IncrementalFileOutcome::PolicySkipped(skipped.clone()),
                            );
                        }
                    }
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

    if let Err(error) = apply_contract_derivation_on(&txn, &r_uid, &contract_plan) {
        drop(txn);
        if let Err(marker_error) = store.set_contract_derivation_failed(&r_uid, &error.to_string())
        {
            tracing::warn!("recording contract derivation failure failed: {marker_error}");
        }
        return Err(error).context("apply incremental contract derivation");
    }

    // 6. Update the stored SHA inside the transaction, then commit.
    // If we crash before commit, the next run replays from the old SHA.
    nestweaver_store::GraphStore::update_repo_sha_on(&txn, &r_uid, &new_sha)
        .with_context(|| "update_repo_sha")?;

    store
        .commit_transaction(&txn)
        .with_context(|| "commit incremental transaction")?;
    drop(txn);
    if let Err(error) = store.clear_contract_derivation_failed(&r_uid) {
        tracing::warn!("clearing contract derivation marker failed: {error}");
    }

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
    let contract_plan_result = prepare_incremental_contract_derivation(reader, &r_uid, repo_url);
    let prepared_files = prepare_incremental_files(reader, &changes)
        .context("prepare server incremental source files")?;

    let _write_guard = acquire_write_guard()?;
    let contract_plan = match contract_plan_result {
        Ok(plan) => plan,
        Err(error) => {
            if let Err(marker_error) =
                store.set_contract_derivation_failed(&r_uid, &error.to_string())
            {
                tracing::warn!("recording contract derivation failure failed: {marker_error}");
            }
            return Err(error).context("prepare server incremental contract derivation");
        }
    };
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
    result
        .skipped_files
        .extend(contract_plan.skipped_files.iter().cloned());
    result.files_skipped += contract_plan.skipped_files.len();

    for change in &changes {
        match change {
            crate::git_diff::FileChange::Added(rel_path) => {
                if path_in_skip_dir(rel_path) || !is_parseable(rel_path) {
                    continue;
                }
                match prepared_files
                    .get(rel_path.to_string_lossy().as_ref())
                    .expect("parseable added file was prepared")
                {
                    PreparedIncrementalOutcome::Ready(prepared) => {
                        let outcome = write_prepared_incremental_file_txn(
                            reader, prepared, &r_uid, repo_url, store, &txn,
                        )?;
                        if record_incremental_file_outcome(&mut result, outcome) {
                            result.files_added += 1;
                        }
                    }
                    PreparedIncrementalOutcome::PolicySkipped(skipped) => {
                        record_incremental_file_outcome(
                            &mut result,
                            IncrementalFileOutcome::PolicySkipped(skipped.clone()),
                        );
                    }
                }
            }
            crate::git_diff::FileChange::Modified(rel_path) => {
                if path_in_skip_dir(rel_path) || !is_parseable(rel_path) {
                    continue;
                }
                let prepared = prepared_files
                    .get(rel_path.to_string_lossy().as_ref())
                    .expect("parseable modified file was prepared");
                let rel_str = rel_path.to_string_lossy();
                let removed =
                    nestweaver_store::GraphStore::delete_symbols_in_file_on(&txn, &r_uid, &rel_str)
                        .with_context(|| format!("delete_symbols_in_file {}", rel_str))?;
                result.symbols_removed += removed;

                match prepared {
                    PreparedIncrementalOutcome::Ready(prepared) => {
                        let outcome = write_prepared_incremental_file_txn(
                            reader, prepared, &r_uid, repo_url, store, &txn,
                        )?;
                        if record_incremental_file_outcome(&mut result, outcome) {
                            result.files_modified += 1;
                        }
                    }
                    PreparedIncrementalOutcome::PolicySkipped(skipped) => {
                        let file_uid = nestweaver_schema::file_uid(&r_uid, &rel_str);
                        nestweaver_store::GraphStore::delete_file_node_on(&txn, &file_uid)
                            .with_context(|| format!("delete policy-skipped file {rel_str}"))?;
                        record_incremental_file_outcome(
                            &mut result,
                            IncrementalFileOutcome::PolicySkipped(skipped.clone()),
                        );
                    }
                }
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

                    match prepared_files
                        .get(to_str.as_ref())
                        .expect("parseable renamed file was prepared")
                    {
                        PreparedIncrementalOutcome::Ready(prepared) => {
                            let outcome = write_prepared_incremental_file_txn(
                                reader, prepared, &r_uid, repo_url, store, &txn,
                            )?;
                            record_incremental_file_outcome(&mut result, outcome);
                        }
                        PreparedIncrementalOutcome::PolicySkipped(skipped) => {
                            record_incremental_file_outcome(
                                &mut result,
                                IncrementalFileOutcome::PolicySkipped(skipped.clone()),
                            );
                        }
                    }
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

    if let Err(error) = apply_contract_derivation_on(&txn, &r_uid, &contract_plan) {
        drop(txn);
        if let Err(marker_error) = store.set_contract_derivation_failed(&r_uid, &error.to_string())
        {
            tracing::warn!("recording contract derivation failure failed: {marker_error}");
        }
        return Err(error).context("apply server incremental contract derivation");
    }

    nestweaver_store::GraphStore::update_repo_sha_on(&txn, &r_uid, new_sha)
        .with_context(|| "update_repo_sha")?;
    store
        .commit_transaction(&txn)
        .with_context(|| "commit incremental transaction")?;
    drop(txn);
    if let Err(error) = store.clear_contract_derivation_failed(&r_uid) {
        tracing::warn!("clearing contract derivation marker failed: {error}");
    }

    finalize_committed_index_with_io(
        publication,
        store.db_path(),
        "server incremental index",
        &FileSystemIndexEpilogueIo,
        true,
    )?;

    Ok(result)
}

struct PreparedIncrementalFile {
    abs_path: std::path::PathBuf,
    rel_str: String,
    source: String,
    parsed: nestweaver_parser::ParsedFile,
}

enum PreparedIncrementalOutcome {
    Ready(PreparedIncrementalFile),
    PolicySkipped(SkippedFile),
}

/// Read and parse before opening the write transaction. This ordering is the
/// rollback guarantee: a transient read/parser failure cannot occur after an
/// incumbent file's nodes or relationships have been detached.
fn prepare_incremental_file(
    reader: &dyn crate::content_reader::ContentReader,
    rel_path: &std::path::Path,
) -> Result<PreparedIncrementalOutcome, anyhow::Error> {
    let abs_path = reader.root().join(rel_path);
    let rel_str = rel_path.to_string_lossy().into_owned();

    if is_minified_or_bundled(&abs_path) {
        return Ok(PreparedIncrementalOutcome::PolicySkipped(SkippedFile::new(
            rel_str,
            SkipReasonCode::Ignored,
            "minified or generated bundle policy",
        )));
    }

    if let Some((_, observed_bytes)) = reader
        .file_meta(rel_path)
        .with_context(|| format!("stat {}", abs_path.display()))?
        && observed_bytes > reader.max_source_file_bytes()
    {
        return Ok(PreparedIncrementalOutcome::PolicySkipped(
            SkippedFile::oversized(rel_str, observed_bytes, reader.max_source_file_bytes()),
        ));
    }
    let source = match reader.read_file(rel_path) {
        Ok(source) => source,
        Err(error) => {
            if let Some(oversized) = error.downcast_ref::<crate::content_reader::SourceTooLarge>() {
                return Ok(PreparedIncrementalOutcome::PolicySkipped(
                    SkippedFile::oversized(
                        rel_str,
                        oversized.observed_bytes,
                        oversized.limit_bytes,
                    ),
                ));
            }
            return Err(error).with_context(|| format!("read {}", abs_path.display()));
        }
    };
    let parsed = nestweaver_parser::parse_source(&abs_path, &source)
        .with_context(|| format!("parse {}", abs_path.display()))?;
    Ok(PreparedIncrementalOutcome::Ready(PreparedIncrementalFile {
        abs_path,
        rel_str,
        source,
        parsed,
    }))
}

fn prepare_incremental_files(
    reader: &dyn crate::content_reader::ContentReader,
    changes: &[crate::git_diff::FileChange],
) -> Result<HashMap<String, PreparedIncrementalOutcome>, anyhow::Error> {
    let mut prepared = HashMap::new();
    for change in changes {
        let path = match change {
            crate::git_diff::FileChange::Added(path)
            | crate::git_diff::FileChange::Modified(path) => Some(path),
            crate::git_diff::FileChange::Renamed { to, .. } => Some(to),
            crate::git_diff::FileChange::Deleted(_) => None,
        };
        let Some(path) = path else { continue };
        if path_in_skip_dir(path) || !is_parseable(path) {
            continue;
        }
        prepared.insert(
            path.to_string_lossy().into_owned(),
            prepare_incremental_file(reader, path)?,
        );
    }
    Ok(prepared)
}

/// Write one already-read and parsed file through the shared transaction.
fn write_prepared_incremental_file_txn(
    reader: &dyn crate::content_reader::ContentReader,
    prepared: &PreparedIncrementalFile,
    r_uid: &str,
    repo_url: &str,
    _store: &nestweaver_store::GraphStore,
    conn: &nestweaver_store::DbConnection<'_>,
) -> Result<IncrementalFileOutcome, anyhow::Error> {
    use nestweaver_parser::{RawReference, RawSymbol};
    use nestweaver_resolver::{discover_workspace_context_with, resolve_references_with_context};
    use nestweaver_schema::{File, Symbol, canonical_symbol_id, file_uid, symbol_uid};

    let abs_path = &prepared.abs_path;
    let rel_str = &prepared.rel_str;
    let source = &prepared.source;
    let parsed = &prepared.parsed;

    let content_hash = content_hash_hex(source);
    let f_uid = file_uid(r_uid, rel_str);

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
        let s_uid = symbol_uid(r_uid, rel_str, &raw_sym.name, raw_sym.start_line);
        let scope_str = raw_sym.scope_chain.as_deref().unwrap_or("");
        let canonical = canonical_symbol_id(repo_url, rel_str, &raw_sym.name, scope_str);
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
    let lang = nestweaver_parser::detect_language(abs_path)
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

    Ok(IncrementalFileOutcome::Indexed(sym_count))
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
    let insertable = build_reresolve_edges(reader, r_uid, changed, rdeps, &db_symbols, None)?;

    // Runs inside the same transaction as the mutation loop.
    let count = insertable.len();
    if count > 0 {
        nestweaver_store::GraphStore::batch_insert_edges_on(conn, &insertable)
            .with_context(|| "batch_insert_edges (transitive re-resolution)")?;
    }
    Ok(count)
}

/// Prepare the live watcher's reverse-dependent edges before publication.
///
/// The watcher supplies the frozen replacement symbols that its transaction
/// will publish. This produces the same post-mutation symbol view as
/// [`reresolve_affected_dependents`] without reading source after the dirty
/// marker has been established.
pub(crate) struct WatcherReresolveInputs<'a> {
    pub(crate) changed: &'a std::collections::HashSet<String>,
    pub(crate) removed: &'a std::collections::HashSet<String>,
    pub(crate) rdeps: &'a std::collections::HashSet<String>,
    pub(crate) replacement_symbols: &'a [nestweaver_schema::Symbol],
    pub(crate) prepared_file_data: &'a PreparedFileData,
}

pub(crate) fn prepare_watcher_reresolve_edges(
    reader: &dyn crate::content_reader::ContentReader,
    store: &nestweaver_store::GraphStore,
    r_uid: &str,
    inputs: WatcherReresolveInputs<'_>,
) -> Result<Vec<nestweaver_schema::ResolvedEdge>, anyhow::Error> {
    if inputs.changed.is_empty() {
        return Ok(Vec::new());
    }
    let mut symbols = store
        .lookup_symbols_by_repo(r_uid)
        .with_context(|| "lookup_symbols_by_repo for watcher edge preparation")?;
    symbols.retain(|symbol| {
        !inputs.changed.contains(&symbol.file_path) && !inputs.removed.contains(&symbol.file_path)
    });
    symbols.extend_from_slice(inputs.replacement_symbols);
    build_reresolve_edges(
        reader,
        r_uid,
        inputs.changed,
        inputs.rdeps,
        &symbols,
        Some(inputs.prepared_file_data),
    )
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
    prepared_file_data: Option<&PreparedFileData>,
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
        let (raw_symbols, raw_references) = if let Some((symbols, references)) =
            prepared_file_data.and_then(|prepared| prepared.get(rel_str))
        {
            (symbols.clone(), references.clone())
        } else {
            let source = match reader.read_file(rel_path) {
                Ok(s) => s,
                Err(_) => continue, // deleted/unreadable — nothing to re-resolve from
            };
            let parsed = match parse_source(&abs_path, &source) {
                Ok(p) => p,
                Err(_) => continue,
            };
            (parsed.symbols, parsed.references)
        };
        if let Some(lang) = detect_language(&abs_path) {
            *lang_counts.entry(lang).or_insert(0) += 1;
        }
        for raw_sym in &raw_symbols {
            let s_uid = symbol_uid(r_uid, rel_str, &raw_sym.name, raw_sym.start_line);
            uid_to_file.insert(s_uid, rel_str.clone());
        }
        file_data.push((rel_str.clone(), raw_symbols, raw_references));
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
    limits: crate::index_limits::IndexLimits,
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
        limits,
        epilogue_io,
    } = request;
    // Load filemeta sidecar for tiered change detection even in fallback.
    // Only this repo's slice feeds change detection — another repo's entry
    // for the same rel path must never match (nw-022).
    crate::migrate_sidecar(db_path, "filemeta.json", ".filemeta.json");
    let filemeta_path = crate::sidecar_path(db_path, ".filemeta.json");
    let r_uid = nestweaver_schema::repo_uid(instance_id, repo_url);
    let mut manifest_cache =
        crate::manifest::load_manifest_cache_for_db(store, db_path).unwrap_or_default();
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

    let reader = crate::content_reader::FilesystemReader::with_limits(repo_path, limits);
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
    manifest_cache.insert(r_uid, manifest);
    if let Err(e) = crate::manifest::save_manifest_cache_for_db(&manifest_cache, store, db_path) {
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
        files_added: result.files_count,
        files_skipped: result.skipped_files.len(),
        skipped_files: result.skipped_files,
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

    /// Stand-in for a binary's response-shape version in tests that exercise
    /// the response cache's GENERATION behavior. Any stable non-zero value
    /// works; these tests hold it fixed so shape versioning is not the variable
    /// under test.
    const RESPONSE_SHAPE_FIXTURE: u64 = 0xE1691E;

    fn write_pagerank_fixture(store: &GraphStore, path: &Path, scores: HashMap<String, f64>) {
        let identity = store.publication_identity().unwrap().unwrap();
        let fingerprint = format!(
            "{}test-fixture",
            nestweaver_store::ranking::PAGERANK_ALGORITHM_FINGERPRINT_PREFIX
        );
        let envelope = nestweaver_store::artifact_envelope::ArtifactEnvelope::new(
            nestweaver_store::artifact_envelope::ArtifactExpectation {
                artifact_kind: nestweaver_store::ranking::PAGERANK_ARTIFACT_KIND,
                artifact_schema_version:
                    nestweaver_store::ranking::PAGERANK_ARTIFACT_SCHEMA_VERSION,
                identity: &identity,
                producer_version: env!("CARGO_PKG_VERSION"),
                source_graph_generation: store.graph_generation(),
                algorithm_fingerprint: &fingerprint,
            },
            &scores,
        )
        .unwrap();
        fs::write(path, serde_json::to_vec_pretty(&envelope).unwrap()).unwrap();
    }

    #[test]
    fn default_source_limit_indexes_nestweaver_sized_rust_file() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join("src")).unwrap();
        let mut source = String::from("pub fn large_file_marker() -> usize { 42 }\n");
        source.push_str(&"// representative source padding\n".repeat(40_000));
        assert!(source.len() > 1_048_576 && source.len() < 2_097_152);
        fs::write(repo.join("src/main.rs"), source).unwrap();
        let db = dir.path().join("graph.lbug");
        let result = index_directory_with_options_and_limits(
            &repo,
            &db,
            "test",
            "https://example.test/large-rust",
            "fixture",
            true,
            None,
            crate::index_limits::IndexLimits::default(),
        )
        .unwrap();
        assert!(
            result.skipped_files.is_empty(),
            "{:?}",
            result.skipped_files
        );
        assert!(result.symbols_count >= 1);
        let store = GraphStore::open_or_create(&db).unwrap();
        assert!(
            store
                .list_all_symbols()
                .unwrap()
                .iter()
                .any(|symbol| symbol.name == "large_file_marker")
        );
    }

    #[test]
    fn sparse_generated_file_is_policy_skipped_by_metadata_preflight() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let file = fs::File::create(repo.join("bundle.js")).unwrap();
        file.set_len(200 * 1024 * 1024).unwrap();
        let db = dir.path().join("graph.lbug");
        let result = index_directory_with_options_and_limits(
            &repo,
            &db,
            "test",
            "https://example.test/sparse",
            "fixture",
            true,
            None,
            crate::index_limits::IndexLimits::default(),
        )
        .unwrap();
        assert_eq!(result.skipped_files.len(), 1);
        assert_eq!(
            result.skipped_files[0].reason_code,
            nestweaver_parser::SkipReasonCode::Oversized
        );
        assert_eq!(
            result.skipped_files[0].observed_bytes,
            Some(200 * 1024 * 1024)
        );
    }

    #[test]
    fn metadata_free_reader_cannot_bypass_the_actual_source_size_limit() {
        struct MetadataFreeReader {
            root: PathBuf,
        }
        impl crate::content_reader::ContentReader for MetadataFreeReader {
            fn read_file(&self, _rel_path: &Path) -> anyhow::Result<String> {
                Ok("x".repeat(1025))
            }
            fn list_files(&self) -> anyhow::Result<Vec<PathBuf>> {
                Ok(vec![PathBuf::from("large.rs")])
            }
            fn file_meta(&self, _rel_path: &Path) -> anyhow::Result<Option<(u64, u64)>> {
                Ok(None)
            }
            fn root(&self) -> &Path {
                &self.root
            }
            fn version_id(&self) -> &str {
                "metadata-free"
            }
            fn max_source_file_bytes(&self) -> u64 {
                1024
            }
        }

        let reader = MetadataFreeReader {
            root: PathBuf::from("/unused"),
        };
        let error = match tiered_change_check(&reader, "large.rs", &FileMetaCache::new()) {
            Err(error) => error,
            Ok(_) => panic!("metadata-free oversized content must be rejected"),
        };
        let oversized = error
            .downcast_ref::<crate::content_reader::SourceTooLarge>()
            .expect("actual returned content must be bounded even without metadata");
        assert_eq!(oversized.observed_bytes, 1025);
        assert_eq!(oversized.limit_bytes, 1024);
    }

    fn owned_contract_uid(repo_uid: &str, bare_shape: &str) -> String {
        format!(
            "contract:{repo_uid}:{}",
            bare_shape.trim_start_matches("contract:")
        )
    }

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

    // ── nw-C1: abandoned-publication recovery ───────────────────────────

    /// A pid that is guaranteed not to name a live process: spawn a child,
    /// wait for it (which reaps the zombie), and return its now-free pid.
    /// `kill(pid, 0)` then reports ESRCH deterministically.
    fn reaped_child_pid() -> i32 {
        // nw-138: resolve `true` via PATH. macOS ships it at /usr/bin/true
        // and has no /bin/true, so hardcoding the path panicked with
        // NotFound on every macOS machine while passing in Linux CI.
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id() as i32;
        child.wait().expect("reap true");
        assert!(
            !crate::index_publication::process_is_alive(pid),
            "a reaped child must not read as alive"
        );
        pid
    }

    fn write_marker_with_pid(marker_path: &Path, pid: i32, reason: Option<&str>) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::fs::write(
            marker_path,
            nestweaver_store::index_publication::format_marker_payload(pid as u32, nanos, reason),
        )
        .unwrap();
    }

    /// Insert a small NOTE-side graph alongside the code graph, so a recovery
    /// that ranks with the wrong scope is detectable.
    fn insert_publication_notes(store: &GraphStore, publisher: &str) {
        let vault_uid = format!("vault:{publisher}");
        store
            .insert_vault(&nestweaver_schema::Vault {
                uid: vault_uid.clone(),
                name: format!("vault-{publisher}"),
                root_path: format!("/tmp/{publisher}"),
                instance_id: "test".into(),
            })
            .unwrap();
        for n in ["one", "two"] {
            let uid = format!("note:{publisher}:{n}");
            store
                .insert_note(&nestweaver_schema::Note {
                    uid: uid.clone(),
                    vault_uid: vault_uid.clone(),
                    file_path: format!("{n}.md"),
                    title: n.to_string(),
                    note_kind: nestweaver_schema::NoteKind::General,
                    word_count: 10,
                    content_hash: format!("hash-{publisher}-{n}"),
                    frontmatter: None,
                    created_at: None,
                    modified_at: None,
                    pagerank_score: None,
                    embedding: None,
                })
                .unwrap();
            store.insert_vault_note_edge(&vault_uid, &uid).unwrap();
        }
    }

    /// Leave the database in exactly the state a SIGKILL between marker
    /// establishment and finalize produces: graph committed, `.index-dirty`
    /// present and naming a dead writer, `.pagerank.json` still holding
    /// pre-crash scores, `.generation` still at the pre-crash canonical value.
    fn abandon_publication_after_commit(db_path: &Path, publisher: &str) -> (i32, u64) {
        let generation_path = crate::sidecar_path(db_path, ".generation");
        let pagerank_path = crate::sidecar_path(db_path, ".pagerank.json");
        let marker_path = crate::sidecar_path(db_path, ".index-dirty");

        let store = GraphStore::open_or_create(db_path).unwrap();
        store.bump_graph_generation();
        store.save_graph_generation(&generation_path).unwrap();
        let canonical = store.graph_generation();
        // A PageRank sidecar that predates the commit below. Serving it after
        // the commit would be the silent-wrong-ranks outcome the guard exists
        // to prevent, so recovery must overwrite rather than preserve it.
        write_pagerank_fixture(
            &store,
            &pagerank_path,
            HashMap::from([("stale-precrash-score".to_string(), 1.0)]),
        );

        let lease = establish_index_publication_marker_with_io(
            &store,
            Some(db_path),
            "crashing publisher",
            &FileSystemIndexEpilogueIo,
        )
        .unwrap();
        insert_publication_graph(&store, publisher);
        insert_publication_notes(&store, publisher);
        // The process dies here: the lease is process-local and simply
        // vanishes, while the marker and both sidecars are durable.
        drop(lease);
        drop(store);

        let pid = reaped_child_pid();
        write_marker_with_pid(&marker_path, pid, None);
        (pid, canonical)
    }

    #[test]
    fn abandoned_publication_recovers_on_read_write_open_with_no_manual_intervention() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let marker_path = crate::sidecar_path(&db_path, ".index-dirty");
        let pagerank_path = crate::sidecar_path(&db_path, ".pagerank.json");

        let (dead_pid, canonical) = abandon_publication_after_commit(&db_path, "crashed");
        assert!(marker_path.exists(), "the crash must leave the marker set");

        // The whole point: an ordinary read-write open, nothing else.
        let store = open_store_for_writing_with_recovery(&db_path).unwrap();

        assert!(
            !marker_path.exists(),
            "recovery must retire the marker for pid {dead_pid}"
        );
        assert!(!store.is_index_publication_dirty());
        assert_eq!(
            store.graph_generation(),
            canonical + 2,
            "recovery publishes the clean N+2 successor"
        );

        // PageRank must reflect the COMMITTED graph, not the pre-crash sidecar.
        let scores = store.pagerank_scores().unwrap();
        assert!(
            !scores.contains_key("stale-precrash-score"),
            "the pre-crash PageRank sidecar must not survive recovery: {scores:?}"
        );
        assert!(
            scores.contains_key("sym:publisher-crashed:source"),
            "recovered PageRank must cover the committed graph: {scores:?}"
        );
        // The sentinel disappearing proves only that SOMETHING was rewritten.
        // It cannot distinguish a correct unified recompute from one that
        // silently dropped every note rank, so assert the note side explicitly:
        // `compute_pagerank` REPLACES the whole map, and the canonical sidecar
        // on a brain database is unified, so a `code_only()` recovery would be
        // data loss wearing the shape of a fix.
        for note in ["note:crashed:one", "note:crashed:two"] {
            assert!(
                scores.contains_key(note),
                "recovery must rank the note side too, not just code: {note} missing from \
                 {} entries",
                scores.len()
            );
        }

        drop(store);
        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        assert!(!reopened.is_index_publication_dirty());
        assert_eq!(reopened.graph_generation(), canonical + 2);
        reopened.load_pagerank_cache(&pagerank_path).unwrap();
        let persisted = reopened.pagerank_scores().unwrap();
        assert!(
            persisted.contains_key("sym:publisher-crashed:source"),
            "the persisted PageRank sidecar must reflect the committed graph"
        );
        assert!(
            persisted.contains_key("note:crashed:one")
                && persisted.contains_key("note:crashed:two"),
            "and must retain note ranks on disk, not only in memory: {persisted:?}"
        );
    }

    #[test]
    fn a_read_only_open_reports_the_abandoned_publication_and_never_clears_it() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let marker_path = crate::sidecar_path(&db_path, ".index-dirty");

        let (dead_pid, _) = abandon_publication_after_commit(&db_path, "readonly");

        let store = GraphStore::open_read_only(&db_path).unwrap();
        let outcome = recover_abandoned_index_publication(&store, false).unwrap();
        assert_eq!(
            outcome,
            IndexPublicationRecovery::ReadOnlyStore {
                abandoned_writer_pid: dead_pid
            },
            "a read-only caller must report, never repair"
        );
        assert!(!outcome.recovered());
        assert!(
            marker_path.exists(),
            "a read-only open must preserve the marker"
        );
        assert!(
            store.is_index_publication_dirty(),
            "a read-only open must keep failing closed"
        );
    }

    #[test]
    fn a_live_publication_is_never_recovered_out_from_under_its_writer() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let marker_path = crate::sidecar_path(&db_path, ".index-dirty");
        let generation_path = crate::sidecar_path(&db_path, ".generation");

        let store = GraphStore::open_or_create(&db_path).unwrap();
        store.bump_graph_generation();
        store.save_graph_generation(&generation_path).unwrap();

        let lease = establish_index_publication_marker_with_io(
            &store,
            Some(&db_path),
            "live publisher",
            &FileSystemIndexEpilogueIo,
        )
        .unwrap();
        let dirty_generation = store.graph_generation();

        // The marker names THIS process, which is alive: liveness alone must
        // stop recovery, before the lease is even consulted.
        let outcome = recover_abandoned_index_publication(&store, true).unwrap();
        assert_eq!(
            outcome,
            IndexPublicationRecovery::WriterAlive {
                pid: std::process::id() as i32
            }
        );
        assert!(marker_path.exists());
        assert_eq!(store.graph_generation(), dirty_generation);

        // And with a DEAD recorded pid but the lease still held in-process,
        // the second half of the predicate must decline too.
        write_marker_with_pid(&marker_path, reaped_child_pid(), None);
        assert_eq!(
            recover_abandoned_index_publication(&store, true).unwrap(),
            IndexPublicationRecovery::LeaseHeld,
            "a held lease means the publication is in flight, not abandoned"
        );
        assert!(marker_path.exists());

        // The live publisher still finishes normally.
        insert_publication_graph(&store, "live");
        finalize_committed_index_with_io(
            lease,
            Some(&db_path),
            "live publisher",
            &FileSystemIndexEpilogueIo,
            true,
        )
        .unwrap();
        assert!(!marker_path.exists());
    }

    #[test]
    fn an_undeterminable_marker_is_reported_not_recovered() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let marker_path = crate::sidecar_path(&db_path, ".index-dirty");
        {
            let _ = GraphStore::open_or_create(&db_path).unwrap();
        }
        // A directory where the marker file belongs: `try_exists` succeeds but
        // the read cannot. `is_index_publication_dirty` is
        // `try_exists().unwrap_or(true)`, so this reads as permanently dirty by
        // design — recovery must not mistake it for an abandoned publication.
        std::fs::create_dir(&marker_path).unwrap();

        let store = GraphStore::open_or_create(&db_path).unwrap();
        let outcome = recover_abandoned_index_publication(&store, true).unwrap();
        assert!(
            matches!(outcome, IndexPublicationRecovery::Undeterminable { .. }),
            "cannot tell is not abandoned: {outcome:?}"
        );
        assert!(marker_path.exists());
        assert!(store.is_index_publication_dirty());
    }

    #[test]
    fn an_unattributed_marker_is_not_auto_recovered() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let marker_path = crate::sidecar_path(&db_path, ".index-dirty");
        {
            let _ = GraphStore::open_or_create(&db_path).unwrap();
        }
        // The pre-nw-C1 hand-created marker, and what an older binary's
        // truncated write leaves behind.
        std::fs::write(&marker_path, b"dirty").unwrap();

        let store = GraphStore::open_or_create(&db_path).unwrap();
        assert_eq!(
            recover_abandoned_index_publication(&store, true).unwrap(),
            IndexPublicationRecovery::WriterUnattributed
        );
        assert!(marker_path.exists());
    }

    #[test]
    fn a_dirty_marker_with_a_missing_generation_recovers_instead_of_exhausting() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let marker_path = crate::sidecar_path(&db_path, ".index-dirty");
        let generation_path = crate::sidecar_path(&db_path, ".generation");

        {
            let store = GraphStore::open_or_create(&db_path).unwrap();
            insert_publication_graph(&store, "nogeneration");
        }
        // Marker present, `.generation` absent: the fail-closed load takes
        // `canonical = u64::MAX`.
        let dead_pid = reaped_child_pid();
        write_marker_with_pid(&marker_path, dead_pid, None);
        assert!(!generation_path.exists());

        // Establish that the second wedge exists, so this test would catch a
        // recovery that quietly ignored the `u64::MAX` arm.
        {
            let store = GraphStore::open_or_create(&db_path).unwrap();
            assert_eq!(store.graph_generation(), u64::MAX);
            let error = establish_index_publication_marker_with_io(
                &store,
                Some(&db_path),
                "blocked publisher",
                &FileSystemIndexEpilogueIo,
            )
            .expect_err("preflight must overflow while the base is u64::MAX");
            assert!(
                format!("{error}").contains("graph generation exhausted during index publication"),
                "the second wedge surfaces as a DIFFERENT error string: {error}"
            );
        }

        let store = open_store_for_writing_with_recovery(&db_path).unwrap();
        assert!(
            !marker_path.exists(),
            "recovery must re-derive rather than add to u64::MAX"
        );
        assert!(!store.is_index_publication_dirty());
        assert!(generation_path.exists());
        assert_eq!(
            store.graph_generation(),
            2,
            "re-derivation falls back to the same canonical 0 a clean open would use"
        );
        assert!(
            store
                .pagerank_scores()
                .unwrap()
                .contains_key("sym:publisher-nogeneration:source")
        );
    }

    #[test]
    fn recovery_reports_a_deliberately_dirty_cancelled_publication_distinctly() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let marker_path = crate::sidecar_path(&db_path, ".index-dirty");
        let generation_path = crate::sidecar_path(&db_path, ".generation");

        let store = GraphStore::open_or_create(&db_path).unwrap();
        store.bump_graph_generation();
        store.save_graph_generation(&generation_path).unwrap();

        let lease = establish_index_publication_marker_with_io(
            &store,
            Some(&db_path),
            "cancelled publisher",
            &FileSystemIndexEpilogueIo,
        )
        .unwrap();
        insert_publication_graph(&store, "cancelled");
        // The committed-after-cancellation path: publish_clean = false.
        finalize_committed_index_for_scope_with_io(
            lease,
            Some(&db_path),
            "cancelled publisher",
            &FileSystemIndexEpilogueIo,
            None,
            false,
        )
        .unwrap();
        assert!(
            marker_path.exists(),
            "a cancelled-but-committed run must stay dirty on purpose"
        );
        let state = nestweaver_store::index_publication::read_marker(&db_path);
        assert!(
            state.record().unwrap().is_deliberately_dirty(),
            "the deliberate hold must be recorded in the marker payload"
        );

        // Still owned by a live process → never recovered.
        assert_eq!(
            recover_abandoned_index_publication(&store, true).unwrap(),
            IndexPublicationRecovery::WriterAlive {
                pid: std::process::id() as i32
            }
        );
        drop(store);

        // Once that writer is gone, the publication IS reconciled — the
        // sidecars really do predate the commit either way — but the outcome
        // says so, so the `index --force` guidance is not silently lost.
        write_marker_with_pid(&marker_path, reaped_child_pid(), Some("cancelled"));
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let outcome = recover_abandoned_index_publication(&store, true).unwrap();
        assert!(outcome.recovered());
        assert!(
            matches!(
                outcome,
                IndexPublicationRecovery::Recovered {
                    was_cancelled_run: true,
                    ..
                }
            ),
            "{outcome:?}"
        );
        assert!(outcome.describe().contains("--force"));
        assert!(!marker_path.exists());
    }

    /// A rewrite must never be observable as a partial payload.
    ///
    /// With truncate-in-place this fails: a reader stat-ing between `truncate`
    /// and `write_all` sees a zero-byte marker, which parses to no pid and no
    /// timestamp — and the wedged predicate reads that as "unattributable,
    /// therefore WEDGED", telling an operator to force-repair a healthy
    /// in-flight publication. With temp-file + rename the state is
    /// unreachable, so a reader only ever sees the old payload or the new one.
    #[test]
    fn rewriting_the_marker_is_never_observable_as_a_partial_payload() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let marker_path = crate::sidecar_path(&db_path, ".index-dirty");
        FileSystemIndexEpilogueIo
            .establish_marker(&marker_path)
            .unwrap();

        let stop = AtomicBool::new(false);
        let observations = AtomicUsize::new(0);
        let partials = AtomicUsize::new(0);

        std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                while !stop.load(AtomicOrdering::Acquire) {
                    let state = nestweaver_store::index_publication::read_marker(&db_path);
                    if let Some(record) = state.record() {
                        observations.fetch_add(1, AtomicOrdering::Relaxed);
                        // A present marker written by this process always
                        // carries a pid. No pid means a torn read.
                        if record.writer_pid.is_none() {
                            partials.fetch_add(1, AtomicOrdering::Relaxed);
                        }
                    }
                }
            });
            for i in 0..300 {
                if i % 2 == 0 {
                    FileSystemIndexEpilogueIo
                        .establish_marker(&marker_path)
                        .unwrap();
                } else {
                    FileSystemIndexEpilogueIo
                        .stamp_marker_reason(
                            &marker_path,
                            nestweaver_store::index_publication::MARKER_REASON_CANCELLED,
                        )
                        .unwrap();
                }
            }
            stop.store(true, AtomicOrdering::Release);
            reader.join().unwrap();
        });

        assert!(
            observations.load(AtomicOrdering::Relaxed) > 0,
            "the reader must actually have observed the marker"
        );
        assert_eq!(
            partials.load(AtomicOrdering::Relaxed),
            0,
            "a marker rewrite must never be observable as a partial payload, or a \
             healthy in-flight publication is reported WEDGED"
        );
        // And the marker is still valid and attributable afterwards.
        let final_state = nestweaver_store::index_publication::read_marker(&db_path);
        assert_eq!(
            final_state.record().unwrap().writer_pid,
            Some(std::process::id() as i32)
        );
    }

    #[test]
    fn force_recovers_an_unattributed_marker_that_auto_heal_must_decline() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let marker_path = crate::sidecar_path(&db_path, ".index-dirty");
        {
            let store = GraphStore::open_or_create(&db_path).unwrap();
            insert_publication_graph(&store, "forced");
            insert_publication_notes(&store, "forced");
        }
        // The legacy / hand-created marker: present, nothing to attribute.
        std::fs::write(&marker_path, b"dirty").unwrap();

        let store = GraphStore::open_or_create(&db_path).unwrap();
        assert_eq!(
            recover_abandoned_index_publication(&store, true).unwrap(),
            IndexPublicationRecovery::WriterUnattributed,
            "auto-heal must still decline what it cannot prove abandoned"
        );
        assert!(marker_path.exists());

        let outcome = force_recover_index_publication(&store).unwrap();
        assert!(outcome.recovered(), "{outcome:?}");
        assert!(
            matches!(
                outcome,
                IndexPublicationRecovery::Recovered {
                    abandoned_writer_pid: None,
                    ..
                }
            ),
            "a forced recovery names no pid because there was none: {outcome:?}"
        );
        assert!(!marker_path.exists());
        assert!(!store.is_index_publication_dirty());
        let scores = store.pagerank_scores().unwrap();
        assert!(scores.contains_key("sym:publisher-forced:source"));
        assert!(scores.contains_key("note:forced:one"));
    }

    #[test]
    fn force_never_overrides_a_live_writer() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let marker_path = crate::sidecar_path(&db_path, ".index-dirty");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        write_marker_with_pid(&marker_path, std::process::id() as i32, None);

        assert_eq!(
            force_recover_index_publication(&store).unwrap(),
            IndexPublicationRecovery::WriterAlive {
                pid: std::process::id() as i32
            },
            "--force overrides 'cannot prove dead', never 'provably alive'"
        );
        assert!(marker_path.exists());
    }

    #[test]
    fn force_recovers_an_undeterminable_marker_only_when_it_can_be_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let marker_path = crate::sidecar_path(&db_path, ".index-dirty");
        {
            let store = GraphStore::open_or_create(&db_path).unwrap();
            insert_publication_graph(&store, "undet");
        }
        std::fs::create_dir(&marker_path).unwrap();

        let store = GraphStore::open_or_create(&db_path).unwrap();
        // Auto-heal always declines: "cannot tell" is never "abandoned".
        assert!(matches!(
            recover_abandoned_index_publication(&store, true).unwrap(),
            IndexPublicationRecovery::Undeterminable { .. }
        ));
        // Forced, it proceeds — and honestly reports the failure to remove a
        // directory rather than pretending the publication is clean.
        let forced = force_recover_index_publication(&store);
        assert!(
            forced.is_err(),
            "clearing a directory-shaped marker must fail loudly: {forced:?}"
        );
        assert!(marker_path.exists(), "and must leave it in place");
        assert!(store.is_index_publication_dirty());
    }

    #[test]
    fn a_clean_store_reports_clean_and_an_in_memory_store_has_nothing_to_recover() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        assert_eq!(
            recover_abandoned_index_publication(&store, true).unwrap(),
            IndexPublicationRecovery::Clean
        );
        let memory = GraphStore::in_memory().unwrap();
        assert_eq!(
            recover_abandoned_index_publication(&memory, true).unwrap(),
            IndexPublicationRecovery::NotFileBacked
        );
    }

    /// Liveness deadline for the cross-thread handoffs in the overlapping
    /// publication test.
    ///
    /// Every wait below is backed by a condvar or an mpsc channel, so it wakes
    /// the instant the event it names actually happens. The deadline therefore
    /// only bounds a genuine hang (a lost notification, a deadlocked lease) —
    /// it is NOT a throughput budget, and nothing about the property under
    /// test depends on how promptly the OS schedules the spawned publisher.
    /// A deadline sized near the test's own idle runtime made a loaded machine
    /// look like a broken lease; a generous one costs nothing when healthy and
    /// still fails in bounded time when the handoff is truly broken. The
    /// ordering assertions in the test remain exact.
    ///
    /// 30s is ~6x the whole test's observed runtime under load (2.1-5.5s), so
    /// it is unreachable without a real deadlock, while still failing fast
    /// enough to be useful: the CI job that runs this sets no `timeout-minutes`,
    /// so a hung wait would otherwise sit until GitHub's 360-minute default.
    const PUBLICATION_HANDOFF_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

    #[test]
    fn overlapping_publications_serialize_before_the_second_mutation() {
        use std::sync::{Arc, mpsc};

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
            store.wait_for_index_publication_waiters(1, PUBLICATION_HANDOFF_DEADLINE),
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
            .recv_timeout(PUBLICATION_HANDOFF_DEADLINE)
            .expect("publisher B must establish after A finalizes");
        b_established.unwrap();
        continue_tx.send(()).unwrap();
        done_rx
            .recv_timeout(PUBLICATION_HANDOFF_DEADLINE)
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
        let expected_scores = store.pagerank_scores().unwrap();
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
        assert_eq!(reopened.pagerank_scores().unwrap(), expected_scores);
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
        assert!(reopened.pagerank_scores().is_err());
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
            true,
        )
        .unwrap();

        assert!(!marker_path.exists());
        assert_eq!(store.graph_generation(), canonical_generation + 2);
        let expected_scores = store.pagerank_scores().unwrap();
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
        assert_eq!(reopened.pagerank_scores().unwrap(), expected_scores);
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
                .any(|c| c.uid.ends_with(":http:POST:/v1/approvals")),
            "expected declared contract; got {:?}",
            contracts.iter().map(|c| &c.uid).collect::<Vec<_>>()
        );

        // The handler implements it (base-path-inferred since @PostMapping has
        // no sub-path; UID still matches the spec's POST /v1/approvals).
        let implemented = store.list_implemented_contract_uids().unwrap();
        assert!(
            implemented
                .iter()
                .any(|uid| uid.ends_with(":http:POST:/v1/approvals")),
            "expected IMPLEMENTS_CONTRACT edge; implemented: {implemented:?}"
        );
    }

    #[test]
    fn identical_contract_shapes_are_owned_and_drifted_per_repository() {
        let dir = tempfile::tempdir().unwrap();
        let store = GraphStore::in_memory().unwrap();
        let spec = "openapi: 3.0.0\ninfo: { title: t, version: \"1\" }\npaths:\n  /health:\n    get:\n      responses: { \"200\": { description: ok } }\n";
        let implemented_dir = dir.path().join("implemented");
        let declared_only_dir = dir.path().join("declared-only");
        fs::create_dir_all(&implemented_dir).unwrap();
        fs::create_dir_all(&declared_only_dir).unwrap();
        fs::write(implemented_dir.join("openapi.yaml"), spec).unwrap();
        fs::write(declared_only_dir.join("openapi.yaml"), spec).unwrap();
        fs::write(
            implemented_dir.join("HealthController.java"),
            "@RestController\npublic class HealthController {\n  @GetMapping(\"/health\")\n  public void health() {}\n}\n",
        )
        .unwrap();

        let implemented_url = "https://example.com/implemented";
        let declared_only_url = "https://example.com/declared-only";
        for (root, url, sha) in [
            (&implemented_dir, implemented_url, "a"),
            (&declared_only_dir, declared_only_url, "b"),
        ] {
            let reader = crate::content_reader::FilesystemReader::new(root);
            index_into_store(
                &reader, &store, "test", url, sha, None, None, None, None, None, None,
            )
            .unwrap();
        }

        let implemented_repo = nestweaver_schema::repo_uid("test", implemented_url);
        let declared_only_repo = nestweaver_schema::repo_uid("test", declared_only_url);
        let implemented_uid = nestweaver_schema::scoped_contract_uid(
            &implemented_repo,
            "http",
            Some("GET"),
            Some("/health"),
            None,
        );
        let declared_only_uid = nestweaver_schema::scoped_contract_uid(
            &declared_only_repo,
            "http",
            Some("GET"),
            Some("/health"),
            None,
        );
        assert_ne!(implemented_uid, declared_only_uid);
        assert_eq!(
            store
                .list_implemented_contract_uids_for_repo(&implemented_repo)
                .unwrap(),
            vec![implemented_uid]
        );
        assert!(
            store
                .list_implemented_contract_uids_for_repo(&declared_only_repo)
                .unwrap()
                .is_empty()
        );
        let drift = crate::contracts::drift_for_store(&store, Some(&declared_only_repo)).unwrap();
        assert_eq!(drift.declared_not_implemented.len(), 1);
        assert_eq!(drift.declared_not_implemented[0].uid, declared_only_uid);
    }

    #[test]
    fn contract_v2_migration_purges_legacy_globally_and_tracks_repo_debt() {
        let store = GraphStore::in_memory().unwrap();
        let repo_a = "repo:test:a";
        let repo_b = "repo:test:b";
        for uid in [repo_a, repo_b] {
            store
                .insert_repo(&nestweaver_schema::Repo {
                    uid: uid.to_string(),
                    url: format!("https://example.com/{uid}"),
                    indexed_sha: "old".into(),
                    staleness_commits_behind: 0,
                    instance_id: "test".into(),
                    name: None,
                    root_path: None,
                })
                .unwrap();
        }
        store
            .batch_insert_contracts(&[nestweaver_schema::Contract {
                uid: "contract:http:GET:/legacy".into(),
                kind: "http".into(),
                verb: Some("GET".into()),
                path: Some("/legacy".into()),
                operation_id: None,
                repo_uid: repo_a.into(),
                source_path: "legacy.yaml".into(),
                confidence: 1.0,
            }])
            .unwrap();
        assert_eq!(
            store.contract_derivation_failures(None).unwrap(),
            vec![repo_a.to_string(), repo_b.to_string()]
        );

        let scoped = nestweaver_schema::Contract {
            uid: nestweaver_schema::scoped_contract_uid(
                repo_a,
                "http",
                Some("GET"),
                Some("/current"),
                None,
            ),
            kind: "http".into(),
            verb: Some("GET".into()),
            path: Some("/current".into()),
            operation_id: None,
            repo_uid: repo_a.into(),
            source_path: "openapi.yaml".into(),
            confidence: 1.0,
        };
        let plan = ContractDerivationPlan {
            contracts: vec![scoped.clone()],
            edges: Vec::new(),
            input_hashes: std::collections::BTreeMap::new(),
            observed_input_hashes: std::collections::BTreeMap::new(),
            skipped_files: Vec::new(),
        };
        let transaction = store.begin_transaction().unwrap();
        apply_contract_derivation_on(&transaction, repo_a, &plan).unwrap();
        store.commit_transaction(&transaction).unwrap();
        drop(transaction);

        let all = store.list_contracts(None).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].uid, scoped.uid);
        assert_eq!(
            store.contract_derivation_failures(None).unwrap(),
            vec![repo_b.to_string()]
        );
    }

    #[test]
    fn contract_collector_skips_reported_oversize_before_reading() {
        struct OversizeReader {
            root: PathBuf,
        }
        impl crate::content_reader::ContentReader for OversizeReader {
            fn read_file(&self, _rel_path: &Path) -> anyhow::Result<String> {
                panic!("oversized contract candidate must not be read")
            }
            fn list_files(&self) -> anyhow::Result<Vec<PathBuf>> {
                Ok(vec![PathBuf::from("HugeController.java")])
            }
            fn file_meta(&self, _rel_path: &Path) -> anyhow::Result<Option<(u64, u64)>> {
                Ok(Some((
                    0,
                    crate::index_limits::DEFAULT_MAX_SOURCE_FILE_BYTES + 1,
                )))
            }
            fn root(&self) -> &Path {
                &self.root
            }
            fn version_id(&self) -> &str {
                "oversize-test"
            }
        }

        let reader = OversizeReader {
            root: PathBuf::from("/unused"),
        };
        let (specs, handlers, symbols, skipped) = collect_contract_derivation_inputs(
            &reader,
            "repo:test:oversize",
            "https://example.com/oversize",
            false,
        )
        .unwrap();
        assert!(specs.is_empty());
        assert!(handlers.is_empty());
        assert!(symbols.is_empty());
        assert_eq!(skipped.len(), 1);
    }

    #[test]
    fn strict_contract_collector_ignores_unreadable_irrelevant_languages() {
        struct IrrelevantReader {
            root: PathBuf,
        }
        impl crate::content_reader::ContentReader for IrrelevantReader {
            fn read_file(&self, _rel_path: &Path) -> anyhow::Result<String> {
                panic!("irrelevant language must be filtered before reading")
            }
            fn list_files(&self) -> anyhow::Result<Vec<PathBuf>> {
                Ok(vec![PathBuf::from("unreadable.py")])
            }
            fn file_meta(&self, _rel_path: &Path) -> anyhow::Result<Option<(u64, u64)>> {
                panic!("irrelevant language must be filtered before metadata")
            }
            fn root(&self) -> &Path {
                &self.root
            }
            fn version_id(&self) -> &str {
                "irrelevant-language-test"
            }
        }

        let reader = IrrelevantReader {
            root: PathBuf::from("/unused"),
        };
        let (specs, handlers, symbols, skipped) = collect_contract_derivation_inputs(
            &reader,
            "repo:test:irrelevant",
            "https://example.com/irrelevant",
            true,
        )
        .unwrap();
        assert!(specs.is_empty());
        assert!(handlers.is_empty());
        assert!(symbols.is_empty());
        assert!(skipped.is_empty());
    }

    #[test]
    fn watcher_contract_plan_rejects_create_delete_and_second_save_races() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let get = "openapi: 3.0.0\ninfo: { title: t, version: \"1\" }\npaths:\n  /items:\n    get:\n      responses: { \"200\": { description: ok } }\n";
        let post = "openapi: 3.0.0\ninfo: { title: t, version: \"1\" }\npaths:\n  /items:\n    post:\n      responses: { \"200\": { description: ok } }\n";
        let spec = repo.join("openapi.yaml");
        fs::write(&spec, get).unwrap();
        let reader = crate::content_reader::FilesystemReader::new(&repo);

        let created = repo.join("openapi.v2.yaml");
        let error = prepare_watcher_contract_derivation_with_hooks(
            &reader,
            "repo:test:watch-race",
            "file:///watch-race",
            || {},
            || fs::write(&created, post).unwrap(),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("openapi.v2.yaml"));
        fs::remove_file(&created).unwrap();

        let error = prepare_watcher_contract_derivation_with_hooks(
            &reader,
            "repo:test:watch-race",
            "file:///watch-race",
            || {},
            || fs::remove_file(&spec).unwrap(),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("openapi.yaml"));
        fs::write(&spec, get).unwrap();

        let error = prepare_watcher_contract_derivation_with_hooks(
            &reader,
            "repo:test:watch-race",
            "file:///watch-race",
            || {},
            || fs::write(&spec, post).unwrap(),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("openapi.yaml"));

        fs::write(&spec, get).unwrap();
        let controller = repo.join("ItemsController.java");
        let controller_get = "@RestController\n@RequestMapping(\"/items\")\npublic class ItemsController { @GetMapping public void get() {} }\n";
        let controller_post = "@RestController\n@RequestMapping(\"/items\")\npublic class ItemsController { @PostMapping public void post() {} }\n";
        fs::write(&controller, controller_get).unwrap();
        let error = prepare_watcher_contract_derivation_with_hooks(
            &reader,
            "repo:test:watch-race",
            "file:///watch-race",
            || {},
            || fs::write(&controller, controller_post).unwrap(),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("ItemsController.java"));

        fs::write(&controller, controller_get).unwrap();
        let error = prepare_watcher_contract_derivation_with_hooks(
            &reader,
            "repo:test:watch-race",
            "file:///watch-race",
            || {},
            || fs::remove_file(&controller).unwrap(),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("ItemsController.java"));

        let error = prepare_watcher_contract_derivation_with_hooks(
            &reader,
            "repo:test:watch-race",
            "file:///watch-race",
            || {},
            || fs::write(&controller, controller_get).unwrap(),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("ItemsController.java"));

        fs::write(&spec, get).unwrap();
        let error = prepare_watcher_contract_derivation_with_hooks(
            &reader,
            "repo:test:watch-race",
            "file:///watch-race",
            || fs::write(&spec, post).unwrap(),
            || fs::write(&spec, get).unwrap(),
        )
        .err()
        .expect("old→new→old must reject the hybrid plan");
        assert!(error.to_string().contains("openapi.yaml"));

        let error = prepare_watcher_contract_derivation_with_hooks(
            &reader,
            "repo:test:watch-race",
            "file:///watch-race",
            || fs::remove_file(&spec).unwrap(),
            || fs::write(&spec, get).unwrap(),
        )
        .err()
        .expect("spec delete→identical recreate must reject the hybrid plan");
        assert!(error.to_string().contains("openapi.yaml"));

        let error = prepare_watcher_contract_derivation_with_hooks(
            &reader,
            "repo:test:watch-race",
            "file:///watch-race",
            || fs::remove_file(&controller).unwrap(),
            || fs::write(&controller, controller_get).unwrap(),
        )
        .err()
        .expect("controller delete→identical recreate must reject the hybrid plan");
        assert!(error.to_string().contains("ItemsController.java"));
    }

    #[test]
    fn incremental_refreshes_contracts_like_force_and_rolls_back_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let incremental_db = dir.path().join("incremental.lbug");
        let forced_db = dir.path().join("forced.lbug");
        fs::create_dir_all(&repo).unwrap();
        let spec = |methods: &str| {
            format!(
                "openapi: 3.0.0\ninfo: {{ title: t, version: \"1.0\" }}\npaths:\n  /v1/items:\n{methods}"
            )
        };
        fs::write(
            repo.join("openapi.yaml"),
            spec(
                "    get:\n      operationId: listItems\n      responses: { \"200\": { description: ok } }\n",
            ),
        )
        .unwrap();
        fs::write(
            repo.join("ItemsController.java"),
            "@RestController\n@RequestMapping(\"/v1/items\")\npublic class ItemsController {\n  @GetMapping\n  public void list() {}\n}\n",
        )
        .unwrap();

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
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "initial GET"]);
        let first_sha = git(&["rev-parse", "HEAD"]);
        let repo_url = "https://example.com/contract-refresh";
        let repo_uid = nestweaver_schema::repo_uid("test", repo_url);
        index_directory(&repo, &incremental_db, "test", repo_url, &first_sha).unwrap();
        index_directory(&repo, &forced_db, "test", repo_url, &first_sha).unwrap();

        let other_repo = dir.path().join("other-repo");
        fs::create_dir_all(&other_repo).unwrap();
        fs::write(
            other_repo.join("openapi.yaml"),
            "openapi: 3.0.0\ninfo: { title: other, version: \"1.0\" }\npaths:\n  /other:\n    get:\n      responses: { \"200\": { description: ok } }\n",
        )
        .unwrap();
        let other_url = "https://example.com/contract-refresh-other";
        let other_uid = nestweaver_schema::repo_uid("test", other_url);
        index_directory(&other_repo, &incremental_db, "test", other_url, "other-sha").unwrap();
        let other_store = GraphStore::open_or_create(&incremental_db).unwrap();
        let other_contracts = |store: &GraphStore| {
            let mut contracts: Vec<(String, String)> = store
                .list_contracts(Some(&other_uid))
                .unwrap()
                .into_iter()
                .map(|contract| (contract.uid, contract.source_path))
                .collect();
            contracts.sort();
            contracts
        };
        let other_before = other_contracts(&other_store);
        drop(other_store);

        fs::rename(repo.join("openapi.yaml"), repo.join("openapi.v2.yaml")).unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "rename spec only"]);
        let rename_sha = git(&["rev-parse", "HEAD"]);
        let rename_result = incremental_index(&repo, &incremental_db, "test", repo_url).unwrap();
        assert!(!rename_result.fell_back_to_full);
        let renamed_store = GraphStore::open_or_create(&incremental_db).unwrap();
        let renamed_get = renamed_store
            .list_contracts(Some(&repo_uid))
            .unwrap()
            .into_iter()
            .find(|contract| {
                contract.uid == owned_contract_uid(&repo_uid, "contract:http:GET:/v1/items")
            })
            .expect("GET survives a spec-only rename");
        assert_eq!(renamed_get.source_path, "openapi.v2.yaml");
        assert!(
            renamed_store
                .list_implemented_contract_uids()
                .unwrap()
                .contains(&renamed_get.uid),
            "unchanged controller must be relinked after spec-only rename"
        );
        drop(renamed_store);
        let tiered = index_directory(&repo, &forced_db, "test", repo_url, &rename_sha).unwrap();
        assert!(
            tiered.files_unchanged > 0,
            "control must exercise the partial/tiered full path"
        );
        let tiered_store = GraphStore::open_or_create(&forced_db).unwrap();
        assert_eq!(
            tiered_store
                .list_contracts(Some(&repo_uid))
                .unwrap()
                .into_iter()
                .find(|contract| {
                    contract.uid == owned_contract_uid(&repo_uid, "contract:http:GET:/v1/items")
                })
                .unwrap()
                .source_path,
            "openapi.v2.yaml"
        );
        drop(tiered_store);

        fs::write(
            repo.join("openapi.v2.yaml"),
            spec(
                "    get:\n      operationId: listItems\n      responses: { \"200\": { description: ok } }\n    post:\n      operationId: createItem\n      responses: { \"200\": { description: ok } }\n",
            ),
        )
        .unwrap();
        fs::write(
            repo.join("ItemsController.java"),
            "@RestController\n@RequestMapping(\"/v1/items\")\npublic class ItemsController {\n  @GetMapping\n  public void list() {}\n  @PostMapping\n  public void create() {}\n}\n",
        )
        .unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "add POST"]);
        let second_sha = git(&["rev-parse", "HEAD"]);

        let incremental = incremental_index(&repo, &incremental_db, "test", repo_url).unwrap();
        assert!(!incremental.fell_back_to_full, "must exercise incremental");
        index_directory_with_options(&repo, &forced_db, "test", repo_url, &second_sha, true, None)
            .unwrap();

        let snapshot = |store: &GraphStore| {
            let mut contracts: Vec<(String, String)> = store
                .list_contracts(Some(&repo_uid))
                .unwrap()
                .into_iter()
                .filter(|contract| contract.repo_uid == repo_uid)
                .map(|contract| (contract.uid, contract.source_path))
                .collect();
            contracts.sort();
            let mut implemented = store.list_implemented_contract_uids().unwrap();
            implemented.sort();
            (contracts, implemented)
        };
        let implementation_pairs = |store: &GraphStore| {
            let mut pairs = Vec::new();
            for symbol in store.lookup_symbols_by_repo(&repo_uid).unwrap() {
                for (contract_uid, _) in store.contracts_implemented_by(&symbol.uid).unwrap() {
                    pairs.push((symbol.name.clone(), contract_uid));
                }
            }
            pairs.sort();
            pairs
        };
        let drift = |store: &GraphStore| {
            crate::contracts::drift_envelope(
                crate::contracts::drift_for_store(store, Some(&repo_uid)).unwrap(),
                50,
            )
        };
        let incremental_store = GraphStore::open_or_create(&incremental_db).unwrap();
        let forced_store = GraphStore::open_or_create(&forced_db).unwrap();
        let incremental_snapshot = snapshot(&incremental_store);
        assert_eq!(incremental_snapshot, snapshot(&forced_store));
        assert_eq!(
            implementation_pairs(&incremental_store),
            implementation_pairs(&forced_store)
        );
        assert_eq!(drift(&incremental_store), drift(&forced_store));
        assert_eq!(
            implementation_pairs(&incremental_store),
            vec![
                (
                    "create".to_string(),
                    owned_contract_uid(&repo_uid, "contract:http:POST:/v1/items"),
                ),
                (
                    "list".to_string(),
                    owned_contract_uid(&repo_uid, "contract:http:GET:/v1/items"),
                ),
            ]
        );
        assert_eq!(
            other_contracts(&incremental_store),
            other_before,
            "refreshing one repo must not alter another repo's contracts"
        );
        assert_eq!(
            incremental_snapshot.0,
            vec![
                (
                    owned_contract_uid(&repo_uid, "contract:http:GET:/v1/items"),
                    "openapi.v2.yaml".to_string(),
                ),
                (
                    owned_contract_uid(&repo_uid, "contract:http:POST:/v1/items"),
                    "openapi.v2.yaml".to_string(),
                ),
            ]
        );
        assert_eq!(
            incremental_snapshot.1,
            vec![
                owned_contract_uid(&repo_uid, "contract:http:GET:/v1/items"),
                owned_contract_uid(&repo_uid, "contract:http:POST:/v1/items"),
            ]
        );
        drop(incremental_store);
        drop(forced_store);

        fs::write(
            repo.join("openapi.v2.yaml"),
            spec(
                "    post:\n      operationId: createItem\n      responses: { \"200\": { description: ok } }\n",
            ),
        )
        .unwrap();
        fs::write(
            repo.join("ItemsController.java"),
            "@RestController\n@RequestMapping(\"/v1/items\")\npublic class ItemsController {\n  @PostMapping\n  public void create() {}\n}\n",
        )
        .unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "delete GET"]);
        let third_sha = git(&["rev-parse", "HEAD"]);
        let deletion = incremental_index(&repo, &incremental_db, "test", repo_url).unwrap();
        assert!(!deletion.fell_back_to_full);
        index_directory_with_options(&repo, &forced_db, "test", repo_url, &third_sha, true, None)
            .unwrap();
        let incremental_store = GraphStore::open_or_create(&incremental_db).unwrap();
        let forced_store = GraphStore::open_or_create(&forced_db).unwrap();
        let post_only_snapshot = snapshot(&incremental_store);
        assert_eq!(post_only_snapshot, snapshot(&forced_store));
        assert_eq!(
            implementation_pairs(&incremental_store),
            implementation_pairs(&forced_store)
        );
        assert_eq!(drift(&incremental_store), drift(&forced_store));
        assert_eq!(
            implementation_pairs(&incremental_store),
            vec![(
                "create".to_string(),
                owned_contract_uid(&repo_uid, "contract:http:POST:/v1/items"),
            )]
        );
        assert_eq!(
            post_only_snapshot.0,
            vec![(
                owned_contract_uid(&repo_uid, "contract:http:POST:/v1/items"),
                "openapi.v2.yaml".to_string(),
            )]
        );
        assert_eq!(
            post_only_snapshot.1,
            vec![owned_contract_uid(
                &repo_uid,
                "contract:http:POST:/v1/items"
            )]
        );

        let generation_path = crate::sidecar_path(&incremental_db, ".generation");
        let generation_before_failure = fs::read(&generation_path).unwrap();
        drop(incremental_store);
        drop(forced_store);
        fs::write(repo.join("openapi.v2.yaml"), "openapi: [malformed").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "malformed spec"]);

        let error = incremental_index(&repo, &incremental_db, "test", repo_url)
            .expect_err("strict incremental contract preflight must fail");
        assert!(
            format!("{error:#}").contains("prepare incremental contract derivation"),
            "unexpected error: {error:#}"
        );
        let incremental_store = GraphStore::open_or_create(&incremental_db).unwrap();
        assert_eq!(snapshot(&incremental_store), post_only_snapshot);
        assert_eq!(
            fs::read(&generation_path).unwrap(),
            generation_before_failure,
            "strict preflight failure must not publish a generation"
        );
        assert_eq!(
            incremental_store
                .lookup_repo(&repo_uid)
                .unwrap()
                .unwrap()
                .indexed_sha,
            third_sha,
            "strict preflight failure must retain the indexed SHA"
        );
        assert_eq!(other_contracts(&incremental_store), other_before);
        assert_eq!(
            incremental_store
                .contract_derivation_failures(Some(&repo_uid))
                .unwrap(),
            vec![repo_uid]
        );
    }

    /// An explicitly invalid prepared plan must leave the previous derived
    /// graph intact. This is the fault seam for atomic replacement tests;
    /// cross-repository UID collisions are no longer a valid way to provoke a
    /// write failure because v2 Contract identities include their owner.
    #[test]
    fn failed_contract_derivation_preserves_existing_contracts() {
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

        let repo_uid = store
            .list_contracts(None)
            .unwrap()
            .first()
            .expect("first index derives at least one contract")
            .repo_uid
            .clone();
        let mut contracts_before: Vec<String> = store
            .list_contracts(Some(&repo_uid))
            .unwrap()
            .into_iter()
            .filter(|c| c.repo_uid == repo_uid)
            .map(|c| c.uid)
            .collect();
        contracts_before.sort();
        let mut implemented_before = store.list_implemented_contract_uids().unwrap();
        implemented_before.sort();
        assert!(!contracts_before.is_empty(), "expected seeded contracts");
        assert!(
            !implemented_before.is_empty(),
            "expected a seeded IMPLEMENTS_CONTRACT edge"
        );

        let invalid = nestweaver_schema::Contract {
            uid: nestweaver_schema::scoped_contract_uid(
                &repo_uid,
                "http",
                Some("GET"),
                Some("/v1/invalid"),
                None,
            ),
            kind: "http".to_string(),
            verb: Some("GET".to_string()),
            path: Some("/v1/invalid".to_string()),
            operation_id: None,
            repo_uid: repo_uid.clone(),
            source_path: "invalid-plan.yaml".to_string(),
            confidence: 1.0,
        };
        let invalid_plan = ContractDerivationPlan {
            contracts: vec![invalid.clone(), invalid],
            edges: Vec::new(),
            input_hashes: std::collections::BTreeMap::new(),
            observed_input_hashes: std::collections::BTreeMap::new(),
            skipped_files: Vec::new(),
        };
        let transaction = store.begin_transaction().unwrap();
        let err = apply_contract_derivation_on(&transaction, &repo_uid, &invalid_plan)
            .expect_err("duplicate rows in an explicit invalid plan must fail COPY");
        drop(transaction);
        assert!(
            err.to_string().contains("COPY Contract"),
            "expected the COPY to be what failed; got {err}"
        );

        // The failed derivation must not have touched the previous graph.
        let mut contracts_after: Vec<String> = store
            .list_contracts(Some(&repo_uid))
            .unwrap()
            .into_iter()
            .filter(|c| c.repo_uid == repo_uid)
            .map(|c| c.uid)
            .collect();
        contracts_after.sort();
        assert_eq!(
            contracts_before, contracts_after,
            "contracts must survive a failed derivation"
        );

        let mut implemented_after = store.list_implemented_contract_uids().unwrap();
        implemented_after.sort();
        assert_eq!(
            implemented_before, implemented_after,
            "IMPLEMENTS_CONTRACT edges must survive a failed derivation"
        );
    }

    /// nw-104: a declared gRPC contract must link to its Rust/tonic
    /// implementation.
    ///
    /// `contracts drift` reported 75 declared RPCs as unimplemented on this very
    /// repo while every one of them was implemented, because nothing ever emitted
    /// an IMPLEMENTS_CONTRACT edge for gRPC. The acceptance criterion for the bug
    /// is that the EDGE appears — not merely that a drift count fell — so this
    /// asserts the edge and that unimplemented RPCs still show as drift.
    #[test]
    fn grpc_proto_links_to_its_tonic_implementation() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("service.proto"),
            "syntax = \"proto3\";\n\
             package demo.v1;\n\
             service Greeter {\n  \
             rpc SayHello (Empty) returns (Empty);\n  \
             rpc GetHTTPStatus (Empty) returns (Empty);\n  \
             rpc NeverBuilt (Empty) returns (Empty);\n\
             }\n\
             message Empty {}\n",
        )
        .unwrap();
        fs::write(
            src.join("server.rs"),
            "#[tonic::async_trait]\n\
             impl Greeter for MyService {\n    \
             async fn say_hello(&self) {}\n    \
             async fn get_http_status(&self) {}\n\
             }\n",
        )
        .unwrap();

        let (_result, store) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();

        let contracts = store.list_contracts(None).unwrap();
        let uids: Vec<&String> = contracts.iter().map(|c| &c.uid).collect();
        assert!(
            uids.iter()
                .any(|u| u.ends_with(":grpc:demo.v1.Greeter/SayHello")),
            "expected the declared gRPC contract; got {uids:?}"
        );

        let implemented = store.list_implemented_contract_uids().unwrap();
        assert!(
            implemented
                .iter()
                .any(|uid| uid.ends_with(":grpc:demo.v1.Greeter/SayHello")),
            "expected IMPLEMENTS_CONTRACT edge for SayHello; implemented: {implemented:?}"
        );
        // The acronym case, which a naive snake_case would have missed.
        assert!(
            implemented
                .iter()
                .any(|uid| uid.ends_with(":grpc:demo.v1.Greeter/GetHTTPStatus")),
            "GetHTTPStatus -> get_http_status must link; implemented: {implemented:?}"
        );
        // A declared RPC with no impl must STILL be drift.
        assert!(
            !implemented
                .iter()
                .any(|uid| uid.ends_with(":grpc:demo.v1.Greeter/NeverBuilt")),
            "an unimplemented RPC must remain drift; implemented: {implemented:?}"
        );
    }

    // ── Contract derivation status (degraded vs genuinely empty) ──────────

    /// Index `src` into `store` a second time through the private full-index
    /// entry point so strict preflight behavior is directly observable.
    /// `index_directory_in_memory` always mints a fresh store, which is no use
    /// when the point is to re-index a store that has been poisoned.
    fn reindex_into(
        store: &GraphStore,
        src: &Path,
        sha: &str,
    ) -> Result<IndexResult, anyhow::Error> {
        let reader = crate::content_reader::FilesystemReader::new(src);
        let local_root = src.display().to_string();
        index_into_store(
            &reader,
            store,
            "test",
            "https://example.com/repo",
            sha,
            None,
            None,
            None,
            None,
            None,
            Some(&local_root),
        )
    }

    /// Write a repo whose only content is an OpenAPI spec declaring
    /// `GET /v1/shared`, plus a handler that implements it.
    fn write_shared_route_repo(src: &Path) {
        fs::create_dir_all(src).unwrap();
        fs::write(
            src.join("openapi.yaml"),
            "openapi: 3.0.0\n\
             info: { title: t, version: \"1.0\" }\n\
             paths:\n  \
             /v1/shared:\n    \
             get:\n      \
             operationId: getShared\n      \
             responses: { \"200\": { description: ok } }\n",
        )
        .unwrap();
        fs::write(
            src.join("SharedController.java"),
            "@RestController\n\
             @RequestMapping(\"/v1/shared\")\n\
             public class SharedController {\n  \
             @GetMapping\n  \
             public void get() {}\n\
             }\n",
        )
        .unwrap();
    }

    /// A post-write contract-apply failure must DEGRADE, not fail the index.
    ///
    /// The preflight parse phase runs before the write guard, so a malformed
    /// spec is rejected with no graph mutation at all (covered by
    /// `degraded_contract_derivation_is_not_reported_clean`). This branch is
    /// the other one: the graph has already committed in earlier transactions
    /// and only the contracts could not be applied. Returning Err there
    /// reported a failed index over a fully written graph — committed work
    /// reported as a failure.
    ///
    /// The two incremental paths deliberately still fail: their contract apply
    /// shares one transaction with their symbol writes and SHA update, so a
    /// failure rolls the whole change back and commits nothing.
    #[test]
    fn post_write_contract_apply_failure_degrades_instead_of_failing_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        write_shared_route_repo(&src);

        let store = GraphStore::in_memory().unwrap();
        fail_next_contract_apply();
        let result = reindex_into(&store, &src, "abc123")
            .expect("a post-write contract failure must not fail the whole index");

        // The index succeeded and says so honestly.
        assert_eq!(
            result.contracts_status,
            crate::blast_radius::AnalysisStatus::Degraded,
            "a failed contract apply must report degraded, not complete"
        );
        assert_eq!(
            result.contracts_derived, 0,
            "no contracts were applied, so none may be claimed"
        );
        // The work that DID commit is still there — that is the whole point of
        // not failing the run.
        assert!(
            result.symbols_count > 0,
            "symbols committed before the contract phase must survive"
        );

        let repo_uid = store
            .list_repos(None)
            .unwrap()
            .into_iter()
            .next()
            .expect("the repo row must have been written")
            .uid;

        // Every downstream consumer must see the degradation, using split 2's
        // vocabulary rather than a second way of saying the same thing.
        let report = crate::contracts::drift_for_store(&store, Some(&repo_uid)).unwrap();
        assert!(!report.is_clean(), "a degraded repo must not report clean");
        assert_eq!(
            report.contracts_status,
            crate::blast_radius::AnalysisStatus::Degraded
        );
        assert_eq!(report.degraded_repos, vec![repo_uid.clone()]);
        let envelope = crate::contracts::drift_envelope(report, 50);
        assert_eq!(envelope["clean"], serde_json::json!(false));
        assert_eq!(envelope["contracts_status"], serde_json::json!("degraded"));

        // A later healthy index clears the marker: degradation is a state, not
        // a permanent stain.
        let healthy = reindex_into(&store, &src, "def456").expect("healthy reindex must succeed");
        assert_eq!(
            healthy.contracts_status,
            crate::blast_radius::AnalysisStatus::Complete
        );
        assert!(healthy.contracts_derived > 0);
        assert!(
            crate::contracts::drift_for_store(&store, Some(&repo_uid))
                .unwrap()
                .is_clean(),
            "a successful re-index must clear the degradation marker"
        );
    }

    /// A malformed recognized spec fails before graph/SHA publication, retains
    /// the prior derived graph, and remains query-visible as degraded.
    #[test]
    fn degraded_contract_derivation_is_not_reported_clean() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        write_shared_route_repo(&src);

        let (first, store) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();
        assert_eq!(
            first.contracts_status,
            crate::blast_radius::AnalysisStatus::Complete,
            "a healthy first index must report a complete contract phase"
        );
        assert!(
            first.contracts_derived > 0,
            "the spec declares a route, so contracts must have been derived"
        );

        let repo_uid = store
            .list_contracts(None)
            .unwrap()
            .first()
            .expect("first index derives at least one contract")
            .repo_uid
            .clone();

        let contracts_before: Vec<_> = store
            .list_contracts(Some(&repo_uid))
            .unwrap()
            .into_iter()
            .map(|contract| (contract.uid, contract.source_path))
            .collect();
        let implemented_before = store
            .list_implemented_contract_uids_for_repo(&repo_uid)
            .unwrap();
        let sha_before = store.lookup_repo(&repo_uid).unwrap().unwrap().indexed_sha;
        fs::write(src.join("openapi.yaml"), "openapi: [malformed").unwrap();

        let error = reindex_into(&store, &src, "def456")
            .expect_err("malformed recognized spec must fail strict preflight");
        assert!(
            format!("{error:#}").contains("prepare strict contract derivation"),
            "unexpected error: {error:#}"
        );
        assert_eq!(
            store
                .list_contracts(Some(&repo_uid))
                .unwrap()
                .into_iter()
                .map(|contract| (contract.uid, contract.source_path))
                .collect::<Vec<_>>(),
            contracts_before
        );
        assert_eq!(
            store
                .list_implemented_contract_uids_for_repo(&repo_uid)
                .unwrap(),
            implemented_before
        );
        assert_eq!(
            store.lookup_repo(&repo_uid).unwrap().unwrap().indexed_sha,
            sha_before
        );

        // 2. The query-time drift analysis says so. This assertion is the one
        //    that catches the original bug: `is_clean()` predates this fix and
        //    returned `true` for exactly this repo.
        let report = crate::contracts::drift_for_store(&store, Some(&repo_uid)).unwrap();
        assert!(
            !report.is_clean(),
            "a degraded repo must not report clean; report: {report:?}"
        );
        assert_eq!(
            report.contracts_status,
            crate::blast_radius::AnalysisStatus::Degraded
        );
        assert_eq!(report.degraded_repos, vec![repo_uid.clone()]);

        // 3. The envelope both front-ends serialize says so.
        let envelope = crate::contracts::drift_envelope(report, 50);
        assert_eq!(envelope["clean"], serde_json::json!(false));
        assert_eq!(envelope["contracts_status"], serde_json::json!("degraded"));

        // A valid empty plan is success, replaces the old derived rows, and
        // heals the repo rather than being mistaken for another failure.
        fs::remove_file(src.join("SharedController.java")).unwrap();
        fs::write(
            src.join("openapi.yaml"),
            "openapi: 3.0.0\ninfo: { title: t, version: \"1.0\" }\npaths: {}\n",
        )
        .unwrap();
        let third = reindex_into(&store, &src, "ghi789").unwrap();
        assert_eq!(
            third.contracts_status,
            crate::blast_radius::AnalysisStatus::Complete,
            "a successful re-index must clear the degraded marker"
        );
        assert!(
            crate::contracts::drift_for_store(&store, Some(&repo_uid))
                .unwrap()
                .degraded_repos
                .is_empty(),
            "the failure marker must not survive a successful derivation"
        );
        assert!(store.list_contracts(Some(&repo_uid)).unwrap().is_empty());
    }

    /// The counterpart the fix must NOT break: a repo that genuinely declares
    /// and implements no contracts is still clean. Reporting "not clean"
    /// everywhere would make the new signal worthless — the whole point is that
    /// empty and broken are now distinguishable.
    #[test]
    fn repo_with_no_contracts_is_still_clean() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("util.rs"), "pub fn add(a: i32) -> i32 { a + 1 }\n").unwrap();

        let (result, store) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();
        assert_eq!(
            result.contracts_status,
            crate::blast_radius::AnalysisStatus::Complete
        );
        assert_eq!(result.contracts_derived, 0, "no specs, no handlers");
        assert!(
            store.list_contracts(None).unwrap().is_empty(),
            "sanity: the repo really has no contracts"
        );

        let report = crate::contracts::drift_for_store(&store, None).unwrap();
        assert!(
            report.is_clean(),
            "an empty-but-healthy repo must still be clean; report: {report:?}"
        );
        assert!(report.degraded_repos.is_empty());

        let envelope = crate::contracts::drift_envelope(report, 50);
        assert_eq!(envelope["clean"], serde_json::json!(true));
        assert_eq!(envelope["contracts_status"], serde_json::json!("complete"));
    }

    // ── Contract UID deduplication (duplicate-PK on the bulk COPY) ────────

    /// Every Contract UID appears at most once, else the COPY would have
    /// aborted. Returns the surviving rows keyed by UID for further assertions.
    fn assert_unique_contract_uids(contracts: &[nestweaver_schema::Contract]) {
        let mut seen: HashMap<&str, usize> = HashMap::new();
        for c in contracts {
            *seen.entry(c.uid.as_str()).or_default() += 1;
        }
        let dupes: Vec<(&str, usize)> = seen.into_iter().filter(|(_, n)| *n > 1).collect();
        assert!(
            dupes.is_empty(),
            "duplicate Contract UIDs reached the store: {dupes:?}"
        );
    }

    #[test]
    fn routes_differing_only_in_path_param_name_collapse_to_one_contract() {
        // BUG repro: `normalize_http_path` discards the *name* of a path
        // parameter, so GET /users/{id} and GET /users/{userId} mint the same
        // UID. Undeduplicated, the bulk COPY died with "Found duplicated
        // primary key value contract:http:GET:/users/{}" and the contract phase
        // lost EVERY contract for the repo.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("UserController.java"),
            "@RestController\n\
             public class UserController {\n  \
             @GetMapping(\"/users/{id}\")\n  \
             public void byId() {}\n  \
             @GetMapping(\"/users/{userId}\")\n  \
             public void byUserId() {}\n\
             }\n",
        )
        .unwrap();

        let (_result, store) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();

        let contracts = store.list_contracts(None).unwrap();
        assert_unique_contract_uids(&contracts);
        let collapsed: Vec<&nestweaver_schema::Contract> = contracts
            .iter()
            .filter(|c| c.uid.ends_with(":http:GET:/users/{}"))
            .collect();
        assert_eq!(
            collapsed.len(),
            1,
            "the two routes must collapse to exactly one contract; got {:?}",
            contracts.iter().map(|c| &c.uid).collect::<Vec<_>>()
        );

        // Dedup CHANGES EDGE CARDINALITY BY DESIGN: both handlers legitimately
        // implement the one surviving contract, so assert the two edges rather
        // than a count that would encode the old one-contract-per-handler shape.
        for handler in ["byId", "byUserId"] {
            let syms = store.lookup_symbols_by_name(handler).unwrap();
            let sym = syms
                .iter()
                .find(|s| s.name == handler)
                .unwrap_or_else(|| panic!("{handler} symbol indexed"));
            let implemented = store.contracts_implemented_by(&sym.uid).unwrap();
            assert!(
                implemented
                    .iter()
                    .any(|(uid, _)| uid.ends_with(":http:GET:/users/{}")),
                "{handler} must link to the surviving contract; got {implemented:?}"
            );
        }
    }

    #[test]
    fn the_same_spec_vendored_at_two_paths_indexes_once() {
        // `contract_uid` derives from verb + path and never from the spec's
        // location, so a spec vendored twice declares each route twice.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(src.join("api")).unwrap();
        fs::create_dir_all(src.join("docs")).unwrap();
        let spec = "openapi: 3.0.0\n\
                    info: { title: t, version: \"1.0\" }\n\
                    paths:\n  \
                    /v1/items:\n    \
                    get:\n      \
                    responses: { \"200\": { description: ok } }\n";
        fs::write(src.join("api").join("openapi.yaml"), spec).unwrap();
        fs::write(src.join("docs").join("openapi.yaml"), spec).unwrap();

        let (_result, store) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();

        let contracts = store.list_contracts(None).unwrap();
        assert_unique_contract_uids(&contracts);
        let survivor = contracts
            .iter()
            .find(|c| c.uid.ends_with(":http:GET:/v1/items"))
            .expect("the declared route survives the collapse");
        // Both copies are declared at confidence 1.0, so the tie-break falls to
        // the lexicographically smallest source_path — NOT to collection order.
        assert_eq!(survivor.source_path, "api/openapi.yaml");
    }

    #[test]
    fn overlapping_proto_and_graphql_definitions_index_cleanly() {
        // `parse_proto` mints "<package>.<Service>/<Method>" and `parse_graphql`
        // collects every field of any Query/Mutation/Subscription type — neither
        // has a cross-file uniqueness check, so two files declaring the same
        // operation collide on the primary key.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repo");
        fs::create_dir_all(&src).unwrap();
        let proto = "syntax = \"proto3\";\n\
                     package demo.v1;\n\
                     service Greeter {\n  \
                     rpc SayHello (Empty) returns (Empty);\n\
                     }\n\
                     message Empty {}\n";
        fs::write(src.join("a.proto"), proto).unwrap();
        fs::write(src.join("b.proto"), proto).unwrap();
        let schema = "type Query {\n  me: String\n}\n";
        fs::write(src.join("one.graphql"), schema).unwrap();
        fs::write(src.join("two.graphql"), schema).unwrap();

        let (_result, store) =
            index_directory_in_memory(&src, "test", "https://example.com/repo", "abc123").unwrap();

        let contracts = store.list_contracts(None).unwrap();
        assert_unique_contract_uids(&contracts);
        let uids: Vec<&str> = contracts.iter().map(|c| c.uid.as_str()).collect();
        assert!(
            uids.iter()
                .any(|uid| uid.ends_with(":grpc:demo.v1.Greeter/SayHello")),
            "the duplicated RPC must survive exactly once; got {uids:?}"
        );
        assert!(
            uids.iter().any(|uid| uid.ends_with(":graphql:Query.me")),
            "the duplicated GraphQL field must survive exactly once; got {uids:?}"
        );
    }

    #[test]
    fn contract_collision_resolves_independently_of_input_order() {
        // The acceptance criterion is that a collision resolves the SAME way
        // whichever order the candidates arrive in. It cannot lean on
        // collection order: `FilesystemReader` walks readdir order while
        // `GitReader` walks git-sorted, so the two readers disagree.
        fn candidate(source_path: &str, confidence: f32) -> nestweaver_schema::Contract {
            nestweaver_schema::Contract {
                uid: "contract:http:GET:/users/{}".to_string(),
                kind: "http".to_string(),
                verb: Some("GET".to_string()),
                path: Some("/users/{}".to_string()),
                operation_id: None,
                repo_uid: "repo:test".to_string(),
                source_path: source_path.to_string(),
                confidence,
            }
        }
        fn survivor(
            first: (ContractOrigin, nestweaver_schema::Contract),
            second: (ContractOrigin, nestweaver_schema::Contract),
        ) -> nestweaver_schema::Contract {
            let mut set = ContractSet::new();
            set.insert(first.0, first.1);
            set.insert(second.0, second.1);
            let mut rows = set.into_contracts();
            assert_eq!(rows.len(), 1, "colliding UIDs must collapse to one row");
            rows.remove(0)
        }

        // 1. Provenance: a spec declaration outranks a code-derived route even
        //    when the code-derived one is more confident and sorts earlier.
        let declared = (ContractOrigin::Declared, candidate("z/openapi.yaml", 0.8));
        let derived = (ContractOrigin::CodeDerived, candidate("a/Ctrl.java", 1.0));
        assert_eq!(
            survivor(declared.clone(), derived.clone()).source_path,
            "z/openapi.yaml"
        );
        assert_eq!(
            survivor(derived, declared).source_path,
            "z/openapi.yaml",
            "provenance must not depend on which candidate arrived first"
        );

        // 2. Then confidence, compared with total_cmp.
        let strong = (ContractOrigin::CodeDerived, candidate("z/Ctrl.java", 1.0));
        let weak = (ContractOrigin::CodeDerived, candidate("a/Ctrl.java", 0.8));
        assert_eq!(
            survivor(strong.clone(), weak.clone()).source_path,
            "z/Ctrl.java"
        );
        assert_eq!(
            survivor(weak, strong).source_path,
            "z/Ctrl.java",
            "confidence must not depend on which candidate arrived first"
        );

        // 3. Then the lexicographically smallest source_path — the common case,
        //    since two @RequestMappings in one controller tie on 1 and 2.
        let early = (ContractOrigin::CodeDerived, candidate("a/Ctrl.java", 1.0));
        let late = (ContractOrigin::CodeDerived, candidate("z/Ctrl.java", 1.0));
        assert_eq!(
            survivor(early.clone(), late.clone()).source_path,
            "a/Ctrl.java"
        );
        assert_eq!(
            survivor(late, early).source_path,
            "a/Ctrl.java",
            "the path tie-break must not depend on which candidate arrived first"
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
        assert!(
            report.declared_not_implemented[0]
                .uid
                .ends_with(":http:GET:/v1/items")
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
            implemented
                .iter()
                .any(|uid| uid.ends_with(":http:POST:/v1/approvals")),
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
            implemented
                .iter()
                .any(|uid| uid.ends_with(":http:GET:/v1/health")),
            "GET /v1/health must be implemented; implemented: {implemented:?}"
        );
        assert!(
            implemented
                .iter()
                .any(|uid| uid.ends_with(":http:POST:/v1/users")),
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

        let persisted_manifest_envelope: nestweaver_store::artifact_envelope::ArtifactEnvelope =
            serde_json::from_slice(
                &fs::read(crate::sidecar_path(&db_path, ".manifests.json")).unwrap(),
            )
            .unwrap();
        assert_eq!(
            persisted_manifest_envelope.source_graph_generation,
            store.graph_generation(),
            "deletion reconciliation must publish manifests at the generation it makes live; payload={}",
            persisted_manifest_envelope.payload
        );
        let manifests = crate::load_manifest_cache_for_db(&store, &db_path).unwrap();
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
        store.set_embedding_metadata("test-model", 2).unwrap();
        assert!(store.add_embedding(&removed_symbol_uid, vec![1.0, 0.0]));
        assert!(store.add_embedding(&survivor_symbol_uid, vec![0.8, 0.6]));
        store.flush_embedding_index().unwrap();
        assert_eq!(
            store.vector_search(&[1.0, 0.0], 1).unwrap()[0].0,
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

        let live_results = store.vector_search(&[1.0, 0.0], 1).unwrap();
        assert_eq!(live_results[0].0, survivor_symbol_uid);
        assert!(!store.has_embedding(&removed_symbol_uid));
        assert!(store.has_embedding(&survivor_symbol_uid));

        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        let persisted_results = reopened.vector_search(&[1.0, 0.0], 1).unwrap();
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
        write_pagerank_fixture(
            &store,
            &pagerank_path,
            HashMap::from([("deleted".to_string(), 1.0)]),
        );
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
            !store.pagerank_scores().unwrap().contains_key("deleted"),
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
        let manifests =
            HashMap::from([(repo_uid.clone(), crate::manifest::ManifestInfo::default())]);
        crate::manifest::save_manifest_cache_for_db(&manifests, &store, &db_path).unwrap();
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
        write_pagerank_fixture(
            &store,
            &pagerank_path,
            HashMap::from([("stale".to_string(), 1.0)]),
        );
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
            !store.pagerank_scores().unwrap().contains_key("stale"),
            "the committed graph must invalidate the live stale PageRank cache"
        );
        let persisted = persisted_pagerank(&db_path);
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
        // Simulated SIGKILL, not an error: the real I/O completes, then the
        // double panics. A panic unwinds in-process state exactly the way
        // process death would (the lease Drop runs, locks release), while the
        // on-disk sidecars stay precisely as the crash instant left them.
        crash_after_clear_marker: bool,
        crash_after_save_pagerank: bool,
        // A reader touching the rank path mid-window: pagerank_scores() fails
        // closed while the marker exists and WIPES the owner's fresh cache.
        // Applied between the compute and the save.
        reader_touch_before_save: bool,
    }

    impl IndexEpilogueIo for InjectedIndexEpilogueIo {
        fn establish_marker(&self, path: &Path) -> Result<(), anyhow::Error> {
            if self.fail_establish {
                anyhow::bail!("injected marker establishment failure");
            }
            FileSystemIndexEpilogueIo.establish_marker(path)
        }

        fn clear_marker(&self, path: &Path) -> Result<(), anyhow::Error> {
            FileSystemIndexEpilogueIo.clear_marker(path)?;
            if self.crash_after_clear_marker {
                panic!("injected crash: process killed just after marker retirement");
            }
            Ok(())
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
            lease: &nestweaver_store::IndexPublicationLease<'_>,
            scope: &nestweaver_store::GraphScope,
        ) -> Result<(), anyhow::Error> {
            if self.fail_compute {
                anyhow::bail!("injected PageRank compute failure");
            }
            FileSystemIndexEpilogueIo.compute_pagerank(lease, scope)
        }

        fn save_pagerank(
            &self,
            lease: &nestweaver_store::IndexPublicationLease<'_>,
            path: &Path,
        ) -> Result<(), anyhow::Error> {
            if self.fail_save {
                anyhow::bail!("injected PageRank save failure");
            }
            if self.reader_touch_before_save {
                let _ = lease.store().pagerank_scores();
            }
            FileSystemIndexEpilogueIo.save_pagerank(lease, path)?;
            if self.crash_after_save_pagerank {
                panic!("injected crash: process killed just after PageRank sidecar save");
            }
            Ok(())
        }
    }

    #[test]
    fn pagerank_compute_failure_is_returned_after_mandatory_commit_publication() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let pagerank_path = crate::sidecar_path(&db_path, ".pagerank.json");
        write_pagerank_fixture(
            &store,
            &pagerank_path,
            HashMap::from([("stale".to_string(), 1.0)]),
        );
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
        // The rank path now fails CLOSED while the publication is dirty —
        // Err(RankingUnavailable), and the failed read itself invalidates the
        // stale cache. Observing ranks requires reconciling first.
        assert!(matches!(
            store.pagerank_scores(),
            Err(nestweaver_store::StoreError::RankingUnavailable)
        ));
        assert!(!pagerank_path.exists());
        assert!(store.graph_generation() > generation_before);
        // The failed refresh BLOCKS marker retirement: the publication stays
        // dirty (fail-closed; the next open reconciles it) instead of
        // reporting clean with no sidecar. The durable generation advanced;
        // the in-memory one stays at the dirty reservation until recovery.
        assert!(
            crate::sidecar_path(&db_path, ".index-dirty").exists(),
            "a failed PageRank refresh must leave the publication dirty"
        );
        assert!(
            fs::read_to_string(crate::sidecar_path(&db_path, ".generation"))
                .unwrap()
                .trim()
                .parse::<u64>()
                .unwrap()
                > generation_before
        );

        // After reconciliation the healed ranks must not contain the stale score.
        let marker_path = crate::sidecar_path(&db_path, ".index-dirty");
        write_marker_with_pid(&marker_path, reaped_child_pid(), None);
        let outcome = recover_abandoned_index_publication(&store, true).unwrap();
        assert!(
            outcome.recovered(),
            "the dirty publication must reconcile: {}",
            outcome.describe()
        );
        assert!(!store.pagerank_scores().unwrap().contains_key("stale"));
    }

    #[test]
    fn pagerank_save_failure_is_returned_without_restoring_stale_persisted_ranks() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let pagerank_path = crate::sidecar_path(&db_path, ".pagerank.json");
        write_pagerank_fixture(
            &store,
            &pagerank_path,
            HashMap::from([("stale".to_string(), 1.0)]),
        );
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
        // Same contract as the compute-failure sibling: dirty reads fail
        // closed, the marker survives, and reconciling the publication must
        // not restore the stale score.
        assert!(matches!(
            store.pagerank_scores(),
            Err(nestweaver_store::StoreError::RankingUnavailable)
        ));
        assert!(!pagerank_path.exists());
        assert!(store.graph_generation() > generation_before);

        let marker_path = crate::sidecar_path(&db_path, ".index-dirty");
        assert!(
            marker_path.exists(),
            "a failed PageRank refresh must leave the publication dirty"
        );
        write_marker_with_pid(&marker_path, reaped_child_pid(), None);
        let outcome = recover_abandoned_index_publication(&store, true).unwrap();
        assert!(
            outcome.recovered(),
            "the dirty publication must reconcile: {}",
            outcome.describe()
        );
        assert!(!store.pagerank_scores().unwrap().contains_key("stale"));
    }

    /// Leave the database in exactly the state a SIGKILL inside the finalizer
    /// produces, then return what the NEXT opener reconciles it to. The
    /// closure runs the finalize with the injected crash point; on-disk state
    /// after the unwind is what a post-mortem recovery would find.
    fn finalize_with_injected_crash(db_path: &Path, publisher: &str, io: InjectedIndexEpilogueIo) {
        let store = GraphStore::open_or_create(db_path).unwrap();
        store.bump_graph_generation();
        store
            .save_graph_generation(&crate::sidecar_path(db_path, ".generation"))
            .unwrap();

        let publication = establish_index_publication_marker_with_io(
            &store,
            Some(db_path),
            publisher,
            &FileSystemIndexEpilogueIo,
        )
        .unwrap();
        insert_publication_graph(&store, publisher);
        insert_publication_notes(&store, publisher);

        let crash = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = finalize_committed_index_for_scope_with_io(
                publication,
                Some(db_path),
                publisher,
                &io,
                // Recovery's own scope: unified, the strict superset. A brain
                // database must come out of this with NOTE ranks, not just
                // code ranks.
                Some(&nestweaver_store::GraphScope::unified()),
                true,
            );
        }));
        assert!(crash.is_err(), "the injected crash must kill the finalizer");
        drop(store);
    }

    /// The persisted sidecar a post-crash opener is entitled to. Fails with a
    /// precise message rather than a bare IO error when the sidecar is absent.
    fn persisted_pagerank(db_path: &Path) -> HashMap<String, f64> {
        let pagerank_path = crate::sidecar_path(db_path, ".pagerank.json");
        let bytes = fs::read(&pagerank_path).unwrap_or_else(|error| {
            panic!(
                "{} must exist — an advanced-generation publication with no \
                 PageRank sidecar reports CLEAN while the note side of the \
                 graph has no ranks: {error}",
                pagerank_path.display()
            )
        });
        let envelope: nestweaver_store::artifact_envelope::ArtifactEnvelope =
            serde_json::from_slice(&bytes).unwrap();
        let identity = nestweaver_store::PublicationIdentity {
            brain_uuid: envelope.brain_uuid.clone(),
            publication_uuid: envelope.publication_uuid.clone(),
        };
        let fingerprint = envelope.algorithm_fingerprint.clone();
        envelope
            .validate_and_decode(nestweaver_store::artifact_envelope::ArtifactExpectation {
                artifact_kind: nestweaver_store::ranking::PAGERANK_ARTIFACT_KIND,
                artifact_schema_version:
                    nestweaver_store::ranking::PAGERANK_ARTIFACT_SCHEMA_VERSION,
                identity: &identity,
                producer_version: env!("CARGO_PKG_VERSION"),
                source_graph_generation: envelope.source_graph_generation,
                algorithm_fingerprint: &fingerprint,
            })
            .unwrap()
    }

    fn assert_note_ranks(persisted: &HashMap<String, f64>, publisher: &str) {
        for note in [
            format!("note:{publisher}:one"),
            format!("note:{publisher}:two"),
        ] {
            assert!(
                persisted.contains_key(&note),
                "note/section ranks must survive an interrupted recovery, not \
                 just code ranks: {note} missing from {} persisted entries",
                persisted.len()
            );
        }
    }

    // Fault injection at the finalizer's ordering boundary: a kill that lands
    // just after `.index-dirty` retires. Whatever the ordering, the state this
    // leaves must be either dirty (the next open reconciles it) or clean WITH
    // a sidecar matching the committed graph — never clean with an advanced
    // generation and no sidecar.
    #[test]
    fn kill_at_marker_retirement_never_leaves_a_clean_rankless_publication() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let marker_path = crate::sidecar_path(&db_path, ".index-dirty");

        finalize_with_injected_crash(
            &db_path,
            "killclear",
            InjectedIndexEpilogueIo {
                crash_after_clear_marker: true,
                ..Default::default()
            },
        );

        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        if marker_path.exists() {
            // Dirty is acceptable: recovery self-heals it right here.
            write_marker_with_pid(&marker_path, reaped_child_pid(), None);
            let outcome = recover_abandoned_index_publication(&reopened, true).unwrap();
            assert!(
                outcome.recovered(),
                "a marker present after the crash must reconcile: {}",
                outcome.describe()
            );
        }
        assert!(
            !reopened.is_index_publication_dirty(),
            "after any needed reconciliation the publication must be clean"
        );
        let persisted = persisted_pagerank(&db_path);
        assert_note_ranks(&persisted, "killclear");
    }

    // The other side of the re-ordered boundary: a kill after the fresh
    // sidecar is saved but before the marker retires. Marker present + fresh
    // sidecar = dirty, and dirty self-heals on the next open.
    #[test]
    fn kill_after_sidecar_save_before_marker_retirement_stays_dirty_and_self_heals() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let marker_path = crate::sidecar_path(&db_path, ".index-dirty");

        finalize_with_injected_crash(
            &db_path,
            "killsave",
            InjectedIndexEpilogueIo {
                crash_after_save_pagerank: true,
                ..Default::default()
            },
        );

        assert!(
            marker_path.exists(),
            "a kill before marker retirement must leave the publication dirty"
        );
        write_marker_with_pid(&marker_path, reaped_child_pid(), None);
        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        let outcome = recover_abandoned_index_publication(&reopened, true).unwrap();
        assert!(
            outcome.recovered(),
            "the dirty publication must reconcile: {}",
            outcome.describe()
        );
        assert!(!reopened.is_index_publication_dirty());
        let persisted = persisted_pagerank(&db_path);
        assert_note_ranks(&persisted, "killsave");
    }

    // No crash at all: a reader touching the rank path BETWEEN the owner's
    // compute and save (the marker is still set, so the read fails closed and
    // wipes the fresh cache). The owner save must then FAIL — blocking marker
    // retirement — rather than silently writing nothing and publishing clean
    // with no sidecar.
    #[test]
    fn reader_in_refresh_window_cannot_leave_a_clean_rankless_publication() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let marker_path = crate::sidecar_path(&db_path, ".index-dirty");
        let pagerank_path = crate::sidecar_path(&db_path, ".pagerank.json");

        let store = GraphStore::open_or_create(&db_path).unwrap();
        store.bump_graph_generation();
        store
            .save_graph_generation(&crate::sidecar_path(&db_path, ".generation"))
            .unwrap();
        let publication = establish_index_publication_marker_with_io(
            &store,
            Some(&db_path),
            "raced publisher",
            &FileSystemIndexEpilogueIo,
        )
        .unwrap();
        insert_publication_graph(&store, "raced");
        insert_publication_notes(&store, "raced");

        let error = finalize_committed_index_for_scope_with_io(
            publication,
            Some(&db_path),
            "raced publisher",
            &InjectedIndexEpilogueIo {
                reader_touch_before_save: true,
                ..Default::default()
            },
            Some(&nestweaver_store::GraphScope::unified()),
            true,
        )
        .expect_err("a wiped owner cache must fail the sidecar save, not publish clean without it");
        assert!(
            error
                .failures
                .iter()
                .any(|f| f.stage == DeletionReconciliationStage::PageRankPersistence),
            "the wipe must surface as a PageRank save failure: {error}"
        );
        assert!(
            marker_path.exists(),
            "the publication must stay dirty so the next open reconciles it"
        );
        assert!(
            !pagerank_path.exists(),
            "no sidecar may be written from a wiped cache"
        );
        drop(store);

        // And it recovers on the next open — with note ranks, not just code.
        write_marker_with_pid(&marker_path, reaped_child_pid(), None);
        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        let outcome = recover_abandoned_index_publication(&reopened, true).unwrap();
        assert!(
            outcome.recovered(),
            "the dirty publication must reconcile: {}",
            outcome.describe()
        );
        let persisted = persisted_pagerank(&db_path);
        assert_note_ranks(&persisted, "raced");
    }

    #[test]
    fn pagerank_removal_failure_quarantines_stale_ranks_and_publishes_generation() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let pagerank_path = crate::sidecar_path(&db_path, ".pagerank.json");
        write_pagerank_fixture(
            &store,
            &pagerank_path,
            HashMap::from([("stale".to_string(), 1.0)]),
        );
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
        assert!(!store.pagerank_scores().unwrap().contains_key("stale"));
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
        write_pagerank_fixture(
            &store,
            &pagerank_path,
            HashMap::from([("stale".to_string(), 1.0)]),
        );
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
        assert!(reopened.pagerank_scores().is_err());
    }

    #[test]
    fn generation_save_failure_keeps_dirty_marker_fail_closed_on_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let pagerank_path = crate::sidecar_path(&db_path, ".pagerank.json");
        let generation_path = crate::sidecar_path(&db_path, ".generation");
        let marker_path = crate::sidecar_path(&db_path, ".index-dirty");
        write_pagerank_fixture(
            &store,
            &pagerank_path,
            HashMap::from([("stale".to_string(), 1.0)]),
        );
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
        assert!(reopened.pagerank_scores().is_err());
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
            let mut cache =
                nestweaver_store::cache::ResponseCache::open(&db_path, 1, RESPONSE_SHAPE_FIXTURE);
            cache.insert(
                cache_key,
                "brain_search",
                br#"{"stale":true}"#,
                store.graph_generation(),
                scope_digest,
            );
            cache.save();
            write_pagerank_fixture(
                &store,
                &pagerank_path,
                HashMap::from([("stale".to_string(), 1.0)]),
            );
        }
        fs::create_dir(&marker_path).unwrap();

        let recovering = GraphStore::open_or_create(&db_path).unwrap();
        assert_eq!(
            recovering.graph_generation(),
            8,
            "dirty recovery must reserve canonical generation 7's successor"
        );
        recovering.load_pagerank_cache(&pagerank_path).unwrap();
        assert!(recovering.pagerank_scores().is_err());
        let mut cache =
            nestweaver_store::cache::ResponseCache::open(&db_path, 1, RESPONSE_SHAPE_FIXTURE);
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
        assert!(!clean.pagerank_scores().unwrap().contains_key("stale"));
        let mut cache =
            nestweaver_store::cache::ResponseCache::open(&db_path, 1, RESPONSE_SHAPE_FIXTURE);
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
        assert!(store.pagerank_scores().unwrap().contains_key(&removed_uid));

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
            !store.pagerank_scores().unwrap().contains_key(&removed_uid),
            "the live server store must not serve the deleted symbol's stale score"
        );
        let persisted = persisted_pagerank(&db_path);
        assert!(
            !persisted.contains_key(&removed_uid),
            "a daemon restart must not reload the deleted symbol from the PageRank sidecar"
        );
        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        reopened.load_pagerank_cache(&pagerank_path).unwrap();
        assert!(
            !reopened
                .pagerank_scores()
                .unwrap()
                .contains_key(&removed_uid),
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
                // A failed refresh blocks marker retirement, so the in-memory
                // generation stays at the dirty reservation while the durable
                // file already holds the clean successor. What this guard pins
                // is that the generation is DURABLE and advanced before the
                // write gate is released — not that the two agree.
                let persisted = fs::read_to_string(&self.generation_path)
                    .unwrap()
                    .trim()
                    .parse::<u64>()
                    .unwrap();
                assert!(
                    persisted > self.generation_before,
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
        write_pagerank_fixture(
            &store,
            &pagerank_path,
            HashMap::from([("stale".to_string(), 1.0)]),
        );
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
        // The publication stays dirty after the failed refresh, so the rank
        // path fails closed — and the failed read itself invalidates the live
        // stale cache.
        assert!(matches!(
            store.pagerank_scores(),
            Err(nestweaver_store::StoreError::RankingUnavailable)
        ));
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

    /// Trips the cancel flag inside `establish_marker`, which the index runs
    /// immediately AFTER the pre-write cancellation poll — so the run passes
    /// every abort point and can only observe the cancellation at the
    /// committed finalizer, exactly like a timeout or Ctrl-C that lands
    /// mid-write.
    struct CancelOnMarkerIo {
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl IndexEpilogueIo for CancelOnMarkerIo {
        fn establish_marker(&self, path: &Path) -> Result<(), anyhow::Error> {
            FileSystemIndexEpilogueIo.establish_marker(path)?;
            self.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        fn clear_marker(&self, path: &Path) -> Result<(), anyhow::Error> {
            FileSystemIndexEpilogueIo.clear_marker(path)
        }

        fn remove_file(&self, path: &Path) -> std::io::Result<()> {
            FileSystemIndexEpilogueIo.remove_file(path)
        }

        fn rename_file(&self, from: &Path, to: &Path) -> std::io::Result<()> {
            FileSystemIndexEpilogueIo.rename_file(from, to)
        }

        fn save_generation(
            &self,
            store: &GraphStore,
            path: &Path,
            generation: u64,
        ) -> Result<(), anyhow::Error> {
            FileSystemIndexEpilogueIo.save_generation(store, path, generation)
        }

        fn compute_pagerank(
            &self,
            lease: &nestweaver_store::IndexPublicationLease<'_>,
            scope: &nestweaver_store::GraphScope,
        ) -> Result<(), anyhow::Error> {
            FileSystemIndexEpilogueIo.compute_pagerank(lease, scope)
        }

        fn save_pagerank(
            &self,
            lease: &nestweaver_store::IndexPublicationLease<'_>,
            path: &Path,
        ) -> Result<(), anyhow::Error> {
            FileSystemIndexEpilogueIo.save_pagerank(lease, path)
        }
    }

    /// A cancellation observed only AFTER the last pre-write poll cannot abort
    /// the run — the graph commits — but the publication must stay dirty:
    /// `.index-dirty` survives and the durable generation is not advanced, so
    /// the next open reconciles the publication fail-closed instead of
    /// trusting a generation/PageRank that predates the commit.
    #[test]
    fn cancelled_commit_keeps_publication_dirty_and_generation_unpublished() {
        use std::sync::{Arc, atomic::AtomicBool};

        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let db_path = dir.path().join("test.lbug");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("kept.js"), "function kept() { return 1; }").unwrap();
        let repo_url = "https://example.com/cancelled-commit";
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let reader = crate::content_reader::FilesystemReader::new(&repo);
        let cancel = Arc::new(AtomicBool::new(false));

        let result = index_with_reader_and_write_gate_and_io(
            ReaderIndexRequest {
                reader: &reader,
                store: &store,
                instance_id: "test",
                repo_url,
                indexed_sha: "sha-1",
                name: None,
                cancel: Some(&cancel),
                epilogue_io: &CancelOnMarkerIo {
                    cancel: Arc::clone(&cancel),
                },
            },
            || Ok::<_, anyhow::Error>(()),
        )
        .expect("a cancellation past the last pre-write poll cannot abort the commit");

        assert!(result.files_count > 0, "the run indexed the file");
        assert!(
            store
                .list_repos(Some("test"))
                .unwrap()
                .iter()
                .any(|repo| repo.url == repo_url),
            "the cancelled run's graph mutation IS persisted"
        );
        let marker_path = crate::sidecar_path(&db_path, ".index-dirty");
        let generation_path = crate::sidecar_path(&db_path, ".generation");
        assert!(
            marker_path.exists(),
            "a cancelled commit must leave the publication dirty"
        );
        assert!(
            !generation_path.exists(),
            "a cancelled commit must not durably advance the generation"
        );
        drop(store);

        // Reconciliation on the next open is fail-closed: the committed graph
        // is there, but the dirty marker blocks trusting generation/PageRank
        // state until a successful writer heals the publication.
        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        assert!(
            reopened.is_index_publication_dirty(),
            "the dirty marker must survive reopen"
        );
        assert_eq!(
            reopened.graph_generation(),
            u64::MAX,
            "with no published `.generation` and the dirty marker present, reopen must \
             report the fail-closed sentinel rather than a trustworthy generation"
        );
        assert!(
            reopened
                .list_repos(Some("test"))
                .unwrap()
                .iter()
                .any(|repo| repo.url == repo_url),
            "the committed graph survives reopen"
        );
    }

    /// In-memory stores have no `.index-dirty` marker for a later open to
    /// reconcile, so a cancelled-but-committed run must still bump the
    /// in-memory generation — otherwise the commit is invisible to
    /// generation-keyed snapshot readers.
    #[test]
    fn cancelled_commit_still_bumps_in_memory_generation() {
        let store = GraphStore::in_memory().unwrap();
        store.bump_graph_generation();
        let before = store.graph_generation();

        let lease = establish_index_publication_marker_with_io(
            &store,
            None,
            "cancelled in-memory commit",
            &FileSystemIndexEpilogueIo,
        )
        .unwrap();
        finalize_committed_index_for_scope_with_io(
            lease,
            None,
            "cancelled in-memory commit",
            &FileSystemIndexEpilogueIo,
            None,
            false,
        )
        .unwrap();

        assert_eq!(
            store.graph_generation(),
            before + 1,
            "in-memory stores have no dirty marker, so the generation bump must still run"
        );
    }

    /// Control for the cancelled-commit test: the same run without a
    /// cancellation retires `.index-dirty` and durably publishes the advanced
    /// generation.
    #[test]
    fn uncancelled_commit_retires_marker_and_publishes_generation() {
        use std::sync::{Arc, atomic::AtomicBool};

        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let db_path = dir.path().join("test.lbug");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("kept.js"), "function kept() { return 1; }").unwrap();
        let repo_url = "https://example.com/uncancelled-commit";
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let generation_before = store.graph_generation();
        let reader = crate::content_reader::FilesystemReader::new(&repo);
        let cancel = Arc::new(AtomicBool::new(false));

        index_with_reader_and_write_gate_and_io(
            ReaderIndexRequest {
                reader: &reader,
                store: &store,
                instance_id: "test",
                repo_url,
                indexed_sha: "sha-1",
                name: None,
                cancel: Some(&cancel),
                epilogue_io: &FileSystemIndexEpilogueIo,
            },
            || Ok::<_, anyhow::Error>(()),
        )
        .expect("an uncancelled index must succeed");

        assert!(
            !crate::sidecar_path(&db_path, ".index-dirty").exists(),
            "a clean commit retires the dirty marker"
        );
        assert!(store.graph_generation() > generation_before);
        assert_eq!(
            fs::read_to_string(crate::sidecar_path(&db_path, ".generation"))
                .unwrap()
                .trim()
                .parse::<u64>()
                .unwrap(),
            store.graph_generation(),
            "a clean commit durably publishes the advanced generation"
        );
        assert!(!store.is_index_publication_dirty());
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

        let pagerank_before = persisted_pagerank(&db_path);
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
        let pagerank_after = persisted_pagerank(&db_path);
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

        let pagerank_before = persisted_pagerank(&db_path);
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
        let pagerank_after = persisted_pagerank(&db_path);
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

        let pagerank_before = persisted_pagerank(&db_path);
        fs::remove_file(removed_repo.join("removed.js")).unwrap();
        git(&["add", "-A"]);
        git(&["commit", "--amend", "--no-edit", "--allow-empty", "-q"]);

        let result = incremental_index(&removed_repo, &db_path, "test", repo_url).unwrap();

        assert!(result.fell_back_to_full);
        assert_eq!(
            result.files_deleted, 1,
            "non-ancestor fallback must report files deleted before the full index"
        );
        let pagerank_after = persisted_pagerank(&db_path);
        assert!(
            pagerank_after.len() < pagerank_before.len(),
            "PageRank sidecar must drop symbols deleted before non-ancestor fallback"
        );
    }

    /// Regression: a crash between the SHA commit and content landing leaves a
    /// Repo row whose indexed_sha matches HEAD but owns zero content. The
    /// `old_sha == new_sha` skip must not self-perpetuate that empty state —
    /// incremental_index must force a full re-index.
    #[test]
    fn sha_set_but_no_content_forces_full_reindex() {
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
        let repo_row = store.lookup_repo(&r_uid).unwrap().unwrap();
        assert!(
            store.repo_has_content(&repo_row).unwrap(),
            "full re-index must land content for the repo"
        );
    }

    #[test]
    fn incremental_policy_skip_removes_stale_symbols_and_reports_degraded() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let db_path = dir.path().join("test.lbug");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("main.rs"), "pub fn incumbent() {}\n").unwrap();
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
        git(&["add", "main.rs"]);
        git(&["commit", "-q", "-m", "initial"]);
        let old_sha = git(&["rev-parse", "HEAD"]);
        let repo_url = "https://example.com/policy-crossing";
        index_directory(&repo, &db_path, "test", repo_url, &old_sha).unwrap();

        let oversized = format!("pub fn replacement() {{}}\n{}", "// pad\n".repeat(200));
        assert!(oversized.len() > crate::index_limits::MIN_MAX_SOURCE_FILE_BYTES as usize);
        fs::write(repo.join("main.rs"), oversized).unwrap();
        git(&["add", "main.rs"]);
        git(&["commit", "-q", "-m", "grow"]);
        let new_sha = git(&["rev-parse", "HEAD"]);

        let result = incremental_index_with_name_and_limits(
            &repo,
            &db_path,
            "test",
            repo_url,
            None,
            crate::index_limits::IndexLimits::new(crate::index_limits::MIN_MAX_SOURCE_FILE_BYTES)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result.skipped_files.len(), 1);
        assert_eq!(
            result.skipped_files[0].reason_code,
            SkipReasonCode::Oversized
        );
        let store = GraphStore::open_or_create(&db_path).unwrap();
        assert!(store.symbols_in_file("main.rs").unwrap().is_empty());
        let r_uid = nestweaver_schema::repo_uid("test", repo_url);
        assert_eq!(
            store.lookup_repo(&r_uid).unwrap().unwrap().indexed_sha,
            new_sha
        );
    }

    #[test]
    fn incremental_unsupported_file_change_is_not_degraded_coverage() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let db_path = dir.path().join("test.lbug");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("main.rs"), "pub fn covered() {}\n").unwrap();
        fs::write(repo.join("README.md"), "first\n").unwrap();
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
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "initial"]);
        let old_sha = git(&["rev-parse", "HEAD"]);
        let repo_url = "https://example.com/unsupported-change";
        index_directory(&repo, &db_path, "test", repo_url, &old_sha).unwrap();

        fs::write(repo.join("README.md"), "second\n").unwrap();
        git(&["add", "README.md"]);
        git(&["commit", "-q", "-m", "docs"]);
        let result = incremental_index_with_name_and_limits(
            &repo,
            &db_path,
            "test",
            repo_url,
            None,
            crate::index_limits::IndexLimits::default(),
        )
        .unwrap();
        assert!(result.skipped_files.is_empty());
        assert_eq!(result.files_skipped, 0);
        let store = GraphStore::open_or_create(&db_path).unwrap();
        assert!(
            store
                .symbols_in_file("main.rs")
                .unwrap()
                .iter()
                .any(|symbol| symbol.name == "covered")
        );
    }

    #[test]
    fn incremental_transient_read_failure_preserves_graph_and_sha() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let db_path = dir.path().join("test.lbug");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("main.rs"), "pub fn incumbent() {}\n").unwrap();
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
        git(&["add", "main.rs"]);
        git(&["commit", "-q", "-m", "initial"]);
        let old_sha = git(&["rev-parse", "HEAD"]);
        let repo_url = "https://example.com/transient-read";
        index_directory(&repo, &db_path, "test", repo_url, &old_sha).unwrap();

        // nw-190: invalid UTF-8 now decodes lossily and is no longer a read
        // failure, so it cannot stand in for one. Use genuinely binary content
        // (a NUL byte), which the reader refuses with a typed BinarySource.
        fs::write(repo.join("main.rs"), [0x00, 0xfe, 0xfd]).unwrap();
        git(&["add", "main.rs"]);
        git(&["commit", "-q", "-m", "binary-source"]);
        let error = incremental_index_with_name_and_limits(
            &repo,
            &db_path,
            "test",
            repo_url,
            None,
            crate::index_limits::IndexLimits::default(),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("prepare incremental source files")
        );

        let store = GraphStore::open_or_create(&db_path).unwrap();
        assert!(
            store
                .symbols_in_file("main.rs")
                .unwrap()
                .iter()
                .any(|symbol| symbol.name == "incumbent")
        );
        let r_uid = nestweaver_schema::repo_uid("test", repo_url);
        assert_eq!(
            store.lookup_repo(&r_uid).unwrap().unwrap().indexed_sha,
            old_sha
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
        let repo_row = store.lookup_repo(&r_uid).unwrap().unwrap();
        assert!(
            store.repo_has_content(&repo_row).unwrap(),
            "full re-index must land content for the repo"
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
        write_pagerank_fixture(
            &store,
            &pagerank_path,
            HashMap::from([("stale".to_string(), 1.0)]),
        );
        store.load_pagerank_cache(&pagerank_path).unwrap();
        let generation_before = store.graph_generation();
        drop(store);

        let error = incremental_index_with_name_and_io(
            &repo,
            &db_path,
            "test",
            repo_url,
            None,
            crate::index_limits::IndexLimits::default(),
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
        assert!(store.pagerank_scores().is_err());
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
        write_pagerank_fixture(
            &store,
            &pagerank_path,
            HashMap::from([("stale".to_string(), 1.0)]),
        );
        store.load_pagerank_cache(&pagerank_path).unwrap();
        let generation_before = store.graph_generation();
        drop(store);

        let error = incremental_index_with_name_and_io(
            &repo,
            &db_path,
            "test",
            repo_url,
            None,
            crate::index_limits::IndexLimits::default(),
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
        assert!(store.pagerank_scores().is_err());
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
