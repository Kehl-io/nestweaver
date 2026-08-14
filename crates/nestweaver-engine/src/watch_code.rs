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

use crate::content_reader::ContentReader;
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

#[derive(Debug)]
enum WatchBatchOutcome {
    Published { files_processed: usize },
    Skipped { reason: anyhow::Error },
}

enum PreparedPath {
    Replace(Box<PreparedCodeFile>),
    Delete { rel_path: String },
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
            true,
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
        // nw-C1: this watcher is a writer, so it reconciles an abandoned
        // publication left by a crashed indexer instead of inheriting the wedge.
        let store = Arc::new(crate::index::open_store_for_writing_with_recovery(
            &self.db_path,
        )?);
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
        // Register before inspecting or cold-indexing the tree. Events that
        // race the initial snapshot are queued by the debouncer and replayed
        // below, closing the former scan-then-watch lost-event window.
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

        // Identity decision (see `resolve_watch_identity`): adopt an existing
        // `file://` graph rather than re-identifying+pruning, so a watch-first
        // start over a legacy DB never empties the graph.
        let (repo_url, r_uid) = resolve_watch_identity(&store, &self.instance_id, &self.repo_root)?;

        // A contract plan is a whole-repo view and must never point at
        // unchanged controllers that a minimal watch-first graph omitted.
        // Cold watchers therefore publish an authoritative initial source +
        // contract snapshot through the exact same atomic batch seam.
        if store.lookup_repo(&r_uid)?.is_none() {
            loop {
                let reader = crate::content_reader::FilesystemReader::new(&self.repo_root);
                let initial_paths: Vec<PathBuf> = reader
                    .list_files()
                    .context("list files for initial watcher snapshot")?
                    .into_iter()
                    .map(|path| self.repo_root.join(path))
                    .filter(|path| is_watcher_input(path))
                    .filter(|path| !path_in_skip_dir(path))
                    .filter(|path| !is_minified_or_bundled(path))
                    .collect();
                match self.process_batch_and_notify(
                    &store,
                    &r_uid,
                    &repo_url,
                    &initial_paths,
                    &crate::index::FileSystemIndexEpilogueIo,
                    on_change.as_deref().map(|callback| callback as &dyn Fn()),
                )? {
                    WatchBatchOutcome::Published { .. } => break,
                    WatchBatchOutcome::Skipped { reason } => {
                        // A save racing the cold snapshot is expected to be
                        // queued because notification was registered first.
                        // Wait through one debounce window, discard those
                        // paths, and rebuild the authoritative whole-repo
                        // snapshot. If no event arrives, this is a stable bad
                        // input rather than a race and startup fails clearly.
                        match rx.recv_timeout(Duration::from_millis(2250)) {
                            Ok(Ok(_)) => {
                                let _ = drain_queued_events(&rx);
                                continue;
                            }
                            Ok(Err(error)) => {
                                return Err(anyhow::Error::new(error).context(
                                    "notification failed while retrying initial watcher snapshot",
                                ));
                            }
                            Err(_) => {
                                return Err(reason.context(
                                    "cannot start code watcher without an authoritative initial graph",
                                ));
                            }
                        }
                    }
                }
            }
        }

        tracing::info!(
            repo = %self.repo_root.display(),
            db = %self.db_path.display(),
            "CodeWatcher running"
        );

        let mut replay_batch = drain_queued_events(&rx);

