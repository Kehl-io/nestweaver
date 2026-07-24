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
    fn establish_graph_publication_with_io<'a>(
        &self,
        store: &'a GraphStore,
        io: &dyn crate::index::IndexEpilogueIo,
    ) -> Result<
        nestweaver_store::IndexPublicationLease<'a>,
        crate::index::DeletionReconciliationError,
    > {
        crate::index::establish_index_publication_marker_with_io(
            store,
            Some(&self.db_path),
            "code watcher batch",
            io,
        )
    }

    fn finalize_graph_publication_with_io(
        &self,
        publication: nestweaver_store::IndexPublicationLease<'_>,
        io: &dyn crate::index::IndexEpilogueIo,
    ) -> Result<(), crate::index::DeletionReconciliationError> {
        crate::index::finalize_committed_index_for_scope_with_io(
            publication,
            Some(&self.db_path),
            "code watcher batch",
            io,
            Some(&GraphScope::code_only()),
        )
    }

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
        // Identity decision (see `resolve_watch_identity`): adopt an existing
        // `file://` graph rather than re-identifying+pruning, so a watch-first
        // start over a legacy DB never empties the graph.
        let root_path = self.repo_root.display().to_string();
        let (repo_url, r_uid) = resolve_watch_identity(&store, &self.instance_id, &self.repo_root)?;

        // Ensure the Repo node exists so incremental updates can attach
        // File and Symbol nodes. If there's no prior index we create a
        // minimal Repo node; the watcher will populate it file-by-file.
        if store.lookup_repo(&r_uid)?.is_none() {
            let publication = self.establish_graph_publication_with_io(
                &store,
                &crate::index::FileSystemIndexEpilogueIo,
            )?;
            let insert_result = store
                .insert_repo(&nestweaver_schema::Repo {
                    uid: r_uid.clone(),
                    url: repo_url.clone(),
                    indexed_sha: "watch".to_string(),
                    staleness_commits_behind: 0,
                    instance_id: self.instance_id.clone(),
                    name: None,
                    root_path: Some(root_path.clone()),
                })
                .context("insert initial Repo node");
            let finalization = self.finalize_graph_publication_with_io(
                publication,
                &crate::index::FileSystemIndexEpilogueIo,
            );
            match (insert_result, finalization) {
                (Ok(()), Ok(())) => {}
                (Err(error), Ok(())) => return Err(error),
                (Ok(()), Err(error)) => return Err(error.into()),
                (Err(error), Err(finalization)) => {
                    return Err(error.context(format!(
                        "initial Repo insert also failed mandatory publication: {finalization}"
                    )));
                }
            }
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

            // Publish a dirty marker and reserve N+1 before the first graph
            // mutation. Any later error leaves reopen fail-closed until the
            // mandatory N+2 finalization completes.
            let publication = self.establish_graph_publication_with_io(
                &store,
                &crate::index::FileSystemIndexEpilogueIo,
            )?;

            let start = Instant::now();
            // Per-file PRE-mutation failures are logged and skipped inside
            // `reindex_paths`: a single unreadable file must not kill the
            // watch session (the next edit batch retries), and because the
            // reindex re-parses BEFORE deleting, a failed file keeps its
            // previous graph data instead of vanishing from the graph while
            // still on disk. A POST-mutation failure (delete landed, later
            // insert failed) is different: the graph holds partial data, so
            // the batch is reported as failed after publication finalizes —
            // it must not look cleanly successful.
            let (files_processed, batch_error) = self.reindex_paths(&store, &r_uid, &relevant);

            let finalization = self.finalize_graph_publication_with_io(
                publication,
                &crate::index::FileSystemIndexEpilogueIo,
            );
            if finalization.is_ok() && batch_error.is_none() {
                let duration = start.elapsed();
                tracing::info!(
                    files_processed,
                    elapsed_secs = format!("{:.1}", duration.as_secs_f64()),
                    "Re-indexed {} file(s) ({:.1}s)",
                    files_processed,
                    duration.as_secs_f64()
                );

                if let Some(ref cb) = on_change {
                    cb();
                }
            }
            if let Err(error) = finalization {
                anyhow::bail!("code watcher batch failed mandatory graph publication: {error}");
            }
            if let Some(error) = batch_error {
                anyhow::bail!(
                    "code watcher batch partially failed mid-mutation (partial index data; \
                     the next edit retries, or repair with `nestweaver index --force`): {error}"
                );
            }
        }
    }

    /// Re-index one batch of changed paths against the live graph. Returns
    /// the number of files processed plus the FIRST post-mutation failure,
    /// if any — a post-mutation failure means the graph holds partial data
    /// for that file and the batch must not be reported as cleanly
    /// successful. Mirrors the engine's incremental index
    /// (`index.rs::incremental_index_with_name_and_io`): nw-008 Phase 0
    /// collects reverse-dependents from the live graph BEFORE any mutation,
    /// and Phase 2 re-resolves them afterwards so the cross-file edges the
    /// per-file `DETACH DELETE` destroys are restored — without this, a
    /// watcher reindex leaves the file's symbols in place but strips ALL
    /// their incoming and outgoing cross-file CALLS/IMPORTS edges.
    fn reindex_paths(
        &self,
        store: &GraphStore,
        r_uid: &str,
        relevant: &[PathBuf],
    ) -> (usize, Option<anyhow::Error>) {
        // Partition into changed (still on disk) / removed (deleted), keeping
        // the absolute path alongside for the mutation loop.
        let mut changed: HashSet<String> = HashSet::new();
        let mut removed: HashSet<String> = HashSet::new();
        let mut paths: Vec<PathBuf> = Vec::new();
        for path in relevant {
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
            let rel_str = rel_path.to_string_lossy().into_owned();
            if path.exists() {
                changed.insert(rel_str);
            } else {
                removed.insert(rel_str);
            }
            paths.push(path.clone());
        }

        // nw-008 Phase 0 — transitive reverse-dependents from the LIVE graph,
        // BEFORE any mutation (the per-file `DETACH DELETE` destroys the
        // edges this query walks).
        let rdeps = crate::index::collect_reverse_dep_files(store, r_uid, &changed, &removed);

        let mut files_processed = 0usize;
        let mut first_post_mutation_error: Option<anyhow::Error> = None;
        for path in &paths {
            let rel_path = match path.strip_prefix(&self.repo_root) {
                Ok(r) => r,
                Err(_) => continue, // already logged above
            };
            let rel_str = rel_path.to_string_lossy();

            if path.exists() {
                // File was created or modified: re-parse, then replace.
                match reindex_file(&self.repo_root, rel_path, r_uid, store) {
                    Ok(Some(syms)) => {
                        tracing::debug!(
                            path = %rel_str,
                            symbols = syms,
                            "re-indexed file"
                        );
                        files_processed += 1;
                    }
                    Ok(None) => {
                        // Parse failure — old graph data preserved
                        // (reindex_file deletes only after a successful
                        // read+parse). Do NOT re-resolve edges for this
                        // file below: its edges were never deleted, and
                        // edge insert is CREATE (duplicates).
                        changed.remove(rel_str.as_ref());
                    }
                    Err(ReindexError::PreMutation(e)) => {
                        // Same keep-old-data treatment for pre-mutation
                        // read/store errors (delete never landed).
                        changed.remove(rel_str.as_ref());
                        tracing::warn!(
                            path = %rel_str,
                            error = %e,
                            "failed to re-index file; keeping previous index data"
                        );
                    }
                    Err(ReindexError::PostMutation(e)) => {
                        // The delete already landed: the graph holds PARTIAL
                        // data for this file, not the old data. Keep the file
                        // in `changed` so Phase 2 re-resolves whatever edges
                        // it can, and report the batch as failed instead of
                        // publishing it as cleanly successful.
                        if first_post_mutation_error.is_none() {
                            first_post_mutation_error = Some(anyhow::anyhow!("{rel_str}: {e}"));
                        }
                        tracing::error!(
                            path = %rel_str,
                            error = %e,
                            "re-index failed mid-mutation; graph holds partial data for this file"
                        );
                    }
                }
            } else {
                // File was deleted: remove its symbols and File node.
                let removed_count = match store.delete_symbols_in_file(r_uid, &rel_str) {
                    Ok(removed) => removed,
                    Err(error) => {
                        tracing::warn!("delete symbols for removed file {rel_str}: {error}");
                        0
                    }
                };
                let f_uid = nestweaver_schema::file_uid(r_uid, &rel_str);
                if let Err(error) = store.delete_file_node(&f_uid) {
                    tracing::warn!("delete removed File node {rel_str}: {error}");
                }
                if removed_count > 0 {
                    tracing::debug!(
                        path = %rel_str,
                        removed = removed_count,
                        "deleted symbols for removed file"
                    );
                }
                files_processed += 1;
            }
        }

        // nw-008 Phase 2 — re-resolve reverse-dependents and surgically
        // restore the cross-file edges the per-file `DETACH DELETE` removed.
        let reader = crate::content_reader::FilesystemReader::new(&self.repo_root);
        match crate::index::reresolve_affected_dependents_on_store(
            &reader, store, r_uid, &changed, &rdeps,
        ) {
            Ok(reresolved) if reresolved > 0 => {
                tracing::info!(
                    edges = reresolved,
                    rdeps = rdeps.len(),
                    "restored cross-file edges via transitive re-resolution"
                );
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("transitive re-resolution failed: {e:#}");
            }
        }

        (files_processed, first_post_mutation_error)
    }
}

