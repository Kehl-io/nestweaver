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
use notify::Event;

use crate::content_reader::ContentReader;
use crate::index::is_minified_or_bundled;
use crate::watch_tree::TreeWatch;
use crate::watcher::{
    RawWatchResult, ShutdownHandle, WatchMutationLease, WatchMutationLeaseFactory,
    WatchMutationRefused, WatchReceive, event_kind_can_mutate, receive_debounced_paths,
};

/// Live file-watcher for a code repository. Construct via `new`, then
/// call `run` — it blocks until `stop()` is signalled or the watcher
/// hits a fatal error.
pub struct CodeWatcher {
    db_path: PathBuf,
    repo_root: PathBuf,
    instance_id: String,
    stop_flag: Arc<AtomicBool>,
    mutation_lease_factory: Option<WatchMutationLeaseFactory>,
    debounce: Duration,
    limits: crate::index_limits::IndexLimits,
    instance_config: Option<Arc<crate::InstanceConfig>>,
    ready_callback: Option<Box<dyn FnOnce() + Send>>,
    /// nw-664: first delay before retrying a failed startup reconciliation;
    /// doubles per failure up to [`crate::watcher::RECONCILE_RETRY_CAP`].
    reconcile_retry_base: Duration,
    /// nw-651 on Linux: directories the filesystem subscription could not
    /// cover (`watch_tree`), disclosed with the rest of this repo's debt.
    unwatched_dirs: Vec<(PathBuf, String)>,
    #[cfg(test)]
    ready_signal: Option<std::sync::mpsc::Sender<()>>,
    /// Test seam: subscribe as inotify would fail on an unreadable subtree.
    #[cfg(test)]
    emulate_inotify_watch: bool,
}