        loop {
            if self.stop_flag.load(Ordering::Relaxed) {
                tracing::info!("CodeWatcher stop requested; exiting");
                return Ok(());
            }

            let batch = if !replay_batch.is_empty() {
                std::mem::take(&mut replay_batch)
            } else {
                match rx.recv_timeout(Duration::from_millis(250)) {
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
                }
            };

            // Deduplicate paths within the batch — the debouncer may
            // fire multiple events for the same file.
            let mut unique_paths: HashSet<PathBuf> = HashSet::new();
            for event in &batch {
                unique_paths.insert(event.path.clone());
            }

            // Contract specs are first-class watcher inputs even though they
            // are not parser-supported source files. A spec-only edit must
            // refresh the derived Contract nodes and IMPLEMENTS_CONTRACT
            // edges just like an ordinary incremental index.
            let relevant: Vec<PathBuf> = unique_paths
                .into_iter()
                .filter(|p| !path_in_skip_dir(p))
                .filter(|p| is_watcher_input(p))
                .filter(|p| !is_minified_or_bundled(p))
                .collect();

            if relevant.is_empty() {
                continue;
            }

            let start = Instant::now();
            let outcome = self.process_batch_and_notify(
                &store,
                &r_uid,
                &repo_url,
                &relevant,
                &crate::index::FileSystemIndexEpilogueIo,
                on_change.as_deref().map(|callback| callback as &dyn Fn()),
            )?;
            let files_processed = match outcome {
                WatchBatchOutcome::Published { files_processed } => files_processed,
                WatchBatchOutcome::Skipped { reason } => {
                    tracing::warn!(
                        error = %reason,
                        "skipping transient watcher batch before publication; previous graph preserved"
                    );
                    continue;
                }
            };
            let duration = start.elapsed();
            tracing::info!(
                files_processed,
                elapsed_secs = format!("{:.1}", duration.as_secs_f64()),
                "Re-indexed {} file(s) ({:.1}s)",
                files_processed,
                duration.as_secs_f64()
            );

            // Notification is coupled to the published outcome by
            // `process_batch_and_notify`; skipped/dirty batches cannot emit a
            // false-positive SSE change event.
        }
    }

    /// Prepare and atomically publish one watcher batch.
    ///
    /// All whole-repo contract reads and parsing finish before publication is
    /// established. Source replacement, reverse-dependent resolution, and
    /// contract replacement then share one transaction. Any failure before
    /// commit preserves the previously committed graph; because finalization
    /// is deliberately skipped, `.index-dirty` remains as the durable
    /// fail-closed signal and callers cannot report success.
    fn process_batch_with_io(
        &self,
        store: &GraphStore,
        r_uid: &str,
        repo_url: &str,
        relevant: &[PathBuf],
        epilogue_io: &dyn crate::index::IndexEpilogueIo,
    ) -> Result<WatchBatchOutcome, anyhow::Error> {
        self.process_batch_with_io_and_hook(store, r_uid, repo_url, relevant, epilogue_io, || {})
    }

    fn process_batch_with_io_and_hook<F>(
        &self,
        store: &GraphStore,
        r_uid: &str,
        repo_url: &str,
        relevant: &[PathBuf],
        epilogue_io: &dyn crate::index::IndexEpilogueIo,
        after_plan: F,
    ) -> Result<WatchBatchOutcome, anyhow::Error>
    where
        F: FnOnce(),
    {
        let insert_initial_repo = store.lookup_repo(r_uid)?.is_none();
        let reader = crate::content_reader::FilesystemReader::new(&self.repo_root);
        let contract_plan =
            match crate::index::prepare_watcher_contract_derivation(&reader, r_uid, repo_url) {
                Ok(plan) => plan,
                Err(reason) => return Ok(WatchBatchOutcome::Skipped { reason }),
            };
        after_plan();
        let mut prepared_paths = Vec::new();
        let mut changed: HashSet<String> = HashSet::new();
        let mut removed: HashSet<String> = HashSet::new();
        for path in relevant.iter().filter(|path| is_supported_source(path)) {
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
            match std::fs::metadata(path) {
                Ok(metadata) if metadata.is_file() => {
                    let prepared =
                        match prepare_code_file(&self.repo_root, rel_path, r_uid, repo_url) {
                            Ok(prepared) => prepared,
                            Err(reason) => return Ok(WatchBatchOutcome::Skipped { reason }),
                        };
                    if contract_plan
                        .input_hashes
                        .get(&rel_str)
                        .is_some_and(|expected| expected != &prepared.file.content_hash)
                    {
                        return Ok(WatchBatchOutcome::Skipped {
                            reason: anyhow::anyhow!(
                                "watched controller changed while contract batch was being prepared: {rel_str}"
                            ),
                        });
                    }
                    changed.insert(rel_str);
                    prepared_paths.push(PreparedPath::Replace(Box::new(prepared)));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    removed.insert(rel_str.clone());
                    prepared_paths.push(PreparedPath::Delete { rel_path: rel_str });
                }
                Ok(_) => continue,
                Err(error) => {
                    return Ok(WatchBatchOutcome::Skipped {
                        reason: anyhow::Error::new(error)
                            .context(format!("inspect watched path {}", path.display())),
                    });
                }
            }
        }

        let final_contract_snapshot = match crate::index::watcher_contract_input_snapshot(&reader) {
            Ok(snapshot) => snapshot,
            Err(reason) => return Ok(WatchBatchOutcome::Skipped { reason }),
        };
        if final_contract_snapshot != contract_plan.observed_input_hashes {
            return Ok(WatchBatchOutcome::Skipped {
                reason: anyhow::anyhow!(
                    "contract inputs changed after watcher plan while changed paths were frozen"
                ),
            });
        }

        // nw-008 Phase 0 — transitive reverse-dependents from the LIVE graph,
        // BEFORE any mutation (the per-file `DETACH DELETE` destroys the
        // edges this query walks).
        let rdeps = crate::index::collect_reverse_dep_files(store, r_uid, &changed, &removed);
        let replacement_symbols: Vec<_> = prepared_paths
            .iter()
            .filter_map(|path| match path {
                PreparedPath::Replace(file) => Some(file.symbols.iter()),
                PreparedPath::Delete { .. } => None,
            })
            .flatten()
            .cloned()
            .collect();
        let prepared_file_data: std::collections::HashMap<_, _> = prepared_paths
            .iter()
            .filter_map(|path| match path {
                PreparedPath::Replace(file) => Some((
                    file.rel_path.clone(),
                    (file.raw_symbols.clone(), file.raw_references.clone()),
                )),
                PreparedPath::Delete { .. } => None,
            })
            .collect();
        let reresolved_edges = match crate::index::prepare_watcher_reresolve_edges(
            &reader,
            store,
            r_uid,
            crate::index::WatcherReresolveInputs {
                changed: &changed,
                removed: &removed,
                rdeps: &rdeps,
                replacement_symbols: &replacement_symbols,
                prepared_file_data: &prepared_file_data,
            },
        ) {
            Ok(edges) => edges,
            Err(reason) => return Ok(WatchBatchOutcome::Skipped { reason }),
        };

        // No graph mutation, including a failure marker, occurs before the
        // complete plan above exists. Once publication starts, every failure
        // intentionally leaves the dirty marker for reopen reconciliation.
        let publication = self
            .establish_graph_publication_with_io(store, epilogue_io)
            .map_err(anyhow::Error::from)?;
        reject_recovered_publication(&publication)?;
        let txn = store
            .begin_transaction()
            .context("begin code watcher batch transaction")?;
        if insert_initial_repo {
            nestweaver_store::GraphStore::insert_repo_on(
                &txn,
                &nestweaver_schema::Repo {
                    uid: r_uid.to_string(),
                    url: repo_url.to_string(),
                    indexed_sha: "watch".to_string(),
                    staleness_commits_behind: 0,
                    instance_id: self.instance_id.clone(),
                    name: None,
                    root_path: Some(self.repo_root.display().to_string()),
                },
            )
            .context("insert initial watcher Repo node")?;
        }

        let mut files_processed = 0usize;
        for prepared_path in &prepared_paths {
            match prepared_path {
                PreparedPath::Replace(prepared) => {
                    let symbols = apply_prepared_code_file(&txn, r_uid, prepared)
                        .with_context(|| format!("apply watched file {}", prepared.rel_path))?;
                    tracing::debug!(path = %prepared.rel_path, symbols, "re-indexed file");
                    files_processed += 1;
                }
                PreparedPath::Delete { rel_path } => {
                    let removed_count = nestweaver_store::GraphStore::delete_symbols_in_file_on(
                        &txn, r_uid, rel_path,
                    )
                    .with_context(|| format!("delete symbols for removed file {rel_path}"))?;
                    let f_uid = nestweaver_schema::file_uid(r_uid, rel_path);
                    nestweaver_store::GraphStore::delete_file_node_on(&txn, &f_uid)
                        .with_context(|| format!("delete removed File node {rel_path}"))?;
                    if removed_count > 0 {
                        tracing::debug!(
                            path = %rel_path,
                            removed = removed_count,
                            "deleted symbols for removed file"
                        );
                    }
                    files_processed += 1;
                }
            }
        }

        let reresolved = reresolved_edges.len();
        if !reresolved_edges.is_empty() {
            nestweaver_store::GraphStore::batch_insert_edges_on(&txn, &reresolved_edges)
                .context("insert prepared watcher reverse-dependent edges")?;
        }
        if reresolved > 0 {
            tracing::info!(
                edges = reresolved,
                rdeps = rdeps.len(),
                "restored cross-file edges via transitive re-resolution"
            );
        }

        if let Err(error) = crate::index::apply_contract_derivation_on(&txn, r_uid, &contract_plan)
        {
            drop(txn);
            if let Err(marker_error) =
                store.set_contract_derivation_failed(r_uid, &error.to_string())
            {
                tracing::warn!(
                    "recording watcher contract derivation failure failed: {marker_error}"
                );
            }
            return Err(error).context("apply watcher contract derivation");
        }

        store
            .commit_transaction(&txn)
            .context("commit code watcher batch transaction")?;
        drop(txn);
        if let Err(error) = store.clear_contract_derivation_failed(r_uid) {
            tracing::warn!("clearing watcher contract derivation marker failed: {error}");
        }
        self.finalize_graph_publication_with_io(publication, epilogue_io)
            .map_err(anyhow::Error::from)?;
        Ok(WatchBatchOutcome::Published { files_processed })
    }

    fn process_batch_and_notify(
        &self,
        store: &GraphStore,
        r_uid: &str,
        repo_url: &str,
        relevant: &[PathBuf],
        epilogue_io: &dyn crate::index::IndexEpilogueIo,
        on_change: Option<&dyn Fn()>,
    ) -> Result<WatchBatchOutcome, anyhow::Error> {
        let outcome = self.process_batch_with_io(store, r_uid, repo_url, relevant, epilogue_io)?;
        if matches!(outcome, WatchBatchOutcome::Published { .. })
            && let Some(callback) = on_change
        {
            callback();
        }
        Ok(outcome)
    }
}