/// The debouncer callback type.
type DebounceResult = Result<Vec<DebouncedEvent>, notify::Error>;

/// Decide the repo identity `(url, uid)` the watcher indexes under.
///
/// The watcher does incremental updates ONLY — it never cold-indexes — so it
/// must NEVER prune-and-empty an existing graph: unlike `nw index`, it cannot
/// repopulate, so a prune would leave the graph empty until individual files
/// happen to change (data loss on a watch-first start over a legacy DB).
///
/// Therefore, when this working tree already has a graph under its legacy
/// `file://` identity, ADOPT that identity — no git-origin read, no prune — so
/// incremental updates land on the existing graph. Adopting (rather than
/// minting a second origin uid) is also what avoids the duplicate-row problem
/// the re-identify logic was originally added to solve. Re-identification to
/// the git origin remote is correctly deferred to the next `nw index`, which
/// cold-indexes properly.
///
/// Only when NO existing repo row is found for this path (a genuinely fresh
/// watch) do we mint the git origin identity: prefer the origin remote when
/// configured, else the `file://` URL. Guard on `.git` at the watched root —
/// `git config` walks up to an enclosing repo, and watching a subdirectory
/// must not capture its parent repo's identity.
fn resolve_watch_identity(
    store: &GraphStore,
    instance_id: &str,
    repo_root: &Path,
) -> Result<(String, String), anyhow::Error> {
    let root_path = repo_root.display().to_string();
    let file_url = format!("file://{root_path}");
    let file_uid = nestweaver_schema::repo_uid(instance_id, &file_url);

    if store.lookup_repo(&file_uid)?.is_some() {
        // Existing legacy graph for this path: adopt it untouched.
        tracing::info!(
            uid = %file_uid,
            root_path = %root_path,
            "watched repo already indexed under its file:// identity; adopting it (re-identify deferred to next `nw index`)"
        );
        return Ok((file_url, file_uid));
    }

    // Fresh watch: mint the git origin identity when configured.
    let repo_url = if repo_root.join(".git").exists() {
        crate::bare_clone::read_origin_url(repo_root).unwrap_or_else(|_| file_url.clone())
    } else {
        file_url.clone()
    };
    let r_uid = nestweaver_schema::repo_uid(instance_id, &repo_url);
    Ok((repo_url, r_uid))
}

