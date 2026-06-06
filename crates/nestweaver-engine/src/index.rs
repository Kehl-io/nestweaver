use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use anyhow::Context;
use indicatif::{ProgressBar, ProgressStyle};
use nestweaver_parser::{
    AstTypeBinding, RawReference, RawSymbol, SkippedFile, detect_language, parse_source,
};
use nestweaver_resolver::{discover_workspace_context, resolve_references_with_context};
use nestweaver_schema::{
    File, Language, Repo, Service, Symbol, file_uid, repo_uid, service_uid, symbol_uid,
};
use nestweaver_store::GraphStore;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

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
//   Tier 3 – size differs → read file, compute SHA-256, compare hash

/// Per-file metadata cached between indexing runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedFileMeta {
    pub mtime_secs: u64,
    pub size_bytes: u64,
    pub content_hash: String,
}

/// Map from repo-relative path to cached file metadata.
pub type FileMetaCache = HashMap<String, CachedFileMeta>;

/// Load the file metadata sidecar. Returns an empty map on missing/corrupt file.
pub fn load_filemeta_cache(path: &Path) -> FileMetaCache {
    match std::fs::read_to_string(path) {
        Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
        Err(_) => FileMetaCache::new(),
    }
}

/// Save the file metadata sidecar alongside the database.
pub fn save_filemeta_cache(cache: &FileMetaCache, path: &Path) -> Result<(), anyhow::Error> {
    let json = serde_json::to_string(cache).with_context(|| "serialize filemeta cache")?;
    std::fs::write(path, json)
        .with_context(|| format!("write filemeta cache to {}", path.display()))?;
    Ok(())
}