/// The debouncer callback type.
type DebounceResult = Result<Vec<DebouncedEvent>, notify::Error>;

fn drain_queued_events(rx: &std::sync::mpsc::Receiver<DebounceResult>) -> Vec<DebouncedEvent> {
    let mut events = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(Ok(mut queued)) => events.append(&mut queued),
            Ok(Err(error)) => tracing::warn!("notify error queued during watcher startup: {error}"),
            Err(std::sync::mpsc::TryRecvError::Empty) => return events,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return events,
        }
    }
}

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

fn is_watcher_input(path: &Path) -> bool {
    is_supported_source(path) || crate::contracts::is_spec_file(&path.to_string_lossy())
}

struct PreparedCodeFile {
    rel_path: String,
    file: nestweaver_schema::File,
    symbols: Vec<nestweaver_schema::Symbol>,
    file_symbol_edges: Vec<(String, String)>,
    resolved_edges: Vec<nestweaver_schema::ResolvedEdge>,
    raw_symbols: Vec<nestweaver_parser::RawSymbol>,
    raw_references: Vec<nestweaver_parser::RawReference>,
}

/// Read, parse, resolve, and annotate a watched source before publication.
/// The returned object owns every row needed by the transaction, eliminating
/// the read/delete/re-read race that could otherwise erase a transiently
/// malformed half-save.
fn prepare_code_file(
    repo_root: &Path,
    rel_path: &Path,
    r_uid: &str,
    repo_url: &str,
) -> Result<PreparedCodeFile, anyhow::Error> {
    use nestweaver_parser::{RawReference, RawSymbol, parse_source};
    use nestweaver_resolver::{discover_workspace_context, resolve_references_with_context};
    use nestweaver_schema::{File, Symbol, canonical_symbol_id, file_uid, symbol_uid};

    let abs_path = repo_root.join(rel_path);
    let rel_str = rel_path.to_string_lossy().into_owned();

    let source = std::fs::read_to_string(&abs_path)
        .with_context(|| format!("read watched source {}", abs_path.display()))?;
    let parsed = parse_source(&abs_path, &source)
        .with_context(|| format!("parse watched source {}", abs_path.display()))?;

    let content_hash = crate::hash::blake3_hex(&source);
    let f_uid = file_uid(r_uid, &rel_str);

    let file = File {
        uid: f_uid.clone(),
        path: rel_str.clone(),
        repo_uid: r_uid.to_string(),
        content_hash,
    };
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
        let scope = raw_sym.scope_chain.as_deref().unwrap_or("");
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
            canonical_id: Some(canonical_symbol_id(
                repo_url,
                &rel_str,
                &raw_sym.name,
                scope,
            )),
        };
        symbols.push(sym);
        file_sym_pairs.push((f_uid.clone(), s_uid));
    }

    // Resolve cross-file edges within this file while the source snapshot is
    // still the exact snapshot represented by `symbols`.
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
    let resolved_edges: Vec<_> = resolved_edges
        .into_iter()
        .filter(|e| !e.target_uid.starts_with("unresolved:"))
        .collect();

    Ok(PreparedCodeFile {
        rel_path: rel_str,
        file,
        symbols,
        file_symbol_edges: file_sym_pairs,
        resolved_edges,
        raw_symbols: parsed.symbols,
        raw_references: parsed.references,
    })
}

