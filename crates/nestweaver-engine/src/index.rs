use std::collections::HashMap;
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
    std::fs::write(path, json)
        .with_context(|| format!("write filemeta sidecar to {}", path.display()))?;
    Ok(())
}

/// Cross-repo "still alive" unions returned by [`merge_save_filemeta`], used
/// to evict the parsed-cache / resolution-deps sidecars. A named struct so
/// the two same-typed sets can't be swapped at a call site.
struct FilemetaEvictionUnions {
    live_hashes: std::collections::HashSet<String>,
    live_files: std::collections::HashSet<String>,
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
    // Eviction unions across ALL repos — feeding only the current repo's
    // entries (the old behavior) evicts every other repo's parse cache.
    let live_hashes = sidecar
        .repos
        .values()
        .flat_map(|files| files.values().map(|m| m.content_hash.clone()))
        .collect();
    let live_files = sidecar
        .repos
        .values()
        .flat_map(|files| files.keys().cloned())
        .collect();
    save_filemeta_sidecar(&sidecar, filemeta_path)?;
    Ok(FilemetaEvictionUnions {
        live_hashes,
        live_files,
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
            false,
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
            false,
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
    resolution_deps.retain_files(&unions.live_files);

    if let Err(e) = parsed_cache.save(&parsed_cache_path) {
        tracing::warn!("failed to save parsed cache: {e}");
    }
    if let Err(e) = resolution_deps.save(&resolution_deps_path) {
        tracing::warn!("failed to save resolution deps: {e}");
    }

    let manifest = crate::manifest::parse_manifest(&reader);
    crate::migrate_sidecar(db_path, "manifests.json", ".manifests.json");
    let cache_path = crate::sidecar_path(db_path, ".manifests.json");
    let mut cache = crate::manifest::load_manifest_cache(&cache_path).unwrap_or_default();
    cache.insert(r_uid, manifest);
    if let Err(e) = crate::manifest::save_manifest_cache(&cache, &cache_path) {
        tracing::warn!("failed to save manifest cache: {e}");
    }

    // nw-029: warm PageRank at index time so first queries (UI overview, impact,
    // repo-map, hubs) never pay the lazy compute. Mirrors the incremental path.
    // Release-build cost is seconds even at ~50k symbols; failure is non-fatal
    // (lazy single-flight compute remains the backstop). The `files_count > 0 ||
    // !exists` guard keeps a no-op re-index of an already-warm DB cheap.
    if result.files_count > 0 || !crate::sidecar_path(db_path, ".pagerank.json").exists() {
        if let Err(e) = store.compute_pagerank(0.85, 20, &nestweaver_store::GraphScope::code_only())
        {
            tracing::warn!("index-time PageRank failed (non-fatal): {e}");
        } else {
            let pr_path = crate::sidecar_path(db_path, ".pagerank.json");
            if let Err(e) = store.save_pagerank_cache(&pr_path) {
                tracing::warn!("saving pagerank sidecar failed (non-fatal): {e}");
            }
        }
    }

    store.bump_and_persist_generation();

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
            symbols
                .iter()
                .filter(|s| s.start_line <= line)
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
        false,
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

    let outcomes: Vec<ParseOutcome> = file_entries
        .par_iter()
        .map(|(path, lang)| {
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

            // Parse the file (CPU-bound tree-sitter work).
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
        })
        .collect();

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

    // Re-identify prune: when a local repo previously indexed under a
    // `file://<root_path>` identity is now indexed under a different
    // identity (its git origin remote), the old file:// node is a stale
    // duplicate of the same working tree. Prune it STRICTLY by uid — never
    // by disk path — so unrelated repos can never be caught by this delete.
    // Detected before the parse phase (see above) so the filemeta cache was
    // already bypassed for this pass.
    if let Some(old_uid) = &reidentify_old_uid {
        tracing::info!(
            old_uid,
            new_uid = %r_uid,
            root_path = root_path.unwrap_or(""),
            url = repo_url,
            "repo re-identified under its origin remote; pruning old file:// node by uid"
        );
        delete_repo_all_data(store, old_uid).context("delete_repo_all_data (re-identify prune)")?;
    }

    // Insert the Repo node if it doesn't exist yet. The target SHA is recorded
    // only after every required graph write succeeds, so a later write failure
    // cannot make retry preparation think this commit is already indexed.
    let existing_repo = store.lookup_repo(&r_uid).context("lookup_repo")?;
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
    if existing_repo.is_some() && !force_reindex {
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
        store
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
    let resolver_view: Vec<(String, Vec<RawSymbol>, Vec<RawReference>)> = parsed_files_for_resolver
        .iter()
        .map(|(path, syms, refs, _)| (path.clone(), syms.clone(), refs.clone()))
        .collect();

    // Compute the incremental resolution filter: only re-resolve files that
    // changed plus files that depend on changed files.
    // When no files changed and we have prior resolution data, skip resolution
    // entirely — edges from the previous run are still valid in the DB.
    let skip_resolution = actually_changed_files.is_empty()
        && resolution_deps.as_ref().is_some_and(|rd| !rd.is_empty());

    let resolve_filter = if !skip_resolution
        && !actually_changed_files.is_empty()
        && files_unchanged > 0
        && resolution_deps.as_ref().is_some_and(|rd| !rd.is_empty())
    {
        let affected = resolution_deps
            .as_ref()
            .unwrap()
            .affected_files(&actually_changed_files);
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
            rd.set_deps(file, deps);
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

    if bump_generation_after_write {
        store.bump_and_persist_generation();
    }

    Ok(IndexResult {
        symbols_count,
        edges_count,
        files_count,
        files_unchanged,
        skipped_files,
    })
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
                repo_path,
                db_path,
                &store,
                instance_id,
                repo_url,
                "local",
                name,
            );
        }
    };

    // 2. If no existing Repo → full index.
    let old_sha = match existing_repo {
        None => {
            tracing::info!("no existing repo found; falling back to full index");
            return full_index_fallback(
                repo_path,
                db_path,
                &store,
                instance_id,
                repo_url,
                &new_sha,
                name,
            );
        }
        Some(r) => r.indexed_sha,
    };

    // 3. Verify old_sha is an ancestor of new_sha.
    if !crate::git_diff::is_ancestor(repo_path, &old_sha, &new_sha) {
        tracing::warn!(
            old_sha,
            new_sha,
            "old SHA is not an ancestor of HEAD; falling back to full re-index"
        );
        // Delete all existing repo data before full re-index.
        delete_repo_all_data(&store, &r_uid)
            .with_context(|| "delete_repo_all_data before full re-index")?;
        return full_index_fallback(
            repo_path,
            db_path,
            &store,
            instance_id,
            repo_url,
            &new_sha,
            name,
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

    // 7. Recompute PageRank (outside the transaction — it's read-heavy and
    // idempotent, so partial completion is fine).
    store
        .compute_pagerank(0.85, 20, &nestweaver_store::GraphScope::code_only())
        .with_context(|| "compute_pagerank after incremental index")?;

    crate::migrate_sidecar(db_path, "pagerank.json", ".pagerank.json");
    let pr_path = crate::sidecar_path(db_path, ".pagerank.json");
    if let Err(e) = store.save_pagerank_cache(&pr_path) {
        tracing::warn!("failed to save pagerank cache: {e}");
    }

    // P0.2: incremental index mutated the graph; bump + persist the generation.
    store.bump_and_persist_generation();

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

    let _write_guard = acquire_write_guard()?;
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

    store
        .compute_pagerank(0.85, 20, &nestweaver_store::GraphScope::code_only())
        .with_context(|| "compute_pagerank after incremental index")?;

    if let Some(db_path) = store.db_path() {
        crate::migrate_sidecar(db_path, "pagerank.json", ".pagerank.json");
        let pr_path = crate::sidecar_path(db_path, ".pagerank.json");
        if let Err(e) = store.save_pagerank_cache(&pr_path) {
            tracing::warn!("failed to save pagerank cache: {e}");
        }
    }

    store.bump_and_persist_generation();

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
fn collect_reverse_dep_files(
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

/// Phase 2 of transitive re-resolution (nw-008). Re-parse `S = changed ∪ rdeps`
/// from `reader`, resolve cross-file references with the full symbol map across
/// `S`, and surgically re-insert ONLY the edges the per-file `DETACH DELETE`
/// removed: those whose TARGET lives in a changed file and whose SOURCE lives
/// in a different file. Intra-file edges and edges into unchanged files were
/// never deleted (or were re-created by single-file resolution in the mutation
/// loop), so re-inserting them would duplicate (edge insert is `CREATE`, not
/// `MERGE`) — the `source_file != target_file` and `target ∈ changed` filters
/// keep the insert duplicate-free without a `delete_resolved_edges_for_file`
/// pass.
///
/// Runs inside the same transaction as the mutation loop. Returns the number of
/// edges inserted.
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
        return Ok(0);
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
    // their file paths.
    let db_symbols = nestweaver_store::GraphStore::lookup_symbols_by_repo_on(conn, r_uid)
        .with_context(|| "lookup_symbols_by_repo_on for forward edge resolution")?;

    let mut unchanged_by_file: HashMap<String, Vec<RawSymbol>> = HashMap::new();
    for sym in &db_symbols {
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

    let count = insertable.len();
    if count > 0 {
        nestweaver_store::GraphStore::batch_insert_edges_on(conn, &insertable)
            .with_context(|| "batch_insert_edges (transitive re-resolution)")?;
    }
    Ok(count)
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
) -> Result<(), anyhow::Error> {
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

    Ok(())
}

/// Full index fallback — uses the already-open store to avoid double-
/// opening the LadybugDB file (which corrupts it).
fn full_index_fallback(
    repo_path: &Path,
    db_path: &Path,
    store: &GraphStore,
    instance_id: &str,
    repo_url: &str,
    new_sha: &str,
    name: Option<&str>,
) -> Result<IncrementalResult, anyhow::Error> {
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
        Some(&filemeta_cache),
        Some(&mut new_filemeta),
        Some(&mut parsed_cache),
        Some(&mut resolution_deps),
        Some(&mut reidentified_old_uid),
        name,
        Some(&local_root),
        false,
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
            resolution_deps.retain_files(&unions.live_files);
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
    crate::migrate_sidecar(db_path, "manifests.json", ".manifests.json");
    let cache_path = crate::sidecar_path(db_path, ".manifests.json");
    let mut cache = crate::manifest::load_manifest_cache(&cache_path).unwrap_or_default();
    cache.insert(r_uid, manifest);
    if let Err(e) = crate::manifest::save_manifest_cache(&cache, &cache_path) {
        tracing::warn!("failed to save manifest cache: {e}");
    }

    // P0.2: full re-index mutated the graph; bump + persist the generation.
    store.bump_and_persist_generation();

    Ok(IncrementalResult {
        fell_back_to_full: true,
        symbols_added: result.symbols_count,
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