#[derive(Debug)]
enum WatchBatchOutcome {
    Unchanged,
    ManifestPending,
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
            mutation_lease_factory: None,
            debounce: Duration::from_secs(2),
            limits: crate::index_limits::IndexLimits::default(),
            instance_config: None,
            ready_callback: None,
            reconcile_retry_base: crate::watcher::RECONCILE_RETRY_BASE,
            unwatched_dirs: Vec::new(),
            #[cfg(test)]
            ready_signal: None,
            #[cfg(test)]
            emulate_inotify_watch: false,
        }
    }

    /// Called once startup has subscribed to the filesystem, published any
    /// cold snapshot and attempted the startup reconciliation (nw-664), so
    /// "ready" normally means the graph matches disk. A failed reconciliation
    /// does not withhold readiness: it is disclosed and retried. Dropping the
    /// watcher without invoking it means startup failed or was cancelled.
    pub fn with_ready_callback(mut self, ready: impl FnOnce() + Send + 'static) -> Self {
        self.ready_callback = Some(Box::new(ready));
        self
    }

    /// Test hook: make the recursive subscription fail the way Linux inotify
    /// does on an unreadable subdirectory (see `watch_tree`).
    /// Linux tests use real inotify instead, so the hook is unused there.
    #[cfg(test)]
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    fn emulating_inotify_watch(mut self) -> Self {
        self.emulate_inotify_watch = true;
        self
    }

    /// Test hook: shorten the startup-reconciliation retry backoff (nw-664).
    #[cfg(test)]
    fn with_reconcile_retry_base(mut self, base: Duration) -> Self {
        self.reconcile_retry_base = base;
        self
    }

    pub fn with_limits(mut self, limits: crate::index_limits::IndexLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn with_instance_config(mut self, config: Option<Arc<crate::InstanceConfig>>) -> Self {
        self.instance_config = config;
        self
    }

    fn reader_for(
        &self,
        repo_url: &str,
    ) -> anyhow::Result<crate::content_reader::FilesystemReader> {
        let mut reader =
            crate::content_reader::FilesystemReader::with_limits(&self.repo_root, self.limits);
        if let Some(config) = &self.instance_config {
            reader = reader
                .excluding(config.exclude_globs_for(repo_url, Some(&self.repo_root)))?
                .unskipping(config.unskip_names_for(repo_url, Some(&self.repo_root)));
        }
        Ok(reader)
    }

    // Used only by `one_code_edit_settles_after_one_hot_batch`, which is
    // `#[cfg(target_os = "linux")]` because it depends on inotify coalescing
    // behaviour. macOS clippy therefore sees these as dead and, under
    // `-D warnings`, they were DELETED in 01c585b8 — which broke the Linux
    // build outright. The platform gate has to match the test's, not the
    // platform the lint happened to run on.
    // nw-664 review: the live-disclosure tests use it on every unix too, so
    // the gate follows theirs (a superset of the Linux test's).
    #[cfg(all(test, unix))]
    fn with_debounce_ms(mut self, debounce_ms: u64) -> Self {
        self.debounce = Duration::from_millis(debounce_ms);
        self
    }

    #[cfg(all(test, target_os = "linux"))]
    fn with_ready_signal(mut self, ready: std::sync::mpsc::Sender<()>) -> Self {
        self.ready_signal = Some(ready);
        self
    }

    /// Install an external RAII lease acquired once around each published
    /// mutation batch, including the inline after-change callback.
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
        let authority =
            nestweaver_store::acquire_db_write_lease(&self.db_path).map_err(|error| {
                anyhow::anyhow!("cannot start code watcher without writer authority: {error:?}")
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
        mut self,
        store: Arc<GraphStore>,
        on_change: Option<Box<dyn Fn() + Send>>,
    ) -> Result<(), anyhow::Error> {
        // Register before inspecting or cold-indexing the tree. Events that
        // race the initial snapshot are queued by the debouncer and replayed
        // below, closing the former scan-then-watch lost-event window.
        let (tx, rx) = std::sync::mpsc::channel::<RawWatchResult>();
        let watcher =
            notify::recommended_watcher(move |result: Result<Event, notify::Error>| match result {
                Ok(event) if event_kind_can_mutate(&event.kind) => {
                    let _ = tx.send(Ok(event.paths));
                }
                Ok(_) => {}
                Err(error) => {
                    let _ = tx.send(Err(error));
                }
            })
            .context("init code filesystem watcher")?;
        // nw-651 on Linux: an unreadable subdirectory must not stop the whole
        // subscription; it is skipped and disclosed instead (`watch_tree`).
        #[cfg(test)]
        let tree = if self.emulate_inotify_watch {
            TreeWatch::start_emulating_inotify(watcher, &self.repo_root)
        } else {
            TreeWatch::start(watcher, &self.repo_root)
        };
        #[cfg(not(test))]
        let tree = TreeWatch::start(watcher, &self.repo_root);
        let mut tree = tree.with_context(|| format!("watch {}", self.repo_root.display()))?;
        #[cfg(test)]
        if let Some(ready) = &self.ready_signal {
            let _ = ready.send(());
        }

        // Identity decision (see `resolve_watch_identity`): adopt an existing
        // `file://` graph rather than re-identifying+pruning, so a watch-first
        // start over a legacy DB never empties the graph.
        let (repo_url, r_uid) = resolve_watch_identity(&store, &self.instance_id, &self.repo_root)?;
        // Always, so a previous run's rows for directories that are now
        // watchable are cleared (no write when nothing changed).
        self.sync_unwatched_dirs(&repo_url, tree.unwatchable(), true);
        let mut subscribed_unwatchable = tree.unwatchable().to_vec();

        // A contract plan is a whole-repo view and must never point at
        // unchanged controllers that a minimal watch-first graph omitted.
        // Cold watchers therefore publish an authoritative initial source +
        // contract snapshot through the exact same atomic batch seam.
        let cold = store.lookup_repo(&r_uid)?.is_none();
        if cold {
            loop {
                let reader = self.reader_for(&repo_url)?;
                let initial_paths: Vec<PathBuf> = reader
                    .list_files()
                    .context("list files for initial watcher snapshot")?
                    .into_iter()
                    .map(|path| self.repo_root.join(path))
                    .filter(|path| is_watcher_input(path))
                    .collect();
                let _mutation_lease = match self.acquire_mutation_lease("watch_code_initial") {
                    Ok(lease) => lease,
                    Err(error) if error.downcast_ref::<WatchMutationRefused>().is_some() => {
                        tracing::info!("CodeWatcher startup refused during shutdown; exiting");
                        return Ok(());
                    }
                    Err(error) => {
                        return Err(error.context("acquire code watcher startup lease"));
                    }
                };
                // Another admitted writer may have completed the authoritative
                // snapshot while this watcher waited for the gate.
                if store.lookup_repo(&r_uid)?.is_some() {
                    break;
                }
                match self.process_batch_and_notify(
                    &store,
                    &r_uid,
                    &repo_url,
                    &initial_paths,
                    &crate::index::FileSystemIndexEpilogueIo,
                    on_change.as_deref().map(|callback| callback as &dyn Fn()),
                )? {
                    WatchBatchOutcome::Published { .. }
                    | WatchBatchOutcome::Unchanged
                    | WatchBatchOutcome::ManifestPending => break,
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

        // nw-664: replay what changed while no watcher was listening. A cold
        // repo was just snapshotted whole, so only an indexed one needs it.
        // The notify subscription is already live, so a save racing this
        // scan is queued and replayed by the loop; overlap is harmless. Runs
        // before readiness so "ready" normally means the graph matches disk.
        // A failure does not stop live watching (a restart would only meet
        // the same failure): it is disclosed and retried below with backoff.
        let mut pending = None;
        if !cold {
            if self.stop_flag.load(Ordering::Acquire) {
                return Ok(());
            }
            pending = match self.attempt_reconciliation(
                &store,
                &r_uid,
                &repo_url,
                on_change.as_deref().map(|callback| callback as &dyn Fn()),
                0,
            ) {
                Ok(pending) => pending,
                Err(error) => {
                    tracing::info!(
                        "CodeWatcher startup reconciliation refused during shutdown; exiting: {error:#}"
                    );
                    return Ok(());
                }
            };
        }
        if let Some(ready) = self.ready_callback.take() {
            ready();
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
            // nw-664: retry an owed startup reconciliation once its backoff
            // elapses — on the receive-timeout tick when idle, between
            // batches when busy.
            if let Some(owed) = pending.take_if(|owed| Instant::now() >= owed.next_attempt) {
                match self.attempt_reconciliation(
                    &store,
                    &r_uid,
                    &repo_url,
                    on_change.as_deref().map(|callback| callback as &dyn Fn()),
                    owed.failures,
                ) {
                    Ok(next) => pending = next,
                    Err(_) => return Ok(()),
                }
            }

            let mut batch = if !replay_batch.is_empty() {
                std::mem::take(&mut replay_batch)
            } else {
                match receive_debounced_paths(&rx, self.debounce, &self.stop_flag) {
                    WatchReceive::Batch(paths) => paths,
                    WatchReceive::NotifyError(err) => {
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
                    WatchReceive::Timeout => {
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
                    WatchReceive::Disconnected => {
                        tracing::warn!("code debouncer disconnected; exiting");
                        return Ok(());
                    }
                    WatchReceive::Stop => {
                        tracing::info!("CodeWatcher stop requested; exiting");
                        return Ok(());
                    }
                }
            };

            // nw-651 on Linux: a directory created under a non-recursively
            // watched one needs its own watch, and its files may predate it.
            let adopted = tree.adopt_new_dirs(&batch);
            batch.extend(adopted);
            if tree.unwatchable() != subscribed_unwatchable.as_slice() {
                subscribed_unwatchable = tree.unwatchable().to_vec();
                self.sync_unwatched_dirs(&repo_url, &subscribed_unwatchable, false);
            }
            let _mutation_lease = match self.acquire_mutation_lease("watch_code_batch") {
                Ok(lease) => lease,
                Err(error) if error.downcast_ref::<WatchMutationRefused>().is_some() => {
                    tracing::info!("CodeWatcher batch refused during shutdown; exiting");
                    return Ok(());
                }
                Err(error) => return Err(error.context("acquire code watcher batch lease")),
            };
            let start = Instant::now();
            let outcome = self.process_batch_and_notify(
                &store,
                &r_uid,
                &repo_url,
                &batch,
                &crate::index::FileSystemIndexEpilogueIo,
                on_change.as_deref().map(|callback| callback as &dyn Fn()),
            )?;
            let files_processed = match outcome {
                WatchBatchOutcome::Published { files_processed } => {
                    // nw-664 review: these paths are now what the graph holds,
                    // so any disclosure about them is stale.
                    // An unwatched directory's row stays: a batch touching
                    // it (or an ancestor) says nothing about its watch.
                    let settled: Vec<PathBuf> = batch
                        .iter()
                        .filter(|path| {
                            !self
                                .unwatched_dirs
                                .iter()
                                .any(|(dir, _)| dir.starts_with(path))
                        })
                        .cloned()
                        .collect();
                    crate::index_md::clear_code_reconciliation_paths(
                        &self.db_path,
                        &self.repo_root,
                        &settled,
                    );
                    files_processed
                }
                WatchBatchOutcome::Unchanged | WatchBatchOutcome::ManifestPending => continue,
                WatchBatchOutcome::Skipped { reason } => {
                    // nw-669: this used to be the whole handling — a warning,
                    // then the batch was gone. Nothing retried it and nothing
                    // disclosed it, so with one unreadable contract-language
                    // source in the repo every live edit was silently lost.
                    // Hand it to the startup reconciliation's retry instead:
                    // an attempt recomputes disk-vs-graph drift (which holds
                    // these paths, since the graph never took them), replays
                    // it, and on failure discloses it as owed and retries on
                    // the shared backoff until it lands.
                    tracing::warn!(
                        error = %reason,
                        "code watcher batch skipped before publication; previous graph \
                         preserved; reconciling with retry"
                    );
                    // nw-669 review: only when no retry is owed yet. An owed
                    // retry keeps its schedule — its recomputed drift will
                    // include these paths — so in a permanently blocked repo
                    // an autosave costs one failing live batch, not also a
                    // full walk and a failing replay every time.
                    if pending.is_none() {
                        pending = Some(crate::watcher::PendingReconciliation {
                            paths: None,
                            also_replay: Vec::new(),
                            failures: 0,
                            next_attempt: Instant::now(),
                        });
                    }
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

    /// Expand only affected subtrees. Missing directories require the indexed
    /// inventory: filesystem metadata alone cannot recover their descendants.
    /// Keep indexed paths even when policy now excludes them so publication
    /// retracts stale coverage instead of silently leaving it behind.
    fn expand_event_paths(
        &self,
        store: &GraphStore,
        r_uid: &str,
        reader: &crate::content_reader::FilesystemReader,
        repo_url: &str,
        events: &[PathBuf],
    ) -> anyhow::Result<Vec<PathBuf>> {
        let mut paths = HashSet::new();
        let mut incumbent_paths = HashSet::new();
        let mut subtrees = Vec::new();
        for path in events.iter().collect::<HashSet<_>>() {
            let Ok(rel) = path.strip_prefix(&self.repo_root) else {
                continue;
            };
            if rel
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
            {
                continue;
            }
            let metadata = std::fs::symlink_metadata(path);
            match metadata {
                Ok(meta) if meta.is_dir() => subtrees.push(path.clone()),
                Ok(meta) if meta.is_file() => {
                    subtrees.push(path.clone());
                    if is_watcher_input(path) {
                        paths.insert(path.clone());
                    }
                }
                Ok(meta) if meta.file_type().is_symlink() => subtrees.push(path.clone()),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    subtrees.push(path.clone());
                    if is_watcher_input(path) {
                        paths.insert(path.clone());
                    }
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("inspect watcher event {}", path.display()));
                }
                _ => {}
            }
        }
        // Collapse overlapping directory notifications before walking. A
        // directory and its file notifications still publish each file once.
        subtrees.sort();
        subtrees.dedup();
        let mut roots: Vec<PathBuf> = Vec::new();
        for subtree in subtrees {
            if !roots.iter().any(|root| subtree.starts_with(root)) {
                roots.push(subtree);
            }
        }
        if !roots.is_empty() {
            for (_, rel) in store.list_files_by_repo(r_uid)? {
                let path = self.repo_root.join(&rel);
                if roots.iter().any(|root| path.starts_with(root)) && is_watcher_input(&path) {
                    incumbent_paths.insert(path.clone());
                    paths.insert(path);
                }
            }
        }
        // Specs need not have File nodes, but their declared contracts retain
        // the input path needed to retract a removed spec-only directory.
        if !roots.is_empty() {
            for contract in store.list_contracts(Some(r_uid))? {
                let path = self.repo_root.join(contract.source_path);
                if roots.iter().any(|root| path.starts_with(root)) && is_watcher_input(&path) {
                    incumbent_paths.insert(path.clone());
                    paths.insert(path);
                }
            }
        }
        for root in roots {
            let relative = root.strip_prefix(&self.repo_root)?;
            if path_has_symlink(&self.repo_root, relative)? || !reader.accepts_path(relative) {
                continue;
            }
            match std::fs::read_dir(&root) {
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    continue;
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("enumerate watched subtree {}", root.display()));
                }
            }
            let policy = self.reader_for(repo_url)?;
            let repo_root = self.repo_root.clone();
            for entry in ignore::WalkBuilder::new(&root)
                .follow_links(false)
                .hidden(false)
                .git_ignore(true)
                .git_global(true)
                .git_exclude(true)
                .filter_entry(move |entry| {
                    entry
                        .path()
                        .strip_prefix(&repo_root)
                        .is_ok_and(|rel| policy.accepts_path(rel))
                })
                .build()
            {
                let entry = entry.context("enumerate watched subtree")?;
                if entry.file_type().is_some_and(|kind| kind.is_file())
                    && is_watcher_input(entry.path())
                    && reader.accepts_path(entry.path().strip_prefix(&self.repo_root)?)
                {
                    paths.insert(entry.into_path());
                }
            }
        }
        paths.retain(|path| {
            incumbent_paths.contains(path)
                || path
                    .strip_prefix(&self.repo_root)
                    .is_ok_and(|rel| reader.accepts_path(rel))
        });
        let mut paths: Vec<_> = paths.into_iter().collect();
        paths.sort();
        Ok(paths)
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
        let reader = self.reader_for(repo_url)?;
        // Manifest edits do not require source parsing or symbol writes. Debt
        // must precede generation invalidation so interruption cannot leave a
        // valid-looking cache for old package inputs.
        let mut manifest_changed = false;
        for path in relevant {
            let Ok(relative) = path.strip_prefix(&self.repo_root) else {
                continue;
            };
            if crate::manifest::is_manifest_input(relative)
                && reader.accepts_path(relative)
                && !path_has_symlink(&self.repo_root, relative)?
                && manifest_path_not_gitignored(&self.repo_root, relative)?
            {
                manifest_changed = true;
            }
        }
        if manifest_changed {
            crate::manifest::mark_manifest_reconciliation_pending(
                &self.db_path,
                "code watcher manifest edit",
            )?;
            let publication = crate::manifest::begin_graph_mutation_publication(
                store,
                "manifest watcher invalidation",
            )?;
            let outcome = publication.finish(true)?;
            anyhow::ensure!(
                !outcome.is_degraded(),
                "manifest source edit committed with degraded generation publication: {:?}",
                outcome.warnings
            );
            tracing::info!(repo = %r_uid, "manifest source changed; complete daemon derivation pending");
        }
        let code_events: Vec<_> = relevant
            .iter()
            .filter(|path| {
                path.strip_prefix(&self.repo_root).is_ok_and(|rel| {
                    !crate::manifest::is_manifest_input(rel)
                        || is_supported_source(path)
                        || crate::contracts::is_spec_file(&path.to_string_lossy())
                })
            })
            .cloned()
            .collect();
        let relevant = match self.expand_event_paths(store, r_uid, &reader, repo_url, &code_events)
        {
            Ok(paths) => paths,
            Err(reason) => return Ok(WatchBatchOutcome::Skipped { reason }),
        };
        if relevant.is_empty() && !insert_initial_repo {
            return Ok(if manifest_changed {
                WatchBatchOutcome::ManifestPending
            } else {
                WatchBatchOutcome::Unchanged
            });
        }
        let contract_plan =
            match crate::index::prepare_watcher_contract_derivation(&reader, r_uid, repo_url) {
                Ok(plan) => plan,
                Err(reason) => return Ok(WatchBatchOutcome::Skipped { reason }),
            };
        for skipped in &contract_plan.skipped_files {
            tracing::warn!(
                path = %skipped.path,
                reason = ?skipped.reason_code,
                detail = %skipped.reason,
                "watcher published degraded coverage for a policy-skipped contract input"
            );
        }
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
            if let Some(reason) = source_policy_exclusion(&reader, &self.repo_root, path)? {
                tracing::warn!(
                    path = %rel_path.display(), reason,
                    "watched source is policy-excluded; removing stale graph coverage"
                );
                removed.insert(rel_str.clone());
                prepared_paths.push(PreparedPath::Delete { rel_path: rel_str });
                continue;
            }
            match std::fs::metadata(path) {
                Ok(metadata) if metadata.is_file() => {
                    let prepared = match prepare_code_file(&reader, rel_path, r_uid, repo_url) {
                        Ok(prepared) => prepared,
                        Err(reason)
                            if reason
                                .downcast_ref::<crate::content_reader::SourceTooLarge>()
                                .is_some()
                                || reason.downcast_ref::<UnparsableSource>().is_some() =>
                        {
                            tracing::warn!(
                                path = %rel_path.display(),
                                %reason,
                                "watched source is policy-skipped; removing stale graph coverage"
                            );
                            removed.insert(rel_str.clone());
                            prepared_paths.push(PreparedPath::Delete { rel_path: rel_str });
                            continue;
                        }
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
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    removed.insert(rel_str.clone());
                    prepared_paths.push(PreparedPath::Delete { rel_path: rel_str });
                }
                Ok(_) => {
                    removed.insert(rel_str.clone());
                    prepared_paths.push(PreparedPath::Delete { rel_path: rel_str });
                }
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

        if insert_initial_repo {
            GraphStore::set_repo_index_policy_on(&txn, r_uid, &reader.eligibility_fingerprint())
                .context("persist initial watcher source eligibility")?;
        }

        let mut files_processed = 0usize;
        // nw-204: every symbol UID this batch removes, so the epilogue can
        // tombstone the ones that do not come back. Both arms delete — a
        // Replace deletes the file's symbols before re-inserting them.
        let mut deleted_symbol_uids: Vec<String> = Vec::new();
        let mut deleted_symbol_files: Vec<String> = Vec::new();
        for prepared_path in &prepared_paths {
            match prepared_path {
                PreparedPath::Replace(prepared) => {
                    let (symbols, removed) = apply_prepared_code_file(&txn, r_uid, prepared)
                        .with_context(|| format!("apply watched file {}", prepared.rel_path))?;
                    deleted_symbol_uids.extend(removed);
                    deleted_symbol_files.push(prepared.rel_path.clone());
                    tracing::debug!(path = %prepared.rel_path, symbols, "re-indexed file");
                    files_processed += 1;
                }
                PreparedPath::Delete { rel_path } => {
                    let removed = nestweaver_store::GraphStore::delete_symbols_in_file_on(
                        &txn, r_uid, rel_path,
                    )
                    .with_context(|| format!("delete symbols for removed file {rel_path}"))?;
                    let f_uid = nestweaver_schema::file_uid(r_uid, rel_path);
                    nestweaver_store::GraphStore::delete_file_node_on(&txn, &f_uid)
                        .with_context(|| format!("delete removed File node {rel_path}"))?;
                    if !removed.is_empty() {
                        tracing::debug!(
                            path = %rel_path,
                            removed = removed.len(),
                            "deleted symbols for removed file"
                        );
                    }
                    deleted_symbol_uids.extend(removed);
                    deleted_symbol_files.push(rel_path.clone());
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

        nestweaver_store::GraphStore::mark_regex_scope_dirty_on(&txn, r_uid, false)
            .context("mark watched regex scope dirty")?;

        store
            .commit_transaction(&txn)
            .context("commit code watcher batch transaction")?;
        drop(txn);
        if let Err(error) = store.clear_contract_derivation_failed(r_uid) {
            tracing::warn!("clearing watcher contract derivation marker failed: {error}");
        }
        // After the commit: a Replace deletes and re-inserts the same UIDs, so
        // the pre-commit graph would report a whole saved file as dead.
        crate::index::tombstone_deleted_symbol_embeddings_after_commit(
            store,
            r_uid,
            &deleted_symbol_files,
            &deleted_symbol_uids,
            "code watcher batch",
        );
        self.finalize_graph_publication_with_io(publication, epilogue_io)
            .map_err(anyhow::Error::from)?;
        // nw-670 review M4: changed symbols change what notes' mentions
        // resolve to (and the cascade dropped links into the changed files):
        // owed to the code-link reconciler.
        crate::code_links::mark_code_links_pending(
            &self.db_path,
            &format!("code watcher batch in {repo_url}"),
        );
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

    /// nw-664: the sources whose graph state no longer matches disk, for the
    /// code watcher to replay as its first batch — the code twin of nw-653's
    /// `index_md::vault_startup_drift`.
    ///
    /// A watcher learns about changes only through events, and events that
    /// arrive while nothing listens (daemon down, laptop asleep, a crash) are
    /// gone. Only a COLD repo used to get a startup snapshot, so an indexed
    /// repo stayed stale until each file was touched again. Startup therefore
    /// diffs disk against the graph:
    ///
    /// - a source the graph has no File for (a lost create), unless it could
    ///   never be graphed: oversized or binary (the index skips and discloses
    ///   those) or recorded unindexable at this mtime by an earlier replay;
    /// - a graphed source whose content differs from the graph's
    ///   `content_hash` (a lost edit);
    /// - a graphed source whose file no longer exists (a lost delete). Only
    ///   inferred when the walk found files at all — an empty scan is an
    ///   unmounted or unreadable tree, not a deletion (nw-287) — and only on
    ///   `NotFound`, so a file under a directory the walk could not read
    ///   (nw-651: EACCES, or an execute-only dir it could stat) is kept.
    ///
    /// Cheap: ONE walk through the index's own `FilesystemReader` (SKIP_DIRS,
    /// gitignore, `[[repos]] exclude`), one `stat` per source against the
    /// `<db>.filemeta.json` cache the index keeps, and a read + BLAKE3 hash
    /// only where the stat moved (a file the watcher re-parsed since the last
    /// index has a stale filemeta entry, so the hash is compared with the
    /// GRAPH, which is what actually has to match). Nothing is parsed here.
    ///
    /// Why not `indexed_sha` vs `HEAD` plus `git status`: watcher batches do
    /// not advance `indexed_sha`, and the graph routinely holds uncommitted
    /// edits the watcher ingested, so a git diff answers "what changed since
    /// the last full index", not "what does the graph lack". It also would not
    /// cover non-git repos. The stat/hash comparison answers the right
    /// question for both.
    ///
    /// Scope: supported SOURCE files, the inputs that carry File nodes.
    ///
    /// nw-664 review: ONE bad file must not block the rest. A source that
    /// cannot be read (EACCES, an I/O fault) is NOT drift — replaying it made
    /// the batch skip every path, so the real lost edits beside it never
    /// landed and the replay retried forever. It is left as the graph has it
    /// (nw-651: unread is not deleted) and returned in `unreadable` for the
    /// caller to disclose; the next start reads it again. A graphed source
    /// turned binary is treated the same way, matching the live batch, which
    /// keeps a file's previous symbols when the reader refuses it; an
    /// ungraphed binary one is remembered by stamp, not re-read per start.
    ///
    /// Checks the stop flag per file: a shutdown mid-walk is a refusal.
    fn startup_drift(
        &self,
        store: &GraphStore,
        r_uid: &str,
        repo_url: &str,
    ) -> Result<CodeStartupDrift, anyhow::Error> {
        let indexed: std::collections::HashMap<String, String> = store
            .list_file_hashes_by_repo(r_uid)
            .context("list indexed repo files")?
            .into_iter()
            .collect();
        let filemeta = crate::index::load_filemeta_sidecar(&crate::sidecar_path(
            &self.db_path,
            ".filemeta.json",
        ))
        .repos
        .remove(r_uid)
        .unwrap_or_default();
        let unindexable = crate::index_md::load_code_unindexable_stamps(&self.db_path);
        let reader = self.reader_for(repo_url)?;
        let listed = reader
            .list_files()
            .context("list files for code watcher startup reconciliation")?;
        let max_bytes = reader.max_source_file_bytes();
        let is_policy_skip = |error: &anyhow::Error| {
            error
                .downcast_ref::<crate::content_reader::BinarySource>()
                .is_some()
                || error
                    .downcast_ref::<crate::content_reader::SourceTooLarge>()
                    .is_some()
        };
        let mut seen = HashSet::new();
        let mut drift = CodeStartupDrift::default();
        for rel_path in listed {
            if self.stop_flag.load(Ordering::Relaxed) {
                return Err(WatchMutationRefused.into());
            }
            let rel_str = rel_path.to_string_lossy().into_owned();
            seen.insert(rel_str.clone());
            let abs_path = self.repo_root.join(&rel_path);
            if !is_supported_source(&abs_path) {
                continue;
            }
            let recorded = indexed.get(&rel_str);
            match source_policy_exclusion(&reader, &self.repo_root, &abs_path) {
                Ok(Some(_)) => {
                    // The batch retracts an excluded file's stale coverage;
                    // one the graph does not hold has nothing to retract.
                    if recorded.is_some() {
                        drift.replay.push(abs_path);
                    }
                    continue;
                }
                Ok(None) => {}
                // nw-664 final review: the policy check stats every path
                // component, and an EACCES there used to fail the WHOLE
                // drift — retried on backoff forever while the real lost
                // edits beside it never landed. It is the unreadable case:
                // disclosed, left as the graph has it, the rest reconciled.
                Err(error) => {
                    drift.unreadable.push((abs_path, format!("{error:#}")));
                    continue;
                }
            }
            // Vanished since the walk: its deletion is an event of its own.
            let Ok(Some((mtime_nanos, size_bytes))) = reader.file_meta_nanos(&rel_path) else {
                continue;
            };
            let drifted = match recorded {
                Some(hash) => {
                    let stat_matches = filemeta.get(&rel_str).is_some_and(|cached| {
                        cached.mtime_nanos == mtime_nanos
                            && cached.size_bytes == size_bytes
                            && &cached.content_hash == hash
                    });
                    if stat_matches || hash.is_empty() {
                        // No recorded hash: leave it alone rather than
                        // re-parse it on every start (nw-653's rule).
                        false
                    } else if size_bytes > max_bytes {
                        // Grown past the cap: the batch retracts it.
                        true
                    } else {
                        match reader.read_file(&rel_path) {
                            Ok(source) => crate::hash::blake3_hex(&source) != *hash,
                            // Unreadable or turned binary: a live batch keeps
                            // the previously indexed symbols for a file the
                            // reader cannot handle (and skips), so the replay
                            // must not carry it either — disclosed instead.
                            Err(error) => {
                                drift.unreadable.push((abs_path, format!("{error:#}")));
                                continue;
                            }
                        }
                    }
                }
                None => {
                    let stamp = code_source_stamp(mtime_nanos, size_bytes);
                    if size_bytes > max_bytes || unindexable.get(&abs_path) == Some(&stamp) {
                        false
                    } else {
                        match reader.read_file(&rel_path) {
                            Ok(_) => true,
                            Err(error) if is_policy_skip(&error) => {
                                drift.unindexable.push((abs_path, stamp));
                                continue;
                            }
                            Err(error) => {
                                drift.unreadable.push((abs_path, format!("{error:#}")));
                                continue;
                            }
                        }
                    }
                }
            };
            if drifted {
                drift.replay.push(abs_path);
            }
        }
        if !seen.is_empty() {
            for rel_str in indexed.keys().filter(|path| !seen.contains(*path)) {
                let abs_path = self.repo_root.join(rel_str);
                if matches!(
                    std::fs::symlink_metadata(&abs_path),
                    Err(error) if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    )
                ) {
                    drift.replay.push(abs_path);
                }
            }
        }
        drift.replay.sort();
        drift.unreadable.sort();
        Ok(drift)
    }

    /// nw-664: one startup reconciliation attempt, mirroring the brain
    /// watcher's (nw-653). `Ok(None)` means the graph now matches disk (bar
    /// any disclosed unreadable source) and no retry is owed; `Ok(Some(_))`
    /// means it failed, was disclosed through nw-653's
    /// `reconciliation_pending` status channel (under this repo's `code:`
    /// key) and is owed a retry with backoff. `Err` only for a shutdown
    /// refusal, which ends the watcher.
    ///
    /// Every attempt, retries included, recomputes the drift: disk may have
    /// moved on, and the unreadable disclosure must describe it now.
    ///
    /// The drifted paths go through `process_batch_and_notify` — the seam
    /// every live batch uses — so reverse-dependent resolution, contracts,
    /// publication, tombstones and the change callback behave identically.
    fn attempt_reconciliation(
        &self,
        store: &GraphStore,
        r_uid: &str,
        repo_url: &str,
        on_change: Option<&dyn Fn()>,
        failures: u32,
    ) -> Result<Option<crate::watcher::PendingReconciliation>, anyhow::Error> {
        // nw-664 review: a repo removed or pruned while a retry was owed is
        // owed nothing. The batch would re-create its Repo node (the cold
        // path's `insert_initial_repo`) and rewrite the debt removal cleared.
        if matches!(store.lookup_repo(r_uid), Ok(None)) {
            tracing::info!(
                repo = %self.repo_root.display(),
                "CodeWatcher: repo no longer in the graph; dropping its startup reconciliation"
            );
            crate::index_md::record_code_reconciliation_debt(
                &self.db_path,
                &self.repo_root,
                Vec::new(),
            );
            return Ok(None);
        }
        let drift = match self.startup_drift(store, r_uid, repo_url) {
            Err(error) if error.downcast_ref::<WatchMutationRefused>().is_some() => {
                return Err(error);
            }
            drift => drift,
        };
        let (paths, unreadable, error) = match drift {
            Ok(drift) => {
                crate::index_md::record_code_unindexable_stamps(
                    &self.db_path,
                    &[],
                    &drift.unindexable,
                );
                if drift.replay.is_empty() {
                    self.record_debt(&[], &drift.unreadable, None);
                    return Ok(None);
                }
                tracing::info!(
                    repo = %self.repo_root.display(),
                    files = drift.replay.len(),
                    unreadable = drift.unreadable.len(),
                    "CodeWatcher startup: reconciling sources changed while no watcher ran"
                );
                let replayed = match self.acquire_mutation_lease("watch_code_batch") {
                    Err(error) if error.downcast_ref::<WatchMutationRefused>().is_some() => {
                        return Err(error);
                    }
                    Err(error) => Err(error.context("acquire code watcher batch lease")),
                    Ok(_lease) => self.process_batch_and_notify(
                        store,
                        r_uid,
                        repo_url,
                        &drift.replay,
                        &crate::index::FileSystemIndexEpilogueIo,
                        on_change,
                    ),
                };
                match replayed {
                    Ok(WatchBatchOutcome::Skipped { reason }) => {
                        (Some(drift.replay), drift.unreadable, reason)
                    }
                    Ok(_) => {
                        self.record_unindexable_after_replay(store, r_uid, repo_url, &drift.replay);
                        self.record_debt(&[], &drift.unreadable, None);
                        return Ok(None);
                    }
                    Err(error) => (Some(drift.replay), drift.unreadable, error),
                }
            }
            Err(error) => (None, Vec::new(), error),
        };
        let failures = failures.saturating_add(1);
        let delay = crate::watcher::reconcile_retry_delay(self.reconcile_retry_base, failures);
        let message = format!("{error:#}");
        tracing::error!(
            repo = %self.repo_root.display(),
            error = %message,
            failures,
            retry_in_ms = delay.as_millis() as u64,
            "CodeWatcher startup reconciliation failed; sources changed while no watcher \
             ran are missing or stale until it succeeds (retrying; `nestweaver index` \
             also heals it)"
        );
        // An uncomputable drift is disclosed against the repo root itself.
        self.record_debt(
            paths
                .as_deref()
                .unwrap_or(std::slice::from_ref(&self.repo_root)),
            &unreadable,
            Some(&message),
        );
        Ok(Some(crate::watcher::PendingReconciliation {
            paths,
            also_replay: Vec::new(),
            failures,
            next_attempt: Instant::now() + delay,
        }))
    }

    /// nw-664: describe this repo's WHOLE debt — the `owed` replay when it
    /// failed with `error`, plus every source the drift could not read — in
    /// the status channel; nothing to describe clears it.
    fn record_debt(&self, owed: &[PathBuf], unreadable: &[(PathBuf, String)], error: Option<&str>) {
        let prefix = crate::index_md::WATCH_RECONCILIATION_PENDING_REASON;
        let mut entries: Vec<nestweaver_parser::SkippedFile> = Vec::new();
        if let Some(error) = error {
            let reason =
                format!("{prefix}: code watcher startup reconciliation failed; retrying ({error})");
            entries.extend(owed.iter().map(|path| {
                nestweaver_parser::SkippedFile::new(
                    path.to_string_lossy().into_owned(),
                    nestweaver_parser::SkipReasonCode::Other,
                    reason.clone(),
                )
            }));
        }
        entries.extend(unreadable.iter().map(|(path, error)| {
            nestweaver_parser::SkippedFile::new(
                path.to_string_lossy().into_owned(),
                nestweaver_parser::SkipReasonCode::ReadError,
                format!(
                    "{prefix}: code watcher could not read this source ({error}); the graph \
                     keeps its last indexed state, re-checked at the next watcher start"
                ),
            )
        }));
        entries.extend(self.unwatched_dirs.iter().map(|(dir, error)| {
            nestweaver_parser::SkippedFile::new(
                dir.to_string_lossy().into_owned(),
                nestweaver_parser::SkipReasonCode::ReadError,
                unwatched_dir_reason(error),
            )
        }));
        crate::index_md::record_code_reconciliation_debt(&self.db_path, &self.repo_root, entries);
    }

    /// nw-651 on Linux: adopt the subscription's current unwatchable
    /// directories — minus those the index ignores anyway (`SKIP_DIRS`,
    /// `[[repos]] exclude`, `.gitignore`: e.g. a container's database
    /// directory), which are no loss — and persist their rows when the set
    /// changed (or `force`), leaving the rest of the debt alone.
    fn sync_unwatched_dirs(&mut self, repo_url: &str, dirs: &[(PathBuf, String)], force: bool) {
        let reader = self.reader_for(repo_url).ok();
        let disclosable: Vec<(PathBuf, String)> = dirs
            .iter()
            .filter(|(dir, _)| {
                let Ok(rel) = dir.strip_prefix(&self.repo_root) else {
                    return true;
                };
                let excluded = reader
                    .as_ref()
                    .is_some_and(|reader| !reader.accepts_path(&rel.join("nestweaver-probe")));
                !excluded
                    && !matches!(
                        manifest_path_not_gitignored(&self.repo_root, rel),
                        Ok(false)
                    )
            })
            .cloned()
            .collect();
        if !force && disclosable == self.unwatched_dirs {
            return;
        }
        for (dir, error) in disclosable
            .iter()
            .filter(|dir| !self.unwatched_dirs.contains(dir))
        {
            tracing::warn!(
                dir = %dir.display(),
                %error,
                "code watcher cannot watch this directory; edits beneath it are not seen live"
            );
        }
        self.unwatched_dirs = disclosable;
        let rows = self
            .unwatched_dirs
            .iter()
            .map(|(dir, error)| {
                nestweaver_parser::SkippedFile::new(
                    dir.to_string_lossy().into_owned(),
                    nestweaver_parser::SkipReasonCode::ReadError,
                    unwatched_dir_reason(error),
                )
            })
            .collect();
        crate::index_md::record_code_unwatched_dirs(&self.db_path, &self.repo_root, rows);
    }

    /// nw-664: after a successful replay, a replayed source that still exists
    /// but that the graph does not hold (unparsable, say) is remembered with
    /// its stamp so the next start does not replay it again until it changes;
    /// every other replayed path is forgotten.
    fn record_unindexable_after_replay(
        &self,
        store: &GraphStore,
        r_uid: &str,
        repo_url: &str,
        replayed: &[PathBuf],
    ) {
        let graphed: HashSet<String> = match store.list_files_by_repo(r_uid) {
            Ok(files) => files.into_iter().map(|(_, path)| path).collect(),
            Err(error) => {
                tracing::warn!("nw-664: could not list graphed files after replay: {error}");
                return;
            }
        };
        let Ok(reader) = self.reader_for(repo_url) else {
            return;
        };
        let unindexable: Vec<(PathBuf, String)> = replayed
            .iter()
            .filter_map(|path| {
                let rel = path.strip_prefix(&self.repo_root).ok()?;
                if graphed.contains(&*rel.to_string_lossy())
                    || !std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
                {
                    return None;
                }
                let (mtime_nanos, size_bytes) = reader.file_meta_nanos(rel).ok()??;
                Some((path.clone(), code_source_stamp(mtime_nanos, size_bytes)))
            })
            .collect();
        crate::index_md::record_code_unindexable_stamps(&self.db_path, replayed, &unindexable);
    }
}

// Do not admit a symlink in any component, including an ancestor replaced
// between events. Missing components are legitimate deletion candidates.
fn path_has_symlink(root: &Path, relative: &Path) -> anyhow::Result<bool> {
    let mut path = root.to_path_buf();
    for part in relative.components() {
        if !matches!(part, std::path::Component::Normal(_)) {
            return Ok(true);
        }
        path.push(part);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Ok(true),
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(false);
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect watched path {}", path.display()));
            }
        }
    }
    Ok(false)
}

/// nw-664: what the code watcher's startup reconciliation found.
#[derive(Debug, Default)]
struct CodeStartupDrift {
    /// Absolute paths to replay through the batch seam, sorted.
    replay: Vec<PathBuf>,
    /// Sources the walk could not read, with the error: disclosed, not
    /// replayed, and never treated as deleted.
    unreadable: Vec<(PathBuf, String)>,
    /// Ungraphed sources seen to be binary: remembered by stamp.
    unindexable: Vec<(PathBuf, String)>,
}

/// nw-664 review: nanosecond mtime plus size (the filemeta quick check), so
/// a restore within the same second is still seen as a change.
fn code_source_stamp(mtime_nanos: u64, size_bytes: u64) -> String {
    format!("{mtime_nanos}:{size_bytes}")
}

/// Why policy keeps the watched source `path` out of the graph, if it does.
///
/// nw-664: ONE predicate for the batch (which retracts a now-excluded file's
/// stale coverage) and the startup drift (which must not replay a file the
/// batch would only drop), so the two cannot disagree (CONTRIBUTING, sibling
/// gaps).
fn source_policy_exclusion(
    reader: &crate::content_reader::FilesystemReader,
    repo_root: &Path,
    path: &Path,
) -> anyhow::Result<Option<&'static str>> {
    let rel_path = path.strip_prefix(repo_root)?;
    Ok(if path_has_symlink(repo_root, rel_path)? {
        Some("symlink outside the admitted source tree")
    } else if !reader.accepts_path(rel_path) {
        Some("configured repository exclusion")
    } else if is_minified_or_bundled(path) {
        Some("minified/generated file policy")
    } else {
        None
    })
}

fn drain_queued_events(rx: &std::sync::mpsc::Receiver<RawWatchResult>) -> Vec<PathBuf> {
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

/// The debt row reason for a directory the code watcher cannot watch.
fn unwatched_dir_reason(error: &str) -> String {
    format!(
        "{}: {} ({error}); edits beneath it are not picked up live until it can be \
         watched again",
        crate::index_md::WATCH_RECONCILIATION_PENDING_REASON,
        crate::index_md::CODE_UNWATCHED_DIR_MARKER,
    )
}

fn is_watcher_input(path: &Path) -> bool {
    is_supported_source(path) || crate::contracts::is_spec_file(&path.to_string_lossy())
}

fn manifest_path_not_gitignored(root: &Path, relative: &Path) -> anyhow::Result<bool> {
    if !root.join(".git").exists() {
        return Ok(true);
    }
    let mut command = std::process::Command::new("git");
    command
        .current_dir(root)
        .args(["check-ignore", "--no-index", "--quiet", "--"])
        .arg(relative);
    crate::git_cmd::apply_git_isolation(&mut command);
    let result = crate::git_cmd::run_git_with_timeout(command, Duration::from_secs(5))?;
    match result.status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => anyhow::bail!(
            "cannot determine manifest gitignore eligibility: {}",
            String::from_utf8_lossy(&result.stderr)
        ),
    }
}

/// nw-601. A watched source whose parse tree has errors and yields no
/// symbols ([`nestweaver_parser::ParsedFile::unparsable_skip`]). Typed so the
/// batch can treat it as a policy skip, exactly like `SourceTooLarge`.
#[derive(Debug, thiserror::Error)]
#[error("unparsable source {}: {}", .0.path, .0.reason)]
struct UnparsableSource(nestweaver_parser::SkippedFile);

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
    reader: &crate::content_reader::FilesystemReader,
    rel_path: &Path,
    r_uid: &str,
    repo_url: &str,
) -> Result<PreparedCodeFile, anyhow::Error> {
    use nestweaver_parser::{RawReference, RawSymbol, parse_source};
    use nestweaver_resolver::{discover_workspace_context, resolve_references_with_context};
    use nestweaver_schema::{File, Symbol, canonical_symbol_id, file_uid, symbol_uid};

    let abs_path = reader.root().join(rel_path);
    let rel_str = rel_path.to_string_lossy().into_owned();

    let source = reader
        .read_file(rel_path)
        .with_context(|| format!("read watched source {}", abs_path.display()))?;
    let parsed = parse_source(&abs_path, &source)
        .with_context(|| format!("parse watched source {}", abs_path.display()))?;
    // nw-601: the same predicate the full and incremental index routes call,
    // routed through the batch's policy-skip arm so the stale coverage is
    // dropped rather than republished as an empty File node.
    if let Some(skipped) = parsed.unparsable_skip(rel_str.clone()) {
        return Err(UnparsableSource(skipped).into());
    }

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
        discover_workspace_context(reader.root())
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

/// Apply one prepared file, returning `(symbols_inserted, deleted_symbol_uids)`.
///
/// nw-204: the delete list matters as much as the insert count here. This is
/// the watcher's per-save path, so it is the single most frequent producer of
/// orphaned embedding vectors in a long-lived brain — every save that removes
/// or renames a symbol leaves one behind. The list is RAW (a re-index rewrites
/// most symbols under their original UIDs); the post-commit liveness filter in
/// `GraphStore::tombstone_deleted_symbol_embeddings` decides what is actually
/// dead.
fn apply_prepared_code_file(
    txn: &nestweaver_store::DbConnection<'_>,
    r_uid: &str,
    prepared: &PreparedCodeFile,
) -> Result<(usize, Vec<String>), anyhow::Error> {
    let deleted_symbol_uids =
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
    Ok((prepared.symbols.len(), deleted_symbol_uids))
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

    /// nw-678: a code-watcher batch replaces the saved file's symbols
    /// (DETACH DELETE, then insert) and used to drop their project
    /// membership with them. Membership is the repo now.
    #[test]
    fn project_code_membership_survives_a_watcher_batch() {
        let dir = tempfile::tempdir().unwrap();
        let (store, db, repo, project, repo_url) =
            crate::project::one_repo_project_fixture(dir.path(), "\"alpha\"");
        let members = |store: &GraphStore| {
            store
                .list_project_symbol_uids_by_pagerank(&project, 50, None, None)
                .unwrap()
        };
        assert_eq!(members(&store).len(), 2, "precondition");
        let file = repo.join("src/lib.rs");
        std::fs::write(
            &file,
            "// shifted\npub fn alpha_one() -> i32 { 1 }\npub fn alpha_two() -> i32 { 2 }\n",
        )
        .unwrap();
        let root = std::fs::canonicalize(&repo).unwrap();
        let watcher = CodeWatcher::new(&db, &root, "default");
        let r_uid = nestweaver_schema::repo_uid("default", &repo_url);
        watcher
            .process_batch_with_io(
                &store,
                &r_uid,
                &repo_url,
                &[root.join("src/lib.rs")],
                &crate::index::FileSystemIndexEpilogueIo,
            )
            .unwrap();
        assert_eq!(members(&store).len(), 2);
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
        store
            .compute_pagerank(0.85, 20, &GraphScope::code_only())
            .unwrap();
        store.save_pagerank_cache(&pagerank_path).unwrap();
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
        assert!(reopened.pagerank_scores().is_err());
    }

    #[test]
    fn skip_dir_detection() {
        let skipped = |p: &str| {
            crate::index::path_in_skip_dirs(
                Path::new(p),
                crate::index::SKIP_DIRS,
                crate::index::nothing_unskipped(),
                &|_| false,
            )
        };
        assert!(skipped("/repo/node_modules/foo/bar.js"));
        assert!(skipped("/repo/.git/HEAD"));
        assert!(!skipped("/repo/src/main.rs"));
    }

    #[test]
    fn startup_event_drain_replays_every_queued_batch() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Ok(vec![PathBuf::from("openapi.yaml")])).unwrap();
        tx.send(Ok(vec![PathBuf::from("ItemsController.java")]))
            .unwrap();
        let replay = drain_queued_events(&rx);
        assert_eq!(replay.len(), 2);
        assert_eq!(replay[0], PathBuf::from("openapi.yaml"));
        assert_eq!(replay[1], PathBuf::from("ItemsController.java"));
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

    #[test]
    fn release_code_watcher_manifest_edit_and_delete_leave_durable_pending_debt() {
        let dir = tempfile::tempdir().unwrap();
        let (store, uid, root) = index_fixture_repo(&dir);
        let db = dir.path().join("graph.lbug");
        let watcher = CodeWatcher::new(&db, &root, "test");
        let path = root.join("package.json");
        std::fs::write(&path, r#"{"name":"app","dependencies":{"dep":"1"}}"#).unwrap();
        let before = store.graph_generation();
        assert!(matches!(
            process_fixture_batch(&watcher, &store, &uid, &root, std::slice::from_ref(&path)),
            WatchBatchOutcome::ManifestPending
        ));
        assert!(store.graph_generation() > before);
        let first_debt = crate::manifest::manifest_debt_revision(&db).unwrap();
        assert!(first_debt.is_some());
        assert!(!crate::manifest::manifest_cache_path(&db).exists());
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(
            process_fixture_batch(&watcher, &store, &uid, &root, &[path]),
            WatchBatchOutcome::ManifestPending
        ));
        assert_ne!(
            crate::manifest::manifest_debt_revision(&db).unwrap(),
            first_debt
        );
        assert_eq!(
            store.lookup_repo(&uid).unwrap().unwrap().indexed_sha,
            "sha1"
        );
    }

    /// nw-601, the watcher route. A source saved into an unparsable state
    /// (syntax errors, no symbols) was republished as an empty File node,
    /// while the full and incremental index routes disclose it as a
    /// `ParseError` skip. All three must agree: the watcher drops the file's
    /// stale coverage through its existing policy-skip arm. Counterweight: a
    /// recoverable partial-error save that still yields a symbol is published.
    #[test]
    fn watcher_drops_a_source_saved_into_an_unparsable_state() {
        let dir = tempfile::tempdir().unwrap();
        let (store, uid, root) = index_fixture_repo(&dir);
        let db = dir.path().join("graph.lbug");
        let watcher = CodeWatcher::new(&db, &root, "test");
        let file_paths = |store: &GraphStore| -> Vec<String> {
            store
                .list_files_by_repo(&uid)
                .unwrap()
                .into_iter()
                .map(|(_, path)| path)
                .collect()
        };
        assert!(file_paths(&store).contains(&"src/a.js".to_string()));

        let broken = root.join("src/a.js");
        std::fs::write(&broken, "}}} ((( @@@ %%% ;;\n").unwrap();
        process_fixture_batch(&watcher, &store, &uid, &root, std::slice::from_ref(&broken));
        assert!(
            !file_paths(&store).contains(&"src/a.js".to_string()),
            "an unparsable save must drop the File node, not keep it empty: {:?}",
            file_paths(&store)
        );
        assert!(store.symbols_in_file("src/a.js").unwrap().is_empty());

        let partial = root.join("src/b.js");
        std::fs::write(
            &partial,
            "export function partial() { return 1; }\nconst x = (;\n",
        )
        .unwrap();
        process_fixture_batch(
            &watcher,
            &store,
            &uid,
            &root,
            std::slice::from_ref(&partial),
        );
        assert!(
            store
                .symbols_in_file("src/b.js")
                .unwrap()
                .iter()
                .any(|symbol| symbol.name == "partial"),
            "a recoverable partial-error save is still published"
        );
    }

    #[test]
    fn directory_event_does_not_mark_manifest_debt() {
        let dir = tempfile::tempdir().unwrap();
        let (store, uid, root) = index_fixture_repo(&dir);
        let db = dir.path().join("graph.lbug");
        let watcher = CodeWatcher::new(&db, &root, "test");
        let src = root.join("src");
        assert!(src.is_dir());
        process_fixture_batch(&watcher, &store, &uid, &root, &[src]);
        assert!(
            crate::manifest::manifest_debt_revision(&db)
                .unwrap()
                .is_none(),
            "a source-directory event is not a package-manifest edit"
        );
    }

    #[test]
    fn release_code_watcher_ignores_excluded_and_gitignored_manifests() {
        let dir = tempfile::tempdir().unwrap();
        let (store, uid, root) = index_fixture_repo(&dir);
        let db = dir.path().join("graph.lbug");
        let watcher = CodeWatcher::new(&db, &root, "test");
        let ignored = root.join("node_modules/dep/package.json");
        std::fs::create_dir_all(ignored.parent().unwrap()).unwrap();
        std::fs::write(&ignored, "{}").unwrap();
        assert!(matches!(
            process_fixture_batch(&watcher, &store, &uid, &root, &[ignored]),
            WatchBatchOutcome::Unchanged
        ));
        let status = std::process::Command::new("git")
            .args(["init", "--template="])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(status.status.success());
        std::fs::write(root.join(".gitignore"), "package.json\n").unwrap();
        let path = root.join("package.json");
        std::fs::write(&path, "{}").unwrap();
        assert!(matches!(
            process_fixture_batch(&watcher, &store, &uid, &root, &[path]),
            WatchBatchOutcome::Unchanged
        ));
        assert!(
            crate::manifest::manifest_debt_revision(&db)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn directory_events_reconcile_moves_deletions_and_prefix_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let (store, r_uid, root) = index_fixture_repo(&dir);
        let watcher = CodeWatcher::new(dir.path().join("graph.lbug"), &root, "test");
        std::fs::create_dir(root.join("src2")).unwrap();
        std::fs::write(root.join("src2/control.js"), "export function control() {}").unwrap();
        process_fixture_batch(
            &watcher,
            &store,
            &r_uid,
            &root,
            &[root.join("src2/control.js")],
        );
        std::fs::rename(root.join("src"), root.join("nested.js")).unwrap();
        process_fixture_batch(
            &watcher,
            &store,
            &r_uid,
            &root,
            &[
                root.join("src"),
                root.join("nested.js"),
                root.join("nested.js/a.js"),
            ],
        );
        let paths: HashSet<_> = store
            .list_files_by_repo(&r_uid)
            .unwrap()
            .into_iter()
            .map(|(_, p)| p)
            .collect();
        assert_eq!(
            paths,
            HashSet::from([
                "nested.js/a.js".into(),
                "nested.js/b.js".into(),
                "nested.js/c.js".into(),
                "src2/control.js".into()
            ])
        );
        std::fs::write(
            root.join("nested.js/a.js"),
            "export function helper() { return 9; }",
        )
        .unwrap();
        process_fixture_batch(
            &watcher,
            &store,
            &r_uid,
            &root,
            &[root.join("nested.js/a.js")],
        );
        assert_eq!(
            store
                .lookup_symbols_by_repo(&r_uid)
                .unwrap()
                .iter()
                .filter(|s| s.name == "helper")
                .count(),
            1
        );
        std::fs::remove_dir_all(root.join("nested.js")).unwrap();
        process_fixture_batch(&watcher, &store, &r_uid, &root, &[root.join("nested.js")]);
        assert_eq!(
            store
                .list_files_by_repo(&r_uid)
                .unwrap()
                .into_iter()
                .map(|(_, p)| p)
                .collect::<Vec<_>>(),
            vec!["src2/control.js"]
        );
    }

    #[test]
    fn directory_events_move_out_and_in_respect_exclusions_and_nested_sources() {
        let dir = tempfile::tempdir().unwrap();
        let (store, r_uid, root) = index_fixture_repo(&dir);
        let config: crate::InstanceConfig = serde_json::from_value(serde_json::json!({
            "instance_id":"test", "repos":[{"url":format!("file://{}",root.display()),"exclude":["returned/b.js"]}],
            "snapshot_storage":{"backend":"local","path":"/tmp"}, "workspace":{"backend":"local","path":"/tmp"},
            "inference":{"endpoint":"","embedding_model":"","summary_model":""}, "git":{"credential_method":"ssh"}
        })).unwrap();
        let watcher = CodeWatcher::new(dir.path().join("graph.lbug"), &root, "test")
            .with_instance_config(Some(Arc::new(config)));
        let outside = dir.path().join("outside");
        std::fs::rename(root.join("src"), &outside).unwrap();
        process_fixture_batch(
            &watcher,
            &store,
            &r_uid,
            &root,
            &[root.join("src"), outside.clone()],
        );
        assert!(store.list_files_by_repo(&r_uid).unwrap().is_empty());
        std::fs::create_dir(outside.join("inner")).unwrap();
        std::fs::write(outside.join("inner/new.js"), "export function nested() {}").unwrap();
        std::fs::rename(&outside, root.join("returned")).unwrap();
        process_fixture_batch(
            &watcher,
            &store,
            &r_uid,
            &root,
            &[root.join("returned"), root.join("returned/inner")],
        );
        let paths: HashSet<_> = store
            .list_files_by_repo(&r_uid)
            .unwrap()
            .into_iter()
            .map(|(_, p)| p)
            .collect();
        assert_eq!(
            paths,
            HashSet::from([
                "returned/a.js".into(),
                "returned/c.js".into(),
                "returned/inner/new.js".into()
            ])
        );
    }

    #[test]
    fn ignored_build_subtree_events_do_not_publish_empty_batches() {
        let dir = tempfile::tempdir().unwrap();
        let (store, r_uid, root) = index_fixture_repo(&dir);
        let watcher = CodeWatcher::new(dir.path().join("graph.lbug"), &root, "test");
        std::fs::create_dir(root.join("target")).unwrap();
        // nw-652: what cargo actually leaves in every build directory. Without
        // a marker or a manifest beside it, `target/` is source and SHOULD
        // publish — see `source_target_dir_events_publish_without_a_manifest`.
        std::fs::write(
            root.join("target/CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        )
        .unwrap();
        std::fs::write(root.join("target/generated.rs"), "fn generated() {}").unwrap();
        assert!(matches!(
            process_fixture_batch(
                &watcher,
                &store,
                &r_uid,
                &root,
                &[root.join("target"), root.join("target/generated.rs")]
            ),
            WatchBatchOutcome::Unchanged
        ));
    }

    /// nw-652: the watcher's per-file route filters with `accepts_path`, which
    /// never consults gitignore — so it, not the walk, is where a name-only
    /// `target` rule did its damage on every save. A ballistics app's
    /// `src/screens/target/` has no build manifest beside it and must publish;
    /// the same edit beside a `Cargo.toml` is build output and must not.
    #[test]
    fn source_target_dir_events_publish_without_a_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let (store, r_uid, root) = index_fixture_repo(&dir);
        let watcher = CodeWatcher::new(dir.path().join("graph.lbug"), &root, "test");
        std::fs::create_dir_all(root.join("src/screens/target")).unwrap();
        let screen = root.join("src/screens/target/TargetEditMode.js");
        std::fs::write(&screen, "export function TargetEditMode() { return 1; }\n").unwrap();
        assert!(matches!(
            process_fixture_batch(&watcher, &store, &r_uid, &root, &[screen]),
            WatchBatchOutcome::Published { .. }
        ));
        let files = store.list_files_by_repo(&r_uid).unwrap();
        assert!(
            files
                .iter()
                .any(|(_, path)| path == "src/screens/target/TargetEditMode.js"),
            "a source `target/` must reach the graph through the watcher: {files:?}"
        );

        std::fs::write(
            root.join("src/screens/Cargo.toml"),
            "[package]\nname = \"x\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/screens/target/built.js"),
            "export const built = 1;\n",
        )
        .unwrap();
        let outcome = process_fixture_batch(
            &watcher,
            &store,
            &r_uid,
            &root,
            &[root.join("src/screens/target/built.js")],
        );
        assert!(
            matches!(outcome, WatchBatchOutcome::Unchanged),
            "an excluded build file must not publish a batch: {outcome:?}"
        );
        let files = store.list_files_by_repo(&r_uid).unwrap();
        assert!(
            !files
                .iter()
                .any(|(_, path)| path == "src/screens/target/built.js"),
            "a `target/` beside a Cargo.toml is build output and must stay out: \
             {outcome:?} {files:?}"
        );
    }

    #[test]
    fn directory_replaced_by_regular_file_retracts_descendants() {
        for target in ["src", "folder.js"] {
            let dir = tempfile::tempdir().unwrap();
            let (store, r_uid, root) = index_fixture_repo(&dir);
            let watcher = CodeWatcher::new(dir.path().join("graph.lbug"), &root, "test");
            if target != "src" {
                std::fs::rename(root.join("src"), root.join(target)).unwrap();
                process_fixture_batch(
                    &watcher,
                    &store,
                    &r_uid,
                    &root,
                    &[root.join("src"), root.join(target)],
                );
            }
            std::fs::remove_dir_all(root.join(target)).unwrap();
            std::fs::write(root.join(target), "export function replacement() {}").unwrap();
            let outcome =
                process_fixture_batch(&watcher, &store, &r_uid, &root, &[root.join(target)]);
            assert!(matches!(outcome, WatchBatchOutcome::Published { .. }));
            let files = store.list_files_by_repo(&r_uid).unwrap();
            assert!(files.iter().all(|(_, path)| path == target));
            assert_eq!(files.len(), usize::from(target.ends_with(".js")));
        }
    }

    #[test]
    fn file_replaced_by_directory_retracts_exact_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let (store, r_uid, root) = index_fixture_repo(&dir);
        let watcher = CodeWatcher::new(dir.path().join("graph.lbug"), &root, "test");
        std::fs::remove_file(root.join("src/a.js")).unwrap();
        std::fs::create_dir(root.join("src/a.js")).unwrap();
        std::fs::write(
            root.join("src/a.js/new.js"),
            "export function replacement() {}",
        )
        .unwrap();
        process_fixture_batch(&watcher, &store, &r_uid, &root, &[root.join("src/a.js")]);
        assert!(store.symbols_in_file("src/a.js").unwrap().is_empty());
        assert!(
            !store
                .list_files_by_repo(&r_uid)
                .unwrap()
                .iter()
                .any(|(_, p)| p == "src/a.js")
        );
        assert!(!store.symbols_in_file("src/a.js/new.js").unwrap().is_empty());
    }

    #[test]
    fn removed_spec_only_directory_retracts_declared_contracts() {
        let dir = tempfile::tempdir().unwrap();
        let (store, r_uid, _, root) = index_contract_fixture(&dir);
        let watcher = CodeWatcher::new(dir.path().join("graph.lbug"), &root, "test");
        std::fs::create_dir(root.join("specs")).unwrap();
        std::fs::rename(root.join("openapi.yaml"), root.join("specs/openapi.yaml")).unwrap();
        process_fixture_batch(
            &watcher,
            &store,
            &r_uid,
            &root,
            &[root.join("openapi.yaml"), root.join("specs")],
        );
        assert!(
            store
                .list_contracts(Some(&r_uid))
                .unwrap()
                .iter()
                .any(|c| c.source_path == "specs/openapi.yaml")
        );
        std::fs::rename(root.join("specs"), dir.path().join("outside-specs")).unwrap();
        process_fixture_batch(&watcher, &store, &r_uid, &root, &[root.join("specs")]);
        assert!(
            store
                .list_contracts(Some(&r_uid))
                .unwrap()
                .iter()
                .all(|c| c.source_path != "specs/openapi.yaml")
        );
    }

    #[test]
    fn directory_move_repairs_unchanged_callers_and_matches_fresh_graph() {
        let dir = tempfile::tempdir().unwrap();
        let (store, r_uid, root) = index_fixture_repo(&dir);
        let watcher = CodeWatcher::new(dir.path().join("graph.lbug"), &root, "test");
        std::fs::write(
            root.join("caller.js"),
            "import { helper } from './src/a.js'; export function caller() { return helper(); }",
        )
        .unwrap();
        process_fixture_batch(&watcher, &store, &r_uid, &root, &[root.join("caller.js")]);
        let caller = uid_of(&store, &r_uid, "caller");
        assert!(
            store
                .callees_of(&caller)
                .unwrap()
                .iter()
                .any(|s| s.name == "helper")
        );
        std::fs::rename(root.join("src"), root.join("moved")).unwrap();
        process_fixture_batch(
            &watcher,
            &store,
            &r_uid,
            &root,
            &[root.join("src"), root.join("moved")],
        );
        let (_, fresh) = crate::index::index_directory_in_memory(
            &root,
            "test",
            &format!("file://{}", root.display()),
            "fresh",
        )
        .unwrap();
        let topology = |db: &GraphStore| {
            db.load_typed_edges()
                .unwrap()
                .into_iter()
                .map(|(a, b, kind, _, _)| (a, b, kind))
                .collect::<std::collections::BTreeSet<_>>()
        };
        assert_eq!(topology(&store), topology(&fresh));
        let symbols = |db: &GraphStore| {
            db.lookup_symbols_by_repo(&r_uid)
                .unwrap()
                .into_iter()
                .map(|s| (s.uid, s.file_path, s.name))
                .collect::<std::collections::BTreeSet<_>>()
        };
        assert_eq!(symbols(&store), symbols(&fresh));
    }

    #[test]
    fn directory_move_rebuilds_mixed_controller_and_spec_contracts() {
        let dir = tempfile::tempdir().unwrap();
        let (store, r_uid, repo_url, root) = index_contract_fixture(&dir);
        let watcher = CodeWatcher::new(dir.path().join("graph.lbug"), &root, "test");
        std::fs::create_dir(root.join("group")).unwrap();
        for file in ["openapi.yaml", "ItemsController.java"] {
            std::fs::rename(root.join(file), root.join("group").join(file)).unwrap();
        }
        process_fixture_batch(
            &watcher,
            &store,
            &r_uid,
            &root,
            &[
                root.join("openapi.yaml"),
                root.join("ItemsController.java"),
                root.join("group"),
            ],
        );
        std::fs::rename(root.join("group"), root.join("renamed")).unwrap();
        process_fixture_batch(
            &watcher,
            &store,
            &r_uid,
            &root,
            &[root.join("group"), root.join("renamed")],
        );
        let (_, fresh) =
            crate::index::index_directory_in_memory(&root, "test", &repo_url, "fresh").unwrap();
        let contracts = |db: &GraphStore| {
            db.list_contracts(Some(&r_uid))
                .unwrap()
                .into_iter()
                .map(|c| (c.uid, c.source_path))
                .collect::<std::collections::BTreeSet<_>>()
        };
        assert_eq!(contracts(&store), contracts(&fresh));
        assert_eq!(
            store.list_implemented_contract_uids().unwrap(),
            fresh.list_implemented_contract_uids().unwrap()
        );
        assert!(
            store
                .symbols_in_file("group/ItemsController.java")
                .unwrap()
                .is_empty()
        );
        assert!(
            !store
                .symbols_in_file("renamed/ItemsController.java")
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn real_directory_rename_then_edit_has_one_live_symbol_and_drains() {
        let dir = tempfile::tempdir().unwrap();
        let (store, r_uid, root) = index_fixture_repo(&dir);
        let store = Arc::new(store);
        let observed = store.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (changed_tx, changed_rx) = std::sync::mpsc::channel();
        let watcher = CodeWatcher::new(dir.path().join("graph.lbug"), &root, "test")
            .with_debounce_ms(100)
            .with_ready_signal(ready_tx);
        let stop = watcher.shutdown_handle();
        let handle = std::thread::spawn(move || {
            watcher.run_with_store(
                store,
                Some(Box::new(move || {
                    let _ = changed_tx.send(());
                })),
            )
        });
        let result = (|| -> anyhow::Result<()> {
            ready_rx.recv_timeout(Duration::from_secs(10))?;
            std::fs::rename(root.join("src"), root.join("moved"))?;
            changed_rx.recv_timeout(Duration::from_secs(15))?;
            anyhow::ensure!(
                observed.symbols_in_file("src/a.js")?.is_empty(),
                "stale old symbols after move"
            );
            anyhow::ensure!(
                observed.symbols_in_file("moved/a.js")?.len() == 1,
                "moved source absent"
            );
            std::fs::write(
                root.join("moved/a.js"),
                "export function helper() { return 99; }",
            )?;
            changed_rx.recv_timeout(Duration::from_secs(15))?;
            anyhow::ensure!(
                observed
                    .lookup_symbols_by_repo(&r_uid)?
                    .iter()
                    .filter(|s| s.name == "helper")
                    .count()
                    == 1,
                "duplicate symbol after edit"
            );
            Ok(())
        })();
        stop.stop();
        handle.join().unwrap().unwrap();
        result.unwrap();
        let before = observed.lookup_symbols_by_repo(&r_uid).unwrap().len();
        std::fs::write(
            root.join("moved/after_stop.js"),
            "export function late() {}",
        )
        .unwrap();
        assert_eq!(
            observed.lookup_symbols_by_repo(&r_uid).unwrap().len(),
            before
        );
    }

    #[cfg(unix)]
    #[test]
    fn directory_events_do_not_follow_external_symlinks_or_parent_components() {
        let dir = tempfile::tempdir().unwrap();
        let (store, r_uid, root) = index_fixture_repo(&dir);
        let watcher = CodeWatcher::new(dir.path().join("graph.lbug"), &root, "test");
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret.js"), "export function secret() {}").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        process_fixture_batch(
            &watcher,
            &store,
            &r_uid,
            &root,
            &[
                root.join("link"),
                root.join("link/secret.js"),
                root.join("../outside/secret.js"),
            ],
        );
        assert!(
            !store
                .lookup_symbols_by_repo(&r_uid)
                .unwrap()
                .iter()
                .any(|s| s.name == "secret")
        );
        assert_eq!(store.list_files_by_repo(&r_uid).unwrap().len(), 3);
    }

    #[test]
    fn watcher_configured_excludes_remove_stale_rows_and_never_reintroduce_changes() {
        let dir = tempfile::tempdir().unwrap();
        let (store, r_uid, root) = index_fixture_repo(&dir);
        let config: crate::InstanceConfig = serde_json::from_value(serde_json::json!({
            "instance_id":"test", "repos":[{"url":format!("file://{}",root.display()),"exclude":["src/a.js"]}],
            "snapshot_storage":{"backend":"local","path":"/tmp"}, "workspace":{"backend":"local","path":"/tmp"},
            "inference":{"endpoint":"","embedding_model":"","summary_model":""}, "git":{"credential_method":"ssh"}
        })).unwrap();
        let watcher = CodeWatcher::new(dir.path().join("graph.lbug"), &root, "test")
            .with_instance_config(Some(Arc::new(config)));
        let path = root.join("src/a.js");
        for (iteration, source) in [
            "export function hidden_one() {}",
            "export function hidden_two() {}",
        ]
        .into_iter()
        .enumerate()
        {
            std::fs::write(&path, source).unwrap();
            let generation = store.graph_generation();
            let outcome =
                process_fixture_batch(&watcher, &store, &r_uid, &root, std::slice::from_ref(&path));
            if iteration == 0 {
                assert!(matches!(outcome, WatchBatchOutcome::Published { .. }));
                assert!(store.graph_generation() > generation);
            } else {
                assert!(matches!(outcome, WatchBatchOutcome::Unchanged));
                assert_eq!(store.graph_generation(), generation);
            }
            assert!(store.symbols_in_file("src/a.js").unwrap().is_empty());
            assert!(!store.symbols_in_file("src/b.js").unwrap().is_empty());
        }
        std::fs::remove_file(&path).unwrap();
        process_fixture_batch(&watcher, &store, &r_uid, &root, std::slice::from_ref(&path));
        assert!(store.symbols_in_file("src/a.js").unwrap().is_empty());
    }

    #[test]
    fn watch_rename_into_minified_policy_removes_stale_symbols() {
        let dir = tempfile::tempdir().unwrap();
        let (store, r_uid, canonical_root) = index_fixture_repo(&dir);
        let old_path = canonical_root.join("src/a.js");
        let minified_path = canonical_root.join("src/a.min.js");
        std::fs::rename(&old_path, &minified_path).unwrap();
        let watcher = CodeWatcher::new(dir.path().join("graph.lbug"), &canonical_root, "test");

        let outcome = process_fixture_batch(
            &watcher,
            &store,
            &r_uid,
            &canonical_root,
            &[old_path, minified_path],
        );
        assert!(matches!(outcome, WatchBatchOutcome::Published { .. }));
        assert!(
            store
                .lookup_symbols_by_repo(&r_uid)
                .unwrap()
                .iter()
                .all(|symbol| symbol.name != "helper"),
            "a policy-excluded rename must not leave stale source coverage"
        );
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

        // nw-190: invalid UTF-8 is now decoded lossily and is NOT a read
        // failure, so it can no longer stand in for one. Use genuinely
        // binary content (a NUL byte), which the reader refuses with a
        // typed BinarySource -- the property under test is that a file the
        // reader cannot handle skips before publication and preserves the
        // previously indexed symbols.
        std::fs::write(
            canonical_root.join("src/b.js"),
            b"export function alpha() { return 1; }\n\x00binary",
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

    #[cfg(target_os = "linux")]
    #[test]
    fn one_code_edit_settles_after_one_hot_batch() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::thread;

        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let source_path = repo_root.join("service.js");
        std::fs::write(&source_path, "export function alpha() { return 1; }\n").unwrap();
        let root = std::fs::canonicalize(repo_root).unwrap();
        let repo_url = format!("file://{}", root.display());
        let db_path = dir.path().join("watch.lbug");
        crate::index::index_directory(&root, &db_path, "test", &repo_url, "sha-1").unwrap();
        let store = Arc::new(GraphStore::open_or_create(&db_path).unwrap());
        let notifications = Arc::new(AtomicUsize::new(0));
        let callback_count = notifications.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let watcher = CodeWatcher::new(&db_path, &root, "test")
            .with_debounce_ms(100)
            .with_ready_signal(ready_tx);
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
            .expect("code watcher should start");
        thread::sleep(Duration::from_millis(100));

        std::fs::write(&source_path, "export function alpha() { return 2; }\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while notifications.load(Ordering::SeqCst) < 1 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
        }
        assert_eq!(
            notifications.load(Ordering::SeqCst),
            1,
            "the real edit must publish one code batch"
        );
        thread::sleep(Duration::from_millis(1200));
        assert_eq!(
            notifications.load(Ordering::SeqCst),
            1,
            "parser reads must not feed another code watcher batch"
        );

        stop.stop();
        handle.join().unwrap().unwrap();
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

    /// Index the `index_fixture_repo` JS files (plus `extra`) into an ON-DISK
    /// graph, as `nestweaver index` would, so the filemeta cache and every
    /// sidecar exist. Returns the db path, canonical root and repo uid.
    fn index_fixture_repo_on_disk(
        dir: &tempfile::TempDir,
        extra: &[(&str, &str)],
    ) -> (PathBuf, PathBuf, String) {
        let repo_root = dir.path().join("repo");
        let files = [
            ("src/a.js", "export function helper() { return 7; }\n"),
            (
                "src/b.js",
                "import { helper } from './a.js';\nexport function alpha() { return helper() + 1; }\n",
            ),
            (
                "src/c.js",
                "import { alpha } from './b.js';\nexport function gamma() { return alpha() * 2; }\n",
            ),
        ];
        for (rel, body) in files.iter().chain(extra) {
            let path = repo_root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        let root = std::fs::canonicalize(&repo_root).unwrap();
        let repo_url = format!("file://{}", root.display());
        let db_path = dir.path().join("graph.lbug");
        crate::index::index_directory(&root, &db_path, "test", &repo_url, "sha1").unwrap();
        (
            db_path,
            root,
            nestweaver_schema::repo_uid("test", &repo_url),
        )
    }

    /// Start a code watcher over `root`, stop it at readiness, and return how
    /// many batch leases (`watch_code_batch`) its startup took — zero means
    /// startup ran no batch at all.
    fn code_startup_batches(db_path: &Path, root: &Path, store: &Arc<GraphStore>) -> u32 {
        use std::sync::atomic::AtomicU32;
        let batches = Arc::new(AtomicU32::new(0));
        let counted = Arc::clone(&batches);
        let factory: WatchMutationLeaseFactory = Arc::new(move |label: &'static str| {
            if label == "watch_code_batch" {
                counted.fetch_add(1, Ordering::SeqCst);
            }
            Ok(Box::new(()) as Box<dyn WatchMutationLease>)
        });
        let watcher = CodeWatcher::new(db_path, root, "test").with_mutation_lease_factory(factory);
        let stop = watcher.shutdown_handle();
        watcher
            .with_ready_callback(move || stop.stop())
            .run_with_store(store.clone(), None)
            .unwrap();
        batches.load(Ordering::SeqCst)
    }

    fn repo_symbol_names(store: &GraphStore, r_uid: &str) -> HashSet<String> {
        store
            .lookup_symbols_by_repo(r_uid)
            .unwrap()
            .into_iter()
            .map(|symbol| symbol.name)
            .collect()
    }

    fn repo_file_paths(store: &GraphStore, r_uid: &str) -> HashSet<String> {
        store
            .list_files_by_repo(r_uid)
            .unwrap()
            .into_iter()
            .map(|(_, path)| path)
            .collect()
    }

    /// nw-664: the code watcher only snapshotted a COLD repo. For an indexed
    /// one, sources created, edited or deleted while no watcher ran (daemon
    /// down, laptop asleep, a crash) produced events nobody received, and the
    /// graph stayed stale until the file was touched again or a manual
    /// `index` ran. Startup must reconcile all three before readiness.
    #[test]
    fn code_watcher_startup_reconciles_sources_changed_while_no_watcher_ran() {
        let dir = tempfile::tempdir().unwrap();
        let (db_path, root, uid) = index_fixture_repo_on_disk(&dir, &[]);

        // The gap: nothing is watching while these land on disk.
        std::fs::write(
            root.join("src/d.js"),
            "export function delta() { return 4; }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/a.js"),
            "export function helperEdited() { return 8; }\n",
        )
        .unwrap();
        std::fs::remove_file(root.join("src/c.js")).unwrap();

        let store = Arc::new(GraphStore::open_or_create(&db_path).unwrap());
        assert!(code_startup_batches(&db_path, &root, &store) > 0);

        let names = repo_symbol_names(&store, &uid);
        assert!(
            names.contains("delta"),
            "a source created while no watcher ran must be ingested before ready: {names:?}"
        );
        assert!(
            names.contains("helperEdited") && !names.contains("helper"),
            "an edit made while no watcher ran must be ingested before ready: {names:?}"
        );
        assert!(
            !names.contains("gamma") && !repo_file_paths(&store, &uid).contains("src/c.js"),
            "a source deleted while no watcher ran must leave the graph: {names:?}"
        );
        assert!(
            names.contains("alpha"),
            "counterweight: untouched b.js stays"
        );

        // Counterweight: once reconciled, the next start finds nothing to do.
        assert_eq!(code_startup_batches(&db_path, &root, &store), 0);
    }

    /// nw-664 counterweight: an unchanged indexed repo publishes nothing at
    /// startup — no batch, no re-parse, no new graph generation.
    #[test]
    fn code_watcher_startup_over_an_unchanged_repo_runs_no_batch() {
        let dir = tempfile::tempdir().unwrap();
        let (db_path, root, _uid) = index_fixture_repo_on_disk(&dir, &[]);
        // A stat-only change (content identical) must not replay either.
        let a = root.join("src/a.js");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&a)
            .unwrap()
            .set_modified(std::time::SystemTime::now() + Duration::from_secs(5))
            .unwrap();
        let store = Arc::new(GraphStore::open_or_create(&db_path).unwrap());
        let generation = store.graph_generation();
        assert_eq!(code_startup_batches(&db_path, &root, &store), 0);
        assert_eq!(store.graph_generation(), generation);
    }

    /// nw-664 / nw-651: a partial scan is not a deletion. Sources under a
    /// subdirectory the walk cannot read stay in the graph, while a genuine
    /// deletion elsewhere in the same scan is still reconciled.
    #[cfg(unix)]
    #[test]
    fn code_watcher_startup_keeps_sources_in_an_unreadable_subdirectory() {
        use std::os::unix::fs::PermissionsExt;
        // Root reads through 0o000, so the fixture cannot lock it out.
        // SAFETY: `geteuid` takes no arguments, touches no memory and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (db_path, root, uid) = index_fixture_repo_on_disk(
            &dir,
            &[(
                "locked/hidden.js",
                "export function hidden() { return 1; }\n",
            )],
        );
        std::fs::remove_file(root.join("src/c.js")).unwrap();
        let locked = root.join("locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let store = Arc::new(GraphStore::open_or_create(&db_path).unwrap());
        let started = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            code_startup_batches(&db_path, &root, &store)
        }));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        started.unwrap();
        let names = repo_symbol_names(&store, &uid);
        assert!(
            names.contains("hidden"),
            "an unreadable subdirectory must not be replayed as a deletion: {names:?}"
        );
        assert!(
            !names.contains("gamma"),
            "a real deletion in the same scan is still reconciled: {names:?}"
        );
    }

    /// nw-651 on Linux (PR #431 CI): notify's inotify backend fails the
    /// WHOLE recursive watch on the first subdirectory it cannot watch, so a
    /// repo holding one unreadable directory had no code watcher at all —
    /// every live edit silently lost. The seam fails the subscription the
    /// way inotify does (FSEvents never would). Startup must proceed, the
    /// unreadable directory must be disclosed and its sources kept, and
    /// edits elsewhere — including in a directory created afterwards — must
    /// still land.
    #[cfg(unix)]
    #[test]
    fn code_watcher_stays_live_when_a_subdirectory_cannot_be_watched() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: `geteuid` takes no arguments, touches no memory and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (db_path, root, uid) = index_fixture_repo_on_disk(
            &dir,
            &[(
                "locked/hidden.js",
                "export function hidden() { return 1; }\n",
            )],
        );
        let locked = root.join("locked");
        // An ignored unreadable directory (a container's data dir under a
        // SKIP_DIRS name) is no loss, so it is not a debt row.
        let ignored = root.join("node_modules");
        std::fs::create_dir_all(&ignored).unwrap();
        let fresh_locked = root.join("fresh_locked");
        let restore = || {
            for dir in [&locked, &ignored, &fresh_locked] {
                let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
            }
        };
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        std::fs::set_permissions(&ignored, std::fs::Permissions::from_mode(0o000)).unwrap();
        let store = Arc::new(GraphStore::open_or_create(&db_path).unwrap());
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // On Linux this is REAL inotify failing on the unreadable
            // directory; elsewhere the seam fails the way it does.
            let watcher = CodeWatcher::new(&db_path, &root, "test");
            #[cfg(not(target_os = "linux"))]
            let watcher = watcher.emulating_inotify_watch();
            let (stop, handle) = spawn_live_code_watcher(watcher, store.clone());
            let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let locked_key = locked.to_string_lossy().into_owned();
                let disclosed = || {
                    code_debt(&db_path).iter().any(|(path, reason)| {
                        *path == locked_key && reason.contains("cannot watch")
                    })
                };
                assert!(
                    disclosed(),
                    "the unwatchable directory must be disclosed: {:?}",
                    code_debt(&db_path)
                );
                let ignored_key = ignored.to_string_lossy().into_owned();
                assert!(
                    !code_debt(&db_path)
                        .iter()
                        .any(|(path, _)| *path == ignored_key),
                    "an ignored unreadable directory is not a debt row: {:?}",
                    code_debt(&db_path)
                );

                std::fs::write(
                    root.join("src/a.js"),
                    "export function helperLive() { return 9; }\n",
                )
                .unwrap();
                wait_until("an edit in a watched directory to land", || {
                    repo_symbol_names(&store, &uid).contains("helperLive")
                });
                std::fs::create_dir(root.join("fresh")).unwrap();
                std::fs::write(
                    root.join("fresh/born.js"),
                    "export function bornLive() { return 1; }\n",
                )
                .unwrap();
                wait_until("a source in a directory created later to land", || {
                    repo_symbol_names(&store, &uid).contains("bornLive")
                });
                // Its own watch, not only the create event, carries later edits.
                std::fs::write(
                    root.join("fresh/born.js"),
                    "export function bornEdited() { return 2; }\n",
                )
                .unwrap();
                wait_until("a later edit in the new directory to land", || {
                    repo_symbol_names(&store, &uid).contains("bornEdited")
                });

                // Deleted and re-created within one debounce window: the new
                // directory needs a watch of its own again.
                std::fs::remove_dir_all(root.join("fresh")).unwrap();
                std::fs::create_dir(root.join("fresh")).unwrap();
                std::fs::write(
                    root.join("fresh/again.js"),
                    "export function againLive() { return 3; }\n",
                )
                .unwrap();
                wait_until("a source in the re-created directory to land", || {
                    repo_symbol_names(&store, &uid).contains("againLive")
                });
                std::thread::sleep(Duration::from_millis(300));
                std::fs::write(
                    root.join("fresh/again.js"),
                    "export function againEdited() { return 4; }\n",
                )
                .unwrap();
                wait_until("a later edit in the re-created directory to land", || {
                    repo_symbol_names(&store, &uid).contains("againEdited")
                });

                // Discovered live: disclosed when it appears unreadable, and
                // retried (and cleared) once its permissions are fixed.
                use std::os::unix::fs::DirBuilderExt;
                std::fs::DirBuilder::new()
                    .mode(0o000)
                    .create(&fresh_locked)
                    .unwrap();
                let fresh_key = fresh_locked.to_string_lossy().into_owned();
                let fresh_disclosed = || {
                    code_debt(&db_path)
                        .iter()
                        .any(|(path, _)| *path == fresh_key)
                };
                wait_until(
                    "a directory found unreadable live to be disclosed",
                    fresh_disclosed,
                );
                std::fs::set_permissions(&fresh_locked, std::fs::Permissions::from_mode(0o755))
                    .unwrap();
                wait_until("its row to clear once it is watchable", || {
                    !fresh_disclosed()
                });
                assert!(
                    repo_symbol_names(&store, &uid).contains("hidden"),
                    "an unwatchable directory's sources are never deleted"
                );
                assert!(
                    disclosed(),
                    "published live batches must not clear the directory's row"
                );
            }));
            stop.stop();
            let joined = handle.join().unwrap();
            if let Err(panic) = checked {
                std::panic::resume_unwind(panic);
            }
            joined.unwrap();
        }));
        restore();
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

    /// Review of 8adb6a5c: a change in the subscription's unwatched
    /// directories replaces only those rows — the rest of the repo's debt
    /// (re-derived by the startup reconciliation) is never wiped first.
    #[test]
    fn unwatched_dir_rows_merge_into_the_code_debt() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("graph.lbug");
        let root = dir.path().join("repo");
        let owed = nestweaver_parser::SkippedFile::new(
            root.join("src/a.js").to_string_lossy().into_owned(),
            nestweaver_parser::SkipReasonCode::Other,
            "owed".to_string(),
        );
        crate::index_md::record_code_reconciliation_debt(&db_path, &root, vec![owed]);
        let row = |dir: &str| {
            nestweaver_parser::SkippedFile::new(
                root.join(dir).to_string_lossy().into_owned(),
                nestweaver_parser::SkipReasonCode::ReadError,
                unwatched_dir_reason("Permission denied"),
            )
        };
        let paths = || {
            code_debt(&db_path)
                .into_iter()
                .map(|(path, _)| path)
                .collect::<Vec<_>>()
        };
        let key = |rel: &str| root.join(rel).to_string_lossy().into_owned();
        crate::index_md::record_code_unwatched_dirs(&db_path, &root, vec![row("locked")]);
        assert_eq!(paths(), vec![key("locked"), key("src/a.js")]);
        crate::index_md::record_code_unwatched_dirs(&db_path, &root, Vec::new());
        assert_eq!(
            paths(),
            vec![key("src/a.js")],
            "only the unwatched row goes"
        );
    }

    /// nw-664 / nw-287: a scan that finds no files at all (unmounted, emptied)
    /// is not evidence that every source was deleted.
    #[test]
    fn code_watcher_startup_never_infers_deletions_from_an_empty_scan() {
        let dir = tempfile::tempdir().unwrap();
        let (db_path, root, uid) = index_fixture_repo_on_disk(&dir, &[]);
        std::fs::remove_dir_all(root.join("src")).unwrap();
        let store = Arc::new(GraphStore::open_or_create(&db_path).unwrap());
        assert_eq!(code_startup_batches(&db_path, &root, &store), 0);
        assert!(repo_symbol_names(&store, &uid).contains("gamma"));
    }

    /// nw-664: a source no watcher batch can graph (here unparsable) is absent
    /// by design. Startup tries it once, then must not replay it on every
    /// start; once it changes it is tried again.
    #[test]
    fn code_watcher_startup_does_not_replay_a_source_that_cannot_be_graphed() {
        let dir = tempfile::tempdir().unwrap();
        let (db_path, root, uid) = index_fixture_repo_on_disk(&dir, &[]);
        let store = Arc::new(GraphStore::open_or_create(&db_path).unwrap());
        let broken = root.join("src/broken.js");
        std::fs::write(&broken, "}}} ((( @@@ %%% ;;\n").unwrap();
        assert!(code_startup_batches(&db_path, &root, &store) > 0);
        assert!(!repo_file_paths(&store, &uid).contains("src/broken.js"));
        assert_eq!(
            code_startup_batches(&db_path, &root, &store),
            0,
            "an unchanged source that cannot be graphed must not be replayed per start"
        );

        // Counterweight: fixed while no watcher ran, it is replayed and lands.
        std::fs::write(&broken, "export function mended() { return 1; }\n").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&broken)
            .unwrap()
            .set_modified(std::time::SystemTime::now() + Duration::from_secs(5))
            .unwrap();
        assert!(code_startup_batches(&db_path, &root, &store) > 0);
        assert!(repo_symbol_names(&store, &uid).contains("mended"));
    }

    /// nw-664: a failed startup replay must not be dropped after a log line.
    /// The watcher still becomes ready and keeps watching, discloses the debt
    /// in the status sidecar nw-653 introduced, retries with backoff, and
    /// clears the disclosure once the retry lands.
    #[test]
    fn failed_code_startup_reconciliation_is_disclosed_and_retried_until_it_lands() {
        let dir = tempfile::tempdir().unwrap();
        let (db_path, root, uid) = index_fixture_repo_on_disk(&dir, &[]);
        std::fs::write(
            root.join("src/d.js"),
            "export function delta() { return 4; }\n",
        )
        .unwrap();
        let store = Arc::new(GraphStore::open_or_create(&db_path).unwrap());

        // The first replay fails at its batch lease; later ones succeed.
        let failed_once = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&failed_once);
        let factory: WatchMutationLeaseFactory = Arc::new(move |label: &'static str| {
            if label == "watch_code_batch" && !flag.swap(true, Ordering::SeqCst) {
                anyhow::bail!("injected replay failure");
            }
            Ok(Box::new(()) as Box<dyn WatchMutationLease>)
        });
        let pending = |db: &Path| {
            crate::index_md::load_skipped_notes_sidecar(db)
                .reconciliation_pending
                .into_values()
                .flatten()
                .map(|file| (file.path, file.reason))
                .collect::<Vec<_>>()
        };

        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let watcher = CodeWatcher::new(&db_path, &root, "test")
            .with_mutation_lease_factory(factory)
            .with_reconcile_retry_base(Duration::from_millis(300))
            .with_ready_callback(move || {
                let _ = ready_tx.send(());
            });
        let stop = watcher.shutdown_handle();
        let running = store.clone();
        let handle = std::thread::spawn(move || watcher.run_with_store(running, None));

        ready_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("a failed reconciliation must not prevent readiness");
        assert!(
            failed_once.load(Ordering::SeqCst),
            "the injected failure ran"
        );
        let owed = pending(&db_path);
        assert_eq!(
            owed.iter()
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>(),
            vec![root.join("src/d.js").to_string_lossy().into_owned()],
            "the owed reconciliation must be disclosed in status"
        );
        assert!(
            owed[0]
                .1
                .starts_with(crate::index_md::WATCH_RECONCILIATION_PENDING_REASON)
                && owed[0].1.contains("code watcher"),
            "{:?}",
            owed[0].1
        );
        assert_eq!(
            crate::index_md::skipped_notes_status_json(Some(&db_path)).0["reconciliation_pending"],
            serde_json::json!(1)
        );
        // Code debt is disclosed as pending reconciliation, never as a
        // "skipped note".
        assert!(
            crate::index_md::load_skipped_notes_sidecar(&db_path)
                .skipped
                .is_empty(),
            "code watcher debt must stay out of the skipped-notes list"
        );

        let deadline = Instant::now() + Duration::from_secs(30);
        while !pending(&db_path).is_empty() {
            assert!(
                !handle.is_finished(),
                "the watcher must keep running after a failed reconciliation"
            );
            assert!(
                Instant::now() < deadline,
                "the reconciliation was never retried"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        stop.stop();
        handle.join().unwrap().unwrap();
        assert!(
            repo_symbol_names(&store, &uid).contains("delta"),
            "the retry must ingest the source"
        );
    }

    fn code_debt(db_path: &Path) -> Vec<(String, String)> {
        let mut owed: Vec<(String, String)> = crate::index_md::load_skipped_notes_sidecar(db_path)
            .reconciliation_pending
            .into_values()
            .flatten()
            .map(|file| (file.path, file.reason))
            .collect();
        owed.sort();
        owed
    }

    /// nw-664 review: one source the watcher cannot read must not block the
    /// rest of the startup reconciliation. Before, a read error counted as
    /// drift and the batch turned it into `Skipped` for EVERY path, so the
    /// real lost edit next to it never landed and the replay was retried
    /// forever. Now the unreadable file is left as the graph has it and
    /// disclosed, the rest lands, and no retry is owed. (Python: it cannot
    /// contribute a contract, so the contract snapshot must not read it
    /// either — see the JS counterpart below.)
    #[cfg(unix)]
    #[test]
    fn code_startup_reconciliation_is_not_blocked_by_an_unreadable_source() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: `geteuid` takes no arguments, touches no memory and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (db_path, root, uid) =
            index_fixture_repo_on_disk(&dir, &[("tools/util.py", "def util():\n    return 1\n")]);
        let repo_url = format!("file://{}", root.display());
        std::fs::write(
            root.join("src/b.js"),
            "import { helper } from './a.js';\nexport function alphaEdited() { return helper() + 1; }\n",
        )
        .unwrap();
        let secret = root.join("src/secret.py");
        std::fs::write(&secret, "def secret():\n    return 1\n").unwrap();
        let graphed = root.join("tools/util.py");
        std::fs::write(&graphed, "def util_edited():\n    return 2\n").unwrap();
        for path in [&secret, &graphed] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).unwrap();
        }
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let watcher = CodeWatcher::new(&db_path, &root, "test");
        let attempt = watcher.attempt_reconciliation(&store, &uid, &repo_url, None, 0);
        for path in [&secret, &graphed] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        assert!(
            attempt.unwrap().is_none(),
            "an unreadable source must not leave the reconciliation owed a retry"
        );
        let names = repo_symbol_names(&store, &uid);
        assert!(
            names.contains("alphaEdited"),
            "the real lost edit lands: {names:?}"
        );
        assert!(
            names.contains("util") && !names.contains("secret"),
            "an unreadable source keeps what the graph had: {names:?}"
        );
        let owed = code_debt(&db_path);
        assert_eq!(
            owed.iter()
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>(),
            vec![
                secret.to_string_lossy().into_owned(),
                graphed.to_string_lossy().into_owned()
            ],
            "{owed:?}"
        );
        assert!(
            owed.iter()
                .all(|(_, reason)| reason.contains("could not read")),
            "{owed:?}"
        );

        // Readable again: the next start picks both up and the disclosure clears.
        assert!(
            watcher
                .attempt_reconciliation(&store, &uid, &repo_url, None, 0)
                .unwrap()
                .is_none()
        );
        let names = repo_symbol_names(&store, &uid);
        assert!(
            names.contains("util_edited") && names.contains("secret"),
            "{names:?}"
        );
        assert!(code_debt(&db_path).is_empty());
    }

    /// nw-664 final review (M6): the policy check that runs BEFORE the read
    /// (`source_policy_exclusion` -> `path_has_symlink`) propagated its stat
    /// error, so one listed source the watcher could not `lstat` (EACCES under
    /// a read-only, non-searchable directory) failed the WHOLE drift — the
    /// real lost edits beside it never landed and the attempt was retried on
    /// backoff indefinitely. It is now the same case as an unreadable source:
    /// disclosed, left as the graph has it, the rest reconciled.
    #[cfg(unix)]
    #[test]
    fn an_unstatable_source_does_not_fail_the_startup_drift() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: `geteuid` takes no arguments, touches no memory and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (db_path, root, uid) = index_fixture_repo_on_disk(&dir, &[]);
        let repo_url = format!("file://{}", root.display());
        std::fs::write(
            root.join("src/b.js"),
            "import { helper } from './a.js';\nexport function alphaEdited() { return helper() + 1; }\n",
        )
        .unwrap();
        // Listable (r) but not searchable (no x): the walk sees `blind.py`,
        // any stat beneath the directory is EACCES.
        let sealed = root.join("sealed");
        std::fs::create_dir_all(&sealed).unwrap();
        let blind = sealed.join("blind.py");
        std::fs::write(&blind, "def blind():\n    return 1\n").unwrap();
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o444)).unwrap();
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let watcher = CodeWatcher::new(&db_path, &root, "test");
        let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            watcher.attempt_reconciliation(&store, &uid, &repo_url, None, 0)
        }));
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o755)).unwrap();
        let attempt = attempt.unwrap();
        let owed = code_debt(&db_path);
        let outcome = match &attempt {
            Ok(None) => "reconciled".to_string(),
            Ok(Some(_)) => "owed a retry".to_string(),
            Err(error) => format!("refused: {error:#}"),
        };
        assert_eq!(
            outcome, "reconciled",
            "an unstatable source must not leave the reconciliation owed a retry: {owed:?}"
        );
        let names = repo_symbol_names(&store, &uid);
        assert!(
            names.contains("alphaEdited"),
            "the real lost edit lands: {names:?}"
        );
        // Counterweight: it is disclosed, not silently dropped.
        assert_eq!(
            owed.iter()
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>(),
            vec![blind.to_string_lossy().into_owned()],
            "{owed:?}"
        );
    }

    /// nw-664 review, the deliberate limit: an unreadable JS/TS/Java source
    /// may be a controller, and the watcher's whole-repo contract plan fails
    /// CLOSED on it (as the full index does) rather than publish a contract
    /// set that silently drops its routes. So a replay cannot land while one
    /// exists: it stays owed and retried, and the disclosure names the
    /// unreadable file as well as the blocked replay.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_contract_language_source_keeps_the_replay_owed_and_disclosed() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: `geteuid` takes no arguments, touches no memory and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (db_path, root, uid) = index_fixture_repo_on_disk(&dir, &[]);
        let repo_url = format!("file://{}", root.display());
        std::fs::write(
            root.join("src/d.js"),
            "export function delta() { return 4; }\n",
        )
        .unwrap();
        let locked = root.join("src/locked.js");
        std::fs::write(&locked, "export function locked() { return 1; }\n").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let watcher = CodeWatcher::new(&db_path, &root, "test");
        let attempt = watcher.attempt_reconciliation(&store, &uid, &repo_url, None, 0);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(attempt.unwrap().is_some(), "the blocked replay stays owed");
        let owed = code_debt(&db_path);
        let reason_for = |path: &Path| {
            owed.iter()
                .find(|(owed, _)| owed == &*path.to_string_lossy())
                .map(|(_, reason)| reason.clone())
                .unwrap_or_default()
        };
        assert!(reason_for(&locked).contains("could not read"), "{owed:?}");
        assert!(
            reason_for(&root.join("src/d.js")).contains("retrying"),
            "{owed:?}"
        );
        assert!(!repo_symbol_names(&store, &uid).contains("delta"));
    }

    /// nw-664 review: an indexed source rewritten as binary must not block
    /// the replay (it made the batch skip every path). Like the live batch,
    /// the graph keeps its previous symbols and the file is disclosed; a lost
    /// create beside it still lands and no retry is owed.
    #[test]
    fn code_startup_is_not_blocked_by_an_indexed_source_turned_binary() {
        let dir = tempfile::tempdir().unwrap();
        let (db_path, root, uid) = index_fixture_repo_on_disk(&dir, &[]);
        let repo_url = format!("file://{}", root.display());
        let binary = root.join("src/a.js");
        std::fs::write(&binary, b"export function helper() {}\0\0\n").unwrap();
        std::fs::write(
            root.join("src/d.js"),
            "export function delta() { return 4; }\n",
        )
        .unwrap();
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let watcher = CodeWatcher::new(&db_path, &root, "test");
        assert!(
            watcher
                .attempt_reconciliation(&store, &uid, &repo_url, None, 0)
                .unwrap()
                .is_none()
        );
        let names = repo_symbol_names(&store, &uid);
        assert!(names.contains("delta"), "{names:?}");
        assert!(names.contains("helper"), "previous symbols kept: {names:?}");
        let owed = code_debt(&db_path);
        assert_eq!(owed.len(), 1, "{owed:?}");
        assert_eq!(owed[0].0, binary.to_string_lossy());
        assert!(owed[0].1.contains("binary"), "{owed:?}");
    }

    /// nw-664 review: a retry owed by a watcher whose repo was removed (or
    /// pruned) meanwhile must not resurrect it: the batch re-creates a missing
    /// Repo node, and would rewrite the debt removal just cleared.
    #[test]
    fn code_startup_retry_does_not_resurrect_a_removed_repo() {
        let dir = tempfile::tempdir().unwrap();
        let (db_path, root, uid) = index_fixture_repo_on_disk(&dir, &[]);
        let repo_url = format!("file://{}", root.display());
        let store = GraphStore::open_or_create(&db_path).unwrap();
        store.bulk_delete_repo_files_and_symbols(&uid).unwrap();
        store.clear_repo_derived_nodes(&uid).unwrap();
        store.delete_repo_node(&uid).unwrap();
        let watcher = CodeWatcher::new(&db_path, &root, "test");
        let next = watcher
            .attempt_reconciliation(&store, &uid, &repo_url, None, 1)
            .unwrap();
        assert!(next.is_none(), "a removed repo is owed nothing");
        assert!(
            store.lookup_repo(&uid).unwrap().is_none(),
            "not resurrected"
        );
        assert!(code_debt(&db_path).is_empty());
    }

    /// nw-664 review: shutdown requested during the startup walk ends the
    /// reconciliation as a refusal — no replay, no debt.
    #[test]
    fn code_startup_reconciliation_honours_a_stop_request() {
        let dir = tempfile::tempdir().unwrap();
        let (db_path, root, uid) = index_fixture_repo_on_disk(&dir, &[]);
        let repo_url = format!("file://{}", root.display());
        std::fs::write(
            root.join("src/d.js"),
            "export function delta() { return 4; }\n",
        )
        .unwrap();
        let store = GraphStore::open_or_create(&db_path).unwrap();
        let watcher = CodeWatcher::new(&db_path, &root, "test");
        watcher.shutdown_handle().stop();
        let attempt = watcher.attempt_reconciliation(&store, &uid, &repo_url, None, 0);
        assert!(attempt.is_err_and(|error| error.downcast_ref::<WatchMutationRefused>().is_some()),);
        assert!(!repo_symbol_names(&store, &uid).contains("delta"));
        assert!(code_debt(&db_path).is_empty());
    }

    /// nw-664 review: unindexable memory is keyed by NANOSECOND mtime and
    /// size, so a fixed file restored with the same whole second and size
    /// (`rsync -a`, `tar x`) is retried rather than ignored.
    #[test]
    fn code_startup_retries_a_fixed_source_restored_within_the_same_second() {
        let dir = tempfile::tempdir().unwrap();
        let (db_path, root, uid) = index_fixture_repo_on_disk(&dir, &[]);
        let store = Arc::new(GraphStore::open_or_create(&db_path).unwrap());
        let broken = root.join("src/broken.js");
        let broken_body = "}}} ((( @@@ %%% ;;\n";
        std::fs::write(&broken, broken_body).unwrap();
        assert!(code_startup_batches(&db_path, &root, &store) > 0);
        let recorded = std::fs::metadata(&broken).unwrap().modified().unwrap();
        let since_epoch = recorded.duration_since(std::time::UNIX_EPOCH).unwrap();
        let same_second = std::time::UNIX_EPOCH
            + Duration::from_secs(since_epoch.as_secs())
            + Duration::from_nanos(if since_epoch.subsec_nanos() == 123_456_789 {
                987_654_321
            } else {
                123_456_789
            });
        let mut fixed = String::from("function mended(){}");
        while fixed.len() < broken_body.len() {
            fixed.push(' ');
        }
        assert_eq!(fixed.len(), broken_body.len());
        std::fs::write(&broken, &fixed).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&broken)
            .unwrap()
            .set_modified(same_second)
            .unwrap();
        assert!(code_startup_batches(&db_path, &root, &store) > 0);
        assert!(repo_symbol_names(&store, &uid).contains("mended"));
    }

    /// Run a code watcher on a thread until `stop`; returns once it is ready.
    #[cfg(unix)]
    fn spawn_live_code_watcher(
        watcher: CodeWatcher,
        store: Arc<GraphStore>,
    ) -> (
        ShutdownHandle,
        std::thread::JoinHandle<Result<(), anyhow::Error>>,
    ) {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let watcher = watcher.with_debounce_ms(100).with_ready_callback(move || {
            let _ = ready_tx.send(());
        });
        let stop = watcher.shutdown_handle();
        let handle = std::thread::spawn(move || watcher.run_with_store(store, None));
        ready_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("code watcher should become ready");
        // Let the filesystem subscription settle before the test edits.
        std::thread::sleep(Duration::from_millis(300));
        (stop, handle)
    }

    #[cfg(unix)]
    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for: {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// nw-664 review: a "could not read" disclosure must not outlive the
    /// problem until the next restart. Once a live batch reads (or deletes)
    /// the path and publishes, `brain status` stops showing it.
    #[cfg(unix)]
    #[test]
    fn a_published_live_batch_clears_a_stale_unreadable_disclosure() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: `geteuid` takes no arguments, touches no memory and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (db_path, root, uid) =
            index_fixture_repo_on_disk(&dir, &[("tools/util.py", "def util():\n    return 1\n")]);
        let util = root.join("tools/util.py");
        std::fs::write(&util, "def util_gap():\n    return 2\n").unwrap();
        std::fs::set_permissions(&util, std::fs::Permissions::from_mode(0o000)).unwrap();
        let store = Arc::new(GraphStore::open_or_create(&db_path).unwrap());
        let (stop, handle) =
            spawn_live_code_watcher(CodeWatcher::new(&db_path, &root, "test"), store.clone());
        let disclosed = code_debt(&db_path);
        std::fs::set_permissions(&util, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            disclosed
                .iter()
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>(),
            vec![util.to_string_lossy().into_owned()],
            "precondition: startup disclosed the unreadable source"
        );

        std::fs::write(&util, "def util_live():\n    return 3\n").unwrap();
        wait_until("the live edit to land", || {
            repo_symbol_names(&store, &uid).contains("util_live")
        });
        wait_until("the stale disclosure to clear", || {
            code_debt(&db_path).is_empty()
        });
        stop.stop();
        handle.join().unwrap().unwrap();
    }

    /// nw-669: the LIVE path dropped a `Skipped` batch after a warning — no
    /// retry, no disclosure. With one unreadable contract-language source in
    /// the repo every live edit was silently lost. A skipped live batch is
    /// now disclosed as owed and retried on the shared backoff until it lands.
    #[cfg(unix)]
    #[test]
    fn a_skipped_live_batch_is_disclosed_and_retried_until_it_lands() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: `geteuid` takes no arguments, touches no memory and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (db_path, root, uid) = index_fixture_repo_on_disk(&dir, &[]);
        let locked = root.join("src/locked.js");
        std::fs::write(&locked, "export function locked() { return 1; }\n").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let store = Arc::new(GraphStore::open_or_create(&db_path).unwrap());
        let watcher = CodeWatcher::new(&db_path, &root, "test")
            .with_reconcile_retry_base(Duration::from_millis(300));
        let (stop, handle) = spawn_live_code_watcher(watcher, store.clone());

        let edited = root.join("src/b.js");
        std::fs::write(
            &edited,
            "import { helper } from './a.js';\nexport function alphaLive() { return helper(); }\n",
        )
        .unwrap();
        let edited_key = edited.to_string_lossy().into_owned();
        let owed = || {
            code_debt(&db_path)
                .into_iter()
                .any(|(path, reason)| path == edited_key && reason.contains("retrying"))
        };
        let blocked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            wait_until("the skipped live edit to be disclosed as owed", owed);
            // Checked while the file is still locked, before anything can land.
            assert!(
                !handle.is_finished(),
                "the watcher keeps running while the edit is owed"
            );
            assert!(!repo_symbol_names(&store, &uid).contains("alphaLive"));
        }));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
        if let Err(panic) = blocked {
            stop.stop();
            let _ = handle.join();
            std::panic::resume_unwind(panic);
        }

        wait_until("the retried edit to land", || {
            repo_symbol_names(&store, &uid).contains("alphaLive")
        });
        wait_until("the disclosure to clear", || code_debt(&db_path).is_empty());
        stop.stop();
        handle.join().unwrap().unwrap();
    }

    /// nw-669 review counterweight: while a retry is already owed and its
    /// backoff is not due, another skipped live batch must NOT trigger an
    /// extra reconciliation. The owed retry's recomputed drift already covers
    /// the new paths; resetting it to "now" made every autosave in a
    /// permanently blocked repo cost a walk + a failing replay.
    #[cfg(unix)]
    #[test]
    fn a_skipped_live_batch_does_not_hurry_an_owed_retry() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::AtomicU32;
        // SAFETY: `geteuid` takes no arguments, touches no memory and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (db_path, root, _uid) = index_fixture_repo_on_disk(&dir, &[]);
        let locked = root.join("src/locked.js");
        std::fs::write(&locked, "export function locked() { return 1; }\n").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let batches = Arc::new(AtomicU32::new(0));
        let counted = Arc::clone(&batches);
        let factory: WatchMutationLeaseFactory = Arc::new(move |label: &'static str| {
            if label == "watch_code_batch" {
                counted.fetch_add(1, Ordering::SeqCst);
            }
            Ok(Box::new(()) as Box<dyn WatchMutationLease>)
        });
        let store = Arc::new(GraphStore::open_or_create(&db_path).unwrap());
        let watcher = CodeWatcher::new(&db_path, &root, "test")
            .with_mutation_lease_factory(factory)
            .with_reconcile_retry_base(Duration::from_secs(600));
        let (stop, handle) = spawn_live_code_watcher(watcher, store);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let first = root.join("src/b.js");
            std::fs::write(&first, "export function first() { return 1; }\n").unwrap();
            let first_key = first.to_string_lossy().into_owned();
            // One live batch, then the immediate reconciliation's replay:
            // both fail, and a retry is owed ten minutes out.
            wait_until("the first skipped edit to be disclosed as owed", || {
                code_debt(&db_path)
                    .iter()
                    .any(|(path, reason)| path == &first_key && reason.contains("retrying"))
            });
            let owed_once = batches.load(Ordering::SeqCst);
            assert_eq!(owed_once, 2, "one live batch plus one replay");

            std::fs::write(
                root.join("src/c.js"),
                "export function second() { return 2; }\n",
            )
            .unwrap();
            wait_until("the second live batch", || {
                batches.load(Ordering::SeqCst) > owed_once
            });
            std::thread::sleep(Duration::from_millis(1500));
            assert_eq!(
                batches.load(Ordering::SeqCst),
                owed_once + 1,
                "a second skipped batch must not trigger another reconciliation \
                 while one is owed and not yet due"
            );
        }));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
        stop.stop();
        let joined = handle.join();
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
        joined.unwrap().unwrap();
    }
}
