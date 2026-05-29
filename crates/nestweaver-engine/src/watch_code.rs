//! File watcher for live incremental code re-indexing.
//!
//! Watches a repository directory for changes to supported source files
//! (any extension recognised by `nestweaver_parser::detect_language`).
//! Uses a 2-second debounce window to batch rapid saves into a single
//! re-index pass. On each trigger the changed files are re-parsed: new
//! or modified files get their symbols replaced via delete + re-insert;
//! deleted files have their symbols and File node removed.
//!
//! Threading model mirrors `BrainWatcher`: synchronous + blocking. The
//! caller owns the thread (the CLI `watch` command runs it in the
//! foreground).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use nestweaver_parser::detect_language;
use nestweaver_store::{GraphScope, GraphStore};
use notify::RecursiveMode;
use notify_debouncer_mini::{DebouncedEvent, new_debouncer};

use crate::index::{is_minified_or_bundled, path_in_skip_dir};
use crate::watcher::ShutdownHandle;

/// Live file-watcher for a code repository. Construct via `new`, then
/// call `run` — it blocks until `stop()` is signalled or the watcher
/// hits a fatal error.
pub struct CodeWatcher {
    db_path: PathBuf,
    repo_root: PathBuf,
    instance_id: String,
    stop_flag: Arc<AtomicBool>,
}

