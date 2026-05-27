use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Context;
use indicatif::{ProgressBar, ProgressStyle};
use nestweaver_parser::{RawReference, RawSymbol, SkippedFile, detect_language, parse_source};
use nestweaver_resolver::resolve_references;
use nestweaver_schema::{
    File, Language, Repo, Service, Symbol, file_uid, repo_uid, service_uid, symbol_uid,
};
use nestweaver_store::GraphStore;
use walkdir::WalkDir;

/// Result returned by the indexing functions.
pub struct IndexResult {
    pub symbols_count: usize,
    pub edges_count: usize,
    pub files_count: usize,
    pub skipped_files: Vec<SkippedFile>,
}

/// Directory names to skip when walking the repository tree.
const SKIP_DIRS: &[&str] = &[
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
pub fn index_directory(
    repo_path: &Path,
    db_path: &Path,
    instance_id: &str,
    repo_url: &str,
    indexed_sha: &str,
) -> Result<IndexResult, anyhow::Error> {
    let store = GraphStore::open_or_create(db_path)
        .with_context(|| format!("failed to open/create GraphStore at {}", db_path.display()))?;
    let result = index_into_store(repo_path, &store, instance_id, repo_url, indexed_sha)?;

    // Parse the manifest and update the sidecar cache alongside the DB.
    let manifest = crate::manifest::parse_manifest(repo_path);
    let cache_path = db_path.with_extension("manifests.json");
    let r_uid = nestweaver_schema::repo_uid(instance_id, repo_url);
    let mut cache = crate::manifest::load_manifest_cache(&cache_path).unwrap_or_default();
    cache.insert(r_uid, manifest);
    if let Err(e) = crate::manifest::save_manifest_cache(&cache, &cache_path) {
        tracing::warn!("failed to save manifest cache: {e}");
    }

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
    let result = index_into_store(repo_path, &store, instance_id, repo_url, indexed_sha)?;
    Ok((result, store))
}

/// Core indexing logic shared by both public functions.
fn index_into_store(
    repo_path: &Path,
    store: &GraphStore,
    instance_id: &str,
    repo_url: &str,
    indexed_sha: &str,
) -> Result<IndexResult, anyhow::Error> {
    let started = Instant::now();

    // 1. Insert the Repo node.
    let r_uid = repo_uid(instance_id, repo_url);
    let repo = Repo {
        uid: r_uid.clone(),
        url: repo_url.to_string(),
        indexed_sha: indexed_sha.to_string(),
        staleness_commits_behind: 0,
        instance_id: instance_id.to_string(),
    };
    store.insert_repo(&repo).context("insert_repo")?;

    // ── Phase 1: Scan files ───────────────────────────────────────────────
    let scan_pb = ProgressBar::new_spinner();
    scan_pb.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner()),
    );
    scan_pb.set_message("Scanning files...");

    let mut file_entries: Vec<(PathBuf, Language)> = Vec::new();

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

    // ── Phase 2: Parse files ──────────────────────────────────────────────
    let total_files = file_entries.len() as u64;
    let parse_pb = ProgressBar::new(total_files);
    parse_pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.cyan} Parsing [{bar:30.cyan/dim}] {pos}/{len} {wide_msg}",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("━╸─"),
    );

    // Vectors for batch inserts.
    let mut all_files: Vec<File> = Vec::new();
    let mut all_symbols: Vec<Symbol> = Vec::new();
    // (repo_uid, file_uid) pairs for REPO_HAS_FILE edges.
    let mut repo_file_edge_pairs: Vec<(String, String)> = Vec::new();
    // (file_uid, symbol_uid) pairs for FILE_HAS_SYMBOL edges.
    let mut file_symbol_edge_pairs: Vec<(String, String)> = Vec::new();

    // Per-file raw data for the full cross-file resolver.
    let mut parsed_files_for_resolver: Vec<(String, Vec<RawSymbol>, Vec<RawReference>)> =
        Vec::new();

    // Track the detected language per file for choosing the resolver language.
    let mut detected_languages: Vec<Language> = Vec::new();

    let mut files_count = 0usize;
    let mut symbols_count = 0usize;
    let mut skipped_files: Vec<SkippedFile> = Vec::new();

    for (path, lang) in &file_entries {
        // Show current file being parsed.
        let display_name = path
            .strip_prefix(repo_path)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();
        parse_pb.set_message(display_name.clone());

        // Read file content.
        let source = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(err) => {
                skipped_files.push(SkippedFile {
                    path: path.to_string_lossy().into_owned(),
                    reason: format!("read error: {err}"),
                });
                parse_pb.inc(1);
                continue;
            }
        };

        // Parse the file.
        let parsed = match parse_source(path, &source) {
            Ok(p) => p,
            Err(err) => {
                skipped_files.push(SkippedFile {
                    path: path.to_string_lossy().into_owned(),
                    reason: err.to_string(),
                });
                parse_pb.inc(1);
                continue;
            }
        };

        // Compute relative path for stable file UIDs.
        let rel_path = display_name;

        let content_hash = sha2_hex(&source);
        let f_uid = file_uid(&r_uid, &rel_path);

        // Collect File node.
        all_files.push(File {
            uid: f_uid.clone(),
            path: rel_path.clone(),
            repo_uid: r_uid.clone(),
            content_hash,
        });

        // Collect REPO_HAS_FILE edge.
        repo_file_edge_pairs.push((r_uid.clone(), f_uid.clone()));

        files_count += 1;
        detected_languages.push(*lang);

        // Collect Symbol nodes and FILE_HAS_SYMBOL edges.
        for raw_sym in &parsed.symbols {
            let s_uid = symbol_uid(&r_uid, &rel_path, &raw_sym.name, raw_sym.start_line);

            all_symbols.push(Symbol {
                uid: s_uid.clone(),
                name: raw_sym.name.clone(),
                kind: raw_sym.kind,
                repo_uid: r_uid.clone(),
                file_path: rel_path.clone(),
                start_line: raw_sym.start_line,
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
            });

            file_symbol_edge_pairs.push((f_uid.clone(), s_uid.clone()));
            symbols_count += 1;
        }

        // Collect raw parsed data for the cross-file resolver.
        parsed_files_for_resolver.push((
            rel_path.clone(),
            parsed.symbols.clone(),
            parsed.references.clone(),
        ));

        parse_pb.inc(1);
    }

    parse_pb.finish_and_clear();

    // 3. Batch insert all File nodes.
    store
        .batch_insert_files(&all_files)
        .context("batch_insert_files")?;

    // 4. Batch insert all Symbol nodes.
    store
        .batch_insert_symbols(&all_symbols)
        .context("batch_insert_symbols")?;

    // 5. Batch insert all REPO_HAS_FILE edges.
    let repo_file_refs: Vec<(&str, &str)> = repo_file_edge_pairs
        .iter()
        .map(|(r, f)| (r.as_str(), f.as_str()))
        .collect();
    store
        .batch_insert_repo_file_edges(&repo_file_refs)
        .context("batch_insert_repo_file_edges")?;

    // 6. Batch insert all FILE_HAS_SYMBOL edges.
    let file_sym_refs: Vec<(&str, &str)> = file_symbol_edge_pairs
        .iter()
        .map(|(f, s)| (f.as_str(), s.as_str()))
        .collect();
    store
        .batch_insert_file_symbol_edges(&file_sym_refs)
        .context("batch_insert_file_symbol_edges")?;

    // 7. Group symbols into Services by directory.
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

    for svc in &all_services {
        store.insert_service(svc).context("insert_service")?;
    }

    let svc_sym_refs: Vec<(&str, &str)> = service_symbol_pairs
        .iter()
        .map(|(s, sym)| (s.as_str(), sym.as_str()))
        .collect();
    store
        .batch_insert_service_symbol_edges(&svc_sym_refs)
        .context("batch_insert_service_symbol_edges")?;

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

    let resolved_edges = resolve_references(&parsed_files_for_resolver, language, &r_uid);

    // Filter out unresolved edges whose target doesn't exist in the DB.
    let insertable_edges: Vec<_> = resolved_edges
        .into_iter()
        .filter(|e| !e.target_uid.starts_with("unresolved:"))
        .collect();

    let edges_count = insertable_edges.len();
    store
        .batch_insert_edges(&insertable_edges)
        .context("batch_insert_edges (resolved)")?;

    resolve_pb.finish_and_clear();

    // ── Summary ───────────────────────────────────────────────────────────
    let elapsed = started.elapsed();
    eprintln!(
        "Done: {} files, {} symbols, {} edges ({:.1}s)",
        files_count,
        symbols_count,
        edges_count,
        elapsed.as_secs_f64(),
    );

    tracing::info!(
        total_files = files_count,
        symbols = symbols_count,
        "indexing complete"
    );

    Ok(IndexResult {
        symbols_count,
        edges_count,
        files_count,
        skipped_files,
    })
}