/// Outcome of the tiered change detection for a single file.
enum ChangeVerdict {
    /// File is unchanged — skip re-indexing it.
    Unchanged,
    /// File is new or changed — `source` contains the file content and
    /// `content_hash` is the freshly-computed SHA-256 hex digest.
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
    abs_path: &Path,
    rel_path: &str,
    cache: &FileMetaCache,
) -> Result<ChangeVerdict, anyhow::Error> {
    let fs_meta =
        std::fs::metadata(abs_path).with_context(|| format!("stat {}", abs_path.display()))?;

    let mtime_secs = fs_meta
        .modified()
        .unwrap_or(SystemTime::UNIX_EPOCH)
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let size_bytes = fs_meta.len();

    if let Some(cached) = cache.get(rel_path) {
        // Tier 1: mtime unchanged → skip.
        if cached.mtime_secs == mtime_secs {
            return Ok(ChangeVerdict::Unchanged);
        }

        // Tier 2: mtime changed but size unchanged → fall through to hash check.
        // Same-size edits are common, so we cannot skip based on size alone.

        // Tier 3: mtime differs → read file, compute hash, compare.
        let source = std::fs::read_to_string(abs_path)
            .with_context(|| format!("read {}", abs_path.display()))?;
        let content_hash = sha2_hex(&source);
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
        let source = std::fs::read_to_string(abs_path)
            .with_context(|| format!("read {}", abs_path.display()))?;
        let content_hash = sha2_hex(&source);
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
    let filemeta_path = crate::sidecar_path(db_path, ".filemeta.json");
    crate::migrate_sidecar(db_path, "filemeta.json", ".filemeta.json");
    let mut new_filemeta = FileMetaCache::new();

    let result = if force {
        index_into_store(
            repo_path,
            store,
            instance_id,
            repo_url,
            indexed_sha,
            None,
            Some(&mut new_filemeta),
            name,
        )?
    } else {
        let filemeta_cache = load_filemeta_cache(&filemeta_path);
        index_into_store(
            repo_path,
            store,
            instance_id,
            repo_url,
            indexed_sha,
            Some(&filemeta_cache),
            Some(&mut new_filemeta),
            name,
        )?
    };

    if let Err(e) = save_filemeta_cache(&new_filemeta, &filemeta_path) {
        tracing::warn!("failed to save filemeta cache: {e}");
    }

    let manifest = crate::manifest::parse_manifest(repo_path);
    crate::migrate_sidecar(db_path, "manifests.json", ".manifests.json");
    let cache_path = crate::sidecar_path(db_path, ".manifests.json");
    let r_uid = nestweaver_schema::repo_uid(instance_id, repo_url);
    let mut cache = crate::manifest::load_manifest_cache(&cache_path).unwrap_or_default();
    cache.insert(r_uid, manifest);
    if let Err(e) = crate::manifest::save_manifest_cache(&cache, &cache_path) {
        tracing::warn!("failed to save manifest cache: {e}");
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
    let result = index_into_store(
        repo_path,
        &store,
        instance_id,
        repo_url,
        indexed_sha,
        None,
        None,
        None,
    )?;
    Ok((result, store))
}

/// Core indexing logic shared by both public functions.
///
/// When `filemeta_cache` is `Some`, tiered change detection skips files
/// whose mtime and/or size match the cached values (avoiding expensive
/// SHA-256 hashing and re-parsing for unchanged files). Entries for all
/// processed files are written to `new_filemeta` so the caller can
/// persist the updated sidecar after indexing completes.
#[allow(clippy::too_many_arguments)]
fn index_into_store(
    repo_path: &Path,
    store: &GraphStore,
    instance_id: &str,
    repo_url: &str,
    indexed_sha: &str,
    filemeta_cache: Option<&FileMetaCache>,
    mut new_filemeta: Option<&mut FileMetaCache>,
    name: Option<&str>,
) -> Result<IndexResult, anyhow::Error> {
    let started = Instant::now();

    // 1. Insert (or update) the Repo node.
    let r_uid = repo_uid(instance_id, repo_url);
    let existing_repo = store.lookup_repo(&r_uid).context("lookup_repo")?;
    if existing_repo.is_some() {
        // Repo already exists — update its SHA rather than creating a duplicate.
        store
            .update_repo_sha(&r_uid, indexed_sha)
            .context("update_repo_sha")?;
    } else {
        let repo = Repo {
            uid: r_uid.clone(),
            url: repo_url.to_string(),
            indexed_sha: indexed_sha.to_string(),
            staleness_commits_behind: 0,
            instance_id: instance_id.to_string(),
            name: name.map(String::from),
        };
        store.insert_repo(&repo).context("insert_repo")?;
    }

    // ── Phase 1: Scan files ───────────────────────────────────────────────
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

    // SECURITY: do NOT follow symlinks (see index_md.rs for rationale).
    let walker = WalkDir::new(repo_path)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            // Skip pruned directory names.
            if e.file_type().is_dir()
                && e.file_name()
                    .to_str()
                    .is_some_and(|name| SKIP_DIRS.contains(&name))
            {
                return false;
            }
            true
        });

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!("walkdir error: {err}");
                continue;
            }
        };

        if entry.file_type().is_dir() {
            continue;
        }

        let path = entry.path();

        // F2.1: collect API spec files regardless of source language.
        if crate::contracts::is_spec_file(&path.to_string_lossy()) {
            spec_files.push(path.to_path_buf());
        }

        // Only process files with a supported language extension.
        let lang = match detect_language(path) {
            Some(l) => l,
            None => continue,
        };

        // Skip minified/bundled files — they produce noise in the graph.
        if is_minified_or_bundled(path) {
            continue;
        }

        file_entries.push((path.to_path_buf(), lang));
        scan_pb.set_message(format!("Scanning files... {}", file_entries.len()));
        scan_pb.tick();
    }

    scan_pb.finish_with_message(format!("Scanned {} files", file_entries.len()));

    // ── Phase 2: Parse files (parallelised with rayon) ─────────────────
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
    // Unchanged (carry forward cached meta), Skipped (error), or Parsed
    // (with all data needed for the sequential collection phase).
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
    }

    // Run change detection + parsing in parallel. Each file is independent:
    // stat, read, hash, and tree-sitter parse are all CPU/IO-bound work
    // that benefits from multi-core execution.
    use rayon::prelude::*;

    let outcomes: Vec<ParseOutcome> = file_entries
        .par_iter()
        .map(|(path, lang)| {
            let display_name = path
                .strip_prefix(repo_path)
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned();

            // Tiered change detection.
            let (source, content_hash, file_meta) =
                match tiered_change_check(path, &display_name, cache) {
                    Ok(ChangeVerdict::Unchanged) => {
                        parse_pb.inc(1);
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

    // ── Sequential collection of parallel results ────────────────────────
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

    for outcome in outcomes {
        match outcome {
            ParseOutcome::Unchanged { rel_path } => {
                // Carry forward the existing cache entry.
                if let (Some(ref mut new_cache), Some(cached)) =
                    (new_filemeta.as_deref_mut(), cache.get(&rel_path))
                {
                    new_cache.insert(rel_path, cached.clone());
                }
                files_unchanged += 1;
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

    // 2b. When re-indexing over an existing store (tiered detection is active
    //     and some files changed), clean up old File nodes and their symbols
    //     for files we are about to re-insert.
    if existing_repo.is_some() {
        for file in &all_files {
            // Remove old symbols belonging to this file.
            let _ = store.delete_symbols_in_file(&r_uid, &file.path);
            // Remove old File node.
            let _ = store.delete_file_node(&file.uid);
        }
        // BUG FIX: clear repo-scoped derived nodes (Service, Contract) before
        // re-insert. `bulk_index_write` plain-CREATEs Service nodes whose UID is
        // derived deterministically from repo_uid + directory, so a forced
        // re-index would otherwise collide on the primary key. Idempotent.
        let _ = store.clear_repo_derived_nodes(&r_uid);
    }

    // 3-7. Build service groupings and perform all bulk inserts in a single transaction.
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

    // ── Phase 3: Resolve cross-file references ────────────────────────────
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
    let workspace_ctx = if matches!(
        language,
        Language::JavaScript
            | Language::TypeScript
            | Language::Vue
            | Language::Svelte
            | Language::Astro
    ) {
        discover_workspace_context(repo_path)
    } else {
        Default::default()
    };

    // Build type environments per file for type-aware resolution.
    let mut type_envs: std::collections::HashMap<
        String,
        nestweaver_resolver::types::TypeEnvironment,
    > = {
        let mut envs = std::collections::HashMap::new();
        for (file_path, symbols, _references, source_opt) in &parsed_files_for_resolver {
            let full_path = repo_path.join(file_path);
            let source_str = match source_opt {
                Some(s) => s.clone(),
                None => match std::fs::read_to_string(&full_path) {
                    Ok(s) => s,
                    Err(_) => continue,
                },
            };
            let empty_bindings = Vec::new();
            let file_ast_bindings = ast_bindings_by_file
                .get(file_path)
                .unwrap_or(&empty_bindings);
            let env = nestweaver_resolver::types::TypeEnvironment::build(
                &source_str,
                language,
                symbols,
                file_ast_bindings,
            );
            if env.binding_count() > 0 {
                envs.insert(file_path.clone(), env);
            }
        }
        tracing::info!(
            files_with_bindings = envs.len(),
            total_bindings = envs.values().map(|e| e.binding_count()).sum::<usize>(),
            "type environments built"
        );
        envs
    };

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
                    let full_path = repo_path.join(file_path);
                    let source_str = match source_opt {
                        Some(s) => s.clone(),
                        None => match std::fs::read_to_string(&full_path) {
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
    let resolved_edges = resolve_references_with_context(
        &resolver_view,
        language,
        &r_uid,
        &workspace_ctx,
        Some(&type_envs),
    );

    // Filter out unresolved edges whose target doesn't exist in the DB.
    let insertable_edges: Vec<_> = resolved_edges
        .into_iter()
        .filter(|e| !e.target_uid.starts_with("unresolved:"))
        .collect();

    let mut edges_count = insertable_edges.len();
    store
        .batch_insert_edges(&insertable_edges)
        .context("batch_insert_edges (resolved)")?;

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

        // (file_path, type_name) → uid
        let mut container_map: HashMap<(String, String), String> = HashMap::new();
        for sym in &all_symbols {
            if container_kinds.contains(&sym.kind) {
                container_map.insert((sym.file_path.clone(), sym.name.clone()), sym.uid.clone());
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

    // ── Phase 4 (F2-core): derive the API contract graph ──────────────────
    // Best-effort: a malformed spec or unexpected store error here must not
    // fail the whole index. Contracts are hypotheses layered on top of the
    // code graph.
    if let Err(e) = derive_contracts(store, repo_path, &r_uid, &spec_files, &handler_files) {
        tracing::warn!("contract derivation failed (non-fatal): {e}");
    }

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
    repo_path: &Path,
    r_uid: &str,
    spec_files: &[PathBuf],
    handler_files: &[HandlerFileData],
) -> Result<(), anyhow::Error> {
    use nestweaver_schema::{EdgeType, ResolvedEdge};
    use std::collections::HashSet;

    // 1. Declared contracts from specs.
    let mut declared_uids: HashSet<String> = HashSet::new();
    for spec_path in spec_files {
        let rel = spec_path
            .strip_prefix(repo_path)
            .unwrap_or(spec_path)
            .to_string_lossy()
            .into_owned();
        let source = match std::fs::read_to_string(spec_path) {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!("skip unreadable spec {rel}: {e}");
                continue;
            }
        };
        for sc in crate::contracts::parse_spec_file(&rel, &source) {
            let contract = sc.into_contract(r_uid, &rel, 1.0);
            declared_uids.insert(contract.uid.clone());
            store.insert_contract(&contract)?;
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
        let base_source = std::fs::read_to_string(repo_path.join(&hf.rel_path))
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
                store.insert_contract(&contract)?;
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

    let mut result = IncrementalResult::default();

    for change in &changes {
        match change {
            crate::git_diff::FileChange::Added(rel_path) => {
                if path_in_skip_dir(rel_path) || !is_parseable(rel_path) {
                    result.files_skipped += 1;
                    continue;
                }
                let added = process_added_or_modified_file(repo_path, rel_path, &r_uid, &store)?;
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
                let removed = store
                    .delete_symbols_in_file(&r_uid, &rel_str)
                    .with_context(|| format!("delete_symbols_in_file {}", rel_str))?;
                result.symbols_removed += removed;

                // Re-parse and insert.
                let added = process_added_or_modified_file(repo_path, rel_path, &r_uid, &store)?;
                result.symbols_added += added;
                result.files_modified += 1;
            }
            crate::git_diff::FileChange::Deleted(rel_path) => {
                let rel_str = rel_path.to_string_lossy();
                let removed = store
                    .delete_symbols_in_file(&r_uid, &rel_str)
                    .with_context(|| format!("delete_symbols_in_file {}", rel_str))?;
                result.symbols_removed += removed;

                let f_uid = nestweaver_schema::file_uid(&r_uid, &rel_str);
                store
                    .delete_file_node(&f_uid)
                    .with_context(|| format!("delete_file_node {}", rel_str))?;
                result.files_deleted += 1;
            }
            crate::git_diff::FileChange::Renamed { from, to } => {
                let from_str = from.to_string_lossy();
                let to_str = to.to_string_lossy();

                if is_parseable(to) && !path_in_skip_dir(to) {
                    // Update symbol file_path references.
                    store
                        .update_symbol_file_paths(&r_uid, &from_str, &to_str)
                        .with_context(|| {
                            format!("update_symbol_file_paths {} -> {}", from_str, to_str)
                        })?;
                } else {
                    // Destination is not parseable — just delete the old symbols.
                    let removed = store
                        .delete_symbols_in_file(&r_uid, &from_str)
                        .with_context(|| format!("delete_symbols_in_file {}", from_str))?;
                    result.symbols_removed += removed;
                }

                // Re-key the File node: delete old, insert new.
                let old_f_uid = nestweaver_schema::file_uid(&r_uid, &from_str);
                store
                    .delete_file_node(&old_f_uid)
                    .with_context(|| format!("delete_file_node (rename from) {}", from_str))?;

                if is_parseable(to) && !path_in_skip_dir(to) {
                    // Re-read from disk and re-insert the file + symbols under the new path.
                    let removed2 = store
                        .delete_symbols_in_file(&r_uid, &to_str)
                        .with_context(|| "delete_symbols_in_file (rename to)")?;
                    result.symbols_removed += removed2;

                    let added = process_added_or_modified_file(repo_path, to, &r_uid, &store)?;
                    result.symbols_added += added;
                }

                result.files_renamed += 1;
            }
        }
    }

    // 6. Update the stored SHA.
    store
        .update_repo_sha(&r_uid, &new_sha)
        .with_context(|| "update_repo_sha")?;

    // 7. Recompute PageRank.
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

/// Parse a single file and insert its File node, Symbol nodes, and edges.
/// Returns the number of symbols inserted.
fn process_added_or_modified_file(
    repo_path: &Path,
    rel_path: &std::path::Path,
    r_uid: &str,
    store: &nestweaver_store::GraphStore,
) -> Result<usize, anyhow::Error> {
    use nestweaver_parser::{RawReference, RawSymbol};
    use nestweaver_resolver::{discover_workspace_context, resolve_references_with_context};
    use nestweaver_schema::{File, Symbol, file_uid, symbol_uid};

    let abs_path = repo_path.join(rel_path);
    let rel_str = rel_path.to_string_lossy().into_owned();

    let source = match std::fs::read_to_string(&abs_path) {
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

    let content_hash = sha2_hex(&source);
    let f_uid = file_uid(r_uid, &rel_str);

    // Insert the File node.
    let file = File {
        uid: f_uid.clone(),
        path: rel_str.clone(),
        repo_uid: r_uid.to_string(),
        content_hash,
    };
    store
        .insert_file(&file)
        .with_context(|| format!("insert_file {}", rel_str))?;
    store
        .insert_repo_file_edge(r_uid, &f_uid)
        .with_context(|| format!("insert_repo_file_edge {}", rel_str))?;

    let mut symbols: Vec<nestweaver_schema::Symbol> = Vec::new();
    let mut file_sym_pairs: Vec<(String, String)> = Vec::new();

    for raw_sym in &parsed.symbols {
        let s_uid = symbol_uid(r_uid, &rel_str, &raw_sym.name, raw_sym.start_line);
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
        };
        symbols.push(sym);
        file_sym_pairs.push((f_uid.clone(), s_uid));
    }

    let sym_count = symbols.len();

    store
        .batch_insert_symbols(&symbols)
        .with_context(|| format!("batch_insert_symbols {}", rel_str))?;

    let file_sym_refs: Vec<(&str, &str)> = file_sym_pairs
        .iter()
        .map(|(f, s)| (f.as_str(), s.as_str()))
        .collect();
    store
        .batch_insert_file_symbol_edges(&file_sym_refs)
        .with_context(|| format!("batch_insert_file_symbol_edges {}", rel_str))?;

    // Resolve cross-file edges within this file only (single-file scope).
    let lang = nestweaver_parser::detect_language(&abs_path)
        .unwrap_or(nestweaver_schema::Language::JavaScript);

    // Load workspace context for JS/TS monorepo resolution.
    let workspace_ctx = if matches!(
        lang,
        nestweaver_schema::Language::JavaScript
            | nestweaver_schema::Language::TypeScript
            | nestweaver_schema::Language::Vue
            | nestweaver_schema::Language::Svelte
            | nestweaver_schema::Language::Astro
    ) {
        discover_workspace_context(repo_path)
    } else {
        Default::default()
    };

    let file_data: Vec<(String, Vec<RawSymbol>, Vec<RawReference>)> = vec![(
        rel_str.clone(),
        parsed.symbols.clone(),
        parsed.references.clone(),
    )];
    let resolved_edges =
        resolve_references_with_context(&file_data, lang, r_uid, &workspace_ctx, None);
    let insertable_edges: Vec<_> = resolved_edges
        .into_iter()
        .filter(|e| !e.target_uid.starts_with("unresolved:"))
        .collect();
    if !insertable_edges.is_empty() {
        store
            .batch_insert_edges(&insertable_edges)
            .with_context(|| format!("batch_insert_edges {}", rel_str))?;
    }

    Ok(sym_count)
}

/// Delete all File nodes (and their symbols) that belong to a repo,
/// then delete the Repo node itself.  Used before a forced full re-index.
///
/// Uses two bulk DETACH DELETE queries (one for Symbol, one for File)
/// instead of the previous per-file loop that issued O(2N) queries.
fn delete_repo_all_data(
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
    crate::migrate_sidecar(db_path, "filemeta.json", ".filemeta.json");
    let filemeta_path = crate::sidecar_path(db_path, ".filemeta.json");
    let filemeta_cache = load_filemeta_cache(&filemeta_path);
    let mut new_filemeta = FileMetaCache::new();

    let result = index_into_store(
        repo_path,
        store,
        instance_id,
        repo_url,
        new_sha,
        Some(&filemeta_cache),
        Some(&mut new_filemeta),
        name,
    )?;

    // Persist the updated filemeta sidecar.
    if let Err(e) = save_filemeta_cache(&new_filemeta, &filemeta_path) {
        tracing::warn!("failed to save filemeta cache: {e}");
    }

    // Update the manifest cache sidecar (same as index_directory does).
    let manifest = crate::manifest::parse_manifest(repo_path);
    crate::migrate_sidecar(db_path, "manifests.json", ".manifests.json");
    let cache_path = crate::sidecar_path(db_path, ".manifests.json");
    let r_uid = nestweaver_schema::repo_uid(instance_id, repo_url);
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

/// Compute a SHA-256 hex digest of a string (used for file content_hash).
fn sha2_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
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

        let cache = FileMetaCache::new();
        match tiered_change_check(&file_path, "hello.js", &cache).unwrap() {
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
                content_hash: sha2_hex(content),
            },
        );

        match tiered_change_check(&file_path, "hello.js", &cache).unwrap() {
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
                content_hash: sha2_hex(content),
            },
        );

        match tiered_change_check(&file_path, "hello.js", &cache).unwrap() {
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
                content_hash: sha2_hex("different content!"),
            },
        );

        match tiered_change_check(&file_path, "hello.js", &cache2).unwrap() {
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
                content_hash: sha2_hex("old content"),
            },
        );

        match tiered_change_check(&file_path, "hello.js", &cache).unwrap() {
            ChangeVerdict::Changed {
                source,
                content_hash,
                ..
            } => {
                assert!(source.contains("return 42"));
                assert_eq!(content_hash, sha2_hex(new_content));
            }
            ChangeVerdict::Unchanged => panic!("expected Changed for different-size file"),
        }
    }

    #[test]
    fn filemeta_sidecar_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let sidecar_path = dir.path().join("test.filemeta.json");

        let mut cache = FileMetaCache::new();
        cache.insert(
            "src/main.js".to_string(),
            CachedFileMeta {
                mtime_secs: 1234567890,
                size_bytes: 42,
                content_hash: "abc123".to_string(),
            },
        );

        save_filemeta_cache(&cache, &sidecar_path).unwrap();
        let loaded = load_filemeta_cache(&sidecar_path);
        assert_eq!(loaded.len(), 1);
        let entry = loaded.get("src/main.js").unwrap();
        assert_eq!(entry.mtime_secs, 1234567890);
        assert_eq!(entry.size_bytes, 42);
        assert_eq!(entry.content_hash, "abc123");
    }

    #[test]
    fn filemeta_cache_missing_file_returns_empty() {
        let cache = load_filemeta_cache(Path::new("/nonexistent/filemeta.json"));
        assert!(cache.is_empty());
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
        let cache = load_filemeta_cache(&filemeta_path);
        assert_eq!(cache.len(), 1, "one file should be in the cache");
        assert!(cache.contains_key("main.js"));
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
}
