//! File watcher for live incremental updates.
//!
//! Watches one vault and prepares changed notes plus affected incoming-link
//! sources before replacing any graph rows. Notes commit under per-file leases;
//! links resolve against the complete prospective batch, and publication happens
//! once after reconciliation. A failed batch stays fail-closed.
//!
//! Threading: synchronous + blocking. The caller owns the thread (the
//! CLI `brain watch` command runs it in the foreground; MCP integration
//! can spawn a dedicated thread once we've verified lbug's
//! multi-writer semantics).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use globset::GlobSet;
use nestweaver_parser::is_markdown;
use nestweaver_schema::{Vault, note_uid, vault_uid};
use nestweaver_store::{GraphScope, GraphStore, TantivyIndex};
use notify::event::{MetadataKind, ModifyKind};
use notify::{Event, EventKind, RecursiveMode, Watcher};

/// Opaque caller-owned protection held for one watcher mutation window.
///
/// Daemon callers use this to compose their shutdown admission guard and
/// single-writer gate. Direct callers omit the factory because opening the
/// store already gives them exclusive writer ownership.
pub trait WatchMutationLease: Send {}

impl<T: Send> WatchMutationLease for T {}

pub type WatchMutationLeaseFactory =
    Arc<dyn Fn(&'static str) -> Result<Box<dyn WatchMutationLease>, anyhow::Error> + Send + Sync>;

/// A batch was refused because daemon shutdown has begun. Watchers treat this
/// as a normal stop, not as graph corruption or a retryable filesystem error.
#[derive(Debug, thiserror::Error)]
#[error("watcher mutation refused because daemon shutdown has started")]
pub struct WatchMutationRefused;

/// Manifest filenames whose changes should trigger a manifest cache refresh.
const MANIFEST_FILES: &[&str] = &[
    "package.json",
    "Cargo.toml",
    "go.mod",
    "pyproject.toml",
    "requirements.txt",
    "composer.json",
    "Gemfile",
    "pubspec.yaml",
    "Package.swift",
    "CMakeLists.txt",
];

/// Names of directories the MARKDOWN VAULT watcher never descends into.
///
/// nw-325 asked whether this should be unified with `index::SKIP_DIRS`. It
/// should NOT. This list guards a vault of notes, not a code index: both call
/// sites are gated on `is_markdown(path)` first, the list carries `.obsidian`
/// and `.trash` which are meaningless to a code walk, and it deliberately omits
/// `ios`/`android`/`public`/`out`/`env`, which are perfectly ordinary folder
/// names for notes. Pointing this at the 33-entry code list would start
/// silently dropping a user's notes.
///
/// The property nw-325 is actually about — a prune must be disclosed — already
/// holds here: both call sites return `UpdateOutcome::Skipped { reason: "in
/// skip dir" }`, which is logged. The code walk had no such channel, which is
/// what made it dangerous rather than merely opinionated.
const SKIP_DIRS: &[&str] = &[
    ".obsidian",
    ".trash",
    ".git",
    "node_modules",
    "target",
    ".next",
    ".nuxt",
    "dist",
    "build",
];

/// Per-event handling outcome — surfaces in logs so users can see what
/// the watcher actually did. Useful when debugging "I saved but the
/// brain didn't update".
#[derive(Debug, Clone)]
pub enum UpdateOutcome {
    Updated {
        path: PathBuf,
        headings: usize,
        sections: usize,
        wikilinks: usize,
        tags: usize,
    },
    Deleted {
        path: PathBuf,
    },
    Skipped {
        path: PathBuf,
        reason: &'static str,
    },
}

/// Live file-watcher for a single vault. Construct via `new`, then call
/// `run` from a dedicated thread — `run` blocks until `stop()` is
/// signalled or the watcher's debouncer hits a fatal error.
pub struct BrainWatcher {
    db_path: PathBuf,
    vault_root: PathBuf,
    instance_id: String,
    vault_name: String,
    stop_flag: Arc<AtomicBool>,
    /// Optional sidecar path for the Tantivy index. When set, the watcher
    /// keeps the BM25 index in sync alongside the graph.
    tantivy_path: Option<PathBuf>,
    /// Optional path for the manifests JSON sidecar (`<db>.manifests.json`).
    /// When set, manifest file changes (Cargo.toml, package.json, …) trigger
    /// a re-parse and sidecar update.
    manifests_path: Option<PathBuf>,
    /// Debounce interval in milliseconds for filesystem events.
    debounce_ms: u64,
    /// Compiled `.brainignore` glob patterns. Loaded once at construction
    /// from the vault root's `.brainignore` file (or built-in defaults).
    ignore_set: GlobSet,
    /// Pre-opened TantivyIndex from the caller (e.g. daemon). When set,
    /// `run_inner` uses this instead of opening its own from `tantivy_path`.
    external_tantivy: Option<Arc<TantivyIndex>>,
    mutation_lease_factory: Option<WatchMutationLeaseFactory>,
    ready_callback: Option<Box<dyn FnOnce() + Send>>,
    #[cfg(test)]
    ready_signal: Option<std::sync::mpsc::Sender<()>>,
}

impl BrainWatcher {
    fn event_targets_graph(&self, path: &Path) -> bool {
        if !is_markdown(path) || path_in_skip_dir(path) {
            return false;
        }
        let rel_path = path
            .strip_prefix(&self.vault_root)
            .unwrap_or(path)
            .to_string_lossy();
        !crate::brainignore::is_ignored(&rel_path, &self.ignore_set)
    }

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
            "brain watcher batch",
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
            "brain watcher batch",
            io,
            Some(&GraphScope::unified()),
            true,
        )
    }

    pub fn new(
        db_path: impl Into<PathBuf>,
        vault_root: impl Into<PathBuf>,
        instance_id: impl Into<String>,
        vault_name: impl Into<String>,
    ) -> Self {
        // Canonicalize the vault root so `strip_prefix` against FSEvents'
        // already-canonicalized paths succeeds. On macOS the difference
        // between `/var/folders/...` and `/private/var/folders/...` is
        // the whole ballgame for getting stable note_uids across
        // (indexer, watcher) pairs.
        let vault_root: PathBuf = vault_root.into();
        let vault_root = std::fs::canonicalize(&vault_root).unwrap_or(vault_root);
        let ignore_set = crate::brainignore::load_brain_ignore(&vault_root, &[]);
        Self {
            db_path: db_path.into(),
            vault_root,
            instance_id: instance_id.into(),
            vault_name: vault_name.into(),
            stop_flag: Arc::new(AtomicBool::new(false)),
            tantivy_path: None,
            manifests_path: None,
            debounce_ms: 200,
            ignore_set,
            external_tantivy: None,
            mutation_lease_factory: None,
            ready_callback: None,
            #[cfg(test)]
            ready_signal: None,
        }
    }

    /// Enable Tantivy index sync. When set, every note update/delete
    /// also updates the BM25 index at this path. Leave unset to skip
    /// Tantivy maintenance (graph stays current but BM25 search falls
    /// behind until `brain reindex-search`).
    pub fn with_tantivy_index(mut self, path: impl Into<PathBuf>) -> Self {
        self.tantivy_path = Some(path.into());
        self
    }

    /// Enable manifest cache sync. When set, changes to manifest files
    /// (package.json, Cargo.toml, go.mod, etc.) trigger a re-parse and
    /// update the sidecar at this path.
    pub fn with_manifests_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.manifests_path = Some(path.into());
        self
    }

    /// Use a pre-opened TantivyIndex instead of opening one from
    /// `tantivy_path`. Used when the daemon spawns the watcher and
    /// already holds the Tantivy writer.
    pub fn with_external_tantivy(mut self, tantivy: Arc<TantivyIndex>) -> Self {
        self.external_tantivy = Some(tantivy);
        self
    }

    /// Install an external RAII lease acquired once around each mutation
    /// window. The returned object remains live through the inline callback.
    pub fn with_mutation_lease_factory(mut self, factory: WatchMutationLeaseFactory) -> Self {
        self.mutation_lease_factory = Some(factory);
        self
    }

    fn acquire_mutation_lease(
        &self,
        label: &'static str,
    ) -> Result<Option<Box<dyn WatchMutationLease>>, anyhow::Error> {
        self.mutation_lease_factory
            .as_ref()
            .map(|factory| factory(label))
            .transpose()
    }

    /// `acquire_mutation_lease`, but logs the "refused during shutdown"
    /// special case once here instead of duplicating the log at every call
    /// site. The `WatchMutationRefused` error itself is passed through
    /// unwrapped (not folded into a different return shape and not given
    /// `.context(...)`, which would still let `downcast_ref` find it through
    /// anyhow's chain but there is no reason to rely on that) so callers
    /// can `?`-propagate it up to `process_batch`'s caller, which is the
    /// single place that turns it into a graceful `Ok(())` exit — exactly
    /// the match the pre-nw-380 code had at its one call site.
    ///
    /// nw-380: this is called once PER FILE (and once around establishing the
    /// publication marker, and once around finalizing it) rather than once
    /// for an entire debounced batch, so a large or slow batch releases and
    /// re-acquires the write gate between files instead of holding it
    /// continuously. `WriteGate` is FIFO-fair (`write_gate.rs`), so each
    /// release point is a real opportunity for a queued `trigram_reconcile`/
    /// `embedding_reconcile` waiter to be served before this batch resumes,
    /// rather than only after the whole batch — however large — completes.
    fn try_acquire_batch_lease(
        &self,
        needed: bool,
        label: &'static str,
    ) -> Result<Option<Box<dyn WatchMutationLease>>, anyhow::Error> {
        if !needed {
            return Ok(None);
        }
        match self.acquire_mutation_lease(label) {
            Ok(lease) => Ok(lease),
            Err(error) if error.downcast_ref::<WatchMutationRefused>().is_some() => {
                tracing::info!("BrainWatcher batch refused during shutdown; exiting");
                Err(error)
            }
            Err(error) => Err(error.context("acquire brain watcher batch lease")),
        }
    }

    fn event_targets_manifest(&self, path: &Path) -> bool {
        self.manifests_path.is_some()
            && path.exists()
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| MANIFEST_FILES.contains(&name))
    }

    /// Set the debounce interval for filesystem events.
    pub fn with_debounce_ms(mut self, ms: u64) -> Self {
        self.debounce_ms = ms;
        self
    }

    /// Called once after filesystem subscription and initial publication have
    /// both succeeded. Dropping the watcher without invoking it means startup
    /// failed or was cancelled; callers must treat that as failed readiness.
    pub fn with_ready_callback(mut self, ready: impl FnOnce() + Send + 'static) -> Self {
        self.ready_callback = Some(Box::new(ready));
        self
    }

    #[cfg(test)]
    fn with_ready_signal(mut self, ready: std::sync::mpsc::Sender<()>) -> Self {
        self.ready_signal = Some(ready);
        self
    }

    /// Replace the ignore set with one that includes additional patterns
    /// (e.g. from the `--ignore` CLI flag). Reloads the `.brainignore`
    /// file (or defaults) combined with `extra`.
    pub fn with_extra_ignore_patterns(mut self, extra: &[String]) -> Self {
        if !extra.is_empty() {
            self.ignore_set = crate::brainignore::load_brain_ignore(&self.vault_root, extra);
        }
        self
    }

    /// Returns a handle that can request graceful shutdown from another
    /// thread. After `stop()` is called the event loop exits the next
    /// time it wakes (≤ 200ms debounce interval).
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle {
            flag: self.stop_flag.clone(),
        }
    }

    /// Block until shutdown is requested or the underlying debouncer
    /// errors. Returns Ok on graceful shutdown.
    ///
    /// Opens its own `GraphStore` from `self.db_path`. For sharing a store
    /// with the web server, use `run_with_store` instead.
    pub fn run(self) -> Result<(), anyhow::Error> {
        // nw-C1: this watcher is a writer, so it reconciles an abandoned
        // publication left by a crashed indexer instead of inheriting the wedge.
        let authority =
            nestweaver_store::acquire_db_write_lease(&self.db_path).map_err(|error| {
                anyhow::anyhow!("cannot start brain watcher without writer authority: {error:?}")
            })?;
        let store = Arc::new(crate::index::open_store_for_writing_with_authority(
            &self.db_path,
            &authority,
        )?);
        self.run_inner(store, None)
    }

    /// Run under an exact writer authority already held by the caller for the
    /// watcher's complete lifetime.
    pub fn run_with_write_lease(
        self,
        authority: &nestweaver_store::DbWriteLease,
    ) -> Result<(), anyhow::Error> {
        let store = Arc::new(crate::index::open_store_for_writing_with_authority(
            &self.db_path,
            authority,
        )?);
        self.run_inner(store, None)
    }

    /// Like `run`, but uses a caller-provided `Arc<GraphStore>` and invokes
    /// `on_change` after every batch that mutates the graph. The callback
    /// also receives the graph-generation bump so the web server can emit
    /// an SSE event to connected clients.
    pub fn run_with_store(
        self,
        store: Arc<GraphStore>,
        on_change: Option<Box<dyn Fn() + Send>>,
    ) -> Result<(), anyhow::Error> {
        self.run_inner(store, on_change)
    }

    /// Shared implementation used by both `run` and `run_with_store`.
    fn run_inner(
        mut self,
        store: Arc<GraphStore>,
        on_change: Option<Box<dyn Fn() + Send>>,
    ) -> Result<(), anyhow::Error> {
        if self.stop_flag.load(Ordering::Acquire) {
            anyhow::bail!("brain watcher stopped before startup");
        }
        // Subscribe before slow initialization: the channel buffers every
        // mutation while initial publication and callbacks are running.
        // Channel from the debouncer into our loop.
        let (tx, rx) = std::sync::mpsc::channel::<RawWatchResult>();
        let mut watcher =
            notify::recommended_watcher(move |result: Result<Event, notify::Error>| match result {
                Ok(event) if event_kind_can_mutate(&event.kind) => {
                    let _ = tx.send(Ok(event.paths));
                }
                Ok(_) => {}
                Err(error) => {
                    let _ = tx.send(Err(error));
                }
            })
            .with_context(|| "init filesystem watcher")?;
        watcher
            .watch(&self.vault_root, RecursiveMode::Recursive)
            .with_context(|| format!("watch {}", self.vault_root.display()))?;
        // Use external Tantivy if provided (daemon mode), otherwise open from path.
        let tantivy: Option<Arc<TantivyIndex>> = if let Some(ext) = self.external_tantivy.take() {
            Some(ext)
        } else {
            match &self.tantivy_path {
                Some(p) => match TantivyIndex::open_or_create(p) {
                    Ok(idx) => Some(Arc::new(idx)),
                    Err(e) => {
                        tracing::warn!(
                            path = %p.display(),
                            error = %e,
                            "BrainWatcher: Tantivy index unavailable; BM25 search will fall behind"
                        );
                        None
                    }
                },
                None => None,
            }
        };

        // Make sure the Vault node exists — first-time runs (no prior
        // `brain add`) still get a working graph.
        let v_uid = vault_uid(&self.instance_id, &self.vault_root.to_string_lossy());
        let _initial_mutation_lease = match self.acquire_mutation_lease("watch_vault_initial") {
            Ok(lease) => lease,
            Err(error) if error.downcast_ref::<WatchMutationRefused>().is_some() => {
                tracing::info!("BrainWatcher startup refused during shutdown; exiting");
                return Ok(());
            }
            Err(error) => return Err(error.context("acquire brain watcher startup lease")),
        };
        let initial_publication = self.establish_graph_publication_with_io(
            &store,
            &crate::index::FileSystemIndexEpilogueIo,
        )?;
        let ensure_result = ensure_vault(
            &store,
            &v_uid,
            &self.vault_root,
            &self.instance_id,
            &self.vault_name,
        );
        let initial_finalization = self.finalize_graph_publication_with_io(
            initial_publication,
            &crate::index::FileSystemIndexEpilogueIo,
        );
        match (ensure_result, initial_finalization) {
            (Ok(()), Ok(())) => {
                if let Some(ref cb) = on_change {
                    cb();
                }
            }
            (Err(error), Ok(())) => return Err(error),
            (Ok(()), Err(error)) => return Err(error.into()),
            (Err(error), Err(finalization)) => {
                return Err(error.context(format!(
                    "Vault upsert also failed mandatory publication: {finalization}"
                )));
            }
        }
        drop(_initial_mutation_lease);

        if self.stop_flag.load(Ordering::Acquire) {
            anyhow::bail!("brain watcher stopped during startup");
        }
        if let Some(ready) = self.ready_callback.take() {
            ready();
        }
        #[cfg(test)]
        if let Some(ready) = &self.ready_signal {
            let _ = ready.send(());
        }

        tracing::info!(
            vault = %self.vault_root.display(),
            db = %self.db_path.display(),
            "BrainWatcher running"
        );

        // Loop until stop_flag flips. recv_timeout lets us poll the flag
        // even on idle vaults so shutdown is responsive.
        loop {
            if self.stop_flag.load(Ordering::Relaxed) {
                tracing::info!("BrainWatcher stop requested; exiting");
                return Ok(());
            }
            let batch = match receive_debounced_paths(
                &rx,
                Duration::from_millis(self.debounce_ms),
                &self.stop_flag,
            ) {
                WatchReceive::Batch(paths) => paths,
                WatchReceive::NotifyError(err) => {
                    if !self.vault_root.exists() {
                        tracing::error!(
                            vault = %self.vault_root.display(),
                            "vault root no longer exists; watcher exiting"
                        );
                        return Err(anyhow::anyhow!(
                            "vault root '{}' was deleted or unmounted",
                            self.vault_root.display()
                        ));
                    }
                    tracing::warn!("notify error: {err}");
                    continue;
                }
                WatchReceive::Timeout => {
                    // Periodic liveness check: detect vault directory
                    // disappearance even when `notify` is silent (e.g.
                    // when the directory's inode is replaced via a
                    // rename rather than removed). Cheap stat — runs
                    // at most every 250 ms.
                    if !self.vault_root.exists() {
                        tracing::error!(
                            vault = %self.vault_root.display(),
                            "vault root vanished during watch; exiting"
                        );
                        return Err(anyhow::anyhow!(
                            "vault root '{}' was deleted or unmounted",
                            self.vault_root.display()
                        ));
                    }
                    continue;
                }
                WatchReceive::Disconnected => {
                    tracing::warn!("debouncer disconnected; exiting");
                    return Ok(());
                }
                WatchReceive::Stop => {
                    tracing::info!("BrainWatcher stop requested; exiting");
                    return Ok(());
                }
            };

            match self.process_batch(&store, tantivy.as_deref(), &v_uid, batch, &on_change) {
                Ok(()) => {}
                Err(error) if error.downcast_ref::<WatchMutationRefused>().is_some() => {
                    return Ok(());
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Process one debounced batch of raw filesystem paths: dedupe, decide
    /// whether it touches the graph, then reindex every path and (for a
    /// graph-touching batch) recompute PPR and publish.
    ///
    /// Split out of `run_inner`'s loop body so it is callable directly with a
    /// synthetic path list — real filesystem-event timing is inherently
    /// flaky in tests (see the `#[ignore]`d `watcher_picks_up_a_new_file`
    /// etc.), but the nw-380 fix (lease acquired per file rather than once
    /// for the whole batch) needs a DETERMINISTIC way to count acquisitions,
    /// which this makes possible via `with_mutation_lease_factory` alone.
    #[allow(clippy::too_many_arguments)]
    fn process_batch(
        &self,
        store: &GraphStore,
        tantivy: Option<&TantivyIndex>,
        v_uid: &str,
        batch: Vec<PathBuf>,
        on_change: &Option<Box<dyn Fn() + Send>>,
    ) -> Result<(), anyhow::Error> {
        let mut unique_paths = batch;
        unique_paths.sort();
        unique_paths.dedup();
        let batch_started = Instant::now();
        let batch_len = unique_paths.len();
        let graph_batch = unique_paths
            .iter()
            .any(|path| self.event_targets_graph(path));
        let mutation_batch = graph_batch
            || unique_paths
                .iter()
                .any(|path| self.event_targets_manifest(path));

        // nw-380: establishing the fail-closed publication marker is itself
        // a store write, so it gets its own freshly acquired lease rather
        // than inheriting one held since before this batch began — there is
        // no reason to hold the gate across the symbol-index/title-map
        // rebuild below, which touches no shared derived state the gate
        // protects.
        let publication = if graph_batch {
            let _lease = self.try_acquire_batch_lease(true, "watch_vault_batch")?;
            // Establish the fail-closed marker before the prepared batch can
            // commit some notes and then fail during a later transaction. It
            // stays established across the whole per-path loop below (crash
            // safety demands that: a crash mid-loop must still find the
            // corpus marked dirty), even though the WRITE GATE itself is
            // released as soon as `_lease` drops here.
            Some(self.establish_graph_publication_with_io(
                store,
                &crate::index::FileSystemIndexEpilogueIo,
            )?)
        } else {
            None
        };

        let symbol_index = crate::cross_domain::build_symbol_index(store).ok();
        let graph_paths: Vec<_> = unique_paths
            .iter()
            .filter(|path| self.event_targets_graph(path))
            .cloned()
            .collect();
        let mut batch_failures = Vec::new();
        if graph_batch {
            let mut embedding_candidates = Vec::new();
            for path in &graph_paths {
                let relative = path.strip_prefix(&self.vault_root)?;
                embedding_candidates.extend(store.note_embedding_candidate_uids(&note_uid(
                    v_uid,
                    &relative.to_string_lossy(),
                ))?);
            }
            // Planning parses changed notes and affected linking sources before
            // any deletion. A failed plan or transaction leaves the publication
            // marker dirty and never emits a successful change callback.
            crate::index_md::refresh_watched_paths(
                store,
                &self.vault_root,
                &self.instance_id,
                &self.vault_name,
                &graph_paths,
                &self.ignore_set,
                &|| self.try_acquire_batch_lease(true, "watch_vault_batch"),
            )?;
            let note_tags: HashMap<_, _> = store.note_tag_sets()?.into_iter().collect();
            for path in &graph_paths {
                let _lease = self.try_acquire_batch_lease(true, "watch_vault_sidecars")?;
                self.refresh_prepared_note_sidecars(
                    store,
                    tantivy,
                    v_uid,
                    path,
                    symbol_index.as_ref(),
                    &note_tags,
                )?;
            }
            let _lease = self.try_acquire_batch_lease(true, "watch_vault_embeddings")?;
            tombstone_vault_embeddings_after_commit(store, &embedding_candidates, "watched batch");
        }
        for path in unique_paths
            .into_iter()
            .filter(|path| !self.event_targets_graph(path))
        {
            let _lease = self.try_acquire_batch_lease(mutation_batch, "watch_vault_batch")?;
            let outcome = self.handle_non_graph_event(store, path)?;
            log_outcome(&outcome);
        }

        // After a batch that touched the graph, recompute PPR over the
        // unified scope so brain_context queries see fresh ranks. This stays
        // a single per-BATCH operation, not per-file: PPR recompute cost
        // scales with total graph size, not with how many files changed, so
        // doing it once per file would multiply an already-nontrivial cost
        // by the batch's file count instead of paying it once. (Per the
        // architecture doc §6.3 this was "fine for <50K-node graphs
        // (~milliseconds)"; nw-380's own production measurement — 192,818
        // live vectors — is well past that, so treat this as a real,
        // currently-unshrunk cost rather than the stale comment's
        // "~milliseconds".)
        if graph_batch {
            let _lease = self.try_acquire_batch_lease(true, "watch_vault_batch")?;
            let finalization = self.finalize_graph_publication_with_io(
                publication.expect("graph batch established publication lease"),
                &crate::index::FileSystemIndexEpilogueIo,
            );
            if finalization.is_ok() {
                tracing::debug!(
                    generation = store.pagerank_generation(),
                    "PPR durably published after watcher batch"
                );
                // Record the watcher commit timestamp so `brain status`
                // shows the actual last-indexed time, not max(modified_at).
                if let Err(e) = crate::extensions::record_last_indexed_at(&self.db_path, v_uid) {
                    batch_failures.push(format!("record last_indexed_at: {e:#}"));
                }

                if let Some(cb) = on_change {
                    cb();
                }
            }
            if let Err(error) = finalization {
                batch_failures.push(format!("mandatory graph publication: {error}"));
            }
        }
        // nw-380: "instrument first" — nothing previously logged batch size
        // or hold duration, so a recurrence could not distinguish "many
        // files" from "one slow file" from "an expensive PPR recompute" as
        // the cause. `elapsed_ms` covers the WHOLE batch (all per-file lease
        // windows plus the final publish), not any single lease hold,
        // precisely because per-file holds are no longer expected to
        // dominate it after this fix.
        tracing::info!(
            files = batch_len,
            elapsed_ms = batch_started.elapsed().as_millis() as u64,
            mutation_batch,
            "BrainWatcher batch complete"
        );
        if !batch_failures.is_empty() {
            anyhow::bail!(
                "brain watcher batch failed after committed graph work: {}",
                batch_failures.join("; ")
            );
        }
        Ok(())
    }

    /// Mirror the committed note representation, rather than reading the file
    /// again: another save can occur while this prepared batch is committing.
    #[allow(clippy::too_many_arguments)]
    fn refresh_prepared_note_sidecars(
        &self,
        store: &GraphStore,
        tantivy: Option<&TantivyIndex>,
        v_uid: &str,
        path: &Path,
        symbols: Option<&crate::cross_domain::SymbolIndex>,
        note_tags: &HashMap<String, Vec<String>>,
    ) -> Result<(), anyhow::Error> {
        let relative = path.strip_prefix(&self.vault_root)?;
        let uid = note_uid(v_uid, &relative.to_string_lossy());
        let note = store
            .lookup_notes_by_uids(std::slice::from_ref(&uid))?
            .into_iter()
            .next();
        let Some(note) = note else {
            if let Some(index) = tantivy {
                index.remove_note(&uid)?;
            }
            return Ok(());
        };
        let cross_domain = if let Some(symbols) = symbols {
            crate::cross_domain::discover_cross_domain_links_for_note_with_index(
                store, &uid, symbols,
            )
        } else {
            crate::cross_domain::discover_cross_domain_links_for_note(store, &uid)
        };
        if let Err(error) = cross_domain {
            tracing::warn!(%error, "watcher cross-domain refresh failed");
        }
        if let Some(index) = tantivy {
            let headings = store.headings_in_note(&uid)?;
            let sections = store.sections_in_note(&uid)?;
            let heading_docs: Vec<_> = headings
                .iter()
                .map(|h| (h.uid.clone(), h.text.clone()))
                .collect();
            let section_docs: Vec<_> = sections
                .iter()
                .map(|s| {
                    let title = s
                        .heading_uid
                        .as_ref()
                        .and_then(|uid| headings.iter().find(|h| &h.uid == uid))
                        .map(|h| h.text.clone())
                        .unwrap_or_default();
                    (s.uid.clone(), s.text_content.clone(), title)
                })
                .collect();
            let mut body = Vec::new();
            if let Some(raw) = note.frontmatter_raw {
                body.push(raw);
            }
            body.extend(sections.iter().map(|s| s.text_content.clone()));
            let names = note_tags.get(&uid).cloned().unwrap_or_default();
            index.update_note(
                &uid,
                &note.title,
                v_uid,
                &body,
                &heading_docs,
                &section_docs,
                &names,
            )?;
        }
        Ok(())
    }

    fn handle_non_graph_event(
        &self,
        store: &GraphStore,
        path: PathBuf,
    ) -> Result<UpdateOutcome, anyhow::Error> {
        // Check for manifest file changes and refresh the sidecar cache.
        // This check runs before the markdown filter so manifest files are
        // never silently dropped as "not markdown".
        if let Some(manifests_path) = &self.manifests_path {
            let is_manifest = path
                .file_name()
                .and_then(|f| f.to_str())
                .is_some_and(|name| MANIFEST_FILES.contains(&name));
            if is_manifest && path.exists() {
                let repo_path = path
                    .parent()
                    .unwrap_or_else(|| std::path::Path::new("."))
                    .to_path_buf();
                let manifest = crate::manifest::parse_manifest(
                    &crate::content_reader::FilesystemReader::new(&repo_path),
                );
                // Load the existing cache, update this repo's entry, and save.
                let repo_key = repo_path.to_string_lossy().into_owned();
                let canonical_manifests_path = crate::manifest::manifest_cache_path(&self.db_path);
                let uses_canonical_path = manifests_path == &canonical_manifests_path;
                let loaded = if uses_canonical_path {
                    crate::manifest::load_manifest_cache_for_db(store, &self.db_path)
                } else {
                    crate::manifest::load_manifest_cache(manifests_path)
                };
                match loaded {
                    Ok(mut cache) => {
                        cache.insert(repo_key.clone(), manifest);
                        let saved = if uses_canonical_path {
                            crate::manifest::save_manifest_cache_for_db(
                                &cache,
                                store,
                                &self.db_path,
                            )
                        } else {
                            crate::manifest::save_manifest_cache(&cache, manifests_path)
                        };
                        if let Err(e) = saved {
                            tracing::warn!(
                                "watcher: failed to save manifest cache after {}: {e}",
                                path.display()
                            );
                        } else {
                            tracing::info!(
                                repo = %repo_key,
                                manifest = %path.display(),
                                "watcher: manifest cache refreshed"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!("watcher: failed to load manifest cache for update: {e}");
                    }
                }
                return Ok(UpdateOutcome::Skipped {
                    path,
                    reason: "manifest file — cache refreshed",
                });
            }
        }

        // Filter: must be a .md file, must not be inside any skipped dir.
        if !is_markdown(&path) {
            return Ok(UpdateOutcome::Skipped {
                path,
                reason: "not markdown",
            });
        }
        if path_in_skip_dir(&path) {
            return Ok(UpdateOutcome::Skipped {
                path,
                reason: "in skip dir",
            });
        }

        let rel_path = path
            .strip_prefix(&self.vault_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();

        // Apply .brainignore patterns.
        if crate::brainignore::is_ignored(&rel_path, &self.ignore_set) {
            return Ok(UpdateOutcome::Skipped {
                path,
                reason: "matched .brainignore pattern",
            });
        }
        Ok(UpdateOutcome::Skipped {
            path,
            reason: "graph event handled by the prepared batch",
        })
    }
}

/// Signal for stopping a running BrainWatcher from outside its thread.
#[derive(Clone)]
pub struct ShutdownHandle {
    flag: Arc<AtomicBool>,
}

impl ShutdownHandle {
    /// Create a `ShutdownHandle` from an existing `AtomicBool` flag.
    /// Used by other watchers (`CodeWatcher`) that share the same
    /// shutdown pattern but manage their own flag.
    pub fn from_flag(flag: Arc<AtomicBool>) -> Self {
        Self { flag }
    }

    pub fn stop(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    /// Returns `true` if a shutdown has been requested.
    pub fn is_stopped(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
}

// ── helpers ────────────────────────────────────────────────────────────────

/// Raw watcher messages preserve notify's event classification. Access-only
/// events are discarded in the callback before they can extend a debounce
/// window, acquire a daemon writer lease, or feed back from parser reads.
pub(crate) type RawWatchResult = Result<Vec<PathBuf>, notify::Error>;

pub(crate) enum WatchReceive {
    Batch(Vec<PathBuf>),
    NotifyError(notify::Error),
    Timeout,
    Disconnected,
    Stop,
}

pub(crate) fn event_kind_can_mutate(kind: &EventKind) -> bool {
    match kind {
        EventKind::Access(_) => false,
        EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::AccessTime
            | MetadataKind::Permissions
            | MetadataKind::Ownership
            | MetadataKind::Extended,
        )) => false,
        EventKind::Any
        | EventKind::Create(_)
        | EventKind::Modify(_)
        | EventKind::Remove(_)
        | EventKind::Other => true,
    }
}

/// Hard ceiling on how long ONE debounced batch may keep growing under a
/// steady trickle of events, regardless of `quiet_period` resetting on every
/// arrival (nw-380). Unlike `quiet_period` this deadline is fixed at the
/// FIRST event's arrival and never extended — it exists only to guarantee a
/// batch eventually gets handed off (and, downstream, that its write-gate
/// lease windows and publication-marker-dirty window are eventually
/// released) instead of growing for as long as the vault keeps being
/// edited. Production measured a single batch's write-gate hold reach 453s
/// this way before this cap existed. The value only needs to be FINITE, not
/// small — it trades a slightly larger-than-minimal batch for not
/// re-triggering a full per-batch PPR recompute (see `run_inner`) on every
/// few-second tick during a long, continuously-active editing session.
const WATCH_BATCH_MAX_AGE: Duration = Duration::from_secs(5);

pub(crate) fn receive_debounced_paths(
    rx: &std::sync::mpsc::Receiver<RawWatchResult>,
    quiet_period: Duration,
    stop_flag: &AtomicBool,
) -> WatchReceive {
    let mut paths = match rx.recv_timeout(Duration::from_millis(250)) {
        Ok(Ok(paths)) => paths,
        Ok(Err(error)) => return WatchReceive::NotifyError(error),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return WatchReceive::Timeout,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            return WatchReceive::Disconnected;
        }
    };
    let max_age_deadline = Instant::now() + WATCH_BATCH_MAX_AGE;
    let mut quiet_deadline = Instant::now() + quiet_period;
    loop {
        if stop_flag.load(Ordering::Relaxed) {
            return WatchReceive::Stop;
        }
        let now = Instant::now();
        if now >= quiet_deadline || now >= max_age_deadline {
            return WatchReceive::Batch(paths);
        }
        let wait = quiet_deadline
            .min(max_age_deadline)
            .saturating_duration_since(now)
            .min(Duration::from_millis(250));
        match rx.recv_timeout(wait) {
            Ok(Ok(mut more_paths)) => {
                paths.append(&mut more_paths);
                quiet_deadline = Instant::now() + quiet_period;
            }
            Ok(Err(error)) => return WatchReceive::NotifyError(error),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return WatchReceive::Disconnected;
            }
        }
    }
}

fn log_outcome(outcome: &UpdateOutcome) {
    match outcome {
        UpdateOutcome::Updated {
            path,
            headings,
            sections,
            wikilinks,
            tags,
        } => {
            tracing::info!(
                "Updated: {} ({} heading(s), {} section(s), {} wikilink(s), {} tag(s))",
                path.display(),
                headings,
                sections,
                wikilinks,
                tags,
            );
        }
        UpdateOutcome::Deleted { path } => {
            tracing::info!("Deleted: {}", path.display());
        }
        UpdateOutcome::Skipped { path, reason } => {
            tracing::debug!("Skipped {}: {}", path.display(), reason);
        }
    }
}

fn path_in_skip_dir(path: &Path) -> bool {
    path.components().any(|c| {
        c.as_os_str()
            .to_str()
            .is_some_and(|name| SKIP_DIRS.contains(&name))
    })
}

fn ensure_vault(
    store: &GraphStore,
    v_uid: &str,
    root: &Path,
    instance_id: &str,
    name: &str,
) -> Result<(), anyhow::Error> {
    store
        .upsert_vault(&Vault {
            uid: v_uid.to_string(),
            name: name.to_string(),
            root_path: root.to_string_lossy().into_owned(),
            instance_id: instance_id.to_string(),
        })
        .context("upsert_vault")?;
    Ok(())
}

// ── tests ──────────────────────────────────────────────────────────────────

/// Tombstone vault embeddings whose nodes did not survive a refresh.
///
/// Best-effort and AFTER the commit, exactly like the symbol epilogue: a
/// tombstoning failure must not fail the refresh that already succeeded, and
/// the periodic reconciler remains the backstop.
fn tombstone_vault_embeddings_after_commit(
    store: &GraphStore,
    candidates: &[String],
    operation: &str,
) {
    if candidates.is_empty() {
        return;
    }
    match store.tombstone_deleted_vault_embeddings(candidates) {
        Ok(0) => {}
        Ok(removed) => {
            tracing::debug!("{operation}: tombstoned {removed} dead vault vector(s)");
        }
        Err(error) => {
            tracing::warn!(
                "{operation}: could not tombstone dead vault vectors: {error}; \
                 the periodic reconciler will reclaim them"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::{Mutex, MutexGuard};
    use std::thread;
    use std::time::Duration;

    static WATCHER_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn serial_watcher_test() -> MutexGuard<'static, ()> {
        WATCHER_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Build a vault on disk with the given files, return temp dir + path.
    fn make_vault(files: &[(&str, &str)]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("vault");
        fs::create_dir_all(&root).unwrap();
        for (rel, content) in files {
            let p = root.join(rel);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&p, content).unwrap();
        }
        (dir, root)
    }

    #[test]
    fn watched_target_edit_preserves_incoming_note_and_heading_links() {
        let (_dir, root) = make_vault(&[
            ("Alpha.md", "# Alpha\n\n[[Beta]] and [[Beta#Details]]\n"),
            ("Beta.md", "# Beta\n\n## Details\n\nold body\n"),
        ]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        crate::index_md::index_markdown_directory(&root, &db_path, "default", "test").unwrap();
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let v_uid = vault_uid("default", &root.to_string_lossy());
        let watcher = BrainWatcher::new(&db_path, &root, "default", "test");
        let before = store.count_wikilink_edges().unwrap();
        assert_eq!(before, 2);
        fs::write(
            root.join("Beta.md"),
            "# Beta\n\nnew paragraph\n\n## Details\n\nnew body\n",
        )
        .unwrap();
        watcher
            .process_batch(&store, None, &v_uid, vec![root.join("Beta.md")], &None)
            .unwrap();
        assert_eq!(store.count_wikilink_edges().unwrap(), before);
        let heading = store
            .headings_in_note(&note_uid(&v_uid, "Beta.md"))
            .unwrap()
            .into_iter()
            .find(|h| h.slug == "details")
            .unwrap();
        let links = store
            .wikilink_edges_for_vault(&v_uid, "WIKILINK_TO_HEADING", "dst:Heading")
            .unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].1, heading.uid);
    }

    #[test]
    fn watched_target_creation_deletion_and_batch_links_match_full_refresh() {
        let (_dir, root) = make_vault(&[("Alpha.md", "# Alpha\n\n[[Beta]]\n")]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        crate::index_md::index_markdown_directory(&root, &db_path, "default", "test").unwrap();
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let v_uid = vault_uid("default", &root.to_string_lossy());
        let watcher = BrainWatcher::new(&db_path, &root, "default", "test");
        assert_eq!(store.all_unresolved_wikilinks().unwrap().len(), 1);
        fs::write(root.join("Beta.md"), "# Beta\n\n[[Alpha]]\n").unwrap();
        watcher
            .process_batch(&store, None, &v_uid, vec![root.join("Beta.md")], &None)
            .unwrap();
        assert_eq!(store.count_wikilink_edges().unwrap(), 2);
        assert!(store.all_unresolved_wikilinks().unwrap().is_empty());
        fs::remove_file(root.join("Beta.md")).unwrap();
        watcher
            .process_batch(&store, None, &v_uid, vec![root.join("Beta.md")], &None)
            .unwrap();
        assert_eq!(store.count_wikilink_edges().unwrap(), 0);
        assert_eq!(store.all_unresolved_wikilinks().unwrap().len(), 1);
        fs::write(root.join("Alpha.md"), "# Alpha\n\n[[Beta#Moved]]\n").unwrap();
        fs::write(root.join("Beta.md"), "# Beta\n\n## Moved\n\n[[Alpha]]\n").unwrap();
        watcher
            .process_batch(
                &store,
                None,
                &v_uid,
                vec![root.join("Alpha.md"), root.join("Beta.md")],
                &None,
            )
            .unwrap();
        let fresh_path = db_dir.path().join("fresh.lbug");
        crate::index_md::index_markdown_directory(&root, &fresh_path, "default", "test").unwrap();
        let fresh = GraphStore::open_or_create(&fresh_path).unwrap();
        for (rel, dst) in [
            ("WIKILINK_TO_NOTE", "dst:Note"),
            ("WIKILINK_TO_HEADING", "dst:Heading"),
        ] {
            assert_eq!(
                store.wikilink_edges_for_vault(&v_uid, rel, dst).unwrap(),
                fresh.wikilink_edges_for_vault(&v_uid, rel, dst).unwrap()
            );
        }
        assert_eq!(
            store.all_unresolved_wikilinks().unwrap(),
            fresh.all_unresolved_wikilinks().unwrap()
        );
    }

    #[test]
    fn watched_heading_rename_and_membership_preservation() {
        let (_dir, root) = make_vault(&[
            ("Alpha.md", "# Alpha\n\n[[Beta#Details]]\n"),
            ("Beta.md", "# Beta\n\n## Details\n\n#shared\n"),
            ("Other.md", "# Other\n\n#shared\n"),
        ]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        crate::index_md::index_markdown_directory(&root, &db_path, "default", "test").unwrap();
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let v_uid = vault_uid("default", &root.to_string_lossy());
        let beta = note_uid(&v_uid, "Beta.md");
        let project = nestweaver_schema::Project {
            uid: "proj:watched".into(),
            name: "watched".into(),
            summary: None,
            instance_id: "default".into(),
        };
        store.insert_project(&project).unwrap();
        store
            .batch_insert_project_note_edges(&[(&project.uid, &beta)])
            .unwrap();
        let other = store.lookup_note(&note_uid(&v_uid, "Other.md")).unwrap();
        let watcher = BrainWatcher::new(&db_path, &root, "default", "test");
        fs::write(
            root.join("Beta.md"),
            "# Beta\n\n## Renamed\n\n#shared #new\n",
        )
        .unwrap();
        watcher
            .process_batch(&store, None, &v_uid, vec![root.join("Beta.md")], &None)
            .unwrap();
        assert_eq!(
            store.list_project_note_uids(&project.uid).unwrap(),
            vec![beta]
        );
        assert_eq!(
            store.lookup_note(&other.uid).unwrap().content_hash,
            other.content_hash
        );
        let fresh_path = db_dir.path().join("fresh.lbug");
        crate::index_md::index_markdown_directory(&root, &fresh_path, "default", "test").unwrap();
        let fresh = GraphStore::open_or_create(&fresh_path).unwrap();
        for (rel, dst) in [
            ("WIKILINK_TO_NOTE", "dst:Note"),
            ("WIKILINK_TO_HEADING", "dst:Heading"),
        ] {
            assert_eq!(
                store.wikilink_edges_for_vault(&v_uid, rel, dst).unwrap(),
                fresh.wikilink_edges_for_vault(&v_uid, rel, dst).unwrap()
            );
        }
        assert_eq!(
            store.all_unresolved_wikilinks().unwrap(),
            fresh.all_unresolved_wikilinks().unwrap()
        );
    }

    #[test]
    fn watched_planning_failure_preserves_graph_and_does_not_acknowledge_publication() {
        let (_dir, root) = make_vault(&[
            ("Alpha.md", "# Alpha\n\n[[Beta]]\n"),
            ("Beta.md", "# Beta\n\nold\n"),
        ]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        crate::index_md::index_markdown_directory(&root, &db_path, "default", "test").unwrap();
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let v_uid = vault_uid("default", &root.to_string_lossy());
        let beta = note_uid(&v_uid, "Beta.md");
        let before = store.lookup_note(&beta).unwrap().content_hash;
        let callbacks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = callbacks.clone();
        let callback: Option<Box<dyn Fn() + Send>> = Some(Box::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
        }));
        fs::write(root.join("Beta.md"), "# Beta\n\nnew\n").unwrap();
        // The unchanged incoming-link source cannot be read safely. Planning
        // must stop before deleting the edited target or any of its edges.
        fs::write(root.join("Alpha.md"), [0x00, 0xff, 0xfe]).unwrap();
        let watcher = BrainWatcher::new(&db_path, &root, "default", "test");
        assert!(
            watcher
                .process_batch(&store, None, &v_uid, vec![root.join("Beta.md")], &callback)
                .is_err()
        );
        assert_eq!(store.lookup_note(&beta).unwrap().content_hash, before);
        assert_eq!(store.count_wikilink_edges().unwrap(), 1);
        assert_eq!(callbacks.load(Ordering::SeqCst), 0);
        assert!(crate::sidecar_path(&db_path, ".index-dirty").exists());
        // Restore the affected source and make the indexed target oversized.
        // The same fail-before-delete contract applies to policy refusals.
        fs::write(root.join("Alpha.md"), "# Alpha\n\n[[Beta]]\n").unwrap();
        fs::write(
            root.join("Beta.md"),
            vec![b'x'; crate::index_md::MAX_NOTE_SIZE_BYTES as usize + 1],
        )
        .unwrap();
        assert!(
            watcher
                .process_batch(&store, None, &v_uid, vec![root.join("Beta.md")], &callback)
                .is_err()
        );
        assert_eq!(store.lookup_note(&beta).unwrap().content_hash, before);
        assert_eq!(store.count_wikilink_edges().unwrap(), 1);
        assert_eq!(callbacks.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn watched_batch_can_share_new_tags_and_delete_last_note() {
        let (_dir, root) =
            make_vault(&[("A.md", "# A\n\n#newtag\n"), ("B.md", "# B\n\n#newtag\n")]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let v_uid = vault_uid("default", &root.to_string_lossy());
        let watcher = BrainWatcher::new(&db_path, &root, "default", "test");
        let paths = vec![root.join("A.md"), root.join("B.md")];
        watcher
            .process_batch(&store, None, &v_uid, paths.clone(), &None)
            .unwrap();
        assert_eq!(store.note_tag_sets().unwrap().len(), 2);
        for path in &paths {
            fs::remove_file(path).unwrap();
        }
        watcher
            .process_batch(&store, None, &v_uid, paths, &None)
            .unwrap();
        assert!(store.list_notes(Some(&v_uid)).unwrap().is_empty());
    }

    #[test]
    fn watcher_subscription_failure_and_prestart_stop_never_signal_ready() {
        let (_dir, root) = make_vault(&[]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        let store = Arc::new(GraphStore::open_or_create(&db_path).unwrap());
        for stopped in [false, true] {
            let ready = Arc::new(AtomicBool::new(false));
            let signalled = ready.clone();
            let path = if stopped {
                root.clone()
            } else {
                root.join("missing")
            };
            let watcher = BrainWatcher::new(&db_path, &path, "default", "test")
                .with_ready_callback(move || {
                    signalled.store(true, Ordering::SeqCst);
                });
            if stopped {
                watcher.shutdown_handle().stop();
            }
            assert!(watcher.run_with_store(store.clone(), None).is_err());
            assert!(!ready.load(Ordering::SeqCst));
        }
    }

    struct FailingPageRankRetirementIo;

    impl crate::index::IndexEpilogueIo for FailingPageRankRetirementIo {
        fn establish_marker(&self, path: &Path) -> Result<(), anyhow::Error> {
            crate::index::FileSystemIndexEpilogueIo.establish_marker(path)
        }

        fn clear_marker(&self, path: &Path) -> Result<(), anyhow::Error> {
            crate::index::FileSystemIndexEpilogueIo.clear_marker(path)
        }

        fn remove_file(&self, _path: &Path) -> std::io::Result<()> {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected watcher PageRank remove failure",
            ))
        }

        fn rename_file(&self, _from: &Path, _to: &Path) -> std::io::Result<()> {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected watcher PageRank quarantine failure",
            ))
        }

        fn save_generation(
            &self,
            store: &GraphStore,
            path: &Path,
            generation: u64,
        ) -> Result<(), anyhow::Error> {
            crate::index::FileSystemIndexEpilogueIo.save_generation(store, path, generation)
        }

        fn compute_pagerank(
            &self,
            lease: &nestweaver_store::IndexPublicationLease<'_>,
            scope: &GraphScope,
        ) -> Result<(), anyhow::Error> {
            crate::index::FileSystemIndexEpilogueIo.compute_pagerank(lease, scope)
        }

        fn save_pagerank(
            &self,
            lease: &nestweaver_store::IndexPublicationLease<'_>,
            path: &Path,
        ) -> Result<(), anyhow::Error> {
            crate::index::FileSystemIndexEpilogueIo.save_pagerank(lease, path)
        }
    }

    #[test]
    fn watcher_pagerank_retirement_failure_keeps_reopen_fail_closed() {
        let (dir, root) = make_vault(&[]);
        let db_path = dir.path().join("brain.lbug");
        let generation_path = crate::sidecar_path(&db_path, ".generation");
        let pagerank_path = crate::sidecar_path(&db_path, ".pagerank.json");
        let marker_path = crate::sidecar_path(&db_path, ".index-dirty");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        store.bump_graph_generation();
        store.save_graph_generation(&generation_path).unwrap();
        let stale_generation = store.graph_generation();
        store
            .compute_pagerank(0.85, 20, &GraphScope::unified())
            .unwrap();
        store.save_pagerank_cache(&pagerank_path).unwrap();
        store.load_pagerank_cache(&pagerank_path).unwrap();
        let watcher = BrainWatcher::new(&db_path, &root, "test", "test");

        let publication = watcher
            .establish_graph_publication_with_io(&store, &crate::index::FileSystemIndexEpilogueIo)
            .unwrap();
        ensure_vault(&store, "vault:watcher-failure", &root, "test", "test").unwrap();
        let error = watcher
            .finalize_graph_publication_with_io(publication, &FailingPageRankRetirementIo)
            .unwrap_err();

        assert!(error.to_string().contains("persisted-pagerank"));
        assert!(marker_path.exists());
        assert!(pagerank_path.exists());
        drop(store);

        let reopened = GraphStore::open_or_create(&db_path).unwrap();
        assert_ne!(reopened.graph_generation(), stale_generation);
        reopened.load_pagerank_cache(&pagerank_path).unwrap();
        assert!(reopened.pagerank_scores().is_err());
    }

    #[test]
    fn canonical_manifest_watch_update_retires_legacy_sidecar() {
        let (_dir, root) = make_vault(&[(
            "package.json",
            r#"{"name":"watched-package","dependencies":{"watched-dep":"1"}}"#,
        )]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let legacy_path = db_path.with_extension("manifests.json");
        crate::save_manifest_cache(&HashMap::new(), &legacy_path).unwrap();
        let canonical_path = crate::manifest_cache_path(&db_path);
        let watcher = BrainWatcher::new(&db_path, &root, "default", "test")
            .with_manifests_path(&canonical_path);

        watcher
            .handle_non_graph_event(&store, root.join("package.json"))
            .unwrap();

        assert!(canonical_path.exists());
        assert!(!legacy_path.exists());
        let manifests = crate::load_manifest_cache_for_db(&store, &db_path).unwrap();
        assert_eq!(manifests.len(), 1);
        assert_eq!(
            manifests.values().next().unwrap().package_name.as_deref(),
            Some("watched-package")
        );
    }

    /// nw-298, the UNSTABLE half: `brain_search`'s ability to find a
    /// frontmatter-only string was an accident of the cold full-reindex path,
    /// which reads the whole file off disk. This incremental path rebuilds the
    /// BM25 body from SECTION text, and sections are cut from the body after
    /// frontmatter is split off — so a note's frontmatter silently dropped out
    /// of search on the first watcher-driven re-index. Same query, different
    /// answer depending on when it was asked, and invisible from the CLI.
    #[test]
    fn watcher_reindex_keeps_frontmatter_searchable() {
        let _guard = serial_watcher_test();
        let (_dir, root) = make_vault(&[(
            "backlog.md",
            "---\nid: nw-231\nnote: saves-and-exits on device\n---\n# Backlog\n\nBody text only.\n",
        )]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let tantivy_path = db_dir.path().join("tantivy");
        let tantivy = TantivyIndex::open_or_create(&tantivy_path).unwrap();

        let watcher = BrainWatcher::new(&db_path, &root, "default", "test");
        watcher
            .process_batch(
                &store,
                Some(&tantivy),
                &vault_uid("default", &root.to_string_lossy()),
                vec![root.join("backlog.md")],
                &None,
            )
            .unwrap();

        assert!(
            !tantivy.search("Backlog", 10).unwrap().is_empty(),
            "precondition: the note itself is indexed"
        );
        assert!(
            !tantivy.search("saves-and-exits", 10).unwrap().is_empty(),
            "a frontmatter-only string must survive an incremental re-index — \
             the cold path finds it by reading the file off disk, so without \
             this the two paths disagree over time (nw-298)"
        );
    }

    #[test]
    fn skip_dir_detection() {
        let p = Path::new("/x/vault/.obsidian/workspace.json");
        assert!(path_in_skip_dir(p));
        let p = Path::new("/x/vault/.git/HEAD");
        assert!(path_in_skip_dir(p));
        let p = Path::new("/x/vault/notes/regular.md");
        assert!(!path_in_skip_dir(p));
    }

    /// nw-380: before this fix, `run_inner` acquired ONE `watch_vault_batch`
    /// lease and held it across establishing the publication marker, every
    /// file transaction in the batch, AND finalizing publication. This
    /// drives `process_batch` directly (real fs-event timing is flaky in
    /// tests — see the `#[ignore]`d cases above) with a counting lease
    /// factory, so the acquisition COUNT is the deterministic signal: a
    /// 3-file graph batch must acquire the lease 5 times (establish + one
    /// per file + finalize), not once for the whole batch.
    #[test]
    fn nw_380_batch_lease_is_acquired_per_file_not_once_for_the_whole_batch() {
        use std::sync::atomic::AtomicU32;

        let _guard = serial_watcher_test();
        let (_dir, root) = make_vault(&[
            ("a.md", "# A\n\nAlpha body.\n"),
            ("b.md", "# B\n\nBravo body.\n"),
            ("c.md", "# C\n\nCharlie body.\n"),
        ]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();

        let acquisitions = Arc::new(AtomicU32::new(0));
        let counted = Arc::clone(&acquisitions);
        let factory: WatchMutationLeaseFactory = Arc::new(move |_label: &'static str| {
            counted.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(()) as Box<dyn WatchMutationLease>)
        });
        let watcher =
            BrainWatcher::new(&db_path, &root, "test", "test").with_mutation_lease_factory(factory);

        watcher
            .process_batch(
                &store,
                None,
                "vlt:test",
                vec![root.join("a.md"), root.join("b.md"), root.join("c.md")],
                &None,
            )
            .unwrap();

        assert!(
            acquisitions.load(Ordering::SeqCst) >= 5,
            "expected establish(1) + one per file(3) + finalize(1) = 5 \
             separate acquisitions, not one held across the whole batch"
        );
    }

    /// nw-380: `receive_debounced_paths` reset `quiet_deadline` on every
    /// arrival with no ceiling, so a steady trickle of saves could extend
    /// one debounced batch indefinitely — the mechanism behind the 453s (and
    /// production's 69-minute) write-gate hold. `quiet_period` here is
    /// deliberately far larger than `WATCH_BATCH_MAX_AGE`: under the
    /// pre-nw-380 design this call would not return until the sending
    /// thread stops AND `quiet_period` elapses with nothing further
    /// arriving — i.e. not before the sender's own ~7s run finishes, plus
    /// the 10s quiet period on top. The age cap must force a hand-off long
    /// before that.
    #[test]
    fn nw_380_receive_debounced_paths_caps_total_batch_age_under_sustained_events() {
        let (tx, rx) = std::sync::mpsc::channel::<RawWatchResult>();
        let stop_flag = AtomicBool::new(false);
        let sender = thread::spawn(move || {
            for i in 0..70u32 {
                thread::sleep(Duration::from_millis(100));
                if tx
                    .send(Ok(vec![PathBuf::from(format!("/tmp/nw380-{i}.md"))]))
                    .is_err()
                {
                    break;
                }
            }
        });

        let started = Instant::now();
        let result = receive_debounced_paths(&rx, Duration::from_secs(10), &stop_flag);
        let elapsed = started.elapsed();
        // Drop the receiver now so the sender's remaining sends fail fast
        // instead of running its full ~7s before the thread can be joined.
        drop(rx);

        let batch_len = match result {
            WatchReceive::Batch(paths) => paths.len(),
            WatchReceive::Timeout => panic!("expected a batch, got a bare timeout"),
            WatchReceive::NotifyError(_) => panic!("expected a batch, got a notify error"),
            WatchReceive::Disconnected => panic!("expected a batch, got disconnected"),
            WatchReceive::Stop => panic!("expected a batch, got stop"),
        };
        assert!(
            elapsed < Duration::from_secs(6),
            "WATCH_BATCH_MAX_AGE must force a hand-off around 5s regardless \
             of the 10s quiet_period continuously resetting under sustained \
             events: took {elapsed:?}"
        );
        assert!(
            batch_len > 0,
            "the capped-off batch must still carry whatever paths arrived \
             before the age ceiling, not discard them"
        );
        let _ = sender.join();
    }

    #[test]
    fn raw_event_filter_rejects_access_and_non_content_metadata() {
        use notify::event::{AccessKind, DataChange, MetadataKind, ModifyKind, RenameMode};

        assert!(!event_kind_can_mutate(&EventKind::Access(AccessKind::Read)));
        assert!(!event_kind_can_mutate(&EventKind::Modify(
            ModifyKind::Metadata(MetadataKind::AccessTime)
        )));
        assert!(!event_kind_can_mutate(&EventKind::Modify(
            ModifyKind::Metadata(MetadataKind::Permissions)
        )));
        assert!(event_kind_can_mutate(&EventKind::Modify(
            ModifyKind::Metadata(MetadataKind::WriteTime)
        )));
        assert!(event_kind_can_mutate(&EventKind::Modify(ModifyKind::Data(
            DataChange::Content
        ))));
        assert!(event_kind_can_mutate(&EventKind::Modify(ModifyKind::Name(
            RenameMode::Both
        ))));
        assert!(event_kind_can_mutate(&EventKind::Any));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn one_vault_edit_settles_after_one_hot_batch() {
        use std::sync::atomic::AtomicUsize;
        use std::time::Instant;

        let _guard = serial_watcher_test();
        let (_dir, root) = make_vault(&[("note.md", "# Original\n\nbody\n")]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        crate::index_md::index_markdown_directory(&root, &db_path, "default", "test").unwrap();
        let store = Arc::new(GraphStore::open_or_create(&db_path).unwrap());
        let notifications = Arc::new(AtomicUsize::new(0));
        let callback_count = notifications.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let watcher =
            BrainWatcher::new(&db_path, &root, "default", "test").with_ready_signal(ready_tx);
        let stop = watcher.shutdown_handle();
        let handle = thread::spawn(move || {
            watcher.run_with_store(
                store,
                Some(Box::new(move || {
                    callback_count.fetch_add(1, Ordering::SeqCst);
                })),
            )
        });
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("watcher should start");
        assert_eq!(notifications.load(Ordering::SeqCst), 1);

        fs::write(root.join("note.md"), "# Updated\n\nchanged once\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while notifications.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
        }
        assert_eq!(
            notifications.load(Ordering::SeqCst),
            2,
            "the real edit must publish one hot batch"
        );
        thread::sleep(Duration::from_millis(1200));
        assert_eq!(
            notifications.load(Ordering::SeqCst),
            2,
            "parser reads must not feed another watcher batch"
        );

        stop.stop();
        handle.join().unwrap().unwrap();
    }

    // Integration-style tests below exercise the live event loop. They
    // depend on platform file-event delivery timing (FSEvents on macOS
    // has a ~500ms+ floor; inotify on Linux is faster) so they are
    // marked #[ignore] and skipped from the default suite. Run them
    // explicitly with `cargo test --lib watcher -- --ignored` when
    // working on the watcher itself.

    #[test]
    #[ignore = "depends on platform fs-event timing"]
    fn watcher_picks_up_a_new_file() {
        let _guard = serial_watcher_test();
        let (_dir, root) = make_vault(&[("seed.md", "# Seed\n\nbody\n")]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");

        // Seed the DB by indexing once so the Vault node is set up.
        crate::index_md::index_markdown_directory(&root, &db_path, "default", "test").unwrap();

        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let watcher =
            BrainWatcher::new(&db_path, &root, "default", "test").with_ready_signal(ready_tx);
        let stop = watcher.shutdown_handle();
        let handle = thread::spawn(|| watcher.run());
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("watcher should start");

        let new_path = root.join("just-added.md");
        fs::write(&new_path, "# Just Added\n\nfresh content\n").unwrap();

        // Wait for the debounce window + processing.
        thread::sleep(Duration::from_millis(700));
        stop.stop();
        handle.join().unwrap().unwrap();

        // The new note should be in the DB.
        let store = GraphStore::open(&db_path).unwrap();
        let notes = store.list_notes(None).unwrap();
        let titles: Vec<&str> = notes.iter().map(|n| n.title.as_str()).collect();
        assert!(
            titles.contains(&"Just Added"),
            "expected 'Just Added' to be indexed; got {titles:?}"
        );
    }

    #[test]
    #[ignore = "depends on platform fs-event timing"]
    fn watcher_handles_modify_via_cascade_delete_then_reinsert() {
        let _guard = serial_watcher_test();
        let (_dir, root) = make_vault(&[("note.md", "# Original Title\n\nbody\n")]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");

        crate::index_md::index_markdown_directory(&root, &db_path, "default", "test").unwrap();

        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let watcher =
            BrainWatcher::new(&db_path, &root, "default", "test").with_ready_signal(ready_tx);
        let stop = watcher.shutdown_handle();
        let handle = thread::spawn(|| watcher.run());
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("watcher should start");
        fs::write(
            root.join("note.md"),
            "# Renamed Title\n\nmore body\n\n## New Heading\n\nmore\n",
        )
        .unwrap();
        thread::sleep(Duration::from_millis(700));
        stop.stop();
        handle.join().unwrap().unwrap();

        let store = GraphStore::open(&db_path).unwrap();
        let notes = store.list_notes(None).unwrap();
        assert_eq!(notes.len(), 1, "should still be one note");
        assert_eq!(notes[0].title, "Renamed Title");
        // Original had 1 heading; new has 2.
        assert!(store.count_headings().unwrap() >= 2);
    }

    #[test]
    #[ignore = "depends on platform fs-event timing"]
    fn watcher_handles_delete() {
        let _guard = serial_watcher_test();
        let (_dir, root) = make_vault(&[("keep.md", "# Keep\n"), ("doomed.md", "# Doomed\n")]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");

        crate::index_md::index_markdown_directory(&root, &db_path, "default", "test").unwrap();
        assert_eq!(
            GraphStore::open(&db_path).unwrap().count_notes().unwrap(),
            2
        );

        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let watcher =
            BrainWatcher::new(&db_path, &root, "default", "test").with_ready_signal(ready_tx);
        let stop = watcher.shutdown_handle();
        let handle = thread::spawn(|| watcher.run());
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("watcher should start");
        fs::remove_file(root.join("doomed.md")).unwrap();
        thread::sleep(Duration::from_millis(700));
        stop.stop();
        handle.join().unwrap().unwrap();

        let store = GraphStore::open(&db_path).unwrap();
        let titles: Vec<String> = store
            .list_notes(None)
            .unwrap()
            .into_iter()
            .map(|n| n.title)
            .collect();
        assert_eq!(titles.len(), 1, "doomed.md should be gone");
        assert_eq!(titles[0], "Keep");
    }

    #[test]
    #[ignore = "depends on platform fs-event timing"]
    fn watcher_ignores_files_in_obsidian_dir() {
        let _guard = serial_watcher_test();
        let (_dir, root) = make_vault(&[(".obsidian/config.json", "{}")]);
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("brain.lbug");
        crate::index_md::index_markdown_directory(&root, &db_path, "default", "test").unwrap();
        let before = GraphStore::open(&db_path).unwrap().count_notes().unwrap();

        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let watcher =
            BrainWatcher::new(&db_path, &root, "default", "test").with_ready_signal(ready_tx);
        let stop = watcher.shutdown_handle();
        let handle = thread::spawn(|| watcher.run());
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("watcher should start");
        fs::write(root.join(".obsidian/note-in-config.md"), "# X\n").unwrap();
        thread::sleep(Duration::from_millis(700));
        stop.stop();
        handle.join().unwrap().unwrap();

        let after = GraphStore::open(&db_path).unwrap().count_notes().unwrap();
        assert_eq!(before, after, ".obsidian/ files must not be indexed");
    }
}