fn apply_prepared_code_file(
    txn: &nestweaver_store::DbConnection<'_>,
    r_uid: &str,
    prepared: &PreparedCodeFile,
) -> Result<usize, anyhow::Error> {
    nestweaver_store::GraphStore::delete_symbols_in_file_on(txn, r_uid, &prepared.rel_path)?;
    let old_file_uid = nestweaver_schema::file_uid(r_uid, &prepared.rel_path);
    nestweaver_store::GraphStore::delete_file_node_on(txn, &old_file_uid)?;
    nestweaver_store::GraphStore::insert_file_on(txn, &prepared.file)?;
    nestweaver_store::GraphStore::insert_repo_file_edge_on(txn, r_uid, &prepared.file.uid)?;
    nestweaver_store::GraphStore::batch_insert_symbols_on(txn, &prepared.symbols)?;
    let file_symbol_edges: Vec<(&str, &str)> = prepared
        .file_symbol_edges
        .iter()
        .map(|(file, symbol)| (file.as_str(), symbol.as_str()))
        .collect();
    nestweaver_store::GraphStore::batch_insert_file_symbol_edges_on(txn, &file_symbol_edges)?;
    if !prepared.resolved_edges.is_empty() {
        nestweaver_store::GraphStore::batch_insert_edges_on(txn, &prepared.resolved_edges)?;
    }
    Ok(prepared.symbols.len())
}