impl CodeWatcher {
    pub fn new(
        db_path: impl Into<PathBuf>,
        repo_root: impl Into<PathBuf>,
        instance_id: impl Into<String>,
    ) -> Self {
        let repo_root: PathBuf = repo_root.into();
        let repo_root = std::fs::canonicalize(&repo_root).unwrap_or(repo_root);
        Self {
            db_path: db_path.into(),
            repo_root,
            instance_id: instance_id.into(),
            stop_flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Returns a handle that can request graceful shutdown from another
    /// thread.
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle::from_flag(self.stop_flag.clone())
    }

    /// Block until shutdown is requested or the underlying debouncer
    /// errors. Returns `Ok(())` on graceful shutdown.
    ///
    /// Opens its own `GraphStore` from `self.db_path`. For sharing a store
    /// with the web server, use `run_with_store` instead.
    pub fn run(self) -> Result<(), anyhow::Error> {
        let store = Arc::new(
            GraphStore::open_or_create(&self.db_path)
                .with_context(|| format!("open GraphStore at {}", self.db_path.display()))?,
        );
        self.run_inner(store, None)
    }

    /// Like `run`, but uses a caller-provided `Arc<GraphStore>` and invokes
    /// `on_change` after every batch that mutates the graph. The callback
    /// also fires after the graph-generation counter is bumped so the web
    /// server can emit an SSE event to connected clients.
    pub fn run_with_store(
        self,
        store: Arc<GraphStore>,
        on_change: Option<Box<dyn Fn() + Send>>,
    ) -> Result<(), anyhow::Error> {
        self.run_inner(store, on_change)
    }

    /// Shared implementation used by both `run` and `run_with_store`.
    fn run_inner(
        self,
        store: Arc<GraphStore>,
        on_change: Option<Box<dyn Fn() + Send>>,
    ) -> Result<(), anyhow::Error> {
        let repo_url = format!("file://{}", self.repo_root.display());
        let r_uid = nestweaver_schema::repo_uid(&self.instance_id, &repo_url);

        // Ensure the Repo node exists so incremental updates can attach
        // File and Symbol nodes. If there's no prior index we create a
        // minimal Repo node; the watcher will populate it file-by-file.
        if store.lookup_repo(&r_uid)?.is_none() {
            store
                .insert_repo(&nestweaver_schema::Repo {
                    uid: r_uid.clone(),
                    url: repo_url.clone(),
                    indexed_sha: "watch".to_string(),
                    staleness_commits_behind: 0,
                    instance_id: self.instance_id.clone(),
                    name: None,
                })
                .context("insert initial Repo node")?;
        }

        // Channel from the debouncer into our loop.
        let (tx, rx) = std::sync::mpsc::channel::<DebounceResult>();
        let mut debouncer = new_debouncer(
            Duration::from_secs(2),
            move |res: Result<Vec<DebouncedEvent>, notify::Error>| {
                let _ = tx.send(res);
            },
        )
        .context("init code debouncer")?;
        debouncer
            .watcher()
            .watch(&self.repo_root, RecursiveMode::Recursive)
            .with_context(|| format!("watch {}", self.repo_root.display()))?;

        tracing::info!(
            repo = %self.repo_root.display(),
            db = %self.db_path.display(),
            "CodeWatcher running"
        );

        loop {
            if self.stop_flag.load(Ordering::Relaxed) {
                tracing::info!("CodeWatcher stop requested; exiting");
                return Ok(());
            }

            let batch = match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(Ok(events)) => events,
                Ok(Err(err)) => {
                    if !self.repo_root.exists() {
                        tracing::error!(
                            repo = %self.repo_root.display(),
                            "repo root no longer exists; watcher exiting"
                        );
                        return Err(anyhow::anyhow!(
                            "repo root '{}' was deleted or unmounted",
                            self.repo_root.display()
                        ));
                    }
                    tracing::warn!("notify error: {err}");
                    continue;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if !self.repo_root.exists() {
                        tracing::error!(
                            repo = %self.repo_root.display(),
                            "repo root vanished during watch; exiting"
                        );
                        return Err(anyhow::anyhow!(
                            "repo root '{}' was deleted or unmounted",
                            self.repo_root.display()
                        ));
                    }
                    continue;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    tracing::warn!("code debouncer disconnected; exiting");
                    return Ok(());
                }
            };

            // Deduplicate paths within the batch — the debouncer may
            // fire multiple events for the same file.
            let mut unique_paths: HashSet<PathBuf> = HashSet::new();
            for event in &batch {
                unique_paths.insert(event.path.clone());
            }

            // Filter to supported source files not in skip dirs.
            let relevant: Vec<PathBuf> = unique_paths
                .into_iter()
                .filter(|p| !path_in_skip_dir(p))
                .filter(|p| is_supported_source(p))
                .filter(|p| !is_minified_or_bundled(p))
                .collect();

            if relevant.is_empty() {
                continue;
            }

            let start = Instant::now();
            let mut files_processed = 0usize;

            for path in &relevant {
                let rel_path = match path.strip_prefix(&self.repo_root) {
                    Ok(r) => r,
                    Err(_) => {
                        tracing::debug!(
                            path = %path.display(),
                            "path outside repo root; skipping"
                        );
                        continue;
                    }
                };
                let rel_str = rel_path.to_string_lossy();

                if path.exists() {
                    // File was created or modified: delete old data, re-parse, re-insert.
                    let removed = store.delete_symbols_in_file(&r_uid, &rel_str).unwrap_or(0);
                    if removed > 0 {
                        tracing::debug!(
                            path = %rel_str,
                            removed,
                            "removed stale symbols before re-index"
                        );
                    }

                    match reindex_file(&self.repo_root, rel_path, &r_uid, &store) {
                        Ok(syms) => {
                            tracing::debug!(
                                path = %rel_str,
                                symbols = syms,
                                "re-indexed file"
                            );
                            files_processed += 1;
                        }
                        Err(e) => {
                            tracing::warn!(
                                path = %rel_str,
                                error = %e,
                                "failed to re-index file"
                            );
                        }
                    }
                } else {
                    // File was deleted: remove its symbols and File node.
                    let removed = store.delete_symbols_in_file(&r_uid, &rel_str).unwrap_or(0);
                    let f_uid = nestweaver_schema::file_uid(&r_uid, &rel_str);
                    let _ = store.delete_file_node(&f_uid);
                    if removed > 0 {
                        tracing::debug!(
                            path = %rel_str,
                            removed,
                            "deleted symbols for removed file"
                        );
                    }
                    files_processed += 1;
                }
            }

            if files_processed > 0 {
                // Recompute PageRank so queries reflect the updated graph.
                if let Err(e) = store.compute_pagerank(0.85, 20, &GraphScope::code_only()) {
                    tracing::warn!("post-batch PageRank recompute failed: {e}");
                } else {
                    let pr_path = crate::sidecar_path(&self.db_path, ".pagerank.json");
                    let _ = store.save_pagerank_cache(&pr_path);
                }

                let duration = start.elapsed();
                tracing::info!(
                    files_processed,
                    elapsed_secs = format!("{:.1}", duration.as_secs_f64()),
                    "Re-indexed {} file(s) ({:.1}s)",
                    files_processed,
                    duration.as_secs_f64()
                );

                // Bump the graph generation counter so consumers (e.g. the
                // web server SSE handler) can detect that the graph changed.
                store.bump_graph_generation();
                if let Some(ref cb) = on_change {
                    cb();
                }
            }
        }
    }
}