/// Returns true if the given path has a supported language extension.
fn is_parseable(path: &Path) -> bool {
    detect_language(path).is_some()
}

/// Returns true if the file looks like a minified bundle, webpack output,
/// or other generated artifact that would produce noise in the graph.
fn is_minified_or_bundled(path: &Path) -> bool {
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
fn path_in_skip_dir(path: &Path) -> bool {
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
pub fn incremental_index(
    repo_path: &Path,
    db_path: &Path,
    instance_id: &str,
    repo_url: &str,
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
            return full_index_fallback(repo_path, db_path, &store, instance_id, repo_url, "local");
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
        return full_index_fallback(repo_path, db_path, &store, instance_id, repo_url, &new_sha);
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

    let pr_path = db_path.with_extension("pagerank.json");
    if let Err(e) = store.save_pagerank_cache(&pr_path) {
        tracing::warn!("failed to save pagerank cache: {e}");
    }

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
    use nestweaver_resolver::resolve_references;
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

    let file_data: Vec<(String, Vec<RawSymbol>, Vec<RawReference>)> = vec![(
        rel_str.clone(),
        parsed.symbols.clone(),
        parsed.references.clone(),
    )];
    let resolved_edges = resolve_references(&file_data, lang, r_uid);
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
fn delete_repo_all_data(
    store: &nestweaver_store::GraphStore,
    r_uid: &str,
) -> Result<(), anyhow::Error> {
    let files = store
        .list_files_by_repo(r_uid)
        .with_context(|| "list_files_by_repo")?;

    for (f_uid, f_path) in &files {
        store
            .delete_symbols_in_file(r_uid, f_path)
            .with_context(|| format!("delete_symbols_in_file {}", f_path))?;
        store
            .delete_file_node(f_uid)
            .with_context(|| format!("delete_file_node {}", f_uid))?;
    }

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
) -> Result<IncrementalResult, anyhow::Error> {
    let result = index_into_store(repo_path, store, instance_id, repo_url, new_sha)?;

    // Update the manifest cache sidecar (same as index_directory does).
    let manifest = crate::manifest::parse_manifest(repo_path);
    let cache_path = db_path.with_extension("manifests.json");
    let r_uid = nestweaver_schema::repo_uid(instance_id, repo_url);
    let mut cache = crate::manifest::load_manifest_cache(&cache_path).unwrap_or_default();
    cache.insert(r_uid, manifest);
    if let Err(e) = crate::manifest::save_manifest_cache(&cache, &cache_path) {
        tracing::warn!("failed to save manifest cache: {e}");
    }

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
}