/// Returns true if the file is a supported source language.
fn is_supported_source(path: &Path) -> bool {
    detect_language(path).is_some()
}

/// Parse a single source file and insert its File node, Symbol nodes, and
/// edges. Returns `Ok(Some(n))` when the file was re-indexed (n symbols
/// inserted), or `Ok(None)` when the reindex was SKIPPED and the file's
/// previous graph data was kept. Mirrors the logic in
/// `index.rs::process_added_or_modified_file`.
///
/// The file is read and parsed BEFORE any graph mutation: on a read or
/// parse failure the file's previous symbols stay in the graph. The old
/// code deleted the file's symbols first, so a transient read error (or a
/// mid-save partial write) left the file's symbols deleted from the graph
/// while the file was still on disk.
fn reindex_file(
    repo_root: &Path,
    rel_path: &Path,
    r_uid: &str,
    store: &GraphStore,
) -> Result<Option<usize>, ReindexError> {
    use nestweaver_parser::{RawReference, RawSymbol, parse_source};
    use nestweaver_resolver::{discover_workspace_context, resolve_references_with_context};
    use nestweaver_schema::{File, Symbol, file_uid, symbol_uid};

    let abs_path = repo_root.join(rel_path);
    let rel_str = rel_path.to_string_lossy().into_owned();

    let source = std::fs::read_to_string(&abs_path).map_err(|e| {
        ReindexError::PreMutation(anyhow::anyhow!("read {}: {e}", abs_path.display()))
    })?;

    let parsed = match parse_source(&abs_path, &source) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(path = %abs_path.display(), "parse error: {e}; keeping previous index data");
            return Ok(None);
        }
    };

    // Only now that the fresh parse is in hand: drop the stale symbols.
    // From this point on the file's graph data is being MUTATED — a failure
    // here is not "kept previous data".
    let removed = store.delete_symbols_in_file(r_uid, &rel_str).map_err(|e| {
        ReindexError::PostMutation(anyhow::anyhow!("delete_symbols_in_file {rel_str}: {e}"))
    })?;
    if removed > 0 {
        tracing::debug!(
            path = %rel_str,
            removed,
            "removed stale symbols before re-index"
        );
    }

    let content_hash = crate::hash::blake3_hex(&source);
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
        .map_err(|e| ReindexError::PostMutation(anyhow::anyhow!("insert_file {rel_str}: {e}")))?;
    store.insert_repo_file_edge(r_uid, &f_uid).map_err(|e| {
        ReindexError::PostMutation(anyhow::anyhow!("insert_repo_file_edge {rel_str}: {e}"))
    })?;

    let mut symbols: Vec<Symbol> = Vec::new();
    let mut file_sym_pairs: Vec<(String, String)> = Vec::new();

    // F2.0: populate framework_hint on re-indexed symbols too, so hints
    // survive incremental watcher updates (matching the full-index path).
    let mut hint_by_index: std::collections::HashMap<usize, nestweaver_schema::FrameworkHint> =
        std::collections::HashMap::new();
    if let Some(lang) = nestweaver_parser::detect_language(&abs_path)
        && let Some(lang_str) = crate::contracts::framework_language_str(lang)
    {
        for (sym_idx, hint) in
            nestweaver_parser::detect_frameworks(&parsed.symbols, &rel_str, lang_str)
        {
            hint_by_index.insert(sym_idx, hint);
        }
        // NestJS `@Controller` lives above the class and is not in the parsed
        // signature; recover it from source (mirrors the full-index path).
        if matches!(
            lang,
            nestweaver_schema::Language::TypeScript | nestweaver_schema::Language::JavaScript
        ) {
            let class_starts: Vec<(usize, u32)> = parsed
                .symbols
                .iter()
                .enumerate()
                .filter(|(_, s)| s.kind == nestweaver_schema::SymbolKind::Class)
                .map(|(i, s)| (i, s.start_line))
                .collect();
            if let Some(ctrl_idx) =
                crate::contracts::detect_nestjs_controller_index(&source, &class_starts)
            {
                hint_by_index
                    .entry(ctrl_idx)
                    .or_insert_with(|| nestweaver_schema::FrameworkHint {
                        framework: "nestjs".into(),
                        role: "controller".into(),
                    });
            }
        }
    }

    for (sym_idx, raw_sym) in parsed.symbols.iter().enumerate() {
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
            framework_hint: hint_by_index.remove(&sym_idx),
            canonical_id: None,
        };
        symbols.push(sym);
        file_sym_pairs.push((f_uid.clone(), s_uid));
    }

    let sym_count = symbols.len();

    store.batch_insert_symbols(&symbols).map_err(|e| {
        ReindexError::PostMutation(anyhow::anyhow!("batch_insert_symbols {rel_str}: {e}"))
    })?;

    let file_sym_refs: Vec<(&str, &str)> = file_sym_pairs
        .iter()
        .map(|(f, s)| (f.as_str(), s.as_str()))
        .collect();
    store
        .batch_insert_file_symbol_edges(&file_sym_refs)
        .map_err(|e| {
            ReindexError::PostMutation(anyhow::anyhow!(
                "batch_insert_file_symbol_edges {rel_str}: {e}"
            ))
        })?;

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
    let resolved_edges =
        resolve_references_with_context(&file_data, lang, r_uid, &workspace_ctx, None, None);
    let insertable_edges: Vec<_> = resolved_edges
        .into_iter()
        .filter(|e| !e.target_uid.starts_with("unresolved:"))
        .collect();
    if !insertable_edges.is_empty() {
        store.batch_insert_edges(&insertable_edges).map_err(|e| {
            ReindexError::PostMutation(anyhow::anyhow!("batch_insert_edges {rel_str}: {e}"))
        })?;
    }

    Ok(Some(sym_count))
}