/// The debouncer callback type.
type DebounceResult = Result<Vec<DebouncedEvent>, notify::Error>;

/// Returns true if the file is a supported source language.
fn is_supported_source(path: &Path) -> bool {
    detect_language(path).is_some()
}

/// Parse a single source file and insert its File node, Symbol nodes, and
/// edges. Returns the number of symbols inserted. Mirrors the logic in
/// `index.rs::process_added_or_modified_file`.
fn reindex_file(
    repo_root: &Path,
    rel_path: &Path,
    r_uid: &str,
    store: &GraphStore,
) -> Result<usize, anyhow::Error> {
    use nestweaver_parser::{RawReference, RawSymbol, parse_source};
    use nestweaver_resolver::{discover_workspace_context, resolve_references_with_context};
    use nestweaver_schema::{File, Symbol, file_uid, symbol_uid};
    use sha2::{Digest, Sha256};

    let abs_path = repo_root.join(rel_path);
    let rel_str = rel_path.to_string_lossy().into_owned();

    let source = std::fs::read_to_string(&abs_path)
        .with_context(|| format!("read {}", abs_path.display()))?;

    let parsed = match parse_source(&abs_path, &source) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(path = %abs_path.display(), "parse error: {e}; skipping");
            return Ok(0);
        }
    };

    let content_hash = {
        let mut h = Sha256::new();
        h.update(source.as_bytes());
        hex::encode(h.finalize())
    };
    let f_uid = file_uid(r_uid, &rel_str);

    // Insert or replace the File node.
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

    let mut symbols: Vec<Symbol> = Vec::new();
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

    // Resolve cross-file edges within this file.
    let lang = detect_language(&abs_path).unwrap_or(nestweaver_schema::Language::JavaScript);

    // Load workspace context for JS/TS monorepo resolution.
    let workspace_ctx = if matches!(
        lang,
        nestweaver_schema::Language::JavaScript
            | nestweaver_schema::Language::TypeScript
            | nestweaver_schema::Language::Vue
            | nestweaver_schema::Language::Svelte
            | nestweaver_schema::Language::Astro
    ) {
        discover_workspace_context(repo_root)
    } else {
        Default::default()
    };

    let file_data: Vec<(String, Vec<RawSymbol>, Vec<RawReference>)> = vec![(
        rel_str.clone(),
        parsed.symbols.clone(),
        parsed.references.clone(),
    )];
    let resolved_edges = resolve_references_with_context(&file_data, lang, r_uid, &workspace_ctx);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_dir_detection() {
        let p = Path::new("/repo/node_modules/foo/bar.js");
        assert!(path_in_skip_dir(p));
        let p = Path::new("/repo/.git/HEAD");
        assert!(path_in_skip_dir(p));
        let p = Path::new("/repo/src/main.rs");
        assert!(!path_in_skip_dir(p));
    }

    #[test]
    fn supported_source_detection() {
        assert!(is_supported_source(Path::new("foo.js")));
        assert!(is_supported_source(Path::new("bar.ts")));
        assert!(is_supported_source(Path::new("baz.py")));
        assert!(is_supported_source(Path::new("qux.rs")));
        assert!(is_supported_source(Path::new("Main.java")));
        assert!(!is_supported_source(Path::new("readme.md")));
        assert!(!is_supported_source(Path::new("data.json")));
        assert!(!is_supported_source(Path::new("Makefile")));
    }

    #[test]
    fn minified_detection() {
        assert!(is_minified_or_bundled(Path::new("app.min.js")));
        assert!(is_minified_or_bundled(Path::new("vendor.bundle.js")));
        assert!(is_minified_or_bundled(Path::new("main.chunk.js")));
        assert!(!is_minified_or_bundled(Path::new("app.js")));
        assert!(!is_minified_or_bundled(Path::new("src/main.rs")));
    }
}