fn reject_recovered_publication(
    publication: &nestweaver_store::IndexPublicationLease<'_>,
) -> Result<(), anyhow::Error> {
    if publication.is_recovered() {
        anyhow::bail!(
            "code watcher found an incomplete prior index publication; refusing to retire unknown dirty state (repair with `nestweaver index --force`)"
        );
    }
    Ok(())
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

    #[test]
    fn startup_event_drain_replays_every_queued_batch() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Ok(vec![DebouncedEvent::new(
            PathBuf::from("openapi.yaml"),
            notify_debouncer_mini::DebouncedEventKind::Any,
        )]))
        .unwrap();
        tx.send(Ok(vec![DebouncedEvent::new(
            PathBuf::from("ItemsController.java"),
            notify_debouncer_mini::DebouncedEventKind::Any,
        )]))
        .unwrap();
        let replay = drain_queued_events(&rx);
        assert_eq!(replay.len(), 2);
        assert_eq!(replay[0].path, PathBuf::from("openapi.yaml"));
        assert_eq!(replay[1].path, PathBuf::from("ItemsController.java"));
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

    fn index_contract_fixture(dir: &tempfile::TempDir) -> (GraphStore, String, String, PathBuf) {
        let repo_root = dir.path().join("contract-repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        std::fs::write(
            repo_root.join("openapi.yaml"),
            "openapi: 3.0.0\ninfo: { title: t, version: \"1\" }\npaths:\n  /v1/items:\n    get:\n      responses: { \"200\": { description: ok } }\n",
        )
        .unwrap();
        std::fs::write(
            repo_root.join("ItemsController.java"),
            "@RestController\n@RequestMapping(\"/v1/items\")\npublic class ItemsController {\n  @GetMapping\n  public void list() {}\n}\n",
        )
        .unwrap();
        let canonical_root = std::fs::canonicalize(&repo_root).unwrap();
        let repo_url = format!("file://{}", canonical_root.display());
        let r_uid = nestweaver_schema::repo_uid("test", &repo_url);
        let (_result, store) =
            crate::index::index_directory_in_memory(&canonical_root, "test", &repo_url, "sha1")
                .unwrap();
        (store, r_uid, repo_url, canonical_root)
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

    fn process_fixture_batch(
        watcher: &CodeWatcher,
        store: &GraphStore,
        r_uid: &str,
        root: &Path,
        paths: &[PathBuf],
    ) -> WatchBatchOutcome {
        watcher
            .process_batch_with_io(
                store,
                r_uid,
                &format!("file://{}", root.display()),
                paths,
                &crate::index::FileSystemIndexEpilogueIo,
            )
            .unwrap()
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
        let WatchBatchOutcome::Published {
            files_processed: processed,
        } = process_fixture_batch(
            &watcher,
            &store,
            &r_uid,
            &canonical_root,
            &[canonical_root.join("src/b.js")],
        )
        else {
            panic!("valid watcher batch must publish")
        };
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
        let WatchBatchOutcome::Skipped { .. } = process_fixture_batch(
            &watcher,
            &store,
            &r_uid,
            &canonical_root,
            &[canonical_root.join("src/b.js")],
        ) else {
            panic!("an unreadable file must skip before publication")
        };
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
        let WatchBatchOutcome::Published {
            files_processed: processed,
        } = process_fixture_batch(
            &watcher,
            &store,
            &r_uid,
            &canonical_root,
            &[canonical_root.join("src/b.js")],
        )
        else {
            panic!("valid retry must publish")
        };
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
        let WatchBatchOutcome::Published {
            files_processed: processed,
        } = process_fixture_batch(
            &watcher,
            &store,
            &r_uid,
            &canonical_root,
            &[canonical_root.join("src/b.js")],
        )
        else {
            panic!("delete batch must publish")
        };
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
    fn spec_only_watcher_batch_refreshes_contract_source_and_notifies_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let (store, r_uid, repo_url, root) = index_contract_fixture(&dir);
        let old = root.join("openapi.yaml");
        let renamed = root.join("openapi.v2.yaml");
        std::fs::rename(&old, &renamed).unwrap();
        let watcher = CodeWatcher::new(dir.path().join("watch.lbug"), &root, "test");
        let notifications = AtomicUsize::new(0);

        let outcome = watcher
            .process_batch_and_notify(
                &store,
                &r_uid,
                &repo_url,
                &[old, renamed],
                &crate::index::FileSystemIndexEpilogueIo,
                Some(&|| {
                    notifications.fetch_add(1, Ordering::SeqCst);
                }),
            )
            .unwrap();
        assert!(matches!(
            outcome,
            WatchBatchOutcome::Published { files_processed: 0 }
        ));
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
        let get = store
            .list_contracts(Some(&r_uid))
            .unwrap()
            .into_iter()
            .find(|contract| contract.uid.ends_with(":http:GET:/v1/items"))
            .unwrap();
        assert_eq!(get.source_path, "openapi.v2.yaml");
        assert!(
            store
                .list_implemented_contract_uids()
                .unwrap()
                .contains(&get.uid)
        );
    }

    #[test]
    fn controller_and_spec_batch_publish_matching_edges_and_symbol_metadata() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let (store, r_uid, repo_url, root) = index_contract_fixture(&dir);
        let spec = root.join("openapi.yaml");
        let controller = root.join("ItemsController.java");
        std::fs::write(
            &spec,
            "openapi: 3.0.0\ninfo: { title: t, version: \"1\" }\npaths:\n  /v1/items:\n    get:\n      responses: { \"200\": { description: ok } }\n    post:\n      responses: { \"200\": { description: ok } }\n",
        )
        .unwrap();
        std::fs::write(
            &controller,
            "@RestController\n@RequestMapping(\"/v1/items\")\npublic class ItemsController {\n  @GetMapping\n  public void list() {}\n  @PostMapping\n  public void create() {}\n}\n",
        )
        .unwrap();
        let watcher = CodeWatcher::new(dir.path().join("watch.lbug"), &root, "test");
        let notifications = AtomicUsize::new(0);

        let outcome = watcher
            .process_batch_and_notify(
                &store,
                &r_uid,
                &repo_url,
                &[spec, controller],
                &crate::index::FileSystemIndexEpilogueIo,
                Some(&|| {
                    notifications.fetch_add(1, Ordering::SeqCst);
                }),
            )
            .unwrap();
        assert!(matches!(
            outcome,
            WatchBatchOutcome::Published { files_processed: 1 }
        ));
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
        let symbols = store.lookup_symbols_by_repo(&r_uid).unwrap();
        let create = symbols
            .iter()
            .find(|symbol| symbol.name == "create")
            .expect("modified controller method must publish");
        assert!(create.canonical_id.is_some());
        assert!(
            store.contracts_implemented_by(&create.uid).unwrap()[0]
                .0
                .ends_with(":http:POST:/v1/items")
        );
        let controller_class = symbols
            .iter()
            .find(|symbol| symbol.name == "ItemsController")
            .unwrap();
        assert_eq!(
            controller_class
                .framework_hint
                .as_ref()
                .map(|hint| hint.role.as_str()),
            Some("controller")
        );
    }

    #[test]
    fn malformed_spec_skips_before_publication_and_preserves_contracts() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let (store, r_uid, repo_url, root) = index_contract_fixture(&dir);
        let spec = root.join("openapi.yaml");
        let before: Vec<_> = store
            .list_contracts(Some(&r_uid))
            .unwrap()
            .into_iter()
            .map(|contract| (contract.uid, contract.source_path))
            .collect();
        std::fs::write(&spec, "openapi: [unfinished").unwrap();
        let db_path = dir.path().join("watch.lbug");
        let watcher = CodeWatcher::new(&db_path, &root, "test");
        let notifications = AtomicUsize::new(0);

        let outcome = watcher
            .process_batch_and_notify(
                &store,
                &r_uid,
                &repo_url,
                &[spec],
                &crate::index::FileSystemIndexEpilogueIo,
                Some(&|| {
                    notifications.fetch_add(1, Ordering::SeqCst);
                }),
            )
            .unwrap();
        assert!(matches!(outcome, WatchBatchOutcome::Skipped { .. }));
        assert_eq!(notifications.load(Ordering::SeqCst), 0);
        let after: Vec<_> = store
            .list_contracts(Some(&r_uid))
            .unwrap()
            .into_iter()
            .map(|contract| (contract.uid, contract.source_path))
            .collect();
        assert_eq!(after, before);
        assert!(!crate::sidecar_path(&db_path, ".index-dirty").exists());
    }

    #[test]
    fn final_snapshot_rejects_after_plan_spec_and_controller_mutations() {
        let dir = tempfile::tempdir().unwrap();
        let (store, r_uid, repo_url, root) = index_contract_fixture(&dir);
        let db_path = dir.path().join("watch.lbug");
        let watcher = CodeWatcher::new(&db_path, &root, "test");
        let spec = root.join("openapi.yaml");
        let controller = root.join("ItemsController.java");
        let spec_get = std::fs::read_to_string(&spec).unwrap();
        let controller_get = std::fs::read_to_string(&controller).unwrap();
        let spec_post = "openapi: 3.0.0\ninfo: { title: t, version: \"1\" }\npaths:\n  /v1/items:\n    post:\n      responses: { \"200\": { description: ok } }\n";
        let controller_post = "@RestController\n@RequestMapping(\"/v1/items\")\npublic class ItemsController { @PostMapping public void create() {} }\n";

        let outcome = watcher
            .process_batch_with_io_and_hook(
                &store,
                &r_uid,
                &repo_url,
                std::slice::from_ref(&spec),
                &crate::index::FileSystemIndexEpilogueIo,
                || std::fs::write(&spec, spec_post).unwrap(),
            )
            .unwrap();
        assert!(matches!(outcome, WatchBatchOutcome::Skipped { .. }));
        std::fs::write(&spec, &spec_get).unwrap();

        let outcome = watcher
            .process_batch_with_io_and_hook(
                &store,
                &r_uid,
                &repo_url,
                std::slice::from_ref(&spec),
                &crate::index::FileSystemIndexEpilogueIo,
                || std::fs::remove_file(&spec).unwrap(),
            )
            .unwrap();
        assert!(matches!(outcome, WatchBatchOutcome::Skipped { .. }));
        std::fs::write(&spec, &spec_get).unwrap();

        let created_spec = root.join("openapi.v2.yaml");
        let outcome = watcher
            .process_batch_with_io_and_hook(
                &store,
                &r_uid,
                &repo_url,
                std::slice::from_ref(&spec),
                &crate::index::FileSystemIndexEpilogueIo,
                || std::fs::write(&created_spec, spec_post).unwrap(),
            )
            .unwrap();
        assert!(matches!(outcome, WatchBatchOutcome::Skipped { .. }));
        std::fs::remove_file(&created_spec).unwrap();

        let outcome = watcher
            .process_batch_with_io_and_hook(
                &store,
                &r_uid,
                &repo_url,
                std::slice::from_ref(&controller),
                &crate::index::FileSystemIndexEpilogueIo,
                || std::fs::write(&controller, controller_post).unwrap(),
            )
            .unwrap();
        assert!(matches!(outcome, WatchBatchOutcome::Skipped { .. }));
        std::fs::write(&controller, &controller_get).unwrap();

        let outcome = watcher
            .process_batch_with_io_and_hook(
                &store,
                &r_uid,
                &repo_url,
                std::slice::from_ref(&controller),
                &crate::index::FileSystemIndexEpilogueIo,
                || std::fs::remove_file(&controller).unwrap(),
            )
            .unwrap();
        assert!(matches!(outcome, WatchBatchOutcome::Skipped { .. }));
        std::fs::write(&controller, &controller_get).unwrap();

        let created_controller = root.join("NewController.java");
        let outcome = watcher
            .process_batch_with_io_and_hook(
                &store,
                &r_uid,
                &repo_url,
                std::slice::from_ref(&controller),
                &crate::index::FileSystemIndexEpilogueIo,
                || std::fs::write(&created_controller, controller_post).unwrap(),
            )
            .unwrap();
        assert!(matches!(outcome, WatchBatchOutcome::Skipped { .. }));
        assert!(!crate::sidecar_path(&db_path, ".index-dirty").exists());
    }

    #[test]
    fn fresh_watcher_batch_builds_authoritative_source_and_contract_graph() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path().join("fresh-repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        std::fs::write(
            repo_root.join("openapi.yaml"),
            "openapi: 3.0.0\ninfo: { title: t, version: \"1\" }\npaths:\n  /fresh:\n    get:\n      responses: { \"200\": { description: ok } }\n",
        )
        .unwrap();
        std::fs::write(
            repo_root.join("FreshController.java"),
            "@RestController\n@RequestMapping(\"/fresh\")\npublic class FreshController {\n  @GetMapping public void get() {}\n}\n",
        )
        .unwrap();
        let root = std::fs::canonicalize(repo_root).unwrap();
        let repo_url = format!("file://{}", root.display());
        let r_uid = nestweaver_schema::repo_uid("test", &repo_url);
        let store = GraphStore::in_memory().unwrap();
        let watcher = CodeWatcher::new(dir.path().join("watch.lbug"), &root, "test");
        let notifications = AtomicUsize::new(0);

        let outcome = watcher
            .process_batch_and_notify(
                &store,
                &r_uid,
                &repo_url,
                &[root.join("openapi.yaml"), root.join("FreshController.java")],
                &crate::index::FileSystemIndexEpilogueIo,
                Some(&|| {
                    notifications.fetch_add(1, Ordering::SeqCst);
                }),
            )
            .unwrap();
        assert!(matches!(
            outcome,
            WatchBatchOutcome::Published { files_processed: 1 }
        ));
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
        assert!(store.lookup_repo(&r_uid).unwrap().is_some());
        let get_symbol = store
            .lookup_symbols_by_repo(&r_uid)
            .unwrap()
            .into_iter()
            .find(|symbol| symbol.name == "get")
            .expect("cold watcher must index unchanged controller source");
        assert!(
            store.contracts_implemented_by(&get_symbol.uid).unwrap()[0]
                .0
                .ends_with(":http:GET:/fresh")
        );
    }

    #[test]
    fn recovered_dirty_publication_is_refused_without_graph_success() {
        let dir = tempfile::tempdir().unwrap();
        let (store, r_uid, repo_url, root) = index_contract_fixture(&dir);
        let db_path = dir.path().join("watch.lbug");
        let watcher = CodeWatcher::new(&db_path, &root, "test");
        let publication = watcher
            .establish_graph_publication_with_io(&store, &crate::index::FileSystemIndexEpilogueIo)
            .unwrap();
        drop(publication);
        let before = store.list_contracts(Some(&r_uid)).unwrap().len();

        let error = watcher
            .process_batch_with_io(
                &store,
                &r_uid,
                &repo_url,
                &[root.join("openapi.yaml")],
                &crate::index::FileSystemIndexEpilogueIo,
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("incomplete prior index publication")
        );
        assert!(crate::sidecar_path(&db_path, ".index-dirty").exists());
        assert_eq!(store.list_contracts(Some(&r_uid)).unwrap().len(), before);
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