/// Why a watcher re-index of one file failed, split by whether the file's
/// graph data had already been mutated. Callers must NOT treat a
/// `PostMutation` failure as "previous data preserved": the
/// delete already landed, so the graph holds partial data for the file and
/// the batch must not publish as cleanly successful.
enum ReindexError {
    /// Failed before any mutation (read/parse/delete) — old graph data intact.
    PreMutation(anyhow::Error),
    /// Failed after the file's symbols were deleted — partial data.
    PostMutation(anyhow::Error),
}

impl std::fmt::Display for ReindexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PreMutation(e) | Self::PostMutation(e) => write!(f, "{e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailingGenerationPublicationIo;

    impl crate::index::IndexEpilogueIo for FailingGenerationPublicationIo {
        fn establish_marker(&self, path: &Path) -> Result<(), anyhow::Error> {
            crate::index::FileSystemIndexEpilogueIo.establish_marker(path)
        }

        fn clear_marker(&self, path: &Path) -> Result<(), anyhow::Error> {
            crate::index::FileSystemIndexEpilogueIo.clear_marker(path)
        }

        fn remove_file(&self, path: &Path) -> std::io::Result<()> {
            crate::index::FileSystemIndexEpilogueIo.remove_file(path)
        }

        fn rename_file(&self, from: &Path, to: &Path) -> std::io::Result<()> {
            crate::index::FileSystemIndexEpilogueIo.rename_file(from, to)
        }

        fn save_generation(
            &self,
            _store: &GraphStore,
            _path: &Path,
            _generation: u64,
        ) -> Result<(), anyhow::Error> {
            anyhow::bail!("injected watcher generation save failure")
        }

        fn compute_pagerank(
            &self,
            store: &GraphStore,
            scope: &GraphScope,
        ) -> Result<(), anyhow::Error> {
            crate::index::FileSystemIndexEpilogueIo.compute_pagerank(store, scope)
        }

        fn save_pagerank(&self, store: &GraphStore, path: &Path) -> Result<(), anyhow::Error> {
            crate::index::FileSystemIndexEpilogueIo.save_pagerank(store, path)
        }
    }

    #[test]
    fn watcher_generation_failure_keeps_reopen_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir(&repo_root).unwrap();
        let db_path = dir.path().join("test.lbug");
        let generation_path = crate::sidecar_path(&db_path, ".generation");
        let pagerank_path = crate::sidecar_path(&db_path, ".pagerank.json");
        let marker_path = crate::sidecar_path(&db_path, ".index-dirty");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        store.bump_graph_generation();
        store.save_graph_generation(&generation_path).unwrap();
        let stale_generation = store.graph_generation();
        std::fs::write(&pagerank_path, r#"{"stale":1.0}"#).unwrap();
        store.load_pagerank_cache(&pagerank_path).unwrap();
        let watcher = CodeWatcher::new(&db_path, &repo_root, "test");

        let publication = watcher
            .establish_graph_publication_with_io(&store, &crate::index::FileSystemIndexEpilogueIo)
            .unwrap();
        store
            .insert_repo(&nestweaver_schema::Repo {
                uid: "repo:watcher-failure".into(),
                url: "file:///watcher-failure".into(),
                indexed_sha: "watch".into(),
                staleness_commits_behind: 0,
                instance_id: "test".into(),
                name: None,
                root_path: None,
            })
            .unwrap();
        let error = watcher
            .finalize_graph_publication_with_io(publication, &FailingGenerationPublicationIo)
            .unwrap_err();

        assert!(error.to_string().contains("generation-persistence"));
        assert!(marker_path.exists());
        assert!(!pagerank_path.exists());
        drop(store);

        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        assert_ne!(reopened.graph_generation(), stale_generation);
        reopened.load_pagerank_cache(&pagerank_path).unwrap();
        assert!(!reopened.pagerank_scores().contains_key("stale"));
    }

    #[test]
    fn skip_dir_detection() {
        let p = Path::new("/repo/node_modules/foo/bar.js");
        assert!(path_in_skip_dir(p));
        let p = Path::new("/repo/.git/HEAD");
        assert!(path_in_skip_dir(p));
        let p = Path::new("/repo/src/main.rs");
        assert!(!path_in_skip_dir(p));
    }

    /// Index a small JS fixture repo (a.js ← b.js ← c.js) into an in-memory
    /// store under its `file://` identity, returning the store, repo uid,
    /// and canonical repo root.
    fn index_fixture_repo(dir: &tempfile::TempDir) -> (GraphStore, String, PathBuf) {
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(repo_root.join("src")).unwrap();
        std::fs::write(
            repo_root.join("src/a.js"),
            "export function helper() { return 7; }\n",
        )
        .unwrap();
        std::fs::write(
            repo_root.join("src/b.js"),
            "import { helper } from './a.js';\nexport function alpha() { return helper() + 1; }\n",
        )
        .unwrap();
        std::fs::write(
            repo_root.join("src/c.js"),
            "import { alpha } from './b.js';\nexport function gamma() { return alpha() * 2; }\n",
        )
        .unwrap();

        let canonical_root = std::fs::canonicalize(&repo_root).unwrap();
        let file_url = format!("file://{}", canonical_root.display());
        let r_uid = nestweaver_schema::repo_uid("test", &file_url);
        let (_result, store) =
            crate::index::index_directory_in_memory(&repo_root, "test", &file_url, "sha1").unwrap();
        (store, r_uid, canonical_root)
    }

    fn uid_of(store: &GraphStore, r_uid: &str, name: &str) -> String {
        store
            .lookup_symbols_by_repo(r_uid)
            .unwrap()
            .into_iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("symbol {name} should be indexed"))
            .uid
    }

    /// Regression (CRITICAL — edge loss across watch reindex): a watcher
    /// reindex of a modified file must restore the SAME cross-file edges a
    /// manual incremental index produces — both the file's outgoing edges
    /// and the incoming edges from its dependents. Before this fix the
    /// watcher ran single-file resolution only, so every cross-file
    /// CALLS/IMPORTS edge incident to the re-indexed file was destroyed by
    /// the per-file `DETACH DELETE` and never rebuilt (symbols present,
    /// all edges gone; only `--force` repaired).
    #[test]
    fn watch_reindex_preserves_cross_file_edges() {
        let dir = tempfile::tempdir().unwrap();
        let (store, r_uid, canonical_root) = index_fixture_repo(&dir);

        // Sanity: the full index produced both cross-file CALLS edges.
        let alpha_uid = uid_of(&store, &r_uid, "alpha");
        assert!(
            store
                .callees_of(&alpha_uid)
                .unwrap()
                .iter()
                .any(|s| s.name == "helper"),
            "fixture must start with alpha→helper"
        );
        assert!(
            store
                .callers_of(&alpha_uid)
                .unwrap()
                .iter()
                .any(|s| s.name == "gamma"),
            "fixture must start with gamma→alpha"
        );

        // Modify b.js on disk and run one watcher batch over it.
        std::fs::write(
            canonical_root.join("src/b.js"),
            "import { helper } from './a.js';\nexport function alpha() { return helper() + 2; }\n",
        )
        .unwrap();
        let watcher = CodeWatcher::new(dir.path().join("brain.lbug"), &canonical_root, "test");
        let (processed, batch_error) =
            watcher.reindex_paths(&store, &r_uid, &[canonical_root.join("src/b.js")]);
        assert!(batch_error.is_none(), "{batch_error:?}");
        assert_eq!(processed, 1);

        // THE critical assertions: outgoing (alpha→helper) and incoming
        // (gamma→alpha) cross-file edges survive a watcher reindex —
        // exactly once each (edge insert is CREATE, not MERGE).
        let alpha_uid = uid_of(&store, &r_uid, "alpha");
        let callees = store.callees_of(&alpha_uid).unwrap();
        assert_eq!(
            callees.iter().filter(|s| s.name == "helper").count(),
            1,
            "outgoing cross-file edge alpha→helper must be restored exactly once, got {callees:?}"
        );
        let callers = store.callers_of(&alpha_uid).unwrap();
        assert_eq!(
            callers.iter().filter(|s| s.name == "gamma").count(),
            1,
            "incoming cross-file edge gamma→alpha must be restored exactly once, got {callers:?}"
        );
    }

    /// Regression (symbol deletion on failed reindex): when a watched file
    /// becomes unreadable, its previous symbols must STAY in the graph and
    /// the watch batch must survive. The old code deleted the file's
    /// symbols BEFORE reading it, so a transient read error left the
    /// file's symbols deleted while the file was still on disk — and the
    /// batch failure then killed the whole watcher, so subsequent edits
    /// were never indexed.
    #[test]
    fn watch_reindex_keeps_previous_symbols_when_file_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let (store, r_uid, canonical_root) = index_fixture_repo(&dir);

        // Corrupt b.js with invalid UTF-8 so `read_to_string` fails.
        std::fs::write(
            canonical_root.join("src/b.js"),
            b"export function alpha() { return \xff\xfe; }\n",
        )
        .unwrap();
        let watcher = CodeWatcher::new(dir.path().join("brain.lbug"), &canonical_root, "test");
        let (processed, batch_error) =
            watcher.reindex_paths(&store, &r_uid, &[canonical_root.join("src/b.js")]);
        assert!(batch_error.is_none(), "{batch_error:?}");

        assert_eq!(
            processed, 0,
            "an unreadable file is skipped, not half-processed"
        );
        assert!(
            store
                .lookup_symbols_by_repo(&r_uid)
                .unwrap()
                .iter()
                .any(|s| s.name == "alpha"),
            "alpha must stay indexed when its file cannot be re-read"
        );
        // The incoming edge from the untouched dependent survives too.
        let alpha_uid = uid_of(&store, &r_uid, "alpha");
        assert!(
            store
                .callers_of(&alpha_uid)
                .unwrap()
                .iter()
                .any(|s| s.name == "gamma"),
            "gamma→alpha edge must survive a failed reindex of b.js"
        );

        // And the watcher keeps working: a later valid save re-indexes.
        std::fs::write(
            canonical_root.join("src/b.js"),
            "import { helper } from './a.js';\nexport function alpha() { return helper() + 3; }\n",
        )
        .unwrap();
        let (processed, batch_error) =
            watcher.reindex_paths(&store, &r_uid, &[canonical_root.join("src/b.js")]);
        assert!(batch_error.is_none(), "{batch_error:?}");
        assert_eq!(processed, 1, "the batch after a failure must re-index");
        let alpha_uid = uid_of(&store, &r_uid, "alpha");
        assert!(
            store
                .callees_of(&alpha_uid)
                .unwrap()
                .iter()
                .any(|s| s.name == "helper"),
            "edges are rebuilt once the file is readable again"
        );
    }

    /// Deleting a watched file removes its symbols (and, via DETACH
    /// DELETE, the edges incident to them) — the remove path of
    /// `reindex_paths`.
    #[test]
    fn watch_reindex_removes_symbols_for_deleted_file() {
        let dir = tempfile::tempdir().unwrap();
        let (store, r_uid, canonical_root) = index_fixture_repo(&dir);

        std::fs::remove_file(canonical_root.join("src/b.js")).unwrap();
        let watcher = CodeWatcher::new(dir.path().join("brain.lbug"), &canonical_root, "test");
        let (processed, batch_error) =
            watcher.reindex_paths(&store, &r_uid, &[canonical_root.join("src/b.js")]);
        assert!(batch_error.is_none(), "{batch_error:?}");

        assert_eq!(processed, 1);
        assert!(
            !store
                .lookup_symbols_by_repo(&r_uid)
                .unwrap()
                .iter()
                .any(|s| s.name == "alpha"),
            "alpha must be removed when b.js is deleted"
        );
        // The untouched files are unaffected.
        assert!(
            store
                .lookup_symbols_by_repo(&r_uid)
                .unwrap()
                .iter()
                .any(|s| s.name == "helper" || s.name == "gamma"),
        );
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

    /// Regression (data loss on watch-first over a legacy DB): a repo whose
    /// full graph already lives under its `file://` identity must be ADOPTED by
    /// the watcher — not re-identified to an origin remote and pruned. Before
    /// this fix the watcher called `delete_repo_all_data(old_file_uid)` on
    /// startup and only inserted a minimal empty Repo node, silently emptying
    /// the graph until individual files happened to change.
    #[test]
    fn watcher_adopts_existing_file_identity_and_graph_survives() {
        use nestweaver_schema::{File, Symbol, SymbolKind, Visibility, repo_uid};

        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();

        // Give the working tree a git origin remote. Pre-fix this is exactly
        // what tripped the re-identify+prune path; post-fix the existing
        // file:// row must short-circuit BEFORE the origin is ever read.
        let _ = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo_root)
            .status();
        let _ = std::process::Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                "https://example.com/acme/demo.git",
            ])
            .current_dir(&repo_root)
            .status();

        let instance = "test";
        let root_path = repo_root.display().to_string();
        let file_url = format!("file://{root_path}");
        let file_uid = repo_uid(instance, &file_url);
        let origin_uid = repo_uid(instance, "https://example.com/acme/demo.git");
        assert_ne!(file_uid, origin_uid);

        // Seed a full graph under the LEGACY file:// identity.
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_repo(&nestweaver_schema::Repo {
                uid: file_uid.clone(),
                url: file_url.clone(),
                indexed_sha: "sha-legacy".to_string(),
                staleness_commits_behind: 0,
                instance_id: instance.to_string(),
                name: None,
                root_path: Some(root_path.clone()),
            })
            .unwrap();
        let f_uid = nestweaver_schema::file_uid(&file_uid, "src/lib.rs");
        store
            .insert_file(&File {
                uid: f_uid.clone(),
                path: "src/lib.rs".to_string(),
                repo_uid: file_uid.clone(),
                content_hash: "hash".to_string(),
            })
            .unwrap();
        store.insert_repo_file_edge(&file_uid, &f_uid).unwrap();
        let s_uid = nestweaver_schema::symbol_uid(&file_uid, "src/lib.rs", "legacy_fn", 1);
        store
            .insert_symbol(&Symbol {
                uid: s_uid.clone(),
                name: "legacy_fn".to_string(),
                kind: SymbolKind::Function,
                repo_uid: file_uid.clone(),
                file_path: "src/lib.rs".to_string(),
                start_line: 1,
                end_line: 3,
                signature: "fn legacy_fn()".to_string(),
                summary: None,
                content_hash: "h".to_string(),
                embedding: None,
                pagerank_score: None,
                is_entry_point: false,
                entry_point_kind: None,
                visibility: Visibility::Inferred,
                type_info: None,
                framework_hint: None,
                canonical_id: None,
            })
            .unwrap();
        store.insert_file_symbol_edge(&f_uid, &s_uid).unwrap();

        // Sanity: the symbol exists under the file:// uid before the watcher runs.
        let before = store.symbol_names_by_repo(&file_uid).unwrap();
        assert!(before.iter().any(|n| n == "legacy_fn"));

        // The watcher's identity decision must ADOPT the file:// identity.
        let (url, uid) = resolve_watch_identity(&store, instance, &repo_root).unwrap();
        assert_eq!(uid, file_uid, "watcher must adopt the existing file:// uid");
        assert_eq!(url, file_url);
        assert_ne!(
            uid, origin_uid,
            "watcher must NOT re-identify to the origin remote when a legacy graph exists"
        );

        // The existing graph SURVIVES — no prune happened.
        let after = store.symbol_names_by_repo(&file_uid).unwrap();
        assert!(
            after.iter().any(|n| n == "legacy_fn"),
            "the legacy graph must survive adoption, got {after:?}"
        );
        assert!(
            store.lookup_repo(&origin_uid).unwrap().is_none(),
            "no second (origin) Repo row must be minted"
        );
    }

    /// A genuinely fresh watch (no prior graph) with a `.git` origin mints the
    /// origin identity, as before — adoption only kicks in for existing rows.
    #[test]
    fn watcher_mints_origin_identity_on_fresh_watch() {
        use nestweaver_schema::repo_uid;

        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let _ = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo_root)
            .status();
        let added = std::process::Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                "https://example.com/acme/fresh.git",
            ])
            .current_dir(&repo_root)
            .status();

        let store = GraphStore::in_memory().unwrap();
        let (url, uid) = resolve_watch_identity(&store, "test", &repo_root).unwrap();

        // Only assert the origin path when git actually configured the remote
        // (keeps the test hermetic if git is unavailable in the environment).
        if matches!(added, Ok(s) if s.success()) {
            assert_eq!(url, "https://example.com/acme/fresh.git");
            assert_eq!(uid, repo_uid("test", "https://example.com/acme/fresh.git"));
        }
    }
}
